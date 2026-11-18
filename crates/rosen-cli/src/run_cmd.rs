//! `rosen run`: one command, run in its turn in the machine's run queue
//! (SPEC R3 "Run queue").

use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rosen_client::run_queue::{
    self, Entry, RunBudget, RunClass, RunQueue, Turn, Waiter, HELD_ENV, POLL, THREAD_VARIABLES,
};

/// How often a waiting run says what it is waiting for.
const SAY_EVERY: Duration = Duration::from_secs(30);

/// Waits for the turn, runs `command` with this process's input and output,
/// and gives its exit status.
/// Beneath a run that already holds a turn, the command runs at once, in it.
pub fn run(json: bool, class: &str, command: &[String]) -> io::Result<ExitStatus> {
    let class = RunClass::parse(class).ok_or_else(|| {
        io::Error::other(format!(
            "unknown class `{class}`: use landing, build, verify or prepare"
        ))
    })?;
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| io::Error::other("a command is required after `--`"))?;
    let (turn, holding) = match run_queue::held() {
        Some(held) => (None, held),
        None => {
            let note = format!("{} in {}", command.join(" "), current_dir());
            (Some(turn(class, &note)?), class)
        }
    };
    let budget = turn.as_ref().map(Turn::budget);
    #[cfg(windows)]
    if std::env::var_os(WORKER_ENV).is_none() {
        return run_tree(json, command, holding, budget);
    }
    #[cfg(not(windows))]
    let _ = json;
    spawn(program, arguments, holding, budget)?.wait()
}

// The job owns a small launcher so Rust can keep doing executable lookup
// and batch-file escaping. Only that launcher skips creating another job;
// its command never inherits this private marker, including nested runs.
#[cfg(windows)]
const WORKER_ENV: &str = "ROSEN_RUN_TREE_WORKER";

#[cfg(windows)]
fn run_tree(
    json: bool,
    command: &[String],
    holding: RunClass,
    budget: Option<RunBudget>,
) -> io::Result<ExitStatus> {
    use rosen_platform::run_tree::ProcessTree;
    use std::ffi::OsStr;

    let mut args = Vec::new();
    if json {
        args.push(OsStr::new("--json"));
    }
    args.extend([
        OsStr::new("run"),
        OsStr::new("--class"),
        OsStr::new(holding.name()),
        OsStr::new("--"),
    ]);
    args.extend(command.iter().map(OsStr::new));
    let mut environment = vec![
        (OsStr::new(WORKER_ENV), OsStr::new("1")),
        (OsStr::new(HELD_ENV), OsStr::new(holding.name())),
    ];
    let threads = budget.map(|budget| budget.jobs.to_string());
    if let Some(threads) = &threads {
        environment.extend(THREAD_VARIABLES.map(|name| (OsStr::new(name), OsStr::new(threads))));
    }
    // Creation into the job is atomic: even a hard kill of this client
    // cannot leave a child running between spawn and job assignment.
    let limit = budget.unwrap_or_else(|| holding.budget()).memory;
    ProcessTree::spawn(&std::env::current_exe()?, &args, &environment, limit)?.wait()
}

fn turn(class: RunClass, note: &str) -> io::Result<Turn> {
    let waiter = RunQueue::machine()?.wait(class, note)?;
    let (started, mut said) = (Instant::now(), None::<Instant>);
    loop {
        if let Some(turn) = waiter.try_start()? {
            return Ok(turn);
        }
        if said.is_none_or(|said| said.elapsed() >= SAY_EVERY) && started.elapsed() >= POLL * 20 {
            say_waiting(&waiter, class, started.elapsed());
            said = Some(Instant::now());
        }
        std::thread::sleep(POLL);
    }
}

