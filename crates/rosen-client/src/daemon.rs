//! Daemon discovery and start (SPEC D8): find the machine's server, and launch it
//! in the background when none is running.

use crate::{Client, ClientError};
use rosen_protocol::Endpoint;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// The environment variable naming the `rosen-server` binary to launch.
pub const SERVER_BINARY_ENV: &str = "ROSEN_SERVER";

/// How long to wait for a newly launched daemon to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The exit code `rosen-server` uses when another server already owns the
/// endpoint; a launcher that loses that race just connects to the winner.
const EXIT_ALREADY_RUNNING: i32 = 3;

/// Connects to the running daemon, starting one first if none is running.
pub async fn connect_or_start(endpoint: &Endpoint) -> Result<Client, ClientError> {
    match Client::connect(endpoint).await {
        Err(ClientError::NotRunning(_)) => {}
        connected => return connected,
    }
    let mut child = spawn_daemon(endpoint)?;
    wait_until_ready(endpoint, &mut child).await
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

fn spawn_daemon(endpoint: &Endpoint) -> Result<Child, ClientError> {
    let binary = server_binary();
    let mut command = Command::new(&binary);
    command
        .arg("--endpoint")
        .arg(endpoint.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command, true);
    command
        .spawn()
        .or_else(|_| detach(&mut command, false).spawn())
        .map_err(|err| ClientError::Start(format!("could not run {}: {err}", binary.display())))
}

/// Detaches the daemon from the launcher's console and job, so closing the
/// terminal or the IDE that started it does not stop the machine's server.
/// `leave_job` also takes the daemon out of the launcher's job object; spawning
/// fails when the job forbids that, and spawn_daemon then retries without it.
#[cfg(windows)]
fn detach(command: &mut Command, leave_job: bool) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let breakaway = if leave_job {
        CREATE_BREAKAWAY_FROM_JOB
    } else {
        0
    };
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | breakaway)
}

#[cfg(not(windows))]
fn detach(command: &mut Command, _leave_job: bool) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // A new process group keeps the terminal's Ctrl-C away from the daemon.
    command.process_group(0)
}

/// Retries the connection until the new daemon answers, it exits, or the
/// start timeout passes.
async fn wait_until_ready(endpoint: &Endpoint, child: &mut Child) -> Result<Client, ClientError> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut delay = Duration::from_millis(10);
    loop {
        match Client::connect(endpoint).await {
            Err(ClientError::NotRunning(_)) => {}
            connected => return connected,
        }
        if let Ok(Some(status)) = child.try_wait() {
            if status.code() != Some(EXIT_ALREADY_RUNNING) {
                return Err(ClientError::Start(format!(
                    "rosen-server exited with {status}"
                )));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ClientError::Start(format!(
                "rosen-server did not start listening on {endpoint} within {START_TIMEOUT:?}"
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}
