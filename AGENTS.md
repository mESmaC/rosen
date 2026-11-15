# AGENTS.md · rosen

Read this file in full before you start any task. Every rule here is mandatory.
SPEC.md indexes the specification; read the linked spec/ files for the feature you change. The project Task List defines what is left.

## Where you work
- Your thread works in a Rosen workspace by default. Velocity's first message says so and names your change. Landing your work below gives that flow.
- A project can choose git instead, and Velocity falls back to a git worktree when Rosen can't make a workspace. Your folder is then a git checkout on a `vel/<task-name>` branch, and you follow the git flow under Landing your work.
- To tell them apart: a Rosen workspace has a `.rosen` folder at its root and no `.git`.

## Commands
- Read the project's build, run and lint commands from its manifests and SPEC.md before running them. Record those commands here when they are established.
- Run every build, test and lint through `rosen run -- <command>`, for example `rosen run -- cargo build -p rosen-core`. Heavy commands on one machine take turns in its run queue (spec/R3-workspaces/README.md R3), so yours gets the cores while it runs, and the checks a land waits on go first. This applies in a git worktree too, whenever `rosen` is on `PATH`. A command already inside a turn runs at once, so `rosen run` never waits on itself.
- For a project with a pinned Lean project, run `lake build` in `proofs/` before pushing proof changes.
- Before you name your work (or mark a PR ready), run the checks your change can affect and see them pass. Lint reports zero warnings. Velocity then runs `rosen.toml`'s pre-land checks on the exact merge result before every land, and its post-land checks (`cargo test --workspace`) on what landed.
- Build and check only what your change can affect. A change to docs or SPEC.md needs no build. A change to one part, such as the UI or one package, needs that part's checks, not the whole project's. Never run a build or check again when nothing it covers changed since it last passed.
- Rust build: `cargo build --workspace`. For one crate, use `cargo build -p <crate>`.
- Rust lint, zero warnings: `cargo clippy --workspace --all-targets -- -D warnings`. For one crate, use `-p <crate>` in place of `--workspace`.
- Rust tests: `cargo test --workspace`. For one crate, use `cargo test -p <crate>`.
- Proofs: `lake build` in `proofs/`.
- Axiom audit: `lake env lean --run ../.github/scripts/audit_axioms.lean` in `proofs/`, after `lake build`. It fails when a declaration uses an axiom other than `propext`, `Classical.choice` and `Quot.sound`.
- Atomic transaction fixtures: `lake exe atomic-transactions-fixtures > fixtures/atomic-transactions.tsv` in `proofs/`.
- Claim release fixtures: `lake exe claim-release-fixtures > fixtures/claim-release.tsv` in `proofs/`.
- Bisection fixtures: `lake exe bisection-fixtures > fixtures/bisection.tsv` in `proofs/`.
- Auto-revert fixtures: `lake exe auto-revert-fixtures > fixtures/auto-revert.tsv` in `proofs/`.
- Re-check fixtures: `lake exe recheck-fixtures > fixtures/recheck.tsv` in `proofs/`.
- Workspace deletion fixtures: `lake exe workspace-deletion-fixtures > fixtures/workspace-deletion.tsv` in `proofs/`.
- Path claims fixtures: `lake exe claims-fixtures > fixtures/claims.tsv` in `proofs/`.
- Piece claims fixtures: `lake exe piece-claims-fixtures > fixtures/piece-claims.tsv` in `proofs/`.
- Crash recovery fixtures: `lake exe crash-recovery-fixtures > fixtures/crash-recovery.tsv` in `proofs/`.
- Batch green fixtures: `lake exe batch-green-fixtures > fixtures/batch-green.tsv` in `proofs/`.
- Conflict routing fixtures: `lake exe conflict-routing-fixtures > fixtures/conflict-routing.tsv` in `proofs/`.
- Targeted rebase fixtures: `lake exe targeted-rebase-fixtures > fixtures/targeted-rebase.tsv` in `proofs/`.
- Stacked changes fixtures: `lake exe stacked-changes-fixtures > fixtures/stacked-changes.tsv` in `proofs/`.
- Land handoff fixtures: `lake env lean --run StackedChanges/Handoff.lean > fixtures/land-handoff.tsv` in `proofs/`.
- Fixture tables, regenerated from `proofs/` after a model changes: `lake exe landing-fixtures > fixtures/landing.tsv` and `lake exe three-way-merge-fixtures > fixtures/three-way-merge.tsv`.
- Stress suite (the merge gate for sensitive PRs): `cargo build -p rosen-server -p rosen-faults`, then `target/debug/rosen-faults drill --seed 1` and `target/debug/rosen-faults drill --seed 2`, as described under Fault drill below. Each run takes about a minute. This is the project's stress command.
- §5 benchmark, not the stress suite and never a merge gate: run it only when the user asks. It takes an hour or more and fails the latency targets until the server performance issues are fixed. On one machine: `cargo build --release -p rosen-server -p rosen-stress`, then `target/release/rosen-stress local --dir <new dir>` for three simulated machines; add `--faults --fault-seed <n>` for the kill and partition run. Across machines: `rosen-stress seed`, `rosen import` on the server, `rosen-stress agents` on each machine with `rosen-stress checker` on one, then `rosen-stress verify <reports…>`; `rosen-stress --help` has the flags.
- Fault drill (spec/5-done-means.md §5): `cargo build -p rosen-server -p rosen-faults`, then `target/debug/rosen-faults drill --seed <n>`. It injects a failing commit and checks R12 bisects and reverts it unless given `--no-failing-commit`, prints a JSON report, exits non-zero when a guarantee breaks, and replays exactly from its seed.
- CI runs the Rust build, lint and tests on Windows and macOS, and the Rust build on Linux. The Proofs workflow runs `lake build --wfail` and the axiom audit on Linux when `proofs/`, the audit script or the workflow changes.

