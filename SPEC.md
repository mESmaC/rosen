# SPEC.md · Rosen

Rosen is a version control system built so that many coding agents can work on one project at the same time: 100 agents in v1 and up to 1000 later. Rosen is two programs: a **server** that holds the project and does the expensive work, and a thin **client** that agents and humans use. Rosen is the source-control layer under [Velocity](https://github.com/mESmaC/velocity), the parallel agentic IDE. Where a cleaner or faster Rosen design needs Velocity to change, Velocity changes (§9).

- **Status:** draft, 2026-11-11. Written from the spec interview, then revised after reviewing the prior art below. Revised 2026-11-12: one server for agents on many machines, with AIRC served by Rosen and zero-touch enrollment (#25); snapshot ignore rules (#28); rebases applied when the workspace owner asks (#29); receipts reused across independent lands (#26); Rosen prepares pool workspaces (#31).
- **Optimizing for:** development velocity. Landing must be fast and must never lose work.

## 0. Prior art and what Rosen takes from it

| Source | Idea | Rosen |
|---|---|---|
| Anthropic, [Redesigning source control](https://www.anthropic.com/research/redesigning-source-control) (2,500 → 30,000 merges/day) | No global merge lock: a ref moves by compare-and-swap on one row | R8 landing |
| Anthropic | Server-side commits, rebases and merges; "nothing to push"; the client "can compute very little on its own" | D17, R2, R8 |
| Anthropic | Refs discouraged except main and release branches, because global state is a scaling problem | Changes are not refs (D18) |
| Anthropic | Changesets stored as deltas against the first parent, with derived data (blame, path history, ancestry) built from them | R11 history API |
| Anthropic | Tiered content-addressed caches: a per-node daemon, then regional, then authoritative, all answering the same RPCs | D8: each machine's daemon is tier 1 in front of the project's server; a hosted server takes that place later behind the same RPCs |
| Anthropic | Source control as a service, so many jobs need no checkout | Server-side workspaces, R11 |
| Anthropic | Post-merge and pre-deploy testing replace pre-merge gating at agent scale | D20, R12 |
| Anthropic | Review intent, with semantic approval rules (comment-only vs logic changes) | R7 review tiers |
| [CarterT27/scion](https://github.com/CarterT27/scion) (clean-room Rust reimplementation) | Landing = rebase a linear stack onto the ref, then versioned compare-and-swap, with retry and backoff | R8 |
| CarterT27/scion | `--json` with versioned output schemas; a per-node daemon on a local socket | R10, D8 |
| CarterT27/scion | Verification: an independent reference model, git differential tests, fault injection | §5; fits the proof fixture tables |
| CarterT27/scion | Measured result: git still checks out large full trees faster than a fetch-and-write client | R3: workspaces clone from a local store, not over the network |
| Google [Scion](https://github.com/googlecloudplatform/scion) (agent orchestration) | Agents have their own identity; notifications when work finishes or needs attention; "less is more", with agents learning the CLI from help text | Change owner identity (§2), targeted events (R9), self-describing CLI (R10) |
| Velocity SPEC §3.3, F5–F7, F14 | Receipts bound to an exact head and base, re-checks when the base moves, targeted rebases, 3-line overlap rule, warm pool, dependency waits and parking | R4, R6, R8; dependency waits become stacked changes (D19) |

**Not adopted:** Spanner, CockroachDB or S3 (v1 is one server with SQLite and a local volume); a virtual filesystem; and Anthropic's "always online, no offline" rule. Rosen keeps an offline fallback (D6).

---

## 1. Decisions

| # | Decision | Choice |
|---|---|---|
| D1 | Product | A VCS with a CLI and a server, which replaces git for high-concurrency agent workflows. |
| D2 | Concurrency model | Each agent works on its own **change**. Changes merge automatically at file level, and conflicts are stored as first-class objects instead of errors. |
| D3 | Landing model | **Optimistic landing, with no global queue or lock.** A land is a compare-and-swap of the base ref. This replaces the merge queue first chosen: Velocity and Anthropic both found queues too slow at agent scale. |
| D4 | Stack | Rust, in one Cargo workspace. |
| D5 | Storage | Objects are stored in **git format** (blobs, trees, commits), as loose content-addressed objects with no packfiles, so no repack or maintenance is needed. Rosen imports git repos and pushes landed history to a git remote. Storage sits behind a pluggable backend interface: the git-backed backend comes first, and a Rosen-native remote store comes later. |
| D6 | Workspaces | Two modes. **Local workspaces** are files on disk for agents' tools. **Server-side workspaces** have no checkout, and are read and written through the API. Both keep resource use bounded. Local workspaces keep working when the server is unreachable (offline fallback, R2). |
| D7 | Velocity integration | A `rosen-client` library crate that Velocity links directly. The `rosen` CLI is a thin layer over the same crate. |
| D8 | Server deployment *(revised)* | **One authoritative server per project**, reachable from every machine that runs agents for it (R14). Every machine runs a **daemon**, which `rosen-client` or Velocity can start. On the server's machine the daemon is the server. On every other machine it is the tier-1 object cache in front of the server, answering the same RPCs, so workspaces materialise from a local store. A hosted server can later take the authoritative role behind the same RPCs. |
| D9 | Check runner | Checks run **outside** Rosen: Velocity's build queue, PR duty, or any other client. They submit a **receipt**. Rosen stores and validates receipts and runs no builds. |
| D10 | Coordination *(revised)* | **Rosen owns all coordination state**: path state (each change's live edited paths and line regions, and path claims) and the project's AIRC channel (task claims, events, messages and ledgers, R13). Velocity keeps AIRC's agent tools and view, and calls Rosen for every one, so every machine sees the same channel. |
| D11 | Platforms | v1 is tested and shipped on Windows 11. The code stays portable, and Linux and macOS are best-effort until a later milestone. |
| D12 | Review | **Rosen-native review**: comments, approvals and receipts are Rosen objects. GitHub receives landed history as a mirror and is not in the landing path. |
| D13 | Metadata store | Embedded SQLite in WAL mode, one database per server. |
| D14 | Scale target *(revised)* | v1: at least 100 concurrent changes on one project, from agents on several machines, typically 30–40 agents per machine (see §5). |
| D15 | Landed history | **One commit per change**: the change's final tree, with trailers for the change ID, receipt and approval. The agent's own commits stay in Rosen and are never rewritten. |
| D16 | Workspace lifecycle | **Rosen owns it**: creation, a warm pool, and deletion behind a guard. |
| D17 | Write-through snapshots *(revised)* | Every workspace write is snapshotted to the server automatically and within seconds, like jj's working-copy commit. There is no push or sync step. An edit that exists only on the client is the exception (offline, or inside the snapshot window), not the norm. This makes the deletion guard simple, gives the server live edit regions, and lets the server do every rebase. |
| D18 | Changes are not refs *(revised)* | The base (and later, release refs) is the only ref. A change is a row in the metadata store, with no branch name to create, delete or collide. |
| D19 | Stacked changes *(revised)* | A change can sit on top of another unlanded change instead of waiting for it to land. When the lower change lands or updates, the server rebases the upper one automatically. This replaces Velocity's dependency waits and parking for most cases. |
| D20 | Landing gate | **Fast pre-land, full post-land.** A land needs a receipt for build and lint on the exact merge result. Full tests run on landed commits in batches. A failure is bisected to the culprit, and the server lands a revert automatically and reopens the culprit change for its owner (Anthropic's post-merge testing, R12). |
| D21 | Network access and enrollment *(new)* | The server listens on a network listener with TLS as well as the local transport. Machines join with **join keys**, with no login, monitor or remote desktop on the joining machine. Each machine gets its own revocable credential. Every agent on a machine works under that machine's credential, and Velocity names the agent and its role (R14). One enrollment adds a machine to both Rosen and Velocity. |

Defaults set during spec writing. These follow from the decisions above, and changing one is a product decision:
- **Transport.** Each daemon listens on a local transport for its own machine's clients: a named pipe on Windows and a Unix socket elsewhere, limited to the OS user. The server also listens on a TCP port with TLS for enrolled machines' daemons (R14). The protocol is versioned and does not assume a local peer.
- **Events.** The server pushes events to subscribed clients. Clients never poll.
- **Garbage collection.** v1 never deletes objects from the store.

---

## 2. Concepts

- **Project.** One repository held by one server. It has one **base** ref (for example `main`), which only a land can move.
- **Machine.** A computer enrolled in a project (R14). It holds its own revocable machine credential and runs a daemon that caches objects for its workspaces.
- **Channel.** The project's AIRC channel, served by the server (R13): task claims, events, messages and ledgers.
- **Change.** One agent's unit of work. Each change has:
  - a stable **change ID**;
  - an **owner** (agent identity: its machine and the nick Velocity gave it, R14);
  - a **parent**: either a base commit or another change (stacked);
  - an ordered list of commits and a current **head** tree;
  - a state;
  - any stored conflicts.

  States: `open` → `ready` → `landed`, or `abandoned`. `ready` means the owner asked to land.
- **Snapshot.** An automatic commit of a workspace's current files. Snapshots are made by the client from file writes and stored on the server (D17). Explicit `rosen commit` messages are named snapshots.
- **Stack.** A chain of changes, each a parent of the next. A stack lands bottom first.
- **Workspace.** The files a change is edited in. Each workspace belongs to exactly one change, and is either **local** or **server-side**.
- **Conflict.** A stored object recording the base, ours and theirs for a region that did not merge automatically. A change that has conflicts cannot land.
- **Receipt.** A record that the required checks passed on one exact merge result. It is bound to a `(change head, base commit, check set)` triple and signed by its submitter's identity.
- **Approval.** A reviewer's approval of a change at a given head.
- **Path claim.** A change's declared intent to edit a path or glob. The first claim wins. Other changes record a **share** (they edit it too, knowingly) or a **wait** (they hold off until the claim is released).
- **Edit regions.** The paths and line ranges a change has modified relative to its parent. The server computes them from snapshots.
- **Overlap.** Two changes overlap when they claim the same path, or their edit regions in one file touch or sit within 3 lines of each other. Per-project configuration lists **generated files** and **registration lists**, which never count as an overlap.
- **Build unit.** A part of the project that builds on its own, together with the units it depends on. By default each package of a Cargo workspace is a unit, read from the workspace's manifests. Project configuration can list units instead, as path globs with their dependencies.
- **Workspace-wide inputs.** Paths every unit's build reads:
  - the root manifest and lockfile;
  - toolchain files and build configuration (such as `.cargo/`);
  - CI config;
  - the project configuration itself;
  - any other path that lies in no unit and is not prose.
- **Reach.** The units a change can affect: the units holding the paths it changed, plus every unit that depends on one of them, directly or through others.
  - A prose-only change (R7) reaches no unit.
  - A change to a workspace-wide input reaches every unit.
- **Independent.** Two changes are independent when their reaches share no unit. Then no unit's build sees both changes.
- **Project configuration.** The file `rosen.toml` at the repository root. The server reads it from the base, never from a change, so a change takes effect only once it lands.
  - It lists generated files, registration lists and sensitive paths as path globs, the pre-land and post-land check sets (a name and the commands checkers run), the warm pool size and the mirror remote. Each part is optional and has a default: no paths, check sets named `pre-land` and `post-land` with no commands, a pool of 4 and no remote.
  - Optional `[build_units.<name>]` tables contain `paths` globs and `dependencies` unit names; absent units are discovered from Cargo workspace manifests, including target, build and dev dependencies. `workspace_inputs` adds shared input globs. Unknown dependency names conservatively reach every unit.
  - Unknown keys are refused, so a misspelt key cannot drop a list silently.
  - An invalid file never takes effect. The configuration in force is the one in the nearest first-parent ancestor of the base whose file is valid, or the defaults. The change that fixes the file still lands under the last valid configuration.
  - `rosen config show` prints the configuration in force and why the base's file was refused, if it was.

---

## 3. Architecture

| Crate | Holds |
|---|---|
| `rosen-core` | Data model: changes, stacks, conflicts, receipts, reviews, claims, regions. Pure logic, with no I/O. |
| `rosen-store` | The storage backend interface and the git-format loose-object backend. |
| `rosen-platform` | Narrow OS primitives with safe handle interfaces, including verified Windows file disposition. |
| `rosen-merge` | Three-way file merge, conflict objects and overlap detection. |
| `rosen-server` | The daemon: protocol, the SQLite metadata store, snapshots, server-side rebase, landing, claims, review, events, the AIRC channel, history queries, server-side workspaces, the git mirror push, the network listener and enrollment. On a machine that is not the server, the same daemon runs as the object cache that forwards to the server. |
| `rosen-client` | The public library API Velocity links: daemon discovery and start, joining a project, workspace materialisation, the file watcher and snapshotting, applying server rebases, and the offline journal. |
| `rosen-cli` | The `rosen` binary, a thin layer over `rosen-client`. |

- **Client–server asymmetry.** The server does every merge, rebase, landed commit and history query. The client materialises files, watches them, and uploads snapshots. Clients never run merge logic.
- **Topology.** One authoritative server per project. On every other enrolled machine, the daemon serves its clients over the local transport, answers object reads from its own cache, and forwards every other request to the server. Refs, claims, the channel and every other piece of mutable state are read and written only on the server, so no machine can fall behind or move ahead of it.
- **Metadata store.** SQLite in WAL mode, on the server only, holds changes, stacks, claims, regions, receipts, reviews, conflicts, workspace records, the channel, enrolled machines and the base ref. Every ref move and every claim is one transaction, and a crash in the middle leaves the store either before or after it, never between.
- **Metadata initialization.** Each server metadata connection enables WAL, full synchronous durability, a bounded busy timeout and foreign keys. The core change, snapshot, claim, conflict, review and landing tables are installed together in one transaction, recorded by SQLite `user_version`. Existing unversioned rows are preserved; a database with a newer schema version is refused. Module-owned derived indexes and event tables are initialized by their services.
- **Object store.** Git object format, stored loose and content-addressed. The server's store is authoritative. Each other machine's daemon keeps a store of the objects its workspaces use, filled from the server on demand and shared by every workspace on that machine. Objects are immutable, so any cache tier can hold them without coordination.
- **Protocol.** Versioned, framed request/response plus one event stream per client. The same protocol runs over the local transport and over the network listener.

---

## 4. Features

### R1 · Repository import and git mirror
- `rosen import <git repo>` loads a git repository's history and makes its default branch the base.
- After each land, the server pushes the base to the configured git remote (for example GitHub). The push runs after the land commits and off its critical path. A failed push is retried and reported as an event, and never blocks or undoes a land.
- A push to the mirror is a fast-forward and never a force-push.
- `rosen import --mirror <remote>` names the remote, a URL or a path. It is kept in the metadata store, so it survives restarts.
- A remote named in the base's project configuration replaces the stored one at the next push. Removing it from the configuration keeps the last remote.
- One background task on the server does every push. It pushes the base tip, so lands made during a push go out with the next one. It also pushes at server start, so a push a crash interrupted still happens.
- Each failed push publishes a `mirror` event with the error and the retry delay. Retries back off from 1 s to at most 5 min. A push the remote refused, for example because the mirror has commits the base lacks, is marked `rejected`: only a person can fix that. The first push that succeeds after failures publishes a `mirror` event too.
- **Accept when:** a repo imported and then landed on shows the same history in `git log` on the mirror, with one commit per change and its trailers.

### R2 · Changes, snapshots and offline work
- `rosen change new [--on <change>]` creates a change on the current base, or stacked on another change, together with its workspace. The change ID stays the same for its whole life.
- **Write-through snapshots.** The client watches the workspace (`ReadDirectoryChangesW` on Windows) and uploads a snapshot within 2 s of the last write, plus on `rosen commit`. A snapshot sends only the changed blobs.
- **Ignored paths.** Snapshots skip paths matched by the workspace's `.gitignore` files and by the project's ignore list (project configuration), such as build output (`target/`, `node_modules/`). An ignored path is never uploaded, never counts as an edit or an overlap (R6), and never blocks deletion (R4). Writes to ignored paths do not delay a snapshot.
- **Offline journal.** When the server is unreachable, snapshots go to a local append-only journal and are uploaded in order on reconnect. Each captured snapshot retains its bytes even after later writes or a client restart. Records live under `.rosen/journal`, with their blobs in a workspace-local object store, and each record is removed once the server acknowledges it. While the server is reachable and no record waits, a snapshot goes straight to the server without a record. A workspace-local tree cache allows the watcher to start offline. Snapshot writers take turns: a second writer, such as `rosen commit` while the watcher runs, waits for the first instead of failing. Landing requires the server.
- Snapshots and commits belong to the change and are never rewritten.
- **Applying server rebases.** When the server rebases a change (R5, R8, D19), the rebased tree is written into a local workspace only when the workspace's owner asks. The client exposes `rebase_change` to compute and persist a conflict-free server rebase, preserving all snapshot commits. Snapshots sent before local application name their workspace basis; their edits are merged onto the rebased head while their original commit trees are kept. The client exposes `apply_rebase(root, owner)` to apply the stored tree explicitly. Applying requires paused tools and watcher, refuses unstored edits and pending offline snapshots, and preserves ignored output. Velocity asks at the agent's step boundary, or while the agent is parked, so files never change under an agent mid-step (§9).
  - The client first flushes a final snapshot, then writes the rebased files, with conflict markers for any stored conflict (R5).
  - If the files on disk differ from that snapshot, it refuses and leaves the workspace unchanged, and the owner asks again after the next snapshot.
  - A server rebase never writes into a local workspace by itself. A server-side workspace (R3) takes the rebased tree directly.
- **Accept when:** a change's commits survive any number of rebases unchanged; a write is on the server within 2 s while the server is reachable; and applying a rebase never overwrites a write that is not in a stored snapshot.

### R3 · Workspaces
- **Local workspaces** are materialised from the local store:
  - **ReFS block clone** (Windows Dev Drive, or any ReFS volume) is the fast path, and true copy-on-write. Rosen recommends putting workspaces on a Dev Drive.
  - Otherwise a **parallel plain copy**.
  - **Hardlinks are never used** for files in a workspace. Agents' tools often write files in place, so a hardlink would let one workspace's write corrupt the shared object and every other workspace that links to it.
  - Sparse checkout lets a workspace hold only chosen paths. A chosen path holds itself and everything under it.
  - A workspace is built in a hidden directory beside its destination and renamed into place when complete, so a crash never leaves a partial workspace.
  - A block clone is checked by hashing one cloned file against its blob before the rest are cloned. On a mismatch, or any clone failure, the workspace is written by plain copy instead.
  - On Windows a symlink in the tree is written as a plain file holding its target, as git does by default there.
- **Server-side workspaces.** Files live on the server, and clients read, write, list and diff them through the API, with no checkout. Writes become snapshots directly. `workspace_read` reads a file or lists a directory on the current change head; `workspace_diff` compares it with its parent. `workspace_write` writes bytes (new files are regular files and existing modes are preserved), or deletes a file with `None`, and returns its stored snapshot. Parent directories are created as needed and empty directories are pruned on deletion. Concurrent writes each build on the preceding head. Reads and writes follow server rebases directly. Rosen recommends them for jobs that touch few files, such as reviewers, check runners and doc edits.
- **Warm pool.** Each machine's daemon keeps N ready local workspaces at the tip of the base (default 4, 0 turns the pool off). Velocity can raise N for a launch. After each land the daemon brings ready workspaces up to the new tip by rewriting only the changed files. Taking one is atomic, so two changes never get the same workspace. A take prefers a prepared workspace and reports whether it is prepared.
- **Prepared workspaces.** Project configuration lists **prepare commands**. These are steps whose results don't depend on the workspace's path or its agent, such as installing pinned toolchains, fetching dependency caches (`lake exe cache get`) and a prebuild (`cargo build --workspace`). The daemon runs them in each ready workspace, so an agent's first build compiles only what it changes.
  - Prepare commands run for one workspace at a time per machine, at below-normal priority, so agents' own builds go first.
  - After a land, the daemon runs them again in each ready workspace unless every change landed since it was prepared is prose-only (R7).
  - A workspace whose prepare failed is retried when the base next moves. It can still be taken, and the take reports it unprepared.
- **Template on ReFS.** On a ReFS volume the daemon keeps one prepared **template** workspace at the base tip. It makes ready workspaces, and any workspace beyond the pool, as block clones of the template, including its ignored build output (R2). Each starts prepared at almost no copy cost. Without ReFS, each ready workspace is prepared on its own.
- **Per-agent setup stays with the owner.** Steps that depend on the agent, its credentials or its final path run after the take, by whoever took the workspace. Velocity copies untracked files such as `.env`, links skills and runs its after-take setup command (§9). Rosen never stores those files.
- **Load limits.** Each daemon caps its machine's concurrent materialisations, and the server caps server-side workspace I/O; both queue the rest. The client caps local disk use. A request over a cap waits; it never fails or degrades another workspace.
  - Machine admission is shared by clients and pool workers through OS file leases under the daemon's data directory. Defaults are 4 simultaneous materialisations and a 100 GiB logical workspace-content budget; the server defaults to 8 concurrent workspace I/O operations. The library can initialize other positive limits and change the disk cap while requests wait.
  - Disk admission reserves the target files plus source-template content before copying, counts existing workspace files (including ignored output) and crash staging files, and keeps pool workspaces charged after a take. Deletion frees their budget. These limits control Rosen's workspace creation; agents' later filesystem writes are counted on the next admission scan.
- **Accept when:** with the warm pool filled, a new prepared workspace is ready in under 2 s; an agent's first build in it compiles only what the agent changed; and two changes never share a workspace.

### R4 · Workspace lifecycle and deletion
- Rosen is the only thing that creates or deletes workspaces. Velocity calls `rosen-client` instead of `git worktree`.
- A workspace is deleted automatically when its change lands or is abandoned.
- **Deletion guard.** Before deleting, Rosen flushes a final snapshot. It refuses deletion while any of these is not stored on the server:
  - a write the watcher has seen but not yet snapshotted;
  - a journal entry that has not been uploaded;
  - a file whose content differs from the last stored snapshot, found by a final hash scan. Ignored paths (R2) are not scanned.

  The refusal names the files and keeps the workspace.
- On Windows, final removal hashes write-excluded file handles and deletes those exact objects while excluding concurrent renames. Directories are removed only when empty; newly created files are retained. Cleanup interrupted after removing durable files keeps the remaining directory for recovery. Platforms without an equivalent verified-removal primitive retain the workspace with an unsupported-operation reason.
- A workspace whose change is closed or failed is kept until the change is explicitly deleted.
- Deleting a workspace removes only that workspace. Local workspace registrations survive daemon restart; the daemon retries guarded cleanup of landed and abandoned workspaces on startup and change events. A refused cleanup retains the registration and reports the reason.
- **Accept when:** no sequence of crashes, deletions, offline periods and reconnects loses an edit that was written to a workspace.

### R5 · Automatic merge and conflicts
- The server merges three ways at file level and line level, against the merge base.
- Generated files listed in project configuration are regenerated, never merged. Registration lists keep both sides' additions.
- A region that does not merge automatically becomes a stored conflict on the change, which records base, ours and theirs. A local workspace shows conflict markers once the rebase is applied (R2). The markers are written as `git merge-file --diff3` writes them, in the file's line endings. `rosen resolve` clears a conflict.
- **Accept when:** a clean merge never drops or duplicates a line that either side kept, and every non-clean region becomes a conflict instead of a silent pick.

### R6 · Path claims and edit tracking
- `rosen claim <paths…>` claims paths or globs for a change. The first claim wins. A refused claim names the holder. `share` and `wait` record the other two outcomes.
- Edit regions update from each snapshot. Ignored paths (R2) have no edit regions. The server derives the live path and region view from one captured head and parent; reverting a path to its parent removes its regions. Regions use zero-based, half-open parent line spans, with an empty span for an insertion. The view survives restart through the stored snapshot objects and change metadata. `rosen-client` exposes it through `Client::edit_regions`, including the captured head and parent tree IDs.
- An **overlap query** returns, for a change or a set of paths, every change that overlaps it. The channel (R13) and Velocity call it.
  - A claim or share covers every line of the paths it matches, so it overlaps another change's claim, share or edit there. A wait does not, because a waiter holds off. A set of paths in a query counts as a claim. A query that names a change and paths returns only that change's overlaps on those paths.
  - Edits are measured against the change's parent. Two changes on different parents are compared in one parent's lines, after carrying the other's edits across the lines both versions share. An added, deleted, binary or mode-changed file counts as edited throughout.
- These are AIRC's file claims: `airc_claim` files, `airc_share` and `airc_wait` record path claims, shares and waits here (R13). There is one claim record per path, not one in AIRC and one in Rosen.
- Claims release when their change lands or is abandoned. Abandonment and removal of its claims, shares and waits commit together. Release transactions retain pending notifications until published, so a restart replays a wait's wake even after its record was removed. Delivery is at least once. A wait is then signalled by an event.
- **Accept when:** two concurrent claims on one path never both succeed, and every released claim wakes its waiters.

### R7 · Review
- Review comments anchor to file, line and change head, and form threads with an open or resolved state.
  - A new thread anchors to a line of a file in the change's current head, and the server refuses a path that is not a file there or a line the file lacks. A comment that names the head it was written against is refused once the change has a different head. The anchor never moves; each reply records the head it was written at.
  - Only open and ready changes take comments. Anyone can resolve a thread. A reply to a resolved thread reopens it. Each comment, resolve and reopen publishes a `review` event.
- `review approve <change> --head <head> [--reviewer <agent>]` refuses stale heads and closed changes, stores the reviewer and timestamp, and publishes a `review` event. `review list <change> --approvals` lists durable approval history, including void approvals.
- An approval is tied to the head it approved. A new head by the author voids the approval. A rebase done by the server (R8, D19) keeps it, because it changes only the parent.
- **Review tiers** follow Anthropic's semantic approval rules and Velocity F7:
  - The server tiers each change from its diff: prose-only, normal, or sensitive.
  - Sensitive paths are listed in project configuration and read from the base, so a change cannot lower its own tier.
  - Every tier still needs an approval.
- Velocity's PR duty reviews through `rosen-client`.
- **Accept when:** a change cannot land without an approval for its current head.

### R8 · Optimistic landing
Landing is the core algorithm. No global queue or lock exists, and unrelated changes never wait on each other.

1. A checker gets the server-computed merge result of a change onto base commit B, runs the project's **pre-land check set** (build and lint, D20), and submits a **receipt** for `(head, B, pre-land check set)`. A prose-only change (R7 tier) needs no receipt.
   - Each landing attempt reads the check-set name and computes the review tier from the configuration at that attempt's base, including retries after a lost compare-and-swap. Prose-only changes still need approval and a clean merge.
2. `rosen land <change>` asks the server to land it. In one transaction the server checks:
   - the change has no conflicts, an approval for its current head, and a valid receipt;
   - **base unchanged** (the base still equals the receipt's B): it writes the single landed commit and compare-and-swaps the base ref from B to it;
   - **base moved**: the server merges again onto the new base and applies the **re-check rules**:
     - every change landed since B is **independent** of this change (§2), and the merge is clean → the existing receipt still counts. Each unit this change reaches has the same inputs the receipt checked, and every other unit is as the landed changes' own receipts left it;
     - otherwise, with a clean merge → a new pre-land receipt on the new merge result is required. A clean textual merge can still break the build. For example, a field one change adds can break another change's struct literal in a file the first never touched. Those two changes are not independent, because the second change's unit depends on the first's;
     - build units and workspace-wide inputs are read from the current base, so a change cannot widen its own independence. A change that edits them reaches every unit. A dependency the configuration misses is caught after landing by post-land verification and reverted (R12);
     - conflict → the change gets stored conflict regions with base, its own side and the incoming base side, and a `conflict` event naming the files, and the land is refused. The refusal names the stored files, including on a repeat request. If a snapshot moves the head or closes the change before routing commits, that attempt attaches nothing and the server decides again on the change's new head; a post-land rebase likewise merges that change again.
   - The ref move, landed-history insertion and registry transition to `landed` commit in one SQLite transaction. A missing or moved registry entry refuses the attempt. Snapshots after that commit are refused, including before post-land recovery finishes.
   - If the compare-and-swap loses a race, the server retries the land itself with backoff, as long as no new receipt is needed. A land is never applied twice.
3. A **stack** lands bottom first. A change cannot land before its parent change.
4. After a land:
   - claims are released;
   - the server rebases, without rewriting commits:
     - changes stacked on the landed change, onto the new base;
     - changes that overlap the landed change, with a `rebased` event to each; overlap targets are saved before closing the landed change or releasing its claims, so recovery reuses the same selection, including an empty selection;
     - a rebased change's local workspace is updated when its owner asks (R2);
   - every other change rebases only at its own landing time;
   - the mirror push is scheduled.
- **Order** is set only between changes that overlap each other, or are stacked.
- **Accept when:** a land with a valid receipt commits in under 200 ms; the base only ever moves by a land whose receipt and approval match; a receipt is reused on a moved base only when every change landed since its base is independent of the change; and a crash at any point leaves the base at either the old or the new commit.

### R9 · Events
- The event types are `snapshot`, `landed`, `rebased`, `conflict`, `updated`, `review`, `claim`, `released`, `green`, `reverted` and `mirror`.
- Each event is delivered only to subscribers it concerns: the change's owner, its reviewers, changes stacked on it, and the holders of overlapping changes or claims.
- An event's audience is fixed when it is published. An event about no change, such as `mirror`, goes to every subscriber.
- A subscriber names its agent. A subscription that names no agent is the project view: it gets every event and stores no cursor.
- Delivery is at least once and resumes from a cursor after reconnect. Cursors and the event log are stored on the server, so they survive a server restart.
  - A subscriber acknowledges the events it has handled, and its stored cursor moves only then, never backwards. A subscription with no explicit cursor resumes after the stored one.
- Rosen's events and the channel's events (R13) share one log, one sequence and one cursor per subscriber. A claim release and the land that caused it are always delivered in order, to subscribers on any machine.

### R10 · CLI
- The `rosen` commands are `import`, `change new|list|show|abandon`, `commit`, `status`, `diff`, `log`, `claim|share|wait|release`, `overlaps`, `review comment|approve|resolve|list`, `receipt submit`, `land`, `resolve`, `workspace list|delete|watch`, `server start|status`, `config show`, `events`, `machine join|list|approve|revoke` and `machine key create|list|revoke` (R14).
- `--json` on every command, with versioned output schemas.
- `--help` is complete enough that an agent can learn Rosen from it alone.

### R11 · History queries
- The server answers log, path history, blame, diff, merge base and file read at any commit or change through the API, with no checkout.
- It derives and caches the indexes these need from landed commits.
- **Accept when:** a reviewer can review a change and a checker can fetch its merge result without materialising a workspace.

### R12 · Post-land verification
- The project configures a **post-land check set** (the full tests). External checkers run it, as with receipts (D9).
- **Batches.** The server offers checkers the newest untested landed commit. One passing run marks every commit up to it **green**, and the server records the last green commit.
- **Failure and bisect.** On a failing run, the server offers the commits between the last green and the failure for bisection, until one commit is found to break, the **culprit**.
- **Auto-revert.** The server lands a revert of the culprit through R8, as a server-owned change that needs no receipt or approval. The culprit change goes back to `open` with its owner, carrying the failing check output, and gets a `reverted` event. Its commits and workspace are kept.
- A flaky result is the checker's concern: a revert happens only after the culprit fails twice and its parent passes.
- **Accept when:** a landed commit that fails the full tests is reverted automatically, its change reopens with its work intact, and no other change is reverted.

### R13 · AIRC channel on the server
The server holds each project's AIRC channel, so agents on every machine coordinate through one source of truth. AIRC's behaviour (claims, events, targeted delivery, messages, message modes and ledgers) is as Velocity SPEC F14 defines it. Rosen is its store and its authority.
- **State.** The channel's state lives in the server's metadata store:
  - **piece claims** on work items (`task:<id>`, `issue:<n>`, `spec:<section>`, or the task text);
  - **file claims, shares and waits**, which are Rosen path claims (R6);
  - **events** and agent **messages** with their `to` and `intent`;
  - **presence**: the nicks joined, their machine, and each one's message mode;
  - the **Discoveries** and **Past work** ledgers.
- **Piece claims** are first-claim-wins, each in one transaction, like path claims. A refusal names the holder and the time of its claim. A claim is kept through failure and disconnects, and released as Velocity F14 says.
- **One log.** Channel events and Rosen's events share one ordered log and one cursor per subscriber (R9). Targeted delivery follows Velocity F14: an agent receives only the lines it must act on.
- **Velocity keeps the tools.** Velocity serves AIRC's tools to every agent as now, with the member ID and nick fixed at launch, and calls the server through `rosen-client` for each one. No machine keeps channel state of its own beyond its delivery cursors.
- **Server unreachable.** Claims and messages need the server, as landing does. A tool call made while the server is unreachable fails with that reason. It is never answered from a local copy.
- **Accept when:** two agents on different machines that claim the same piece or path at the same moment never both succeed; an event or message reaches its recipient on another machine within 1 s while both machines reach the server; and no two machines ever see different claim holders.

### R14 · Network access and enrollment
Machines join a project with no login, monitor or remote desktop on the joining machine, so headless machines can be added from a script.
- **Network listener.** The server listens on a TCP port with TLS, alongside the local transport. Its certificate is created when the project is created. Daemons on other machines connect to it, and serve their own machine's clients over the local transport.
- **Join keys.** The project owner creates a join key from any enrolled machine, through Velocity or `rosen machine key create`. A join key has an expiry, a use limit and a role (agent machine or checker), and carries the server's address and the fingerprint of its certificate.
- **Zero-touch join.** A machine joins with one non-interactive command, such as `rosen machine join <key>` in an install or provisioning script, or a machine image.
  1. The daemon connects to the server and checks the server's certificate against the key's fingerprint. On a mismatch it stops before sending the key.
  2. It exchanges the key for the machine's own **machine credential**.
  3. It stores the credential encrypted (Windows: DPAPI through Credential Manager). The client rotates it automatically before it expires.

  The machine never uses the join key again.
- **Approval.** A machine that joins with a valid key is admitted at once. A machine that asks to join without one waits as **pending** until the owner approves it from an enrolled machine.
- **Machine list and revocation.** The server lists every machine with its role, last contact and running agents. Revoking a machine refuses its credential on its next request. Its changes, claims and workspaces are kept, as for a failed change.
- **Discovery.** On a local network, daemons find the server by DNS-SD (mDNS), so a key without a reachable address still works there.
- **Agents.** Every agent on a machine works under that machine's credential. Velocity names each agent (its nick) and its role when it starts the agent's session, as AIRC does now. Owners, reviewers and receipt submitters are recorded as machine plus nick.
- **One enrollment for Rosen and Velocity.** Velocity's cross-system dispatch uses the same join key, so one command adds a machine to both (§9).
- **Accept when:** a headless machine joins from one command with no interactive step; a revoked machine's next request is refused; and a daemon that meets a certificate other than the key's fingerprint never sends the key.

---

## 5. Done means (v1)

- R1–R14 are built and run on Windows 11. The Linux build compiles.
- **The stress run passes on at least three Windows 11 machines**, enrolled with one join key and no interactive step, one of them serving the project:
  - 100 concurrent agents spread across the machines, each creating a change (some stacked), editing, claiming, submitting receipts and landing, over at least 1000 lands;
  - a new workspace is ready in under 2 s (p95) with the warm pool filled;
  - a land with a valid receipt commits in under 200 ms (p95);
  - no edit is lost, no land is applied twice, and the base never moves without a matching receipt and approval (or as a server revert);
  - an injected failing commit is bisected and reverted, and its change reopens with its work intact;
  - no two agents ever hold the same piece or path claim, on any machine;
  - the run is repeated with the server, machine daemons and clients killed at random points, and with the server unreachable from some machines for a period, and still meets those guarantees.
- Velocity can create workspaces, claim pieces and paths, message agents, stack changes, review, submit receipts and land through `rosen-client` alone, with no git worktree, GitHub PR or local AIRC files in the loop.
- The proofs on the proof list check, and each is tested against its fixture table. Merge is also checked against `git merge-file` on a differential corpus.
- `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass, as does `lake build` in `proofs/`.

---

## 6. Out of scope (v1)

- The Rosen-native remote storage backend (designed for behind the backend interface, built later).
- A hosted server, an internet-facing web login (OAuth), and regional cache tiers. The network listener and join-key enrollment are in v1 (R14). The hosted platform's web login will mint the same join keys.
- Clustered servers and the 1000-agent target.
- First-class Linux and macOS support (best-effort only).
- Running checks or builds inside Rosen.
- A virtual filesystem (FUSE or ProjFS) for workspaces.
- Live two-way sync with a git remote. The mirror is push-only.
- Object garbage collection, and redaction or purge of objects.
- Per-directory access control.
- A GUI. Velocity is the UI.
- AIRC's agent tools and its view. These stay in Velocity; the channel's state is on the server (R13).

---

## 7. Proof list

Lean proofs are written only for these features (AGENTS.md "Proofs"):

| Feature | SPEC section | Why it needs a proof |
|---|---|---|
| Optimistic landing and auto-revert | R8, R12 | It decides when the base moves: the compare-and-swap, receipt and approval binding, re-check rules, stack order, and the culprit revert. A wrong rule lands unbuilt code, applies a land twice, reverts the wrong change, or drops another change's work. |
| Path and piece claims | R6, R13 | First-claim-wins with shares and waits is the coordination lock for every agent on every machine. A wrong rule lets two agents both own a path or a piece of work, or leaves a waiter stuck forever. |
| Workspace deletion guard | R4 | Deletion must never remove an edit that is not yet stored on the server, including across the snapshot window and the offline journal. A wrong guard loses an agent's work. |
| Three-way merge | R5 | Every land and rebase merges automatically. A wrong merge silently drops or duplicates lines, which corrupts work without a conflict. |

---

## 8. Open questions

None open. Questions raised since spec lock were decided on 2026-11-12:
- **#26.** A receipt stays valid on a moved base when every change landed since is independent of it (§2, R8 step 2). Batch receipts are not adopted.
- **#31.** Rosen prepares pool workspaces, and Velocity keeps the per-agent steps after a take (R3, §9).

---

## 9. Velocity changes Rosen needs

Rosen depends on these changes to Velocity. They are built in Velocity's repository, not here.

- **Worktrees.** Threads get workspaces from `rosen-client`. The worktree warm pool (F5), its prebuild worker and `git worktree` calls go. Rosen prepares pool workspaces (R3). The project's prepare commands take over Velocity's toolchain, Lean cache and prebuild steps. Velocity keeps the per-agent steps after a take: copying untracked files, linking skills and its after-take setup command. A swarm launch raises the pool size on the machines it uses.
- **PR watcher.** GitHub polling every 15 s (F6) is replaced by the Rosen event stream.
- **PR duty.** Duty agents review, approve, submit receipts and land through `rosen-client`. Branch updates (F6) become server-side rebases.
- **AIRC.** Velocity's AIRC tools and view call the Rosen server (R13) instead of local files. File claims, shares, waits and overlap notices become Rosen path claims (R6). Task claims, events, messages, presence and ledgers become channel state on the server.
- **Rebases.** Velocity asks `rosen-client` to apply a pending server rebase at the agent's step boundary, or while it is parked, in place of `airc_rebase` (R2).
- **Enrollment and dispatch.** Velocity creates join keys and lists, approves and revokes machines through R14. Its cross-system dispatch uses the same join key, so one command adds a headless machine to both Velocity and Rosen.
- **Dependency waits and parking.** Most waits become stacked changes. Parking remains only for path-claim waits.
- **Build queue.** Velocity keeps its build queue. It produces pre-land receipts with priority, and runs post-land full tests and bisection (R12) at the next priority.

## Proof list

These features are proven in Lean after their code (F17). Every other feature ships with no Lean.

- Optimistic landing and auto-revert (R8, R12): decides when the base moves; a wrong compare-and-swap, receipt binding, re-check rule, stack order or culprit revert lands unbuilt code, applies a land twice, reverts the wrong change or drops another change's work
- Path and piece claims (R6, R13): first-claim-wins with shares and waits is the coordination lock for every agent on every machine; a wrong rule lets two agents own one path or piece, or strands a waiter
- Workspace deletion guard (R4): must never delete an edit not yet stored on the server, across the snapshot window and the offline journal; a wrong guard loses an agent's work
- Three-way merge (R5): every land and rebase merges automatically; a wrong merge silently drops or duplicates lines
