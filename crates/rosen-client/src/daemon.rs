//! Daemon discovery and start (SPEC D8): find the machine's server, and launch it
//! in the background when none is running.

use crate::{Client, Error};
use rosen_protocol::Endpoint;
use std::path::PathBuf;
use std::time::Duration;

/// The environment variable naming the `rosen-server` binary to launch.
pub const SERVER_BINARY_ENV: &str = "ROSEN_SERVER";

/// How long to wait for a newly launched daemon to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The exit code `rosen-server` uses when another server already owns the
/// endpoint; a launcher that loses that race just connects to the winner.
const EXIT_ALREADY_RUNNING: i32 = 3;

/// Connects to the running daemon, starting one first if none is running.
pub async fn connect_or_start(endpoint: &Endpoint) -> Result<Client, Error> {
    match Client::connect(endpoint).await {
        Err(Error::NotRunning(_)) => {}
        connected => return connected,
    }
    let mut daemon = spawn_daemon(endpoint)?;
    wait_until_ready(endpoint, &mut daemon).await
}

/// The `rosen-server` binary: `$ROSEN_SERVER`, else the one installed next to the
/// current executable, else whatever `rosen-server` is on the PATH.
pub fn server_binary() -> PathBuf {
    if let Some(path) = std::env::var_os(SERVER_BINARY_ENV) {
        return PathBuf::from(path);
    }
    let file_name = format!("rosen-server{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&file_name)))
        .filter(|sibling| sibling.is_file())
        .unwrap_or_else(|| PathBuf::from(file_name))
}

/// The launched daemon, polled until it listens or exits.
#[cfg(windows)]
type Daemon = rosen_platform::process::DetachedProcess;
#[cfg(not(windows))]
type Daemon = std::process::Child;

/// Starts the daemon detached from the launcher's console and job, so closing
/// the terminal or the IDE that started it does not stop the machine's server.
/// On Windows it inherits no handles: a daemon holding the pipe a shell gave the
/// launcher as stdout would keep `rosen ... | cat` from ever finishing.
/// Leaving the launcher's job fails when the job forbids it; the daemon is then
/// started inside the job.
#[cfg(windows)]
fn spawn_daemon(endpoint: &Endpoint) -> Result<Daemon, Error> {
    use rosen_platform::process::spawn_detached;
    use std::ffi::OsStr;
    let binary = resolve_on_path(server_binary());
    let args = [OsStr::new("--endpoint"), OsStr::new(endpoint.as_str())];
    spawn_detached(&binary, &args, true)
        .or_else(|_| spawn_detached(&binary, &args, false))
        .map_err(|err| Error::Start(format!("could not run {}: {err}", binary.display())))
}

/// A bare program name, as `server_binary` falls back to, found on the PATH the
/// way `std::process::Command` finds it; CreateProcessW would also search the
/// current directory.
#[cfg(windows)]
fn resolve_on_path(binary: PathBuf) -> PathBuf {
    if binary.components().count() != 1 {
        return binary;
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&binary))
        .find(|candidate| candidate.is_file())
        .unwrap_or(binary)
}

#[cfg(not(windows))]
fn spawn_daemon(endpoint: &Endpoint) -> Result<Daemon, Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let binary = server_binary();
    Command::new(&binary)
        .arg("--endpoint")
        .arg(endpoint.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A new process group keeps the terminal's Ctrl-C away from the daemon.
        .process_group(0)
        .spawn()
        .map_err(|err| Error::Start(format!("could not run {}: {err}", binary.display())))
}

/// The daemon's exit code once it has exited; `Some(None)` when a signal ended it.
fn exit_code(daemon: &mut Daemon) -> Option<Option<i32>> {
    #[cfg(windows)]
    let code = daemon.try_wait().ok()?.map(|code| Some(code as i32));
    #[cfg(not(windows))]
    let code = daemon.try_wait().ok()?.map(|status| status.code());
    code
}

/// Retries the connection until the new daemon answers, it exits, or the
/// start timeout passes.
async fn wait_until_ready(endpoint: &Endpoint, daemon: &mut Daemon) -> Result<Client, Error> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut delay = Duration::from_millis(10);
    loop {
        match Client::connect(endpoint).await {
            Err(Error::NotRunning(_)) => {}
            connected => return connected,
        }
        if let Some(code) = exit_code(daemon) {
            if code != Some(EXIT_ALREADY_RUNNING) {
                let how = code.map_or("on a signal".to_owned(), |code| format!("with code {code}"));
                return Err(Error::Start(format!("rosen-server exited {how}")));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Start(format!(
                "rosen-server did not start listening on {endpoint} within {START_TIMEOUT:?}"
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}
