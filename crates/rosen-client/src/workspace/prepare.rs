//! Running the project's prepare commands in a ready workspace (SPEC R3).
//!
//! The pool worker is the only caller, and it prepares one workspace at a
//! time. Daemons share the machine's queue for prepares. Each command runs
//! at below-normal priority, inside the prepare's resource budget.

use std::path::Path;
use std::process::ExitStatus;
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::Duration;

#[cfg(not(windows))]
use tokio::{io::AsyncReadExt, process::Command};

use crate::run_queue::{self, RunBudget, RunClass, RunQueue, Turn, THREAD_VARIABLES};

/// A prepare that has not finished in this long is treated as failed, so a
/// hung command cannot stop the pool worker from following the base.
const COMMAND_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How often a prepare waiting for its turn looks at the queue again. A
/// prepare is in no hurry, and each look reads the queue's folder.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// How much of a failed command's output is kept for the daemon's log.
const OUTPUT_TAIL: usize = 4096;

/// Runs every command in order from `root`, stopping at the first failure.
/// The error names the command and ends with the tail of its output.
pub(super) async fn run(root: &Path, commands: &[String]) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    let turn = turn(root)
        .await
        .map_err(|error| format!("prepare queue: {error}"))?;
    let budget = turn
        .as_ref()
        .map_or_else(|| RunClass::Prepare.budget(), Turn::budget);
    for command in commands {
        run_one(root, command, budget)
            .await
            .map_err(|why| format!("prepare command `{command}` in {}: {why}", root.display()))?;
    }
    Ok(())
}

/// Library processes can embed the pool without enlisting in the daemon queue.
async fn turn(root: &Path) -> std::io::Result<Option<Turn>> {
    if !run_queue::prepares_queue() {
        return Ok(None);
    }
    let note = format!("prepare {}", root.display());
    let waiter = RunQueue::machine()?.wait(RunClass::Prepare, &note)?;
    loop {
        if let Some(turn) = waiter.try_start()? {
            return Ok(Some(turn));
        }
        // Dropping this future leaves the line: the waiter goes with it.
        tokio::time::sleep(WAIT_POLL).await;
    }
}

async fn run_one(root: &Path, command: &str, budget: RunBudget) -> Result<(), String> {
    let (status, output) = tokio::time::timeout(COMMAND_LIMIT, run_shell(root, command, budget))
        .await
        .map_err(|_| {
            format!(
                "did not finish within {} minutes",
                COMMAND_LIMIT.as_secs() / 60
            )
        })?
        .map_err(|error| error.to_string())?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "{status}\n{}",
        String::from_utf8_lossy(&output).trim_end()
    ))
}

#[cfg(windows)]
async fn run_shell(
    root: &Path,
    command: &str,
    budget: RunBudget,
) -> std::io::Result<(ExitStatus, Vec<u8>)> {
    use rosen_platform::run_tree::ProcessTree;
    use std::ffi::OsStr;
    use std::io::Read;

    let jobs = budget.jobs.to_string();
    let mut environment = vec![(
        OsStr::new(run_queue::HELD_ENV),
        OsStr::new(RunClass::Prepare.name()),
    )];
    environment.extend(THREAD_VARIABLES.map(|name| (OsStr::new(name), OsStr::new(&jobs))));
    let (tree, mut pipe) = ProcessTree::spawn_shell(root, command, &environment, budget.memory)?;
    let output = tokio::task::spawn_blocking(move || {
        let mut tail = Vec::new();
        let mut buffer = [0; OUTPUT_TAIL];
        while let Ok(size @ 1..) = pipe.read(&mut buffer) {
            keep_tail(&mut tail, &buffer[..size]);
        }
        tail
    });
    let status = loop {
        if let Some(status) = tree.try_wait()? {
            break status;
        }
        tokio::time::sleep(run_queue::POLL).await;
    };
    Ok((status, output.await.map_err(std::io::Error::other)?))
}

#[cfg(not(windows))]
async fn run_shell(
    root: &Path,
    command: &str,
    budget: RunBudget,
) -> std::io::Result<(ExitStatus, Vec<u8>)> {
    let jobs = budget.jobs.to_string();
    let mut child = shell(command)
        .current_dir(root)
        // A prepare command that runs `rosen run` runs in this prepare's turn.
        .env(run_queue::HELD_ENV, RunClass::Prepare.name())
        .envs(THREAD_VARIABLES.map(|name| (name, &jobs)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    // Drop the process group guard while the child still owns its PID.
    let tree = TreeGuard(child.id());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (status, mut out, err) = tokio::join!(child.wait(), read_tail(stdout), read_tail(stderr));
    let status = status?;
    tree.finished();
    keep_tail(&mut out, &err);
    Ok((status, out))
}

fn keep_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    let discard = (tail.len() + bytes.len()).saturating_sub(OUTPUT_TAIL);
    tail.drain(..discard);
    tail.extend_from_slice(bytes);
}

/// Ends a prepare's whole process tree if dropped before it finished: at
/// the time limit, or when the daemon stops the pool worker. `kill_on_drop`
/// ends only the shell, and a build it started, such as cargo or rustc,
/// would keep running with files open in the workspace, so removing the
/// slot or template would fail until it exited (issue #169).
#[cfg(not(windows))]
struct TreeGuard(Option<u32>);

#[cfg(not(windows))]
impl TreeGuard {
    fn finished(mut self) {
        self.0 = None;
    }
}

#[cfg(not(windows))]
impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // Waited for, so the files are closed before the caller goes
            // on to remove the workspace. Rare: only a cut-short prepare.
            let _ = kill_tree(pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// The shell leads its own process group (see `shell`), which everything
/// it starts joins.
#[cfg(not(windows))]
fn kill_tree(pid: u32) -> std::process::Command {
    let mut kill = std::process::Command::new("kill");
    kill.args(["-KILL", "--", &format!("-{pid}")]);
    kill
}

#[cfg(not(windows))]
async fn read_tail<R: AsyncReadExt + Unpin>(pipe: Option<R>) -> Vec<u8> {
    let mut tail = Vec::new();
    if let Some(mut pipe) = pipe {
        let mut buffer = [0; OUTPUT_TAIL];
        while let Ok(size @ 1..) = pipe.read(&mut buffer).await {
            keep_tail(&mut tail, &buffer[..size]);
        }
    }
    tail
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let mut shell = Command::new("nice");
    shell
        .args(["-n", "10", "sh", "-c", command])
        .process_group(0);
    shell
}
