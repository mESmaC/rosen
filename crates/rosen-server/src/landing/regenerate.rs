//! Server-side regeneration of generated artifacts at land (SPEC R5, R8, #428).
//!
//! When a merge leaves generated paths needing regeneration and every such path
//! has a command registered in the base's `rosen.toml` `[generated.commands]`,
//! the server materialises the merged input tree into a scratch directory,
//! acquires a `landing` turn in the machine's run queue, runs each registered
//! command, and applies only the regenerated paths back onto the merged tree.
//! Unrelated files or untracked build output never enter the landed commit.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rosen_client::run_queue::{
    self, RunBudget, RunClass, RunQueue, Turn, HELD_ENV, POLL, THREAD_VARIABLES,
};
use rosen_client::workspace::{materialise, Options};
use rosen_core::{ChangeId, FileMode, ObjectId, ProjectConfig, Tree};
use rosen_store::{LooseObjectStore, ObjectStore};

use crate::history::trees::{self, Entry};
use crate::ServerState;

/// Upper bound on one registered regeneration command so a hung generator
/// cannot hold a landing run-queue turn forever.
const COMMAND_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Maximum trailing bytes of combined command output kept for a failure report.
const OUTPUT_TAIL: usize = 16 * 1024;

/// Maximum cached `(merged_tree, plan) -> regenerated_tree` entries per server.
const MAX_CACHE_ENTRIES: usize = 256;

/// Whether this process's regeneration runs wait in the machine's run queue.
/// Enabled by `rosen-server`'s daemon `main`; tests that set `ROSEN_RUN_QUEUE`
/// also wait in that queue.
static QUEUE_REGENERATION: AtomicBool = AtomicBool::new(false);

pub fn enable_run_queue() {
    QUEUE_REGENERATION.store(true, Ordering::Relaxed);
}

fn should_queue() -> bool {
    QUEUE_REGENERATION.load(Ordering::Relaxed) || std::env::var_os(run_queue::QUEUE_ENV).is_some()
}

type RegenKey = (ObjectId, Vec<(String, String)>);

/// Bounded cache of completed regenerations keyed by immutable merged input
/// tree and exact `(path, command)` plan. Only successes are cached.
#[derive(Default)]
pub(crate) struct RegenCache(Mutex<HashMap<RegenKey, ObjectId>>);

impl RegenCache {
    pub(crate) fn get(&self, tree: ObjectId, pairs: &[(String, String)]) -> Option<ObjectId> {
        let key = (tree, pairs.to_vec());
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
    }

    pub(crate) fn remember(&self, tree: ObjectId, pairs: Vec<(String, String)>, result: ObjectId) {
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert((tree, pairs), result);
    }
}

/// Which registered commands cover a set of generated paths, and which paths
/// have none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegenPlan {
    pub commands: BTreeMap<String, String>,
    pub missing: Vec<String>,
}

impl RegenPlan {
    pub(crate) fn for_paths(config: &ProjectConfig, paths: &[String]) -> Self {
        let mut plan = Self::default();
        for path in paths {
            match config.generated_command(path) {
                Some(command) => {
                    plan.commands.insert(path.clone(), command.to_owned());
                }
                None => plan.missing.push(path.clone()),
            }
        }
        plan
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.missing.is_empty() && !self.commands.is_empty()
    }

    pub(crate) fn pairs(&self) -> Vec<(String, String)> {
        self.commands
            .iter()
            .map(|(path, cmd)| (path.clone(), cmd.clone()))
            .collect()
    }

    /// Unique commands in deterministic first-seen path order, so multiple
    /// artifacts sharing one generator command run it once.
    fn unique_commands(&self) -> Vec<(&str, Vec<&str>)> {
        let mut ordered: Vec<(&str, Vec<&str>)> = Vec::new();
        for (path, command) in &self.commands {
            if let Some((_, targets)) = ordered.iter_mut().find(|(cmd, _)| *cmd == command) {
                targets.push(path.as_str());
            } else {
                ordered.push((command.as_str(), vec![path.as_str()]));
            }
        }
        ordered
    }
}

