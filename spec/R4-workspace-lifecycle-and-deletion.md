### R4 · Workspace lifecycle and deletion
- Rosen is the only thing that creates or deletes workspaces.
  Velocity calls `rosen-client` instead of `git worktree`.
- A workspace is deleted automatically when its change lands or is abandoned.
- **Deletion guard.**
  Before deleting, Rosen flushes a final snapshot.
  It refuses deletion while any of these is not stored on the server:
  - a write the watcher has seen but not yet snapshotted;
  - a journal entry that has not been uploaded;
  - a file whose content differs from the last stored snapshot, found by a final hash scan.
    Ignored paths (R2) are not scanned.

  The refusal keeps the workspace and reports the total number and up to five examples when it names a set of files.
- On Windows, final removal hashes write-excluded file handles and deletes those exact objects while excluding concurrent renames.
  When Windows denies a delete on a read-only object, Rosen clears `FILE_ATTRIBUTE_READONLY` and retries across workspace deletion, staging sweeps and the warm pool.
  Junctions and symlinks are classified through handles opened without following reparse points, then removed through those same handles without traversing their targets.
  Ignored links need no stored content; other links must match their stored target text, and unknown reparse tags keep the workspace with a reason naming the path and tag.
  On Unix, best-effort removal opens files without following symlinks, verifies their stored blob IDs through those handles, isolates each selected file in a private directory relative to its opened parent, rechecks its device and inode against the held handle, rehashes it, and removes it with `unlinkat`.
  A replacement detected after isolation stays in that directory, named in the refusal, for recovery.
  Nonempty isolation directories always keep the workspace, even when ignore rules match them.
  Unix does not exclude writes through already-open file descriptors; its final hash detects changes completed before that read.
  Directories are removed only when empty; newly created files are retained.
  Cleanup interrupted after removing durable files keeps the remaining directory for recovery; an interrupted Unix isolation may require recovery of the named entry before retry.
  Other platforms retain the workspace with an unsupported-operation reason.
- A workspace whose change is closed or failed is kept until the change is explicitly deleted.
- Deleting a workspace removes only that workspace.
  Local workspace registrations survive daemon restart; the daemon retries guarded cleanup of landed and abandoned workspaces on startup and change events.
  Pending warm pool prepares (R3) never postpone it: the daemon acts on a change event as soon as the prepare command running at the time finishes.
  A refused cleanup retains the registration and records the reason on it, which `rosen workspace list` shows.
  An automatic retry waits 1 s after the first refusal, then doubles the wait after each subsequent refusal up to 1 min.
  Each workspace has its own retry deadline; a change event can trigger cleanup of other workspaces without advancing that deadline.
  Cleanup passes on one machine run one at a time.
  A cleanup interrupted after removing verified files finishes on retry, falling back to stored `.gitignore` files when an earlier pass already removed them on disk.
  Startup and cleanup passes also sweep leftover `.rosen-delete-*` folders owned by the project under known workspace roots, applying the same guard and per-folder retry backoff.
- **Accept when:** no sequence of crashes, deletions, offline periods and reconnects loses an edit that was written to a workspace.