## Code
- Keep functions under 60 lines. Split anything longer.
- Name functions, types and variables for what they do.
- Comment code whose purpose is not clear from reading it. Explain why, not what.

## Scope
- R2, R3 and R8 keep one rule per file; add new rules as new files in their spec/ folder, with one sentence per line.
- The spec move can be repeated with `python .github/scripts/split_spec.py --source <monolithic-SPEC.md>`; `--join` prints its original order.
- Work only on the piece you claimed.
- If you find an unrelated problem, file an issue for it. Do not fix it in your PR.
- Add to the Task List only when the user asks. Otherwise, file an issue.
- Update the relevant spec/ rule file when behaviour changes. Never change a product decision yourself; file an issue when the spec contradicts itself.

## Implementation first
- Build the feature first, end to end, then run it and see it work.
- Implementation is the larger part of every PR. Proofs, tests and docs together stay smaller than the implementation code.
- Only after the feature works, add the proofs, tests or docs it needs in the same PR.
- Lockfiles and generated files count on neither side of the implementation size comparison.
- Never claim only proofs, tests or docs while implementation work in the milestone is unclaimed.
- The size rule binds a PR that builds or changes a feature. A piece an issue or review opens to add or strengthen proofs, tests or docs for a merged feature is exempt: it carries only the code fixes that work exposes, and never padding to meet the rule. Its PR description says it is such a piece and links that issue or review.

## Proofs
- Write Lean only for features on the proof list in SPEC.md §7. Every other piece ships with no Lean.
- For a proof-list feature, write the code first. Then write `proofs/<Feature>/Design.lean` in the same PR with exactly three theorems:
  - `correct`: the model meets the spec/ section it implements; name that file and section.
  - `cost_bound`: its cost stays within a bound, under a cost model the theorem states.
  - `optimal`: no alternative in a class the theorem states costs less.
- Export a compact fixture table to `proofs/fixtures/<feature>.tsv`, and add one test that checks the implementation against it.
- Never write a theorem about one behaviour, case or example. That is a test.
- Never use `sorry`, `admit`, `axiom` or `native_decide`.
- Never weaken a reviewed theorem statement to make its proof go through.
- Lemmas are allowed only as steps toward those three theorems.
- If a design proof passes 300 lines, stop and file an issue.
- State the cost model and the class of alternatives in the theorem itself.
- Run code translation tools only when an issue asks for a translation.
- For a bug fix in a proof-list feature, add the failing input to its fixture table. A bug fix elsewhere has no Lean.
- Audit every proof declaration: `#print axioms` lists only `propext`, `Classical.choice` and `Quot.sound`.

## Tests and docs
- Default: add no tests. Add a targeted test only for a critical contract identified in SPEC.md, with its risk in a comment.
- Scouts and PR duty agents write the adversarial tests and stress scripts.
- Update the relevant spec/ rule file when behaviour changes.
- Tests and docs are low priority; add them only after the feature works and only where they pay for themselves.
- Never add a test suite for a whole module or a test that restates the implementation.
- Copy a scout's failing script into `stress/` unchanged when fixing its bug. Never edit it.
- Comment code whose purpose is not clear; explain why. Never add a new Markdown file unless the piece asks for one.

