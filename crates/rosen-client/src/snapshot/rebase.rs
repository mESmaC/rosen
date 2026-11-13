//! Keeps rebases exclusive with snapshots and offline journal replay.
use super::Snapshotter;
use rosen_core::ObjectId;
use rosen_store::ObjectStore;

impl Snapshotter {
    pub(crate) fn apply_rebased_tree(
        &mut self,
        store: &dyn ObjectStore,
        tree: ObjectId,
    ) -> Result<(), crate::Error> {
        self.hold_for_deletion()?;
        let _writer = self.journal.writer()?;
        if !self.journal.pending()?.is_empty() || !self.unstored_paths()?.is_empty() {
            return Err(crate::Error::InvalidArgument(
                "store local edits and replay offline snapshots before applying a rebase".into(),
            ));
        }
        crate::workspace::rebase::apply(store, &self.root, &self.marker, tree)?;
        self.marker.tree = tree;
        self.marker.write(&self.root)?;
        // Cache the new tree metadata for the next offline watcher.
        super::open_index(store, &self.root, &self.journal, tree).map_err(crate::Error::from)?;
        Ok(())
    }
}
