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
    shell.args(["-n", "10", "sh", "-c", command]);
    shell
}