## Coordination (AIRC)
- Before editing, claim your task in AIRC, and every file you expect to touch with `rosen claim <paths>`. Path claims are notices by default and report concurrent work. Use `rosen claim --exclusive` only when concurrent edits should hold off; generated files and registration lists cannot be exclusive.
- If a piece claim or exclusive path claim is refused, pick different unclaimed work.
- Before editing a file another change claimed, record it with `rosen share <paths>`, or `rosen wait <paths>` when you need its change landed first.
- If your work depends on another change, record a wait; continue, and take the rebase Rosen makes when it lands.
- Claims are released when your change lands or is abandoned. `rosen release <paths>` gives one up sooner.
- In a git worktree, claim your branch and files in AIRC instead.
- Message another agent only when a claim, overlap or dependency needs words. Never repeat events Velocity posts.
- Answer AIRC questions addressed to you. Reply to requests with done or declined and a reason. Never reply to inform messages.
- Log discoveries other agents need in the Discoveries ledger.

## Shared files
- Identify generated files and registration lists for this project here as they are established.
- Regenerate generated files after a merge. Never merge them by hand.
- Keep both additions to shared registration lists, inside each module's group.
- In Rosen, `rosen.toml` lists these. Rosen merges a registration list by keeping both sides' additions, and gives a generated file the base's copy. When Rosen rebases your change onto a base that changed a generated file, regenerate it in your workspace before you name the work again.
- Generated files:
  - `Cargo.lock`: regenerate with cargo.
  - `proofs/lake-manifest.json`: regenerate with lake.
  - `proofs/fixtures/*.tsv`: regenerate with the fixture commands under Commands.
- Registration lists:
  - `Cargo.toml` `[workspace.dependencies]`.
  - `crates/rosen-protocol/src/message.rs`: the `Request`, `Response`, `ErrorCode` and `EventKind` enums.
  - `crates/rosen-server/src/rpc.rs`: the `dispatch` match.
  - `crates/rosen-cli/src/commands.rs`: the `Command` enum and its subcommand enums.
  - `proofs/lakefile.toml`: the `lean_lib` and `lean_exe` entries.

## Sensitive paths
- PR duty gives a change or PR that touches a path listed here the sensitive review: full review, then the stress suite (the two fault drill seeds under Commands) and `cargo test --workspace` on the merge result before merge. Never run the §5 benchmark for a review. A proof-list feature the PR changes needs its confidence proof at reference level: a fixture table exported from the Lean model and a Rust test that checks the implementation against it. Code level (a translation) is required only when an issue asks for one.
- List one path, folder or glob per bullet, as inline code, followed by why it is sensitive. Add migrations, worktree deletion and auth code as they are established.
- `rosen.toml`: Rosen's own project configuration; it names the checks a land needs, the sensitive paths below and the mirror remote. Keep its `sensitive` list the same as the list in this file. Its `registration_lists` also holds `Cargo.lock` and the fixture tables, for the reason it gives.
- `.github/workflows/`: CI config.
- `crates/rosen-server/src/landing/`: decides when the base moves; a bug lands unchecked code or applies a land twice (spec/R8-optimistic-landing/README.md R8).
- `crates/rosen-server/src/metadata.rs`: shared SQLite connection settings and schema initialization; a bad migration can corrupt authoritative metadata.
- `crates/rosen-server/src/changes/schema.rs`: SQLite schema and migration for changes and snapshots; a bad migration corrupts or loses metadata.
- `crates/rosen-server/src/channel/`: the AIRC channel's SQLite schema and piece-claim lock; a bug lets two agents own one piece or loses channel state (spec/R13-airc-channel-on-the-server.md R13).
- `crates/rosen-server/src/conflicts.rs`: SQLite schema and storage for stored conflicts; a bug lets a conflicted change land (spec/R5-automatic-merge-and-conflicts.md R5).
- `crates/rosen-client/src/workspace/lifecycle.rs`: guarded workspace deletion; an unsafe deletion loses local edits (spec/R4-workspace-lifecycle-and-deletion.md R4).
- `crates/rosen-client/src/workspace/delete.rs`: holds verified files and removes exact objects; a pathname race can lose another workspace's edits.
- `crates/rosen-client/src/workspace/delete/`: Unix verified removal and recovery; a pathname race can delete an unstored replacement.
- `crates/rosen-platform/`: the audited OS file-removal boundary; its handle and ABI contract protects verified deletion.
- `crates/rosen-store/`: the authoritative object store; a bug loses or corrupts objects, and objects are never deleted (SPEC.md D5).
- `crates/rosen-protocol/src/transport_windows.rs`: restricts the named pipe to the current user; a bug exposes the server to other users.
- `crates/rosen-merge/`: every land and rebase merges through it; a bug silently drops or duplicates lines (spec/R5-automatic-merge-and-conflicts.md R5).
- `crates/rosen-server/src/machines/`: join-key consumption, enrollment and credential rotation; a bug admits an unauthorized machine (spec/R14-network-access-and-enrollment.md R14).
- `crates/rosen-client/src/enrollment.rs`: TLS-pinned enrollment and Credential Manager persistence; a bug discloses a join key or machine credential (spec/R14-network-access-and-enrollment.md R14).
- `crates/rosen-server/src/network/`: the TLS listener, certificate pinning and machine credential check; a bug admits an unenrolled peer or sends the credential to an impostor (spec/R14-network-access-and-enrollment.md R14).

