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
- **Catch up from the mirror.**
  When another Rosen server for the same project landed on the mirror, the operator runs `rosen mirror catch-up` on the server's local endpoint.
  The server pauses landing and mirror pushes, fetches the mirror branch and compares its tip with the base.
  A tip the base already contains changes nothing; the ordinary push sends the rest.
  When the base is an ancestor of the tip, the server stores the new commits with their git IDs unchanged and moves the base to the tip by compare-and-swap.
  The same transaction records each commit on the tip's first-parent line in base history as imported (`mirror` tier, no change, post-land work done), records the tip as pushed and stages a `mirror` event with status `caught_up`.
  Imported commits are excluded from landed-change checks, landed reviews and bisection, because the other server landed and checked them.
  The server then rebases every open change built on an older base onto the new base, with the usual `rebased` and `conflict` events; a change that cannot move is reported and rebases when it lands.
  When the base has commits the tip lacks and the tip has commits the base lacks, the server refuses and moves nothing, naming both tips and up to 20 commits on each side, so a person or `rosen mirror adopt` decides.
  Catching up keeps the server's event sequence, cursors, machines and channel history, so clients do not see a new database.
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
- **Mirror owner.**
  Only the project's authoritative server pushes its mirror.
  A server pushes only when it is marked as the mirror owner; Velocity marks the one it starts for each project.
  `rosen-server --mirror-owner` sets the mark, `--not-mirror-owner` removes it, and a daemon a client starts is marked when `ROSEN_MIRROR_OWNER` names its data directory.
  The mark is kept in the data directory with that directory's own path, so it survives restarts and a copied data directory is not an owner.
  Any other server, including a second machine's server for the same project or one an agent starts for a test, keeps the mirror setting but never pushes.
  The same mirror-owner mark gates the Collapse state feed (R9), ledger push (R13) and action relay (R14), so a server that is not the owner sends nothing to Collapse and runs no relayed action.
  It publishes one `mirror` event with status `not_owner` per run when its base is ahead of the mirror, and `rosen server status` says whether the server pushes and reports `not the mirror owner; Collapse state not sent` when it is not the owner.
- **Remote tip check.**
  Before each push, the server reads the mirror branch's tip.
  The push goes ahead when the branch is missing, is the commit this server last pushed, or is another commit the base holds, such as a push a crash kept it from recording; a tip equal to the base is recorded as pushed without a push.
  Any other tip means another writer pushed there: the server refuses with a `rejected` failure naming the tip, the commit it last pushed, the base and up to 20 commits the mirror has that the base lacks, and pushes nothing.
- **Proactive HTTPS auth fallback.**
  When an HTTPS mirror read fails with repository not found, the server retries once with `-c http.proactiveAuth=basic` when git is 2.46 or later, and reports that git must be upgraded or `http.<url>.proactiveAuth` set when git is older.
  Once that retry succeeds, the server uses proactive basic auth for later mirror reads in its lifetime and reports it in `rosen server status`.
- **Link with Collapse (`rosen collapse link|unlink|check`).**
  `rosen collapse link <owner>/<name> [--host https://api.collapse.sh] [--relay approve,land,mark,config] [--land] [--reset]` links a running server to Collapse in one command.
  `--relay` defaults to `approve,land,mark,config` and accepts the R14 kinds (`approve`, `land`, `mark`, `config`, `revoke`, or `none` alone) as well as the aliases `review-mark`, `config-change` and `machine-revoke` (with `-` or `_`).
  It reads a push token from stdin or `ROSEN_COLLAPSE_TOKEN` (never as an argument), sets `credential.<origin>.useHttpPath` to `true` before approving git credentials for `.git`, `/rosen/state` and `/ledgers`, sets `http.<mirror>.proactiveAuth` to `basic`, opens and names a change setting `[mirror].remote` and `collapse` in `rosen.toml` (landing it when `--land` is given), sets the relay allow-list and runs `rosen collapse check` (polling briefly if the relay is on and `/rosen/doorbell` is not yet published).
  With `--reset` (also accepted without `<owner>/<name>` on an already-linked server), it asks Collapse (`POST <collapse>/rosen/state/reset`) to clear that repository's `rosen_*` feed tables, persists a fresh `feed_id`, resets local feed cursors and resumes sending after a `diverged` feed.
  `rosen collapse unlink [--land] [--forget-credentials]` reverses the `rosen.toml`, relay and proactive-auth steps and leaves stored credentials and `credential.<origin>.useHttpPath` alone unless `--forget-credentials` is given, which rejects only that repository's path-scoped entries.
  `rosen collapse check` reads the server's configuration without making changes and probes each requirement of a Collapse link: the mirror-owner mark, the `collapse` root in the base's `rosen.toml`, owner and name agreement between `collapse` and `[mirror]`, `credential.useHttpPath` for HTTPS origins, git push credential presence for `.git`, `/rosen/state` and `/ledgers` (never printing any token), a mirror `ls-remote` with and without proactive auth, empty-batch reads of `/rosen/state` and `/ledgers` (reporting `diverged` on HTTP 409 `source_changed` or when Collapse holds a higher sequence than this server's local log), and the relay allow-list and `/rosen/doorbell` read (`skipped` when the relay is off, and treating HTTP 404 as `skipped` "not yet published" for about 2 minutes after enabling the relay).
  Each check reports `pass`, `fail` or `skipped` with a fix hint, and the command exits non-zero when any check fails.
  Golden wire fixtures for wire schema `v1` (`"format": 1`) live in `fixtures/collapse/v1/`, covering the state feed (R9), ledger push (R13) and action relay (R14).
- Each failed push publishes a `mirror` event with the error and the retry delay.
  Retries back off from 1 s to at most 5 min.
  A push the remote refused is marked `rejected`.
  A non-fast-forward rejection points the operator to `rosen mirror catch-up` when another server landed there, and otherwise to `rosen mirror adopt`.
  The first push that succeeds after failures publishes a `mirror` event too.
- **Accept when:** a repo imported and then landed on shows the same history in `git log` on the mirror, with one commit per change and its trailers.
