//! The daemon's log file (`--log`, SPEC R3): standard error, appended to a
//! file that is set aside as `<FILE>.old` once past [`ROTATE_BYTES`].
//!
//! The size is checked at start and every [`CHECK_EVERY`] while the daemon
//! runs, since some diagnostics repeat about once a second and a daemon can
//! run for weeks. Two daemons can share one log: one that loses a start race
//! sets the winner's file aside before it exits. So each check also makes
//! sure this daemon still writes the file at the log's path, and follows the
//! path when another daemon has moved the file away.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use same_file::Handle;

/// A log past this size is set aside as `<FILE>.old`, replacing the
/// previous one, so at most twice this much is kept.
const ROTATE_BYTES: u64 = 4 << 20;
/// Writing 4 MiB within a minute would take a far louder daemon than any
/// diagnostic here makes.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// The log this daemon's standard error goes to.
pub struct DaemonLog {
    path: PathBuf,
    /// A second handle on the file standard error writes to, for its size
    /// and identity.
    file: File,
}

impl DaemonLog {
    /// Points standard error at `path`, appending, after setting a large
    /// log aside.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut log = Self {
            path: path.to_owned(),
            file: open_append(path)?,
        };
        if log.file.metadata()?.len() > ROTATE_BYTES {
            log.reopen(true)?;
        } else {
            rosen_platform::redirect_stderr(log.file.try_clone()?)?;
        }
        Ok(log)
    }

    /// Checks the log every [`CHECK_EVERY`] for the rest of the process.
    pub fn keep_rotating(mut self) {
        std::thread::spawn(move || loop {
            std::thread::sleep(CHECK_EVERY);
            if let Err(err) = self.check() {
                eprintln!("rosen-server: cannot rotate {}: {err}", self.path.display());
            }
        });
    }

    fn check(&mut self) -> io::Result<()> {
        let ours = Handle::from_file(self.file.try_clone()?)?;
        // A missing file counts as moved: the path is opened afresh.
        let at_path = Handle::from_path(&self.path).ok();
        if at_path.as_ref() != Some(&ours) {
            return self.reopen(false);
        }
        if self.file.metadata()?.len() > ROTATE_BYTES {
            return self.reopen(true);
        }
        Ok(())
    }

    /// Opens the file at the log's path for standard error, first setting
    /// the current one aside when `rotate`. Windows keeps a replaced
    /// standard handle open, one handle per rotation; Unix closes it.
    fn reopen(&mut self, rotate: bool) -> io::Result<()> {
        if rotate {
            let mut old = self.path.as_os_str().to_owned();
            old.push(".old");
            // Another daemon sharing the log may have just done the same;
            // the file is then opened at its path as it now stands.
            let _ = fs::rename(&self.path, old);
        }
        let file = open_append(&self.path)?;
        rosen_platform::redirect_stderr(file.try_clone()?)?;
        self.file = file;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}
