use super::error::ChangeError;
use super::registry::{self, NewParent};
use rosen_core::ObjectId;
use rosen_protocol::{ChangeFilter, ChangeRecord, NewChange};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

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

/// The server's change registry: one SQLite connection, opened on first use
/// so a server that never sees a change request never creates the database.
pub struct ChangeService {
    db_path: Option<PathBuf>,
    conn: Mutex<Option<Connection>>,
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
            db_path,
            conn: Mutex::new(None),
            base: RwLock::new(Arc::new(NoBase)),
        }
    }

    pub fn set_base_source(&self, base: Arc<dyn BaseSource>) {
        *self.base.write().unwrap_or_else(PoisonError::into_inner) = base;
    }

    pub fn create(&self, request: &NewChange) -> Result<ChangeRecord, ChangeError> {
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
            registry::create(conn, &request.owner, parent)
        })
    }

    pub fn list(&self, filter: &ChangeFilter) -> Result<Vec<ChangeRecord>, ChangeError> {
        self.with_conn(|conn| registry::list(conn, filter))
    }

    pub fn show(&self, change: &str) -> Result<ChangeRecord, ChangeError> {
        self.with_conn(|conn| registry::get(conn, registry::resolve(conn, change)?))
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
        }
        work(slot.as_mut().expect("connection opened above"))
    }

    fn open(&self) -> Result<Connection, ChangeError> {
        let conn = match &self.db_path {
            Some(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let conn = Connection::open(path)?;
                conn.pragma_update(None, "journal_mode", "WAL")?;
                conn
            }
            None => Connection::open_in_memory()?,
        };
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        super::schema::migrate(&conn)?;
        Ok(conn)
    }
}
