# SPEC.md · Rosen

Rosen is a version control system built so that many coding agents can work on one project at the same time: 100 agents in v1 and up to 1000 later. Rosen is two programs: a **server** that holds the project and does the expensive work, and a thin **client** that agents and humans use. Rosen is the source-control layer under [Velocity](https://github.com/mESmaC/velocity), the parallel agentic IDE. Where a cleaner or faster Rosen design needs Velocity to change, Velocity changes (§9).

- **Status:** draft, 2026-11-11. Written from the spec interview, then revised after reviewing the prior art below.
- **Optimizing for:** development velocity. Landing must be fast and must never lose work.

## 0. Prior art and what Rosen takes from it

| Source | Idea | Rosen |
|---|---|---|
| Anthropic, [Redesigning source control](https://www.anthropic.com/research/redesigning-source-control) (2,500 → 30,000 merges/day) | No global merge lock: a ref moves by compare-and-swap on one row | R8 landing |
| Anthropic | Server-side commits, rebases and merges; "nothing to push"; the client "can compute very little on its own" | D17, R2, R8 |
| Anthropic | Refs discouraged except main and release branches, because global state is a scaling problem | Changes are not refs (D18) |
| Anthropic | Changesets stored as deltas against the first parent, with derived data (blame, path history, ancestry) built from them | R11 history API |
| Anthropic | Tiered content-addressed caches: a per-node daemon, then regional, then authoritative, all answering the same RPCs | D8: the daemon is tier 1; the hosted server is added later behind the same RPCs |
| Anthropic | Source control as a service, so many jobs need no checkout | Server-side workspaces, R11 |
| Anthropic | Post-merge and pre-deploy testing replace pre-merge gating at agent scale | D20, R12 |
| Anthropic | Review intent, with semantic approval rules (comment-only vs logic changes) | R7 review tiers |
| [CarterT27/scion](https://github.com/CarterT27/scion) (clean-room Rust reimplementation) | Landing = rebase a linear stack onto the ref, then versioned compare-and-swap, with retry and backoff | R8 |
| CarterT27/scion | `--json` with versioned output schemas; a per-node daemon on a local socket | R10, D8 |
| CarterT27/scion | Verification: an independent reference model, git differential tests, fault injection | §5; fits the proof fixture tables |
| CarterT27/scion | Measured result: git still checks out large full trees faster than a fetch-and-write client | R3: workspaces clone from a local store, not over the network |
| Google [Scion](https://github.com/googlecloudplatform/scion) (agent orchestration) | Agents have their own identity; notifications when work finishes or needs attention; "less is more", with agents learning the CLI from help text | Change owner identity (§2), targeted events (R9), self-describing CLI (R10) |
| Velocity SPEC §3.3, F5–F7, F14 | Receipts bound to an exact head and base, re-checks when the base moves, targeted rebases, 3-line overlap rule, warm pool, dependency waits and parking | R4, R6, R8; dependency waits become stacked changes (D19) |

**Not adopted:** Spanner, CockroachDB or S3 (v1 is a local daemon); a virtual filesystem; and Anthropic's "always online, no offline" rule. Rosen keeps an offline fallback (D6).

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
| D8 | Server deployment | In v1 the server is a local daemon, one per machine, which `rosen-client` or Velocity can start. It holds the authoritative store and is also the machine's shared object cache. Its protocol works over the network, so it can later become the per-node tier in front of a hosted server answering the same RPCs. |
| D9 | Check runner | Checks run **outside** Rosen: Velocity's build queue, PR duty, or any other client. They submit a **receipt**. Rosen stores and validates receipts and runs no builds. |
| D10 | Path claims | **Rosen owns path state**: each change's live edited paths and line regions, and path claims. AIRC keeps task claims and messaging, and asks Rosen about overlaps. |
| D11 | Platforms | v1 is tested and shipped on Windows 11. The code stays portable, and Linux and macOS are best-effort until a later milestone. |
| D12 | Review | **Rosen-native review**: comments, approvals and receipts are Rosen objects. GitHub receives landed history as a mirror and is not in the landing path. |
| D13 | Metadata store | Embedded SQLite in WAL mode, one database per server. |
| D14 | Scale target | v1: 100 concurrent changes on one machine (see §5). |
| D15 | Landed history | **One commit per change**: the change's final tree, with trailers for the change ID, receipt and approval. The agent's own commits stay in Rosen and are never rewritten. |
| D16 | Workspace lifecycle | **Rosen owns it**: creation, a warm pool, and deletion behind a guard. |
| D17 | Write-through snapshots *(revised)* | Every workspace write is snapshotted to the server automatically and within seconds, like jj's working-copy commit. There is no push or sync step. An edit that exists only on the client is the exception (offline, or inside the snapshot window), not the norm. This makes the deletion guard simple, gives the server live edit regions, and lets the server do every rebase. |
| D18 | Changes are not refs *(revised)* | The base (and later, release refs) is the only ref. A change is a row in the metadata store, with no branch name to create, delete or collide. |
| D19 | Stacked changes *(revised)* | A change can sit on top of another unlanded change instead of waiting for it to land. When the lower change lands or updates, the server rebases the upper one automatically. This replaces Velocity's dependency waits and parking for most cases. |
| D20 | Landing gate | **Fast pre-land, full post-land.** A land needs a receipt for build and lint on the exact merge result. Full tests run on landed commits in batches. A failure is bisected to the culprit, and the server lands a revert automatically and reopens the culprit change for its owner (Anthropic's post-merge testing, R12). |

Defaults set during spec writing. These follow from the decisions above, and changing one is a product decision:
- **Transport.** v1 listens only on a local transport: a named pipe on Windows and a Unix socket elsewhere. Access is limited to the OS user. The network listener and its authentication are out of scope for v1, but the protocol is versioned and does not assume a local peer.
- **Events.** The server pushes events to subscribed clients. Clients never poll.
- **Garbage collection.** v1 never deletes objects from the store.

---

## 2. Concepts

- **Project.** One repository held by one server. It has one **base** ref (for example `main`), which only a land can move.
- **Change.** One agent's unit of work. Each change has:
  - a stable **change ID**;
  - an **owner** (agent identity);
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

---

## 3. Architecture

| Crate | Holds |
|---|---|
| `rosen-core` | Data model: changes, stacks, conflicts, receipts, reviews, claims, regions. Pure logic, with no I/O. |
| `rosen-store` | The storage backend interface and the git-format loose-object backend. |
| `rosen-merge` | Three-way file merge, conflict objects and overlap detection. |
| `rosen-server` | The daemon: protocol, the SQLite metadata store, snapshots, server-side rebase, landing, claims, review, events, history queries, server-side workspaces and the git mirror push. |
| `rosen-client` | The public library API Velocity links: daemon discovery and start, workspace materialisation, the file watcher and snapshotting, and the offline journal. |
| `rosen-cli` | The `rosen` binary, a thin layer over `rosen-client`. |

- **Client–server asymmetry.** The server does every merge, rebase, landed commit and history query. The client materialises files, watches them, and uploads snapshots. Clients never run merge logic.
- **Metadata store.** SQLite in WAL mode holds changes, stacks, claims, regions, receipts, reviews, conflicts, workspace records and the base ref. Every ref move and every claim is one transaction, and a crash in the middle leaves the store either before or after it, never between.
- **Object store.** Git object format, stored loose and content-addressed, on the server's volume. Because the v1 server is local, its store is the machine's shared object cache. Objects are immutable, so any later cache tier can hold them without coordination.
- **Protocol.** Versioned, framed request/response plus one event stream per client. The same protocol runs over the local transport in v1 and over the network later.

---

## 4. Features

### R1 · Repository import and git mirror
- `rosen import <git repo>` loads a git repository's history and makes its default branch the base.
- After each land, the server pushes the base to the configured git remote (for example GitHub). The push runs after the land commits and off its critical path. A failed push is retried and reported as an event, and never blocks or undoes a land.
- A push to the mirror is a fast-forward and never a force-push.
- **Accept when:** a repo imported and then landed on shows the same history in `git log` on the mirror, with one commit per change and its trailers.

### R2 · Changes, snapshots and offline work
- `rosen change new [--on <change>]` creates a change on the current base, or stacked on another change, together with its workspace. The change ID stays the same for its whole life.
- **Write-through snapshots.** The client watches the workspace (`ReadDirectoryChangesW` on Windows) and uploads a snapshot within 2 s of the last write, plus on `rosen commit`. A snapshot sends only the changed blobs.
- **Offline journal.** When the server is unreachable, snapshots go to a local append-only journal and are uploaded in order on reconnect. Landing requires the server.
- Snapshots and commits belong to the change and are never rewritten.
- **Accept when:** a change's commits survive any number of rebases unchanged, and a write is on the server within 2 s while the server is reachable.

### R3 · Workspaces
- **Local workspaces** are materialised from the local store:
  - **ReFS block clone** (Windows Dev Drive, or any ReFS volume) is the fast path, and true copy-on-write. Rosen recommends putting workspaces on a Dev Drive.
  - Otherwise a **parallel plain copy**.
  - **Hardlinks are never used** for files in a workspace. Agents' tools often write files in place, so a hardlink would let one workspace's write corrupt the shared object and every other workspace that links to it.
  - Sparse checkout lets a workspace hold only chosen paths.
- **Server-side workspaces.** Files live on the server, and clients read, write, list and diff them through the API, with no checkout. Writes become snapshots directly. Rosen recommends them for jobs that touch few files, such as reviewers, check runners and doc edits.
- **Warm pool.** Rosen keeps N ready local workspaces at the tip of the base (default 4, 0 turns the pool off). After each land it brings them up to the new tip by rewriting only the changed files. Taking one is atomic, so two changes never get the same workspace.
- **Load limits.** The server caps concurrent materialisations and server-side workspace I/O, and queues the rest. The client caps local disk use. A request over a cap waits; it never fails or degrades another workspace.
- **Accept when:** with the warm pool filled, a new workspace is ready in under 2 s, and two changes never share a workspace.

### R4 · Workspace lifecycle and deletion
- Rosen is the only thing that creates or deletes workspaces. Velocity calls `rosen-client` instead of `git worktree`.
- A workspace is deleted automatically when its change lands or is abandoned.
- **Deletion guard.** Before deleting, Rosen flushes a final snapshot. It refuses deletion while any of these is not stored on the server:
  - a write the watcher has seen but not yet snapshotted;
  - a journal entry that has not been uploaded;
  - a file whose content differs from the last stored snapshot, found by a final hash scan.

  The refusal names the files and keeps the workspace.
- A workspace whose change is closed or failed is kept until the change is explicitly deleted.
- Deleting a workspace removes only that workspace.
- **Accept when:** no sequence of crashes, deletions, offline periods and reconnects loses an edit that was written to a workspace.

### R5 · Automatic merge and conflicts
- The server merges three ways at file level and line level, against the merge base.
- Generated files listed in project configuration are regenerated, never merged. Registration lists keep both sides' additions.
- A region that does not merge automatically becomes a stored conflict on the change, which records base, ours and theirs. The workspace shows conflict markers. `rosen resolve` clears a conflict.
- **Accept when:** a clean merge never drops or duplicates a line that either side kept, and every non-clean region becomes a conflict instead of a silent pick.

### R6 · Path claims and edit tracking
- `rosen claim <paths…>` claims paths or globs for a change. The first claim wins. A refused claim names the holder. `share` and `wait` record the other two outcomes.
- Edit regions update from each snapshot.
- An **overlap query** returns, for a change or a set of paths, every change that overlaps it. AIRC and Velocity call it.
- Claims release when their change lands or is abandoned. A wait is then signalled by an event.
- **Accept when:** two concurrent claims on one path never both succeed, and every released claim wakes its waiters.

### R7 · Review
- Review comments anchor to file, line and change head, and form threads with an open or resolved state.
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
2. `rosen land <change>` asks the server to land it. In one transaction the server checks:
   - the change has no conflicts, an approval for its current head, and a valid receipt;
   - **base unchanged** (the base still equals the receipt's B): it writes the single landed commit and compare-and-swaps the base ref from B to it;
   - **base moved**: the server merges again onto the new base and applies the **re-check rules**:
     - every change landed since B is prose-only, and the merge is clean → the existing receipt still counts;
     - otherwise, with a clean merge → a new pre-land receipt on the new merge result is required. A clean textual merge can still break the build, for example when a field one change adds breaks another change's struct literal in a file the first never touched;
     - conflict → the change gets a stored conflict and a `conflict` event naming the files, and the land is refused.
   - If the compare-and-swap loses a race, the server retries the land itself with backoff, as long as no new receipt is needed. A land is never applied twice.
3. A **stack** lands bottom first. A change cannot land before its parent change.
4. After a land:
   - claims are released;
   - the server rebases, without rewriting commits:
     - changes stacked on the landed change, onto the new base;
     - changes that overlap the landed change, with a `rebased` event to each;
   - every other change rebases only at its own landing time;
   - the mirror push is scheduled.
- **Order** is set only between changes that overlap each other, or are stacked.
- **Accept when:** a land with a valid receipt commits in under 200 ms; the base only ever moves by a land whose receipt and approval match; and a crash at any point leaves the base at either the old or the new commit.

### R9 · Events
- The event types are `snapshot`, `landed`, `rebased`, `conflict`, `updated`, `review`, `claim`, `released`, `green`, `reverted` and `mirror`.
- Each event is delivered only to subscribers it concerns: the change's owner, its reviewers, changes stacked on it, and the holders of overlapping changes or claims.
- Delivery is at least once and resumes from a cursor after reconnect.

### R10 · CLI
- The `rosen` commands are `import`, `change new|list|show|abandon`, `commit`, `status`, `diff`, `log`, `claim|share|wait|release`, `overlaps`, `review comment|approve|resolve`, `receipt submit`, `land`, `resolve`, `workspace list|delete`, `server start|status` and `events`.
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

---

## 5. Done means (v1)

- R1–R12 are built and run on Windows 11. The Linux build compiles.
- **The stress run passes on one Windows 11 machine:**
  - 100 concurrent agents, each creating a change (some stacked), editing, claiming, submitting receipts and landing, over at least 1000 lands;
  - a new workspace is ready in under 2 s (p95) with the warm pool filled;
  - a land with a valid receipt commits in under 200 ms (p95);
  - no edit is lost, no land is applied twice, and the base never moves without a matching receipt and approval (or as a server revert);
  - an injected failing commit is bisected and reverted, and its change reopens with its work intact;
  - the run is repeated with the server and clients killed at random points and with the server unreachable for a period, and still meets those three guarantees.
- Velocity can create workspaces, claim paths, stack changes, review, submit receipts and land through `rosen-client` alone, with no git worktree or GitHub PR in the loop.
- The proofs on the proof list check, and each is tested against its fixture table. Merge is also checked against `git merge-file` on a differential corpus.
- `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass, as does `lake build` in `proofs/`.

---

## 6. Out of scope (v1)

- The Rosen-native remote storage backend (designed for behind the backend interface, built later).
- A network listener, authentication, a hosted server, and regional cache tiers.
- Clustered servers and the 1000-agent target.
- First-class Linux and macOS support (best-effort only).
- Running checks or builds inside Rosen.
- A virtual filesystem (FUSE or ProjFS) for workspaces.
- Live two-way sync with a git remote. The mirror is push-only.
- Object garbage collection, and redaction or purge of objects.
- Per-directory access control.
- A GUI. Velocity is the UI.
- Task claims and agent messaging (these stay in AIRC).

---

## 7. Proof list

Lean proofs are written only for these features (AGENTS.md "Proofs"):

| Feature | SPEC section | Why it needs a proof |
|---|---|---|
| Optimistic landing and auto-revert | R8, R12 | It decides when the base moves: the compare-and-swap, receipt and approval binding, re-check rules, stack order, and the culprit revert. A wrong rule lands unbuilt code, applies a land twice, reverts the wrong change, or drops another change's work. |
| Path claims | R6 | First-claim-wins with shares and waits is the coordination lock for every agent. A wrong rule lets two agents both own a path, or leaves a waiter stuck forever. |
| Workspace deletion guard | R4 | Deletion must never remove an edit that is not yet stored on the server, including across the snapshot window and the offline journal. A wrong guard loses an agent's work. |
| Three-way merge | R5 | Every land and rebase merges automatically. A wrong merge silently drops or duplicates lines, which corrupts work without a conflict. |

---

## 8. Open questions

None at spec lock.

---

## 9. Velocity changes Rosen needs

Rosen depends on these changes to Velocity. They are built in Velocity's repository, not here.

- **Worktrees.** Threads get workspaces from `rosen-client`. The worktree warm pool (F5) and `git worktree` calls go.
- **PR watcher.** GitHub polling every 15 s (F6) is replaced by the Rosen event stream.
- **PR duty.** Duty agents review, approve, submit receipts and land through `rosen-client`. Branch updates (F6) become server-side rebases.
- **AIRC file claims.** File claims, shares, waits and overlap notices move to Rosen path claims (F14). Task claims and messaging stay in AIRC.
- **Dependency waits and parking.** Most waits become stacked changes. Parking remains only for path-claim waits.
- **Build queue.** Velocity keeps its build queue. It produces pre-land receipts with priority, and runs post-land full tests and bisection (R12) at the next priority.

## Proof list

These features are proven in Lean after their code (F17). Every other feature ships with no Lean.

- Optimistic landing and auto-revert (R8, R12): decides when the base moves; a wrong compare-and-swap, receipt binding, re-check rule, stack order or culprit revert lands unbuilt code, applies a land twice, reverts the wrong change or drops another change's work
- Path claims (R6): first-claim-wins with shares and waits is the coordination lock for every agent; a wrong rule lets two agents own one path or strands a waiter
- Workspace deletion guard (R4): must never delete an edit not yet stored on the server, across the snapshot window and the offline journal; a wrong guard loses an agent's work
- Three-way merge (R5): every land and rebase merges automatically; a wrong merge silently drops or duplicates lines
