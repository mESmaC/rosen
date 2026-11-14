//! Running the project's prepare commands in a ready workspace (SPEC R3).
//!
//! The pool worker is the only caller, and it prepares one workspace at a
//! time, so a machine never runs two prepares at once. Each command runs at
//! below-normal priority, so agents' own builds go first.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// A prepare that has not finished in this long is treated as failed, so a
/// hung command cannot stop the pool worker from following the base.
const COMMAND_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How much of a failed command's output is kept for the daemon's log.
const OUTPUT_TAIL: usize = 4096;

/// Runs every command in order from `root`, stopping at the first failure.
/// The error names the command and ends with the tail of its output.
pub(super) async fn run(root: &Path, commands: &[String]) -> Result<(), String> {
    for command in commands {
        run_one(root, command)
            .await
            .map_err(|why| format!("prepare command `{command}` in {}: {why}", root.display()))?;
    }
    Ok(())
}

async fn run_one(root: &Path, command: &str) -> Result<(), String> {
    let mut child = shell(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    // Declared after `child`, so it drops first, while the child's handle
    // still keeps its process ID from being reused.
    let tree = TreeGuard(child.id());
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    // Both pipes are drained while the command runs, so a chatty build never
    // blocks on a full pipe.
    let finished = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let read_out = read_all(stdout.as_mut(), &mut out);
        let read_err = read_all(stderr.as_mut(), &mut err);
        let (status, _, _) = tokio::join!(child.wait(), read_out, read_err);
        out.extend_from_slice(&err);
        (status, out)
    };
    let (status, output) = tokio::time::timeout(COMMAND_LIMIT, finished)
        .await
        .map_err(|_| {
            format!(
                "did not finish within {} minutes",
                COMMAND_LIMIT.as_secs() / 60
            )
        })?;
    tree.finished();
    let status = status.map_err(|error| error.to_string())?;
    if status.success() {
        return Ok(());
    }
    let start = output.len().saturating_sub(OUTPUT_TAIL);
    Err(format!(
        "{status}\n{}",
        String::from_utf8_lossy(&output[start..]).trim_end()
    ))
}

/// Ends a prepare's whole process tree if dropped before it finished: at
/// the time limit, or when the daemon stops the pool worker. `kill_on_drop`
/// ends only the shell, and a build it started, such as cargo or rustc,
/// would keep running with files open in the workspace, so removing the
/// slot or template would fail until it exited (issue #169).
struct TreeGuard(Option<u32>);

impl TreeGuard {
    fn finished(mut self) {
        self.0 = None;
    }
}

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

/// `/T` finds the shell's descendants by parent process ID, which holds
/// while the shell still waits on them, as it does at the time limit.
#[cfg(windows)]
fn kill_tree(pid: u32) -> std::process::Command {
    let mut kill = std::process::Command::new("taskkill");
    kill.args(["/F", "/T", "/PID", &pid.to_string()]);
    kill
}

/// The shell leads its own process group (see `shell`), which everything
/// it starts joins.
#[cfg(not(windows))]
fn kill_tree(pid: u32) -> std::process::Command {
    let mut kill = std::process::Command::new("kill");
    kill.args(["-KILL", "--", &format!("-{pid}")]);
    kill
}

async fn read_all<R: AsyncReadExt + Unpin>(pipe: Option<&mut R>, into: &mut Vec<u8>) {
    if let Some(pipe) = pipe {
        let _ = pipe.read_to_end(into).await;
    }
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    // BELOW_NORMAL_PRIORITY_CLASS; children such as rustc inherit it.
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    let mut shell = Command::new("cmd");
    shell
        .arg("/C")
        .raw_arg(command)
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
    shell
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let mut shell = Command::new("nice");
    shell.args(["-n", "10", "sh", "-c", command]).process_group(0);
    shell
}
