- **Offline journal.**
  When the server is unreachable, snapshots go to a local append-only journal and are uploaded in order on reconnect.
  Each captured snapshot retains its bytes even after later writes or a client restart.
  Records live under `.rosen/journal`, with their blobs in a workspace-local object store, and each record is removed once the server acknowledges it.
  While the server is reachable and no record waits, a snapshot goes straight to the server without a record.
  While the server stays unreachable, the watcher tries again after 1 s, doubling the pause after each further failure up to 16 s; a retry only uploads what the journal holds, without reading the workspace again, and a new write is captured on the usual schedule whatever the pause.
  Each replayed record is reported as its own snapshot.
  A record the server refuses for good, as when the change landed or was abandoned while the client was away, halts the journal: that record and every later one stay, later snapshots are journaled behind them without uploading, and each attempt reports the refusal, so the deletion guard (R4) keeps the workspace.
  The halted journal replays again once the change is open and is no longer as the server showed it just before the refused records were sent: reopened, as after a post-land revert (R12), or given a new head, as by a server rebase.
  Until then an attempt only reads the change and uploads nothing, since the same change would refuse the same record again.
  The watcher stops on a halt for a closed or missing change; on any other refusal it keeps capturing writes and retries on the usual schedule.
  A record the server fails with an internal error, which may be a passing store or database failure, stays first in the journal and the watcher keeps retrying it on the same schedule; a retry of a tree whose objects the server already took sends only the snapshot request again.
  A workspace-local tree cache allows the watcher to start offline.
  A workspace-local stat cache keeps each tracked file's size and modification time with the blob it held, recorded when the workspace is materialised and after each full scan, and only for times at least 2 s old, as a later write in the same clock tick could leave a newer one unchanged.
  A full scan by a new writer, such as `rosen commit` or a watcher start, skips tracked files whose size and modification time match a trusted cached stat; it rereads new files and files with missing or differing stats.
  A write that preserves size and restores the cached modification time can therefore be missed by a full scan. A watcher notification for a file forces that file to be reread regardless of its stats; workspace deletion (R4) always verifies content. See [Held files](held-files.md) for the full-read commit escape hatch.
  Snapshot writers take turns: a second writer, such as `rosen commit` while the watcher runs, waits for the first instead of failing.
  A writer that finds another writer's snapshot captured since its own scans every file, so it never stores an older version of a file the other stored.
  A named snapshot whose answer was lost is looked up before it is replayed, so its commit is recorded once.
  Landing requires the server.