- `crates/rosen-client/src/workspace/pool_worker.rs`: removes unused ready directories on restart and pool shrink; a wrong root could delete a taken workspace (spec/R3-workspaces/README.md R3).

## Landing your work
One piece, one change (or one PR). Describe what changed, why, the theorems proved and the Task List item or issue it completes.

### In a Rosen workspace
- The folder is not a git checkout. Never run `git` or `gh` in it, and skip every branch, commit, push and pull request step.
- Every file you write is stored on the Rosen server within 2 seconds. There is nothing to push. `rosen status` shows what is stored, and whether the server has rebased your change; `rosen diff` shows your change.
- When the work is done and its checks pass, name it: `rosen commit -m "<what changed, and the item or issue it completes>"`. That is your request for review. End your turn.
- PR duty reviews your change's merge result onto the base. Velocity runs the pre-land checks on that exact tree and lands it. Never run `rosen review approve`, `rosen receipt submit` or `rosen land` on your own change.
- When PR duty requests changes, or the pre-land checks fail, Velocity sends you the reason. Fix it, then name the work again with `rosen commit -m`.
- When your change conflicts with the base: run `rosen rebase`, combine both sides between the conflict markers, run `rosen resolve <file>` for each file, then `rosen commit -m` again.
- After a land, the post-land checks run the full tests. If your change broke them, Rosen reverts it and Velocity reopens your workspace with the failing output. Your work is kept: fix it and name it again.
- Velocity creates and deletes your workspace. Never delete it, and never move files out of it to keep them.

### In a git worktree (the fallback)
- Name the branch `vel/<task-name>`. Use your thread's own worktree. Velocity creates and removes it. Never run `git worktree add`, `git worktree remove` or `git worktree prune` yourself.
- Opening the PR does not finish the task. Monitor it until it is merged.
- Fix failing checks on your change and push.
- Address every PR duty comment: fix and push, or reply with the reason. Resolve each thread and re-request review after fixes.
- Pull before every push; PR duty can update the branch. Never force-push.
- If the PR closes without merging, report why in your thread and keep the worktree.
- Never merge your own PR. Only the PR duty agent that approved a PR merges it.

## Review severity
- Block only data loss or corruption, a crash or hang, a security hole, a SPEC.md product-rule violation, a failing local check, or the claimed feature's main path failing its spec.
- File other findings as issues with evidence, link them in the review and approve.
- Put every blocking finding in the first review. Later rounds review new commits and block only findings in those commits or new data loss or a crash.
- When unsure whether a finding blocks, file an issue; do not block.

## Done means
- The piece meets SPEC.md and has been run successfully.
- The required checks pass on the merge result and the required proofs check.
- The change has landed and its post-land checks passed (or the PR is merged), and Velocity has removed its workspace or worktree.

## Never
- Never wait for a plan or tool approval. Run with full permissions and the provider's full toolset.
- Never start build work before the project spec is locked.
- Never push directly to the project's base branch, and never land your own change.
- Never commit secrets, tokens or `.env` files.
- Never skip, delete or weaken a scout's failing script to make checks pass.
