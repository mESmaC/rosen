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
| D3 | Landing model | **Optimistic landing, with an exclusive retry turn after repeated contention.** A land is a compare-and-swap of the base ref. Normal attempts run concurrently; a request that exhausts its optimistic budget waits its turn rather than returning `Contended` because other lands won (#393). |
| D4 | Stack | Rust, in one Cargo workspace. |
| D5 | Storage | Objects are stored in **git format** (blobs, trees, commits), as loose content-addressed objects with no packfiles, so no repack or maintenance is needed. Rosen imports git repos and pushes landed history to a git remote. Storage sits behind a pluggable backend interface: the git-backed backend comes first, and a Rosen-native remote store comes later. |
| D6 | Workspaces | Two modes. **Local workspaces** are files on disk for agents' tools. **Server-side workspaces** have no checkout, and are read and written through the API. Both keep resource use bounded. Local workspaces keep working when the server is unreachable (offline fallback, R2). |
| D7 | Velocity integration | A `rosen-client` library crate that Velocity links directly. The `rosen` CLI is a thin layer over the same crate. |
| D8 | Server deployment *(revised)* | **One authoritative server per project**, reachable from every machine that runs agents for it (R14). Every machine runs a **daemon**, which `rosen-client` or Velocity can start. On the server's machine the daemon is the server. On every other machine it is the tier-1 object cache in front of the server, answering the same RPCs, so workspaces materialise from a local store. A hosted server can later take the authoritative role behind the same RPCs. |
| D9 | Check runner | Checks run **outside** Rosen: Velocity's build queue, PR duty, or any other client. Before-land checks submit a **receipt**; post-land checks submit typed results. Rosen validates them and runs no builds. |
| D10 | Coordination *(revised)* | **Rosen owns all coordination state**: path state (each change's live edited paths and line regions, and path claims) and the project's AIRC channel (task claims, events, messages and ledgers, R13). Velocity keeps AIRC's agent tools and view, and calls Rosen for every one, so every machine sees the same channel. |
| D11 | Platforms | v1 is tested and shipped on Windows 11. The code stays portable, and Linux and macOS are best-effort until a later milestone. |
| D12 | Review | **Rosen-native review**: comments, approvals and receipts are Rosen objects. GitHub receives landed history as a mirror and is not in the landing path. |
| D13 | Metadata store | Embedded SQLite in WAL mode, one database per server. |
| D14 | Scale target *(revised)* | v1: at least 100 concurrent changes on one project, from agents on several machines, typically 30–40 agents per machine (see §5). |
| D15 | Landed history | **One commit per change**: the change's final tree, with trailers for the change ID, receipt and approval. The agent's own commits stay in Rosen and are never rewritten. The commit's only parent is the base it landed on, or the preceding layer's landed commit in an atomic stack land, its author is the change's owner as `<nick>@<machine>` and its message is the change's newest `rosen commit` message. Its last paragraph holds git trailers: `Rosen-Change: <change ID>`; `Rosen-Receipt: <check set> <head> <base> <nick>@<machine>`, the receipt the land counted with its submitter as nick plus machine (R14), shared by every layer of an atomic stack land, omitted when none was needed; and one `Rosen-Approved-By: <nick>@<machine>` per approval that holds. `rosen log` shows them, including the older 3-field receipt form `<check set> <base> <nick>@<machine>` that lands wrote before the head was recorded. |
| D16 | Workspace lifecycle | **Rosen owns it**: creation, a warm pool, and deletion behind a guard. |
| D17 | Write-through snapshots *(revised)* | Every workspace write is snapshotted to the server automatically and within seconds, like jj's working-copy commit. There is no push or sync step. An edit that exists only on the client is the exception (offline, or inside the snapshot window), not the norm. This makes the deletion guard simple, gives the server live edit regions, and lets the server do every rebase. |
| D18 | Changes are not refs *(revised)* | The base (and later, release refs) is the only ref. A change is a row in the metadata store, with no branch name to create, delete or collide. |
| D19 | Stacked changes *(revised)* | A change can sit on top of another unlanded change instead of waiting for it to land. A request to land an upper change includes its unlanded ancestors atomically, bottom first (R8 step 3). When a lower change lands separately or updates, the server rebases the upper one automatically. This replaces Velocity's dependency waits and parking for most cases. |
| D20 | Landing gate | **Project review mode.** `review = "after-land"` needs only a clean merge and compare-and-swap; neither approval nor receipt gates landing. Build/lint and full tests run after landing. Both failure kinds are bisected and recorded; only broken builds revert in this mode. The default `before-land` retains approval, pre-land build/lint receipts, and full-test auto-revert (R7, R8, R12; owner decision #365). Receipt reuse follows `recheck` (R8; #366). |
| D21 | Network access and enrollment *(new)* | The server listens on a network listener with TLS as well as the local transport. Machines join with **join keys**, with no login, monitor or remote desktop on the joining machine. Each machine gets its own revocable credential. Every agent on a machine works under that machine's credential, and Velocity names the agent and its role (R14). One enrollment adds a machine to both Rosen and Velocity. |

Defaults set during spec writing. These follow from the decisions above, and changing one is a product decision:
- **Transport.** Each daemon listens on a local transport for its own machine's clients: a named pipe on Windows and a Unix socket elsewhere, limited to the OS user. A client refuses an endpoint served by another OS user, since another user can create the endpoint first. The server also listens on a TCP port with TLS for enrolled machines' daemons (R14). The protocol is versioned and does not assume a local peer.
  - On Windows, the listener keeps eight waiting pipe instances and refills each before handing off its connection. Clients retry busy instances with asynchronous exponential backoff from 10 ms to 250 ms for up to 30 seconds; connection failures name the endpoint. Fifty clients connecting at once must all succeed (#392).
  - On Windows, an access-denied server-user check refuses the connection and names elevation or another user as possible causes; it suggests stopping one's own daemon and restarting it from a non-elevated shell.
- **Events.** The server pushes events to subscribed clients. Clients never poll.
- **Garbage collection.** v1 never deletes objects from the store.
- **Stress verification.** The §5 harness matches landed receipt and approval trailers to the run's checked trees and reviewer identities, preserving valid server-rebase and independent-unit receipt reuse (#173).

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
- **Stack.** A chain of changes, each a parent of the next. One land request includes the chosen change and its unlanded ancestors, with one commit per change, chained bottom first, and one compare-and-swap.
- **Workspace.** The files a change is edited in. Each workspace belongs to exactly one change, and is either **local** or **server-side**.
  Guarded deletion removes classified junctions and symlinks themselves without traversing their targets; unknown reparse kinds keep the workspace (R4; #388).
- **Conflict.** A stored object recording the base, ours and theirs for a region that did not merge automatically. A change that has conflicts cannot land.
- **Receipt.** A record that the required checks passed on one exact merge result. It records a signed `(change head, base commit, check set)` binding and the server-computed tree it checked. A trusted receipt for the current check set counts whenever that tree equals the current merge result, even after its head or base changes.
- **Approval.** A reviewer's approval of a change at a given head. It gates landing only in `before-land` projects.
- **Review coverage.** Each landed commit starts `unreviewed`; agents mark it `reviewed` or `issues-filed` (wire value `issues_filed`) without gating development. A repaired land starts fresh.
- **Post-land failure.** A durable record of a confirmed culprit, its change, check kind (`build` or `test`), failure output, and any inverse commit (R12).
  A refused inverse publishes `revert_refused` with the culprit, overlapping later commits and reason, and retries as the base moves.
  Later build searches exclude recorded culprits and treat an exact match of the outstanding culprit's output as a known failure, without marking it built or green (#390).
- **Path claim.** A change's declared intent to edit a path or glob: a notice by default, reporting concurrent work. An opt-in **exclusive claim** uses first-claim-wins. Other changes may record a **share** (they edit it too, knowingly) or a **wait** (they hold off until claims are released).
- **Edit regions.** The paths and line ranges a change has modified relative to its parent. The server computes them from snapshots.
- **Overlap.** Two changes overlap when they claim the same path, or their edit regions in one file touch or sit within 3 lines of each other. Per-project configuration lists **generated files** and **registration lists**, which never count as an overlap.
- **Build unit.** A part of the project that builds on its own, together with the units it depends on. By default each package of a Cargo workspace is a unit, including implicit in-tree path-dependency members, read from the workspace's manifests. Project configuration can list units instead, as path globs with their dependencies.
- **Workspace-wide inputs.** Paths every unit's build reads:
  - toolchain files and build configuration (such as `.cargo/`);
  - project configuration keys `checks`, `build_units`, `workspace_inputs`, `generated` and `registration_lists`, compared after parsing the base and merge result;
  - any other path that lies in no unit and is not prose.
  - Mirror CI workflows (`.github/workflows/`) reach no unit unless a unit includes them or `workspace_inputs` lists them. Projects whose checks read CI configuration list it there.
- **Reach.** The units a change can affect: the units holding the paths it changed, plus every unit that depends on one of them, directly or through others.
  - A prose-only change (R7) reaches no unit.
  - A change to a workspace-wide input reaches every unit.
  - A `rosen.toml` edit only to `ignore`, `prepare`, `warm_pool`, `mirror`, `sensitive` or `review` reaches no unit. An unparseable configuration reaches every unit.
  - Cargo inputs are compared in both trees: a changed lockfile package entry reaches its direct and transitive workspace consumers; a changed root `[workspace.dependencies]` entry reaches its users; a member manifest reaches its own unit and its dependents. Other shared root settings, unsupported inputs and inputs the server cannot parse conservatively reach every unit. Root package settings reach that package and its dependents.
  - A Cargo package input maps to every build unit whose paths hold its manifest, then their dependents. Configured unit names do not establish ownership. Both unit discovery and Cargo input comparison include implicit members and their dependency edges; unresolved workspace boundaries conservatively require a new receipt.
- **Independent.** Two changes are independent when their reaches share no unit. Then no unit's build sees both changes.
- **Project configuration.** The file `rosen.toml` at the repository root. The server reads it from the base, never from a change, so a change takes effect only once it lands.
  - Top-level `review` selects `before-land` (default) or `after-land` (R7, R8, R12).
  - Top-level `recheck` selects `reach` (default) or `merge`. In before-land mode, `merge` reuses a valid current-head receipt across a clean merge onto a moved base, provided its unlanded stack inputs still match; `reach` retains build-unit independence. Exact checked-tree reuse remains available in both modes (R8; #366).
  - It lists generated files, registration lists and sensitive paths as path globs, the pre-land and post-land check sets (a name and the commands checkers run), the warm pool size, the mirror remote and the landed file limit `max_file_size` (default 104857600 bytes). Each part is optional and has a default: no paths, check sets named `pre-land` and `post-land` with no commands, a pool of 4 and no remote.
  - `ignore` lists untracked paths snapshots skip (R2), in `.gitignore` syntax from the repository root, such as `target/`. It ranks below every `.gitignore` file. A workspace records the base's list when it is made or a rebase is applied, so a watcher started offline skips the same paths and a change cannot alter what its own snapshots skip.
  - `[prepare] commands` lists the prepare commands daemons run in ready workspaces (R3). The configured `warm_pool.size` is each daemon's target, read again after every land, unless a pool-size request has raised or lowered it since the daemon started.
  - Each ignore line and prepare command is one non-empty line with no control characters.
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
| `rosen-client` | The public library API Velocity links: daemon discovery and start, joining a project, workspace materialisation, the file watcher and snapshotting, applying server rebases, and the offline journal. Its session runs every step of an agent's flow under the agent's identity, and its channel membership runs every AIRC tool under the member ID and nick Velocity gave it (§5). |
| `rosen-cli` | The `rosen` binary, a thin layer over `rosen-client`. |

- **Generated merge artifacts.** R5/R8 preserve regeneration paths through landing; a land that needs regeneration is refused with the owning change and paths, in either review mode (#389).
  Merge-result trees awaiting regeneration cannot bind receipts; the owner rebases, regenerates and snapshots the repaired artifacts before retrying.
- **Text merge line endings.** R5 compares LF and CRLF lines equally and writes merged text in the base file's newline convention, so an editor rewrite does not displace concurrent additions (#391).
- **Client–server asymmetry.** The server does every merge, rebase, landed commit and history query. The client materialises files, watches them, and uploads snapshots. Clients never run merge logic.
- **Topology.** One authoritative server per project. On every other enrolled machine, the daemon serves its clients over the local transport, answers object reads from its own cache, and forwards every other request to the server. Refs, claims, the channel and every other piece of mutable state are read and written only on the server, so no machine can fall behind or move ahead of it.
- **Metadata store.** SQLite in WAL mode, on the server only, holds changes, stacks, claims, regions, receipts, reviews, conflicts, workspace records, the channel, enrolled machines and the base ref. Every ref move and every claim is one transaction, and a crash in the middle leaves the store either before or after it, never between.
- **Metadata initialization.** Each server metadata connection enables WAL, full synchronous durability, a bounded busy timeout and foreign keys. The core change, snapshot, claim, conflict, review and landing tables are installed together in one transaction, recorded by SQLite `user_version`. Existing unversioned rows are preserved; a database with a newer schema version is refused. Module-owned derived indexes and event tables are initialized by their services.
- **Object store.** Git object format, stored loose and content-addressed. The server's store is authoritative. Each other machine's daemon keeps a store of the objects its workspaces use, filled from the server on demand and shared by every workspace on that machine. For a sparse workspace the daemon fetches every tree but only the blobs under its chosen paths, and records the trees it stored without all their blobs, so a later fill for a wider selection fetches the rest. Objects are immutable, so any cache tier can hold them without coordination.
  - Cache fills verify the selected descendants even below a previously complete tree. Cache object-read RPCs, including chunk reads, refetch and repair a corrupt stored copy from the server; an unreachable upstream or corrupt authoritative copy remains an error (#363). Chunk reads use a checked local copy when available, with one streamed inflate for a large object rather than one per slice; an absent object's slices are forwarded without storing unverified partial bytes. Checked slice copies are cleared on cache startup because a crash can leave them torn.
- **Protocol.** Versioned, framed request/response plus one event stream per client. The same protocol runs over the local transport and over the network listener.

---
## 4. Features
| Feature <!-- split-spec: index --> | Feature | Feature |
|---|---|---|
| <a id="r1--repository-import-and-git-mirror"></a>[R1 · Repository import and git mirror](spec/R1-repository-import-and-git-mirror.md) | <a id="r2--changes-snapshots-and-offline-work"></a>[R2 · Changes, snapshots and offline work](spec/R2-changes-snapshots-and-offline-work/README.md) | <a id="r3--workspaces"></a>[R3 · Workspaces](spec/R3-workspaces/README.md) |
| <a id="r4--workspace-lifecycle-and-deletion"></a>[R4 · Workspace lifecycle and deletion](spec/R4-workspace-lifecycle-and-deletion.md) | <a id="r5--automatic-merge-and-conflicts"></a>[R5 · Automatic merge and conflicts](spec/R5-automatic-merge-and-conflicts.md) | <a id="r6--path-claims-and-edit-tracking"></a>[R6 · Path claims and edit tracking](spec/R6-path-claims-and-edit-tracking.md) |
| <a id="r7--review"></a>[R7 · Review](spec/R7-review.md) | <a id="r8--optimistic-landing"></a>[R8 · Optimistic landing](spec/R8-optimistic-landing/README.md) | <a id="r9--events"></a>[R9 · Events](spec/R9-events.md) |
| <a id="r10--cli"></a>[R10 · CLI](spec/R10-cli.md) | <a id="r11--history-queries"></a>[R11 · History queries](spec/R11-history-queries.md) | <a id="r12--post-land-verification"></a>[R12 · Post-land verification](spec/R12-post-land-verification.md) |
| <a id="r13--airc-channel-on-the-server"></a>[R13 · AIRC channel on the server](spec/R13-airc-channel-on-the-server.md) | <a id="r14--network-access-and-enrollment"></a>[R14 · Network access and enrollment](spec/R14-network-access-and-enrollment.md) |  |

<a id="5-done-means-v1"></a>[5. Done means (v1)](spec/5-done-means.md) · <a id="6-out-of-scope-v1"></a>[6. Out of scope (v1)](spec/6-out-of-scope.md) · <a id="8-open-questions"></a>[8. Open questions](spec/8-open-questions.md) · <a id="9-velocity-changes-rosen-needs"></a>[9. Velocity changes Rosen needs](spec/9-velocity-changes.md) · <a id="proof-list"></a>[Proof list](spec/proof-list.md)
## 7. Proof list
Lean proofs are written only for these features (AGENTS.md "Proofs"):

| Feature | SPEC section | Why it needs a proof |
|---|---|---|
| Optimistic landing and auto-revert | R8, R12 | It decides when the base moves: the compare-and-swap, receipt and approval binding, reach/merge re-check rules, stack order, and the culprit revert. A wrong rule bypasses configured gates, applies a land twice, reverts a test-only failure after land or the wrong change, or drops another change's work. |
| Path and piece claims | R6, R13 | Piece claims and opt-in exclusive path claims use first-claim-wins; default path notices never refuse. A wrong rule serializes authors unnecessarily, grants two exclusive holders, or leaves a waiter stuck forever. |
| Workspace deletion guard | R4 | Deletion must never remove an edit that is not yet stored on the server, including across the snapshot window and the offline journal. A wrong guard loses an agent's work. |
| Three-way merge | R5 | Every land and rebase merges automatically. A wrong merge silently drops or duplicates lines, which corrupts work without a conflict. |

---
