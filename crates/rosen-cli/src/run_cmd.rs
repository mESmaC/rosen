//! `rosen run`: one command, run in its turn in the machine's run queue
//! (SPEC R3 "Run queue").

mod log;

use std::fs::File;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Instant;

use rosen_client::run_queue::{
    self, RunBudget, RunClass, RunQueue, Turn, Waiter, HELD_ENV, POLL, STARVING, THREAD_VARIABLES,
};

use crate::commands::RunArgs;
use log::RunLog;

/// Waits for the turn, runs the command, and gives its exit status.
/// Beneath a run that already holds a turn, the command runs at once, in it.
pub fn run(json: bool, args: &RunArgs) -> io::Result<ExitStatus> {
    let class = RunClass::parse(&args.class).ok_or_else(|| {
        io::Error::other(format!(
            "unknown class `{}`: use landing, build, verify or prepare",
            args.class
        ))
    })?;
    let (program, arguments) = args
        .command
        .split_first()
        .ok_or_else(|| io::Error::other("a command is required after `--`"))?;
    #[cfg(windows)]
    if let Some(output) = std::env::var_os(WORKER_ENV) {
        let log = (output != "1").then(|| Path::new(&output));
        return spawn(program, arguments, class, None, log)?.wait();
    }
    let (turn, holding) = match run_queue::held() {
        Some(held) => (None, held),
        None => {
            let note = format!("{} in {}", args.command.join(" "), current_dir());
            (
                Some(turn(class, requested_budget(args, class), &note)?),
                class,
            )
        }
    };
    let budget = turn.as_ref().map(Turn::budget);
    let log = (!args.full && !json).then(RunLog::create).transpose()?;
    let output = log.as_ref().map(RunLog::path);
    let started = Instant::now();
    #[cfg(windows)]
    let result = run_tree(json, &args.command, holding, budget, output);
    #[cfg(not(windows))]
    let result =
        spawn(program, arguments, holding, budget, output).and_then(|mut child| child.wait());
    drop(turn);
    if let Some(log) = log {
        log.report(&args.command, &result, started.elapsed());
    }
    result
}

// The job owns a small launcher so Rust can keep doing executable lookup
// and batch-file escaping. Only that launcher skips creating another job;
// its command never inherits this private marker, including nested runs.
// The marker carries the log path, or `1` when output is inherited.
#[cfg(windows)]
const WORKER_ENV: &str = "ROSEN_RUN_TREE_WORKER";

#[cfg(windows)]
fn run_tree(
    json: bool,
    command: &[String],
    holding: RunClass,
    budget: Option<RunBudget>,
    log: Option<&Path>,
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
        (
            OsStr::new(WORKER_ENV),
            log.map_or(OsStr::new("1"), Path::as_os_str),
        ),
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

/// The budget `--jobs` and `--memory-gib` ask for, or `None` for the
/// class's full allocation. A missing half follows the class's rule: its
/// cap of jobs, and one GiB per job plus one.
fn requested_budget(args: &RunArgs, class: RunClass) -> Option<RunBudget> {
    if args.jobs.is_none() && args.memory_gib.is_none() {
        return None;
    }
    let jobs = args.jobs.map_or(class.budget().jobs, |jobs| {
        usize::try_from(jobs).unwrap_or(usize::MAX)
    });
    let memory_gib = args
        .memory_gib
        .unwrap_or_else(|| u64::try_from(jobs).unwrap_or(u64::MAX).saturating_add(1));
    Some(RunBudget::new(jobs, memory_gib))
}

fn turn(class: RunClass, requested: Option<RunBudget>, note: &str) -> io::Result<Turn> {
    let queue = RunQueue::machine()?;
    let waiter = match requested {
        Some(budget) => queue.wait_with(class, budget, note)?,
        None => queue.wait(class, note)?,
    };
    let started = Instant::now();
    let mut next_report = started + POLL * 20;
    let mut next_starving_report = started + STARVING;
    let mut previous = None;
    loop {
        if let Some(turn) = waiter.try_start()? {
            return Ok(turn);
        }
        // A long wait says what holds it, since the runs ahead may not change.
        if Instant::now() >= next_starving_report {
            next_starving_report = Instant::now() + STARVING;
            if let Some(reason) = waiter.waiting_for() {
                eprintln!(
                    "rosen run: waited {}s; {}: {reason}",
                    started.elapsed().as_secs(),
                    class.name()
                );
            }
        }
        if Instant::now() >= next_report {
            next_report = Instant::now() + POLL * 10;
            if let Ok(progress) = WaitProgress::read(&waiter) {
                if previous.as_ref() != Some(&progress) {
                    progress.print(&queue, class, waiter.budget(), started);
                    previous = Some(progress);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

#[derive(PartialEq, Eq)]
struct WaitProgress {
    running: Vec<String>,
    waiting: usize,
}

impl WaitProgress {
    fn read(waiter: &Waiter) -> io::Result<Self> {
        let ahead = waiter.ahead()?;
        let mut running: Vec<_> = ahead
            .iter()
            .filter(|entry| entry.running)
            .map(|entry| format!("{} ({})", entry.note, entry.class.name()))
            .collect();
        running.sort_unstable();
        let waiting = ahead.len() - running.len();
        Ok(Self { running, waiting })
    }

    fn print(&self, queue: &RunQueue, class: RunClass, budget: RunBudget, started: Instant) {
        let running = if self.running.is_empty() {
            "nothing".to_owned()
        } else {
            self.running.join(", ")
        };
        eprintln!(
            "rosen run: waited {}s for a turn ({}; {}); running now: {running}; {} ahead in line; {}",
            started.elapsed().as_secs(),
            class.name(),
            resources(class, budget),
            self.waiting,
            memory_resources(queue, Some(budget)),
        );
    }
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
    log: Option<&Path>,
) -> io::Result<std::process::Child> {
    // Both handles append to the same file so neither stream can overwrite the other.
    let log = log
        .map(|path| File::options().append(true).open(path))
        .transpose()?;
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
        child.args(arguments).env(HELD_ENV, holding.name());
        if let Some(file) = &log {
            child.stdout(file.try_clone()?).stderr(file.try_clone()?);
        }
        child.spawn()
    };
    match start(program) {
        Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::NotFound => {
            start(&format!("{program}.cmd")).or_else(|_| start(&format!("{program}.bat")))
        }
        started => started,
    }
}

fn resources(class: RunClass, budget: RunBudget) -> String {
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
            resources(class, class.budget())
        );
    }
    println!("{}", memory_resources(&queue, None));
    // Waiters held past `STARVING` say what they wait for.
    for entry in queue.line().unwrap_or_default() {
        if let Some(reason) = entry.waiting_for {
            println!("{}: {reason}", entry.class.name());
        }
    }
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

/// `waiting` is the budget of the run waiting for its turn, if any.
fn memory_resources(queue: &RunQueue, waiting: Option<RunBudget>) -> String {
    let memory = queue.memory_headroom();
    let format_bytes = |bytes: Option<u64>| {
        bytes.map_or_else(
            || "unavailable".to_owned(),
            |bytes| format!("{:.2} GiB", bytes as f64 / (1_u64 << 30) as f64),
        )
    };
    let reserved = queue.running_budget().ok().map(|budget| budget.memory);
    let state = match (waiting, reserved) {
        (Some(budget), Some(reserved)) if !memory.fits(reserved.saturating_add(budget.memory)) => {
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