/// On standard error, so a waiting run's output stays the command's own.
fn say_waiting(waiter: &Waiter, class: RunClass, waited: Duration) {
    let ahead = waiter.ahead().unwrap_or_default();
    let running: Vec<String> = ahead
        .iter()
        .filter(|entry| entry.running)
        .map(describe)
        .collect();
    let waiting = ahead.iter().filter(|entry| !entry.running).count();
    eprintln!(
        "rosen run: waited {}s for a turn ({}; {}); running now: {}; {waiting} ahead in line",
        waited.as_secs(),
        class.name(),
        resources(class),
        if running.is_empty() {
            "nothing".to_owned()
        } else {
            running.join(", ")
        }
    );
    if let Ok(queue) = RunQueue::machine() {
        eprintln!("rosen run: {}", memory_resources(&queue, Some(class)));
    }
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
    budget: Option<RunBudget>,
) -> io::Result<std::process::Child> {
    let start = |name: &str| {
        let mut child = prioritized_command(name, holding);
        #[cfg(windows)]
        child.env_remove(WORKER_ENV);
        if let Some(budget) = budget {
            let threads = budget.jobs.to_string();
            for variable in THREAD_VARIABLES {
                child.env(variable, &threads);
            }
        }
        child
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

fn resources(class: RunClass) -> String {
    let budget = class.budget();
    format!(
        "CARGO_BUILD_JOBS={}, memory {} GiB, priority {}",
        budget.jobs,
        budget.memory >> 30,
        priority(class),
    )
}

#[cfg(windows)]
fn priority(class: RunClass) -> &'static str {
    match class {
        RunClass::Landing => "NORMAL_PRIORITY_CLASS",
        _ => "BELOW_NORMAL_PRIORITY_CLASS",
    }
}

#[cfg(windows)]
fn prioritized_command(program: &str, class: RunClass) -> Command {
    use std::os::windows::process::CommandExt;
    // Set the class at creation so descendants inherit it, without a race
    // where a build starts at normal priority before it can be lowered.
    let flags = match class {
        RunClass::Landing => 0x0000_0020, // NORMAL_PRIORITY_CLASS
        _ => 0x0000_4000,                 // BELOW_NORMAL_PRIORITY_CLASS
    };
    let mut command = Command::new(program);
    command.creation_flags(flags);
    command
}

#[cfg(not(windows))]
fn priority(class: RunClass) -> &'static str {
    match class {
        RunClass::Landing => "nice -n 0",
        RunClass::Build => "nice -n 5",
        RunClass::Verify | RunClass::Prepare => "nice -n 10",
    }
}

#[cfg(not(windows))]
fn prioritized_command(program: &str, class: RunClass) -> Command {
    // nice adds to inherited niceness. A nested run already has its turn's
    // priority, so applying it again would penalize every nested step.
    if run_queue::held().is_some() {
        return Command::new(program);
    }
    let increment = match class {
        RunClass::Landing => "0",
        RunClass::Build => "5",
        RunClass::Verify | RunClass::Prepare => "10",
    };
    let mut command = Command::new("nice");
    command.args(["-n", increment, "--", program]);
    command
}

/// `rosen run --slots`: applies each `CLASS=N` (or `CLASS=default`) to this
/// machine's queue, then prints slots, thread settings, priority and memory.
pub fn slots(settings: &[String]) -> io::Result<()> {
    let queue = RunQueue::machine()?;
    for setting in settings {
        if let Some(value) = setting.strip_prefix("reserve-gib=") {
            queue.set_memory_reserve(parse_memory_reserve(value)?)?;
            continue;
        }
        let (class, slots) = parse_slot_setting(setting)?;
        queue.set_slots(class, slots)?;
    }
    for class in RunClass::ALL {
        let (slots, default) = (queue.slots(class), class.default_slots());
        let source = if slots == default {
            "default".to_owned()
        } else {
            format!("set on this machine; default {default}")
        };
        println!(
            "{:<8} {slots} ({source}); {}",
            class.name(),
            resources(class)
        );
    }
    println!("{}", memory_resources(&queue, None));
    Ok(())
}

fn parse_memory_reserve(value: &str) -> io::Result<Option<u64>> {
    if value.trim() == "default" {
        return Ok(None);
    }
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|gib| *gib > 0)
        .and_then(|gib| gib.checked_mul(1 << 30))
        .map(Some)
        .ok_or_else(|| {
            io::Error::other("use reserve-gib=N (positive whole GiB) or reserve-gib=default")
        })
}

fn memory_resources(queue: &RunQueue, waiting: Option<RunClass>) -> String {
    let memory = queue.memory_headroom();
    let format_bytes = |bytes: Option<u64>| {
        bytes.map_or_else(
            || "unavailable".to_owned(),
            |bytes| format!("{:.2} GiB", bytes as f64 / (1_u64 << 30) as f64),
        )
    };
    let reserved = queue.running_budget().ok().map(|budget| budget.memory);
    let state = match (waiting, reserved) {
        (Some(class), Some(reserved))
            if !memory.fits(reserved.saturating_add(class.budget().memory)) =>
        {
            "; waiting for memory"
        }
        _ => "",
    };
    format!(
        "memory: {} available; {} reserved by runs; {} reserve{state}",
        format_bytes(memory.available),
        format_bytes(reserved),
        format_bytes(memory.reserve),
    )
}

/// `CLASS=N` sets a count, `CLASS=default` clears it.
fn parse_slot_setting(setting: &str) -> io::Result<(RunClass, Option<u32>)> {
    let invalid = || {
        io::Error::other(format!(
            "bad slot setting `{setting}`: use CLASS=N or CLASS=default, \
             with CLASS one of landing, build, verify or prepare"
        ))
    };
    let (class, slots) = setting.split_once('=').ok_or_else(invalid)?;
    let class = RunClass::parse(class.trim()).ok_or_else(invalid)?;
    match slots.trim() {
        "default" => Ok((class, None)),
        count => match count.parse::<u32>() {
            Ok(count) if count > 0 => Ok((class, Some(count))),
            _ => Err(invalid()),
        },
    }
}
