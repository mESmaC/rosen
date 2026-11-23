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
    let mut extractor = FailureExtractor::default();
    let mut bytes = Vec::new();
    while log.read_until(b'\n', &mut bytes)? != 0 {
        let line = String::from_utf8_lossy(&bytes);
        let line = line.trim_end_matches(['\r', '\n']);
        extractor.push_line(line, cargo, &mut out)?;
        bytes.clear();
    }
    extractor.finish(&mut out)
}

#[derive(Default)]
struct FailureExtractor {
    test_run: Option<TestRun>,
    cargo_diagnostic: bool,
    found: bool,
    tail: VecDeque<String>,
}

impl FailureExtractor {
    fn push_line(&mut self, line: &str, cargo: bool, out: &mut impl Write) -> io::Result<()> {
        if !self.found {
            if self.tail.len() == TAIL_LINES {
                self.tail.pop_front();
            }
            self.tail.push_back(line.to_owned());
        }
        let plain = without_sgr(line);
        if let Some(run) = self.test_run.as_mut() {
            if cargo && matches!(run.phase, RunPhase::Live) && is_cargo_test_abort(&plain) {
                self.test_run = None;
            } else {
                match run.push(line, &plain) {
                    RunStep::Continue => return Ok(()),
                    RunStep::Finished(outcome) => {
                        let run = self.test_run.take().unwrap();
                        if outcome == TestOutcome::Failed {
                            run.write_failure(out)?;
                            self.found = true;
                            self.cargo_diagnostic = false;
                        }
                        return Ok(());
                    }
                }
            }
        }
        if is_running_tests(&plain) {
            self.test_run = Some(TestRun::new(line));
            self.cargo_diagnostic = false;
            return Ok(());
        }
        if !cargo {
            return Ok(());
        }
        if is_cargo_diagnostic_start(&plain) {
            self.cargo_diagnostic = true;
            writeln!(out, "{line}")?;
            self.found = true;
        } else if self.cargo_diagnostic && diagnostic_continuation(&plain) {
            writeln!(out, "{line}")?;
        } else {
            self.cargo_diagnostic = false;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if let Some(run) = self.test_run.take() {
            if run.has_pending_failure() {
                run.write_failure(out)?;
                self.found = true;
            }
        }
        if !self.found {
            for line in self.tail.drain(..) {
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestOutcome {
    Ok,
    Failed,
}

enum RunStep {
    Continue,
    Finished(TestOutcome),
}

enum RunPhase {
    Live,
    Successes(SectionState),
    Failures(SectionState),
}

/// Buffers one `libtest` binary run (`running <N> test(s)` through its
/// structural `test result:` summary) so passing binaries can be dropped and
/// failing binaries can preserve live output when capture is disabled.
struct TestRun {
    lines: Vec<String>,
    phase: RunPhase,
    live_end: Option<usize>,
    failures_start: Option<usize>,
    saw_failed_result: bool,
}

impl TestRun {
    fn new(header: &str) -> Self {
        Self {
            lines: vec![header.to_owned()],
            phase: RunPhase::Live,
            live_end: None,
            failures_start: None,
            saw_failed_result: false,
        }
    }

    fn has_pending_failure(&self) -> bool {
        matches!(self.phase, RunPhase::Failures(_)) || self.saw_failed_result
    }

    fn push(&mut self, raw: &str, plain: &str) -> RunStep {
        let index = self.lines.len();
        self.lines.push(raw.to_owned());
        match &mut self.phase {
            RunPhase::Live => self.push_live(index, plain),
            RunPhase::Successes(section) => match section.advance(plain, "successes:") {
                SectionStep::Continue => RunStep::Continue,
                SectionStep::Revert => {
                    self.live_end = None;
                    self.phase = RunPhase::Live;
                    self.push_live(index, plain)
                }
                SectionStep::ListEnd => {
                    if plain == "failures:" {
                        self.failures_start = Some(index);
                        self.phase = RunPhase::Failures(SectionState::new());
                        RunStep::Continue
                    } else if parse_test_result(plain) == Some(TestOutcome::Ok) {
                        RunStep::Finished(TestOutcome::Ok)
                    } else {
                        self.revert_or_output(true, index, plain)
                    }
                }
            },
            RunPhase::Failures(section) => match section.advance(plain, "failures:") {
                SectionStep::Continue => RunStep::Continue,
                SectionStep::Revert => {
                    if self.live_end == self.failures_start {
                        self.live_end = None;
                    }
                    self.failures_start = None;
                    self.phase = RunPhase::Live;
                    self.push_live(index, plain)
                }
                SectionStep::ListEnd => {
                    if parse_test_result(plain) == Some(TestOutcome::Failed) {
                        RunStep::Finished(TestOutcome::Failed)
                    } else {
                        self.revert_or_output(false, index, plain)
                    }
                }
            },
        }
    }

    fn push_live(&mut self, index: usize, plain: &str) -> RunStep {
        if plain == "successes:" {
            self.live_end = Some(index);
            self.phase = RunPhase::Successes(SectionState::new());
            return RunStep::Continue;
        }
        if plain == "failures:" {
            self.live_end.get_or_insert(index);
            self.failures_start = Some(index);
            self.phase = RunPhase::Failures(SectionState::new());
            return RunStep::Continue;
        }
        match parse_test_result(plain) {
            Some(TestOutcome::Ok) => RunStep::Finished(TestOutcome::Ok),
            Some(TestOutcome::Failed) => {
                self.saw_failed_result = true;
                RunStep::Continue
            }
            None => RunStep::Continue,
        }
    }

    /// Reverts a false heading when no captured output block was seen yet,
    /// otherwise returns to captured output scanning inside the section.
    fn revert_or_output(&mut self, is_successes: bool, index: usize, plain: &str) -> RunStep {
        let has_captured = match &self.phase {
            RunPhase::Successes(s) | RunPhase::Failures(s) => s.first_captured.is_some(),
            RunPhase::Live => false,
        };
        if has_captured {
            if let RunPhase::Successes(s) | RunPhase::Failures(s) = &mut self.phase {
                s.sub = SectionSub::Output;
            }
            return RunStep::Continue;
        }
        if is_successes || self.live_end == self.failures_start {
            self.live_end = None;
        }
        if !is_successes {
            self.failures_start = None;
        }
        self.phase = RunPhase::Live;
        self.push_live(index, plain)
    }

    fn write_failure(&self, out: &mut impl Write) -> io::Result<()> {
        let live_end = self.live_end.unwrap_or(self.lines.len());
        let include_live =
            self.failures_start.is_none() || has_live_output(&self.lines[..live_end]);
        if include_live {
            for line in &self.lines[..live_end] {
                if !is_passing_test_status(&without_sgr(line)) {
                    writeln!(out, "{line}")?;
                }
            }
        }
        if let Some(start) = self.failures_start {
            for line in &self.lines[start..] {
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
}

struct SectionState {
    first_captured: Option<String>,
    sub: SectionSub,
}

enum SectionSub {
    Start,
    Output,
    List {
        has_name: bool,
        matched_first: bool,
        saw_blank: bool,
    },
}

enum SectionStep {
    Continue,
    Revert,
    ListEnd,
}

impl SectionState {
    fn new() -> Self {
        Self {
            first_captured: None,
            sub: SectionSub::Start,
        }
    }

    fn advance(&mut self, plain: &str, heading: &str) -> SectionStep {
        match &mut self.sub {
            SectionSub::Start => self.advance_start(plain, heading),
            SectionSub::Output => {
                if plain == heading {
                    self.sub = SectionSub::List {
                        has_name: false,
                        matched_first: false,
                        saw_blank: false,
                    };
                }
                SectionStep::Continue
            }
            SectionSub::List { .. } => self.advance_list(plain, heading),
        }
    }

    fn advance_start(&mut self, plain: &str, heading: &str) -> SectionStep {
        if plain.is_empty() {
            return SectionStep::Continue;
        }
        if let Some(name) = parse_stdout_header(plain) {
            self.first_captured = Some(name.to_owned());
            self.sub = SectionSub::Output;
            return SectionStep::Continue;
        }
        if plain == heading {
            self.sub = SectionSub::List {
                has_name: false,
                matched_first: false,
                saw_blank: false,
            };
            return SectionStep::Continue;
        }
        if parse_list_name(plain).is_some() {
            self.sub = SectionSub::List {
                has_name: true,
                matched_first: false,
                saw_blank: false,
            };
            return SectionStep::Continue;
        }
        SectionStep::Revert
    }

    fn advance_list(&mut self, plain: &str, heading: &str) -> SectionStep {
        let SectionSub::List {
            has_name,
            matched_first,
            saw_blank,
        } = &mut self.sub
        else {
            unreachable!()
        };
        if plain == heading {
            *has_name = false;
            *matched_first = false;
            *saw_blank = false;
            return SectionStep::Continue;
        }
        if let Some(name) = parse_stdout_header(plain) {
            self.first_captured.get_or_insert_with(|| name.to_owned());
            self.sub = SectionSub::Output;
            return SectionStep::Continue;
        }
        if !*saw_blank {
            if let Some(name) = parse_list_name(plain) {
                *has_name = true;
                if self.first_captured.as_deref() == Some(name) {
                    *matched_first = true;
                }
                return SectionStep::Continue;
            }
        }
        if plain.is_empty() {
            if *has_name {
                *saw_blank = true;
            } else {
                self.sub = if self.first_captured.is_some() {
                    SectionSub::Output
                } else {
                    SectionSub::Start
                };
            }
            return SectionStep::Continue;
        }
        if *has_name && (self.first_captured.is_none() || *matched_first) {
            return SectionStep::ListEnd;
        }
        if self.first_captured.is_some() {
            self.sub = SectionSub::Output;
            SectionStep::Continue
        } else {
            SectionStep::Revert
        }
    }
}

fn is_running_tests(line: &str) -> bool {
    line.strip_prefix("running ")
        .and_then(|s| s.split_once(' '))
        .is_some_and(|(count, noun)| {
            matches!(noun, "test" | "tests") && count.parse::<usize>().is_ok()
        })
}

fn parse_test_result(line: &str) -> Option<TestOutcome> {
    let rest = line.strip_prefix("test result: ")?;
    let (outcome, stats) = if let Some(stats) = rest.strip_prefix("ok. ") {
        (TestOutcome::Ok, stats)
    } else {
        (TestOutcome::Failed, rest.strip_prefix("FAILED. ")?)
    };
    let mut parts = stats.split("; ");
    parse_count(parts.next()?, "passed")?;
    parse_count(parts.next()?, "failed")?;
    parse_count(parts.next()?, "ignored")?;
    parse_count(parts.next()?, "measured")?;
    parse_count(parts.next()?, "filtered out")?;
    match parts.next() {
        None => Some(outcome),
        Some(tail) if tail.starts_with("finished in ") && parts.next().is_none() => Some(outcome),
        Some(_) => None,
    }
}

fn parse_count(part: &str, suffix: &str) -> Option<usize> {
    part.strip_suffix(suffix)?.strip_suffix(' ')?.parse().ok()
}

fn parse_stdout_header(line: &str) -> Option<&str> {
    let name = line.strip_prefix("---- ")?.strip_suffix(" stdout ----")?;
    (!name.is_empty()).then_some(name)
}

fn parse_list_name(line: &str) -> Option<&str> {
    let name = line.strip_prefix("    ")?;
    if name.is_empty()
        || name.starts_with(char::is_whitespace)
        || name.starts_with("---- ")
        || name.starts_with("test result:")
    {
        return None;
    }
    Some(name)
}

fn is_passing_test_status(line: &str) -> bool {
    let Some((name, outcome)) = line
        .strip_prefix("test ")
        .and_then(|s| s.rsplit_once(" ... "))
    else {
        return false;
    };
    !name.is_empty()
        && (outcome == "ok"
            || (outcome.starts_with("ok <") && outcome.ends_with('>'))
            || outcome == "ignored"
            || outcome.starts_with("ignored, ")
            || outcome.starts_with("bench:"))
}

fn is_failing_test_status(line: &str) -> bool {
    let Some((name, outcome)) = line
        .strip_prefix("test ")
        .and_then(|s| s.rsplit_once(" ... "))
    else {
        return false;
    };
    !name.is_empty()
        && (outcome == "FAILED" || (outcome.starts_with("FAILED <") && outcome.ends_with('>')))
}

fn has_live_output(lines: &[String]) -> bool {
    lines.iter().skip(1).any(|line| {
        let plain = without_sgr(line);
        !plain.is_empty() && !is_passing_test_status(&plain) && !is_failing_test_status(&plain)
    })
}

fn is_cargo_diagnostic_start(line: &str) -> bool {
    ["error:", "error[", "warning:", "warning["]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn is_cargo_test_abort(line: &str) -> bool {
    line.starts_with("error: test failed, to rerun pass ")
        || line.starts_with("error: process didn't exit successfully: ")
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
