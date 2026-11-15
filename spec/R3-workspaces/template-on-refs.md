- **Template on ReFS.**
  On a ReFS volume the daemon keeps one prepared **template** workspace at the base tip.
  It makes ready workspaces, and any workspace beyond the pool, as block clones of the template, including its ignored build output (R2).
  Each starts prepared at almost no copy cost.
  A cloned file keeps its source's modified time: build tools compare those times, and a clone stamped with the moment it was made would have its whole build output rebuilt.
  Until the template is prepared at the tip, ready workspaces stay as they are and the take reports them unprepared.
  Without ReFS, each ready workspace is prepared on its own.
