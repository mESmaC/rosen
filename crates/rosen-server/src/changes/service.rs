use super::error::ChangeError;
use super::registry::{self, NewParent};
use rosen_core::ObjectId;
use rosen_protocol::{ChangeFilter, ChangeRecord, NewChange};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

#[path = "readers.rs"]
mod readers;

/// File name of the metadata database inside the server's data directory
/// (D13). Landing, import and conflicts share this one database.
pub const METADATA_DB: &str = "metadata.sqlite";

/// The base tip an unstacked change starts from.
#[derive(Debug, Clone, Copy)]
pub struct BaseTip {
    pub commit: ObjectId,
    pub tree: ObjectId,
}

/// Reads the base ref and its tree. The ref belongs to landing and import,
/// and the tree to the object store, so changes ask through this trait rather
/// than reaching into either.
pub trait BaseSource: Send + Sync {
    fn base_tip(&self) -> Result<BaseTip, String>;
}

/// The base source until import and landing install theirs: there is no base,
/// so only stacked changes could be created, and those need a first change.
pub struct NoBase;

impl BaseSource for NoBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Err("the project has no base yet; import a repository with `rosen import`".into())
    }
}

/// The server's change registry: a lazy writer and bounded read-only WAL
/// connections. A server that never sees a change request creates no database.
pub struct ChangeService {
    db_path: Option<PathBuf>,
    conn: Mutex<Option<Connection>>,
    initialized: AtomicBool,
    readers: readers::Readers,
    base: RwLock<Arc<dyn BaseSource>>,
}

impl ChangeService {
    /// A registry stored in `<data_dir>/metadata.sqlite`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self::with_path(Some(data_dir.into().join(METADATA_DB)))
    }

    /// A registry that lives only as long as the server, for tests.
    pub fn in_memory() -> Self {
        Self::with_path(None)
    }

    fn with_path(db_path: Option<PathBuf>) -> Self {
        ChangeService {
            readers: readers::Readers::new(db_path.clone()),
            db_path,
            conn: Mutex::new(None),
            initialized: AtomicBool::new(false),
            base: RwLock::new(Arc::new(NoBase)),
        }
    }

    pub fn set_base_source(&self, base: Arc<dyn BaseSource>) {
        *self.base.write().unwrap_or_else(PoisonError::into_inner) = base;
    }

    /// Creates a change for a client on the server's own machine.
    pub fn create(&self, request: &NewChange) -> Result<ChangeRecord, ChangeError> {
        self.create_on(request, crate::machines::signing::SERVER_MACHINE)
    }

    /// Creates a change whose owner works on `machine`, which the caller
    /// took from the connection (R14). The request names only the nick, so
    /// one holding `@` could pass for another machine's agent: refused.
    pub fn create_on(
        &self,
        request: &NewChange,
        machine: &str,
    ) -> Result<ChangeRecord, ChangeError> {
        if request.owner.as_str().contains('@') {
            return Err(ChangeError::QualifiedOwner(request.owner.to_string()));
        }
        let base = self
            .base
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.with_conn(|conn| {
            let parent = match &request.on {
                Some(text) => NewParent::Change(registry::resolve(conn, text)?),
                None => {
                    let tip = base.base_tip().map_err(ChangeError::Base)?;
                    NewParent::Base {
                        commit: tip.commit,
                        tree: tip.tree,
                    }
                }
            };
            registry::create_on(conn, &request.owner, machine, parent)
        })
    }

    /// The base's tip a new unstacked change would start from now.
    pub fn base_tip(&self) -> Result<BaseTip, ChangeError> {
        let base = self
            .base
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        base.base_tip().map_err(ChangeError::Base)
    }

    /// Creates a change for `owner` on `machine` starting at `tip`, a base
    /// commit the caller read and checked. Starting there rather than at
    /// the tip on creation means a land in between cannot slip another
    /// start under what the caller checked; the change rebases as usual.
    pub fn create_at(
        &self,
        owner: &rosen_core::AgentId,
        machine: &str,
        tip: BaseTip,
    ) -> Result<ChangeRecord, ChangeError> {
        if owner.as_str().contains('@') {
            return Err(ChangeError::QualifiedOwner(owner.to_string()));
        }
        let parent = NewParent::Base {
            commit: tip.commit,
            tree: tip.tree,
        };
        self.with_conn(|conn| registry::create_on(conn, owner, machine, parent))
    }

    pub fn list(&self, filter: &ChangeFilter) -> Result<Vec<ChangeRecord>, ChangeError> {
        self.with_read(|conn| registry::list(conn, filter))
    }

    pub fn show(&self, change: &str) -> Result<ChangeRecord, ChangeError> {
        self.with_read(|conn| registry::get(conn, registry::resolve(conn, change)?))
    }

    pub fn abandon(&self, change: &str) -> Result<ChangeRecord, ChangeError> {
        self.abandon_releasing(change).map(|(record, _)| record)
    }

    /// Retains the freed paths and waiter identities for post-commit events.
    pub(crate) fn abandon_releasing(
        &self,
        change: &str,
    ) -> Result<(ChangeRecord, rosen_protocol::ReleaseOutcome), ChangeError> {
        self.with_conn(|conn| registry::abandon_releasing(conn, registry::resolve(conn, change)?))
    }

    /// Reads through WAL connections so a registry writer waiting for a land's
    /// SQLite transaction cannot hold foreground facts behind its mutex.
    pub fn with_read<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, ChangeError>,
    ) -> Result<T, ChangeError> {
        if self.db_path.is_none() {
            return self.with_conn(|conn| work(conn));
        }
        if !self.initialized.load(Ordering::Acquire) {
            self.with_conn(|_| Ok(()))?;
        }
        self.readers.with_conn(work)
    }

    /// Runs `work` on the connection, opening and migrating it first if needed.
    /// One connection serialises change writes in-process; SQLite's own locks
    /// cover any other connection to the same file.
    pub fn with_conn<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, ChangeError>,
    ) -> Result<T, ChangeError> {
        let mut slot = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(self.open()?);
            self.initialized.store(true, Ordering::Release);
        }
        work(slot.as_mut().expect("connection opened above"))
    }

    fn open(&self) -> Result<Connection, ChangeError> {
        let conn = match &self.db_path {
            Some(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                return Ok(crate::metadata::open(path)?);
            }
            None => Connection::open_in_memory()?,
        };
        crate::metadata::configure(&conn)?;
        super::schema::migrate(&conn)?;
        Ok(conn)
    }
}