/// Result of attempting server-side regeneration on a merged input tree.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegenOutcome {
    Regenerated(ObjectId),
    Cancelled,
    MissingCommands {
        commands: BTreeMap<String, String>,
    },
    Failed {
        commands: BTreeMap<String, String>,
        log: String,
    },
}

/// Removes scratch directories left by a previous crashed server.
pub(crate) fn clear_scratch(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("rosen-server: clearing {}: {error}", dir.display());
        }
    }
}

/// Inspects registered commands and any cached regeneration result without
/// executing commands.
pub(crate) fn inspect(
    state: &ServerState,
    policy_base: &ObjectId,
    merged_tree: ObjectId,
    paths: &[String],
) -> Result<super::RegenerationInfo, String> {
    let config = state
        .project_config
        .at_commit(policy_base)
        .map_err(|err| err.to_string())?
        .config;
    let plan = RegenPlan::for_paths(&config, paths);
    let cached = plan
        .is_complete()
        .then(|| state.regenerations.get(merged_tree, &plan.pairs()))
        .flatten();
    Ok(super::RegenerationInfo {
        cached,
        all_registered: plan.is_complete(),
        commands: plan.commands,
    })
}

/// Regenerates `paths` on `merged_tree` using commands registered in
/// `policy_base`'s `rosen.toml`.
pub(crate) fn regenerate_tree(
    state: &ServerState,
    policy_base: &ObjectId,
    merged_tree: ObjectId,
    paths: &[String],
    current: &dyn Fn() -> Result<bool, String>,
) -> Result<RegenOutcome, String> {
    let config = state
        .project_config
        .at_commit(policy_base)
        .map_err(|err| err.to_string())?
        .config;
    let plan = RegenPlan::for_paths(&config, paths);
    if !plan.is_complete() {
        return Ok(RegenOutcome::MissingCommands {
            commands: plan.commands,
        });
    }
    let pairs = plan.pairs();
    let queued_at = crate::stats::now_millis();
    if let Some(cached) = state.regenerations.get(merged_tree, &pairs) {
        record_regen_timing(state, queued_at, queued_at, queued_at, Some(0), true);
        return Ok(RegenOutcome::Regenerated(cached));
    }
    let note = format!("regenerate {} on {merged_tree}", paths.join(", "));
    let Some((turn, budget)) = acquire_turn(state, &note, current)? else {
        return Ok(RegenOutcome::Cancelled);
    };
    let started_at = crate::stats::now_millis();
    let result = run_in_scratch(state, merged_tree, paths, &plan, budget);
    let finished_at = crate::stats::now_millis();
    drop(turn);
    let exit = match &result {
        Ok(RegenOutcome::Regenerated(_)) => Some(0),
        Ok(RegenOutcome::Failed { .. }) => Some(1),
        _ => None,
    };
    record_regen_timing(state, queued_at, started_at, finished_at, exit, false);
    if let Ok(RegenOutcome::Regenerated(tree)) = &result {
        state.regenerations.remember(merged_tree, pairs, *tree);
    }
    result
}

fn record_regen_timing(
    state: &ServerState,
    queued_at_ms: i64,
    started_at_ms: i64,
    finished_at_ms: i64,
    exit_status: Option<i32>,
    reused: bool,
) {
    let record = rosen_protocol::RecordRunTiming {
        label: "regenerate".to_owned(),
        class: "landing".to_owned(),
        change: None,
        queued_at_ms,
        started_at_ms,
        finished_at_ms,
        exit_status,
        reused,
    };
    if let Ok(conn) = crate::metadata::open(&state.config.metadata_path()) {
        if crate::stats::store::record_run(
            &conn,
            &record,
            None,
            crate::machines::signing::SERVER_MACHINE,
        )
        .is_ok()
        {
            state.stats.bump();
        }
    }
}

