//! Reads the project configuration (SPEC "Project configuration") from a
//! commit, by default the current base.
//!
//! An invalid `rosen.toml` on the base never takes effect. The base keeps
//! the configuration of its nearest first-parent ancestor whose file was
//! valid, so a bad edit can neither drop the sensitive list nor rename the
//! check set out from under every pending receipt. It also cannot wedge the
//! project: the change that fixes the file still lands under the last valid
//! configuration.

use std::sync::{Arc, Mutex, PoisonError};

use rosen_core::{FileMode, ObjectId, ProjectConfig, CONFIG_PATH};
use rosen_protocol::{ConfigQuery, ConfigRecord, ErrorBody, ErrorCode, Outcome, Response};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};

use crate::landing::Ledger;
use crate::{ServerConfig, ServerState};

/// The configuration in force at one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub commit: ObjectId,
    /// The commit whose file is in force; `None` when the defaults are.
    pub source: Option<ObjectId>,
    /// Why `commit`'s own file was refused, if it was.
    pub error: Option<String>,
    pub config: Arc<ProjectConfig>,
}

/// Resolves the configuration in force at `commit`, walking first parents
/// past commits whose file is invalid.
pub fn resolve(store: &dyn ObjectStore, commit: &ObjectId) -> Result<Resolved, StoreError> {
    let mut current = *commit;
    let mut error = None;
    loop {
        let read = store.read_commit(&current)?;
        let found = match read_file(store, &read.tree)? {
            None => Some((None, ProjectConfig::default())),
            Some(Ok(config)) => Some((Some(current), config)),
            Some(Err(why)) => {
                if current == *commit {
                    error = Some(why);
                }
                // A root commit with an invalid file falls to the defaults.
                read.parents
                    .is_empty()
                    .then(|| (None, ProjectConfig::default()))
            }
        };
        if let Some((source, config)) = found {
            return Ok(Resolved {
                commit: *commit,
                source,
                error,
                config: Arc::new(config),
            });
        }
        current = read.parents[0];
    }
}

/// The parsed file in `tree`, `None` when the tree has none, or why the
/// file present was refused.
fn read_file(
    store: &dyn ObjectStore,
    tree: &ObjectId,
) -> Result<Option<Result<ProjectConfig, String>>, StoreError> {
    let tree = store.read_tree(tree)?;
    let Some(entry) = tree.get(CONFIG_PATH.as_bytes()) else {
        return Ok(None);
    };
    if !is_regular_file(entry.mode) {
        return Ok(Some(Err(format!("{CONFIG_PATH} is not a regular file"))));
    }
    let blob = store.read_blob(&entry.id)?;
    let parsed = match std::str::from_utf8(&blob.data) {
        Ok(text) => ProjectConfig::parse(text).map_err(|error| error.to_string()),
        Err(_) => Err(format!("{CONFIG_PATH} is not UTF-8")),
    };
    Ok(Some(parsed))
}

fn is_regular_file(mode: FileMode) -> bool {
    match mode {
        FileMode::Regular | FileMode::Executable => true,
        FileMode::Legacy(bits) => bits & 0o170000 == 0o100000,
        _ => false,
    }
}

/// Serves configuration reads, remembering the last commit resolved: the
/// base moves only on a land, so most reads hit it.
pub struct ConfigService {
    config: ServerConfig,
    /// Opened on first use, so a server that never reads the configuration
    /// never creates the database.
    ledger: Mutex<Option<Arc<Ledger>>>,
    last: Mutex<Option<Resolved>>,
}

impl ConfigService {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            ledger: Mutex::new(None),
            last: Mutex::new(None),
        }
    }

    /// The configuration in force at the current base, with the base ref's
    /// name, or `None` before the project has a base.
    pub fn at_base(&self) -> Result<Option<(String, Resolved)>, String> {
        let base = self.ledger()?.project_base().map_err(|e| e.to_string())?;
        let Some((base_ref, commit)) = base else {
            return Ok(None);
        };
        let resolved = self.at_commit(&commit).map_err(|e| e.to_string())?;
        Ok(Some((base_ref, resolved)))
    }

    /// The configuration in force at `commit`.
    pub fn at_commit(&self, commit: &ObjectId) -> Result<Resolved, StoreError> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = last.as_ref().filter(|cached| cached.commit == *commit) {
            return Ok(cached.clone());
        }
        let store = LooseObjectStore::open(self.config.objects_dir())?;
        let resolved = resolve(&store, commit)?;
        *last = Some(resolved.clone());
        Ok(resolved)
    }

    fn ledger(&self) -> Result<Arc<Ledger>, String> {
        let mut slot = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ledger) = slot.as_ref() {
            return Ok(Arc::clone(ledger));
        }
        std::fs::create_dir_all(&self.config.data_dir).map_err(|e| e.to_string())?;
        let ledger = Ledger::open(&self.config.metadata_path()).map_err(|e| e.to_string())?;
        let ledger = Arc::new(ledger);
        *slot = Some(Arc::clone(&ledger));
        Ok(ledger)
    }
}

/// Answers `config_show` on the blocking pool: store and SQLite reads must
/// not stall the async workers that carry every other connection.
pub async fn show(state: &Arc<ServerState>, query: ConfigQuery) -> Outcome {
    let state = Arc::clone(state);
    let work = move || record(&state.project_config, &query);
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(record)) => Outcome::Ok(Response::Config(Box::new(record))),
        Ok(Err(body)) => Outcome::Error(body),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn record(service: &ConfigService, query: &ConfigQuery) -> Result<ConfigRecord, ErrorBody> {
    let internal = |why: String| ErrorBody::new(ErrorCode::Internal, why);
    let base = service.at_base().map_err(internal)?;
    let (base_ref, resolved) = match (query.at, base) {
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::Refused,
                "the project has no base yet; import a repository with `rosen import`",
            ))
        }
        (None, Some((name, resolved))) => (Some(name), resolved),
        (Some(at), base) => {
            let resolved = service.at_commit(&at).map_err(store_body)?;
            let name = base.and_then(|(name, tip)| (tip.commit == at).then_some(name));
            (name, resolved)
        }
    };
    Ok(ConfigRecord {
        commit: resolved.commit,
        base_ref,
        source: resolved.source,
        error: resolved.error,
        config: (*resolved.config).clone(),
    })
}

fn store_body(error: StoreError) -> ErrorBody {
    let code = match error {
        StoreError::NotFound(_) => ErrorCode::NotFound,
        StoreError::WrongKind { .. } => ErrorCode::InvalidArgument,
        _ => ErrorCode::Internal,
    };
    ErrorBody::new(code, error.to_string())
}
