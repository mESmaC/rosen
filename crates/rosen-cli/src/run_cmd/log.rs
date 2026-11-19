use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rosen_client::snapshot::{WorkspaceMarker, STATE_DIR};

const TAIL_LINES: usize = 40;

pub(super) struct RunLog {
    path: PathBuf,
}

impl RunLog {
    pub(super) fn create() -> io::Result<Self> {
        let cwd = std::env::current_dir()?;
        let dir = WorkspaceMarker::find_root(&cwd).map_or_else(
            || std::env::temp_dir().join("rosen-run-logs"),
            |root| root.join(STATE_DIR).join("run-logs"),
        );
        fs::create_dir_all(&dir)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let mut options = File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        for sequence in 0_u64.. {
            let path = dir.join(format!("run-{stamp}-{pid}-{sequence}.log"));
            match options.open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn report(
        &self,
        command: &[String],
        result: &io::Result<ExitStatus>,
        duration: Duration,
    ) {
        let mut out = io::stdout().lock();
        let status = result.as_ref().map_or_else(
            |error| format!("failed to run: {error}"),
            |status| status.to_string(),
        );
        let _ = writeln!(
            out,
            "{} ({status}, {:.2}s)",
            command.join(" "),
            duration.as_secs_f64()
        );
        if !result.as_ref().is_ok_and(|status| status.success()) {
            let cargo = Path::new(&command[0])
                .file_stem()
                .is_some_and(|name| name.eq_ignore_ascii_case("cargo"));
            if let Err(error) = File::open(&self.path)
                .and_then(|file| write_failure(BufReader::new(file), &mut out, cargo))
            {
                let _ = writeln!(out, "Could not read log summary: {error}");
            }
        }
        // A closed output pipe must not replace the command's exit status.
        let _ = writeln!(out, "Log: {}", self.path.display());
    }
}

fn write_failure(mut log: impl BufRead, mut out: impl Write, cargo: bool) -> io::Result<()> {
    let mut detail = FailureDetail::default();
    let mut found = false;
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    let mut bytes = Vec::new();
    while log.read_until(b'\n', &mut bytes)? != 0 {
        let line = String::from_utf8_lossy(&bytes);
        let line = line.trim_end_matches(['\r', '\n']);
        if detail.includes(&without_sgr(line), cargo) {
            writeln!(out, "{line}")?;
            found = true;
        } else if !found {
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.to_owned());
        }
        bytes.clear();
    }
    if !found {
        for line in tail {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct FailureDetail {
    running_tests: bool,
    test_failures: bool,
    cargo_diagnostic: bool,
}

impl FailureDetail {
    fn includes(&mut self, line: &str, cargo: bool) -> bool {
        if self.test_failures {
            if line.starts_with("test result:") {
                self.running_tests = false;
                self.test_failures = false;
            }
            return true;
        }
        if let Some((count, noun)) = line
            .strip_prefix("running ")
            .and_then(|s| s.split_once(' '))
        {
            self.running_tests = matches!(noun, "test" | "tests") && count.parse::<usize>().is_ok();
        }
        if line.starts_with("test result:") {
            self.running_tests = false;
        }
        if self.running_tests && line == "failures:" {
            self.test_failures = true;
            self.cargo_diagnostic = false;
            return true;
        }
        if !cargo {
            return false;
        }
        if ["error:", "error[", "warning:", "warning["]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            self.cargo_diagnostic = true;
            return true;
        }
        self.cargo_diagnostic &= diagnostic_continuation(line);
        self.cargo_diagnostic
    }
}

fn diagnostic_continuation(line: &str) -> bool {
    if line.is_empty() || line == "Caused by:" {
        return true;
    }
    let trimmed = line.trim_start();
    match trimmed.split_whitespace().next() {
        Some(
            "Adding" | "Blocking" | "Checking" | "Compiling" | "Documenting" | "Downloaded"
            | "Downloading" | "Finished" | "Fresh" | "Locking" | "Removing" | "Running"
            | "Updating",
        ) => false,
        _ => {
            trimmed.len() < line.len()
                || trimmed.starts_with(['|', '='])
                || trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit)
        }
    }
}

fn without_sgr(line: &str) -> Cow<'_, str> {
    let mut parts = line.split("\x1b[");
    let first = parts.next().unwrap_or_default();
    let Some(second) = parts.next() else {
        return Cow::Borrowed(first);
    };
    let mut plain = first.to_owned();
    for part in std::iter::once(second).chain(parts) {
        let parameters =
            part.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, ';' | ':'));
        if let Some(text) = parameters.strip_prefix('m') {
            plain.push_str(text);
        } else {
            plain.push_str("\x1b[");
            plain.push_str(part);
        }
    }
    Cow::Owned(plain)
}