fn acquire_turn(
    state: &ServerState,
    note: &str,
    current: &dyn Fn() -> Result<bool, String>,
) -> Result<Option<(Option<Turn>, RunBudget)>, String> {
    if !current()? || state.is_shutting_down() {
        return Ok(None);
    }
    if let Some(held) = run_queue::held() {
        return Ok(Some((None, held.budget())));
    }
    if !should_queue() {
        return Ok(Some((None, RunClass::Landing.budget())));
    }
    let queue = RunQueue::machine().map_err(|err| format!("run queue: {err}"))?;
    let waiter = queue
        .wait(RunClass::Landing, note)
        .map_err(|err| format!("run queue wait: {err}"))?;
    loop {
        if !current()? || state.is_shutting_down() {
            return Ok(None);
        }
        if let Some(turn) = waiter
            .try_start()
            .map_err(|err| format!("run queue start: {err}"))?
        {
            let budget = turn.budget();
            return Ok(Some((Some(turn), budget)));
        }
        std::thread::sleep(POLL);
    }
}

fn run_in_scratch(
    state: &ServerState,
    merged_tree: ObjectId,
    paths: &[String],
    plan: &RegenPlan,
    budget: RunBudget,
) -> Result<RegenOutcome, String> {
    let scratch_root = state.config.regen_scratch_dir();
    std::fs::create_dir_all(&scratch_root)
        .map_err(|err| format!("creating {}: {err}", scratch_root.display()))?;
    let temp = tempfile::TempDir::new_in(&scratch_root)
        .map_err(|err| format!("creating scratch dir: {err}"))?;
    let work_dir = temp.path().join("tree");
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(|e| e.to_string())?;
    materialise(&store, merged_tree, &work_dir, &Options::default())
        .map_err(|err| format!("materialising {merged_tree}: {err}"))?;
    for (command, targets) in plan.unique_commands() {
        if let Err(log) = run_one(state, &work_dir, command, &targets, budget) {
            return Ok(RegenOutcome::Failed {
                commands: plan.commands.clone(),
                log,
            });
        }
    }
    match collect_regenerated(&store, merged_tree, &work_dir, paths) {
        Ok(tree) => Ok(RegenOutcome::Regenerated(tree)),
        Err(log) => Ok(RegenOutcome::Failed {
            commands: plan.commands.clone(),
            log,
        }),
    }
}

fn run_one(
    state: &ServerState,
    root: &Path,
    command: &str,
    targets: &[&str],
    budget: RunBudget,
) -> Result<(), String> {
    let (status, output) = match run_shell(state, root, command, budget) {
        Ok(pair) => pair,
        Err(why) => {
            return Err(format!(
                "command `{command}` (for {}): {why}",
                targets.join(", ")
            ));
        }
    };
    if status.success() {
        return Ok(());
    }
    let body = String::from_utf8_lossy(&output);
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        Err(format!(
            "command `{command}` (for {}) exited with {status} (no output)",
            targets.join(", ")
        ))
    } else {
        Err(format!(
            "command `{command}` (for {}) exited with {status}:\n{trimmed}",
            targets.join(", ")
        ))
    }
}

#[cfg(windows)]
fn run_shell(
    state: &ServerState,
    root: &Path,
    command: &str,
    budget: RunBudget,
) -> Result<(ExitStatus, Vec<u8>), String> {
    use rosen_platform::run_tree::ProcessTree;
    use std::ffi::OsStr;
    use std::io::Read;

    let jobs = budget.jobs.to_string();
    let mut environment = vec![(OsStr::new(HELD_ENV), OsStr::new(RunClass::Landing.name()))];
    environment.extend(THREAD_VARIABLES.map(|name| (OsStr::new(name), OsStr::new(&jobs))));
    let (mut tree, mut pipe) = ProcessTree::spawn_shell(root, command, &environment, budget.memory)
        .map_err(|err| err.to_string())?;
    let reader = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(size @ 1..) = pipe.read(&mut buffer) {
            keep_tail(&mut tail, &buffer[..size]);
        }
        tail
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = tree.try_wait().map_err(|err| err.to_string())? {
            break status;
        }
        if state.is_shutting_down() {
            drop(tree);
            let _ = reader.join();
            return Err("server is shutting down".into());
        }
        if started.elapsed() >= COMMAND_LIMIT {
            drop(tree);
            let output = reader.join().unwrap_or_default();
            return Err(timeout_message(&output));
        }
        std::thread::sleep(POLL);
    };
    drop(tree);
    let output = reader.join().unwrap_or_default();
    Ok((status, output))
}

