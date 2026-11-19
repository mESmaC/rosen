### R1 · Repository import and git mirror
- `rosen import <git repo>` loads a git repository's history and makes its default branch the base.
- After each land, the server pushes the base to the configured git remote (for example GitHub).
  The push runs after the land commits and off its critical path.
  A failed push is retried and reported as an event, and never blocks or undoes a land.
- A push to the mirror is a fast-forward and never a force-push.
- **Adopt a direct push.**
  After landing the remote edits through Rosen, the operator runs `rosen mirror adopt <commit>` on the authoritative server's local endpoint with the full ID of the mirror branch tip.
  The server fetches that branch, requires the named tip and a single best common ancestor, and checks that every file changed between the common ancestor and the mirror tip matches the Rosen base, including deletions and modes.
  Additional Rosen changes on other paths are preserved.
  The server refuses a stale tip or missing edits and tells the operator to inspect the tip or land the edits before retrying.
  A tip already in the base's ancestry needs no new commit.
  The server pauses landing and mirror pushes during adoption and rechecks the remote tip before publishing.
  It stores the remote objects and appends a merge commit with the Rosen base as first parent, the fetched tip as second parent and the existing Rosen tree.
  The base ref, history entry and `mirror` event are committed atomically.
  This ancestry-only entry is excluded from landed-change checks and bisection.
  The mirror worker wakes and pushes the new base through the ordinary fast-forward path.
- Before landing, every resulting tree (including intermediate stacked layers and server reverts) must contain no file larger than `max_file_size` bytes.
  This root `rosen.toml` setting defaults to 104857600 (100 MiB, GitHub's limit), and applies even without a mirror.
  The setting in the attempt's base governs it; a change cannot raise its own limit.
  A refusal has reason `oversize_files`, names every offending path and its byte length, leaves the base unchanged and asks the owner to remove/ignore output or have the operator land a suitable limit first.
  Snapshots may still store larger files.
- **Unmirrored history repair.**
  The operator runs `rosen mirror drop-paths <path>...` against the authoritative server's local endpoint.
  Paths are literal repository-relative files or directories.
  The server pauses requests, landing, recovery and mirror pushes for the operation, drains outstanding post-land work, and rewrites the complete commit graph after the last recorded successful mirror push with those paths removed (the entire reachable graph if no push was recorded).
  It preserves authors, dates and messages, removes signatures invalidated by the rewrite, and never force-pushes.
  It checks actual reachability from all remote refs, including a push accepted before a crash recorded it; if any commit to be changed is already published, it refuses and directs the operator to a normal deletion change or a separately coordinated public history rewrite.
  If any named path is absent from the entire selected range, it refuses and asks the operator to inspect that range and retry with the correct path.
  An inaccessible or changing mirror refuses safely.
- Replacement objects are durable before one metadata transaction moves the base, landed-history/revert references and every affected open/ready change's parent.
  The same transaction filters named paths from open heads and stack bases, invalidates their approvals and verification progress tied to replaced commits, and stages `rebased` and `mirror` events.
  Authored snapshots, receipts, original objects and historical evidence remain; repair prevents public mirroring, not local disclosure or object purging.
  Owners apply server rebases to local workspaces afterwards.
  A crash exposes either the complete old metadata or the complete new metadata; the mirror then resumes fast-forward pushes.
- `rosen import --mirror <remote>` names the remote, a URL or a path.
  It is kept in the metadata store, so it survives restarts.
- A remote named in the base's project configuration replaces the stored one at the next push.
  Removing it from the configuration keeps the last remote.
- A configured remote must be a network remote: an `https://`, `ssh://` or `git://` URL, or an scp-style `[user@]host:path` ssh address.
  The configuration is repository content, so a local path, a remote helper (`<helper>::…`) or a URL with credentials in it makes the file invalid.
  Only `rosen import --mirror`, which the operator runs on the server, can name a local path.
- One background task on the server does every push.
  It pushes the base tip, so lands made during a push go out with the next one.
  It also pushes at server start, so a push a crash interrupted still happens.
- Each failed push publishes a `mirror` event with the error and the retry delay.
  Retries back off from 1 s to at most 5 min.
  A push the remote refused is marked `rejected`.
  A non-fast-forward rejection points the operator to `rosen mirror adopt`.
  The first push that succeeds after failures publishes a `mirror` event too.
- **Accept when:** a repo imported and then landed on shows the same history in `git log` on the mirror, with one commit per change and its trailers.
