//! Keeps rebases exclusive with snapshots and offline journal replay.
use super::Snapshotter;
use rosen_core::ObjectId;
use rosen_store::ObjectStore;

impl Snapshotter {
    pub(crate) fn apply_rebased_tree(
        &mut self,
        store: &dyn ObjectStore,
        basis: ObjectId,
        tree: ObjectId,
        ignore: Vec<String>,
    ) -> Result<(), crate::Error> {
        let _writer = self.journal.writer()?;
        self.refresh_basis()?;
        if self.marker.tree != basis {
            return Err(crate::Error::InvalidArgument(
                "workspace changed while fetching the rebase; retry applying it".into(),
            ));
        }
        if !self.journal.pending()?.is_empty() || !self.unstored_paths()?.is_empty() {
            return Err(crate::Error::InvalidArgument(
                "store local edits and replay offline snapshots before applying a rebase".into(),
            ));
        }
        crate::workspace::rebase::apply(store, &self.root, &self.marker, tree)?;
        // Publish cached trees before the new basis: the watcher's next
        // capture rebuilds its index from them while holding this same lock.
        super::open_index(store, &self.root, &self.journal, tree).map_err(crate::Error::from)?;
        self.marker.tree = tree;
        self.marker.ignore = ignore;
        self.marker.write(&self.root)?;
        Ok(())
    }
}