#[cfg(not(windows))]
fn run_shell(
    state: &ServerState,
    root: &Path,
    command: &str,
    budget: RunBudget,
) -> Result<(ExitStatus, Vec<u8>), String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let jobs = budget.jobs.to_string();
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .env(HELD_ENV, RunClass::Landing.name())
        .envs(THREAD_VARIABLES.map(|name| (name, &jobs)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|err| err.to_string())?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_stream = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut tail = Vec::new();
            if let Some(mut stream) = stream {
                let mut buffer = [0; 4096];
                while let Ok(size @ 1..) = stream.read(&mut buffer) {
                    keep_tail(&mut tail, &buffer[..size]);
                }
            }
            tail
        })
    };
    let out_reader = read_stream(stdout.map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err_reader = read_stream(stderr.map(|s| Box::new(s) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            break status;
        }
        if state.is_shutting_down() {
            kill_unix_group(pid, &mut child);
            let _ = out_reader.join();
            let _ = err_reader.join();
            return Err("server is shutting down".into());
        }
        if started.elapsed() >= COMMAND_LIMIT {
            kill_unix_group(pid, &mut child);
            let mut output = out_reader.join().unwrap_or_default();
            let err_output = err_reader.join().unwrap_or_default();
            keep_tail(&mut output, &err_output);
            return Err(timeout_message(&output));
        }
        std::thread::sleep(POLL);
    };
    let mut output = out_reader.join().unwrap_or_default();
    let err_output = err_reader.join().unwrap_or_default();
    keep_tail(&mut output, &err_output);
    Ok((status, output))
}

#[cfg(not(windows))]
fn kill_unix_group(pid: u32, child: &mut std::process::Child) {
    use std::process::{Command, Stdio};
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

fn timeout_message(output: &[u8]) -> String {
    let mins = COMMAND_LIMIT.as_secs() / 60;
    let body = String::from_utf8_lossy(output);
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        format!("did not finish within {mins} minutes")
    } else {
        format!("did not finish within {mins} minutes:\n{trimmed}")
    }
}

fn keep_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    let discard = (tail.len() + bytes.len()).saturating_sub(OUTPUT_TAIL);
    tail.drain(..discard);
    tail.extend_from_slice(bytes);
}

/// Reads back only the regenerated `paths` from `work_dir` (refusing symlinks
/// or directories) and applies them onto `merged_tree`.
fn collect_regenerated(
    store: &LooseObjectStore,
    merged_tree: ObjectId,
    work_dir: &Path,
    paths: &[String],
) -> Result<ObjectId, String> {
    let mut edits = BTreeMap::new();
    for path in paths {
        let entry = read_regenerated_file(store, merged_tree, work_dir, path)?;
        edits.insert(path.clone(), entry);
    }
    let mut collisions = Vec::new();
    let updated = trees::apply(store, Some(merged_tree), &edits, &mut collisions)
        .map_err(|err| format!("applying regenerated files: {err}"))?;
    if !collisions.is_empty() {
        return Err(format!(
            "regenerated paths collide with directories: {}",
            collisions.join(", ")
        ));
    }
    match updated {
        Some(tree) => Ok(tree),
        None => store
            .put_tree(&Tree::default())
            .map_err(|err| format!("storing empty tree: {err}")),
    }
}

