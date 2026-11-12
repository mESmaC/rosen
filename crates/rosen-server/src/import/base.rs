//! The base that new changes start from, read from where import and landing
//! keep it: the base ref in the metadata store and its commit in the object
//! store.

use std::sync::{Arc, Mutex, PoisonError};

use rosen_store::{LooseObjectStore, ObjectStore};

use crate::changes::{BaseSource, BaseTip};
use crate::landing::Ledger;
use crate::ServerConfig;

/// Reads the base ref on every call, so a change always starts from the
/// current tip, whether import set it or a land has moved it since.
pub struct LedgerBase {
    config: ServerConfig,
    /// Opened on first use, so a server that never creates a change never
    /// creates the database.
    ledger: Mutex<Option<Arc<Ledger>>>,
}

impl LedgerBase {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            ledger: Mutex::new(None),
        }
    }

    fn ledger(&self) -> Result<Arc<Ledger>, String> {
        let mut slot = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ledger) = slot.as_ref() {
            return Ok(Arc::clone(ledger));
        }
        std::fs::create_dir_all(&self.config.data_dir).map_err(|e| e.to_string())?;
        let ledger =
            Arc::new(Ledger::open(&self.config.metadata_path()).map_err(|e| e.to_string())?);
        *slot = Some(Arc::clone(&ledger));
        Ok(ledger)
    }
}

impl BaseSource for LedgerBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        let base = self.ledger()?.project_base().map_err(|e| e.to_string())?;
        let Some((_, commit)) = base else {
            return Err(
                "the project has no base yet; import a repository with `rosen import`".into(),
            );
        };
        let store = LooseObjectStore::open(self.config.objects_dir()).map_err(|e| e.to_string())?;
        let tree = store.read_commit(&commit).map_err(|e| e.to_string())?.tree;
        Ok(BaseTip { commit, tree })
    }
}
