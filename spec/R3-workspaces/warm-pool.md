- **Warm pool.**
  Each machine's daemon keeps N ready local workspaces at the tip of the base (default 4, 0 turns the pool off).
  Velocity can raise N for a launch, up to the configuration's limit of 1000.
  Only the daemon's own machine may resize or take from its pool: the server refuses both from network peers.
  After each land the daemon brings ready workspaces up to the new tip by rewriting only the changed files, reading only the directories that changed.
  Taking one is atomic, so two changes never get the same workspace.
  A take prefers a prepared workspace and reports whether it is prepared.
  When no ready workspace or template holds the change's tree yet, as when lands outpace the daemon, the take brings another ready workspace to that tree by rewriting only the files that differ, and reports it unprepared.
  Each ready workspace already holds its tree and stat caches (R2), so a take only renames, marks and registers it, whatever the tree's size, and its first snapshot rereads only files written since.
  A caller can ask for a workspace without the pool (`rosen change new --workspace <DIR> --no-pool`): the client sends no take and writes the files from the object store, so a busy pool never holds that workspace up.
