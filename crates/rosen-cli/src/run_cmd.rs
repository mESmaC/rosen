//! `rosen run`: one command, run in its turn in the machine's run queue
//! (SPEC R3 "Run queue").

use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rosen_client::run_queue::{self, Entry, RunClass, RunQueue, Turn, Waiter, HELD_ENV, POLL};

/// How often a waiting run says what it is waiting for.
const SAY_EVERY: Duration = Duration::from_secs(30);

/// Waits for the turn, runs `command` with this process's input and output,
/// and gives its exit status. A queue that can't be used holds nothing back.
/// Beneath a run that already holds a turn, the command runs at once, in it.
pub fn run(class: &str, command: &[String]) -> io::Result<ExitStatus> {
    let class = RunClass::parse(class).ok_or_else(|| {
        io::Error::other(format!(
            "unknown class `{class}`: use landing, build, verify or prepare"
        ))
    })?;
    let note = format!("{} in {}", command.join(" "), current_dir());
    let (_turn, holding) = match run_queue::held() {
        Some(held) => (None, held),
        None => (turn(class, &note), class),
    };
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| io::Error::other("a command is required after `--`"))?;
    spawn(program, arguments, holding)?.wait()
}

/// This run's turn, or `None` when the queue can't be used.
fn turn(class: RunClass, note: &str) -> Option<Turn> {
    let waiter = RunQueue::machine().ok()?.wait(class, note).ok()?;
    let (started, mut said) = (Instant::now(), None::<Instant>);
    loop {
        match waiter.try_start() {
            Ok(Some(turn)) => return Some(turn),
            Ok(None) => {}
            Err(_) => return None,
        }
        if said.is_none_or(|said| said.elapsed() >= SAY_EVERY) && started.elapsed() >= POLL * 20 {
            say_waiting(&waiter, started.elapsed());
            said = Some(Instant::now());
        }
        std::thread::sleep(POLL);
    }
}

/// On standard error, so a waiting run's output stays the command's own.
fn say_waiting(waiter: &Waiter, waited: Duration) {
    let ahead = waiter.ahead().unwrap_or_default();
    let running: Vec<String> = ahead
        .iter()
        .filter(|entry| entry.running)
        .map(describe)
        .collect();
    let waiting = ahead.iter().filter(|entry| !entry.running).count();
    eprintln!(
        "rosen run: waited {}s for a turn; running now: {}; {waiting} ahead in line",
        waited.as_secs(),
        if running.is_empty() {
            "nothing".to_owned()
        } else {
            running.join(", ")
        }
    );
}

fn describe(entry: &Entry) -> String {
    format!("{} ({})", entry.note, entry.class.name())
}

fn current_dir() -> String {
    std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default()
}

/// Starts `program`. On Windows a name with no extension may be a script,
/// such as `pnpm.cmd`, which only its full name starts.
fn spawn(
    program: &str,
    arguments: &[String],
    holding: RunClass,
) -> io::Result<std::process::Child> {
    let start = |name: &str| {
        Command::new(name)
            .args(arguments)
            .env(HELD_ENV, holding.name())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
    };
    match start(program) {
        Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::NotFound => {
            start(&format!("{program}.cmd")).or_else(|_| start(&format!("{program}.bat")))
        }
        started => started,
    }
}
