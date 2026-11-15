- Its owner recovers it with `rosen change recover <change> <basis>`, naming the parent tree the change started from.
  The server accepts only a tree the stack's history recorded: a snapshot of the parent change, by commit or tree, or the commit the change's first snapshot followed.
  It never picks one itself and never replaces a recorded basis.
  Recovery refuses a legacy child with no authored snapshots: its head already supplies its exact implicit parent-tree basis.
  The parent's later edits are then merged in like any parent update, keeping every authored snapshot and storing conflicts on the child.