/// Walks every path segment with `symlink_metadata` so a generator cannot
/// smuggle a symlink to read files outside `work_dir`.
fn read_regenerated_file(
    store: &LooseObjectStore,
    merged_tree: ObjectId,
    work_dir: &Path,
    rel_path: &str,
) -> Result<Option<Entry>, String> {
    let segments = safe_segments(rel_path)?;
    let mut current = PathBuf::from(work_dir);
    for (index, segment) in segments.iter().enumerate() {
        current.push(segment);
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("reading {rel_path}: {err}")),
        };
        let is_last = index + 1 == segments.len();
        if !is_last {
            if !meta.file_type().is_dir() {
                return Err(format!(
                    "generator replaced parent directory of {rel_path} with a non-directory"
                ));
            }
            continue;
        }
        if !meta.file_type().is_file() {
            return Err(format!(
                "generator left a non-regular file (symlink or directory) at {rel_path}"
            ));
        }
        let bytes = std::fs::read(&current).map_err(|err| format!("reading {rel_path}: {err}"))?;
        let mode = file_mode(store, merged_tree, rel_path, &meta);
        let id = store
            .put_blob(&bytes)
            .map_err(|err| format!("storing blob for {rel_path}: {err}"))?;
        return Ok(Some(Entry { mode, id }));
    }
    Ok(None)
}

fn safe_segments(rel_path: &str) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    for component in Path::new(rel_path).components() {
        match component {
            Component::Normal(part) => segments.push(part.to_string_lossy().into_owned()),
            _ => return Err(format!("invalid generated path {rel_path:?}")),
        }
    }
    if segments.is_empty() {
        return Err(format!("empty generated path {rel_path:?}"));
    }
    Ok(segments)
}

fn file_mode(
    store: &LooseObjectStore,
    merged_tree: ObjectId,
    rel_path: &str,
    meta: &std::fs::Metadata,
) -> FileMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return FileMode::Executable;
        }
    }
    let _ = meta;
    match trees::file_at(store, merged_tree, rel_path) {
        Ok(Some(Entry {
            mode: FileMode::Executable,
            ..
        })) => FileMode::Executable,
        _ => FileMode::Regular,
    }
}

pub(crate) fn format_commands(commands: &BTreeMap<String, String>) -> String {
    commands
        .iter()
        .map(|(path, cmd)| format!("{path}: `{cmd}`"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Actionable message naming exact paths, merged input tree, and registered
/// commands when automatic regeneration did not complete (#428).
pub(crate) fn required_message(
    change: ChangeId,
    paths: &[String],
    merged_tree: Option<ObjectId>,
    commands: &BTreeMap<String, String>,
) -> String {
    let tree_note = merged_tree
        .map(|tree| format!(" on merged tree {tree}"))
        .unwrap_or_default();
    let cmd_note = if commands.is_empty() {
        "; register commands under [generated.commands] in rosen.toml to regenerate at land"
            .to_owned()
    } else {
        let missing: Vec<&str> = paths
            .iter()
            .filter(|path| !commands.contains_key(*path))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            format!(" (registered: {})", format_commands(commands))
        } else {
            format!(
                " (registered: {}; unregistered: {})",
                format_commands(commands),
                missing.join(", ")
            )
        }
    };
    format!(
        "change {change} requires regeneration of {}{tree_note}{cmd_note}; its owner must apply \
         `rosen rebase` (landing its ancestors first if stacked), regenerate these files, \
         and name the repaired head before retrying",
        paths.join(", ")
    )
}

/// Actionable message naming exact paths, merged input tree, registered
/// command(s), and command output log when regeneration failed at land (#428).
pub(crate) fn failed_message(
    change: ChangeId,
    paths: &[String],
    merged_tree: ObjectId,
    commands: &BTreeMap<String, String>,
    log: &str,
) -> String {
    let cmd_note = if commands.is_empty() {
        String::new()
    } else {
        format!(" ({})", format_commands(commands))
    };
    format!(
        "regenerating {} for change {change} on merged tree {merged_tree}{cmd_note} failed:\n{log}",
        paths.join(", ")
    )
}
