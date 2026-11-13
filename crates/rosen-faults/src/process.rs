//! A process the fault plan can kill and start again.

use anyhow::Context;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::{Child, ChildStdout, Command};

/// Builds the command for each start, so a restart runs with the same
/// arguments and environment as the first start.
pub type Recipe = Box<dyn Fn() -> Command + Send>;

pub struct Slot {
    pub name: String,
    recipe: Recipe,
    child: Option<Child>,
    /// Every start's stderr is appended here, so a crash's last words survive.
    log: Option<PathBuf>,
    pub starts: u32,
    pub kills: u32,
    /// Exits the process made by itself before a planned kill reached it.
    /// Kept, because the kill replaces the child and its status with it.
    pub own_exits: Vec<std::process::ExitStatus>,
}

impl Slot {
    pub fn new(name: impl Into<String>, recipe: Recipe) -> Self {
        Self {
            name: name.into(),
            recipe,
            child: None,
            log: None,
            starts: 0,
            kills: 0,
            own_exits: Vec::new(),
        }
    }

    /// Appends the process's stderr to `path` instead of the caller's.
    pub fn log_to(mut self, path: PathBuf) -> Self {
        self.log = Some(path);
        self
    }

    /// Starts the process. Its stdout is piped and handed back, so the
    /// caller can read what it reports.
    pub fn start(&mut self) -> anyhow::Result<Option<ChildStdout>> {
        let mut command = (self.recipe)();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = &self.log {
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(file, "--- {} start {} ---", self.name, self.starts + 1)?;
            command.stderr(Stdio::from(file));
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", self.name))?;
        let stdout = child.stdout.take();
        self.child = Some(child);
        self.starts += 1;
        Ok(stdout)
    }

    /// Kills the process at once. Tokio's kill is `TerminateProcess` on
    /// Windows and SIGKILL elsewhere: the process gets no chance to flush,
    /// release locks or say goodbye, which is the point.
    pub async fn kill(&mut self) -> anyhow::Result<()> {
        let mut child = self
            .child
            .take()
            .with_context(|| format!("{} is not running", self.name))?;
        match child.try_wait()? {
            Some(status) => self.own_exits.push(status),
            None => {
                child.kill().await?;
                self.kills += 1;
            }
        }
        Ok(())
    }

    /// True while the process runs. A process that exited on its own is
    /// reported by [`Slot::exited`].
    pub fn running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// The exit status, if the process ended without being killed.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    /// Stops the process for good, killing it if it is still running.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}
