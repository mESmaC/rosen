# AGENTS.md · rosen

Read this file in full before you start any task. Every rule here is mandatory.
SPEC.md defines what to build. The project Task List defines what is left.

## Commands
- Read the project's build, run and lint commands from its manifests and SPEC.md before running them. Record those commands here when they are established.
- For a project with a pinned Lean project, run `lake build` in `proofs/` before pushing proof changes.
- Run the project's local checks on the merge result before marking a PR ready. Lint reports zero warnings.
- Build and check only what your change can affect. A change to docs or SPEC.md needs no build. A change to one part, such as the UI or one package, needs that part's checks, not the whole project's. Never run a build or check again when nothing it covers changed since it last passed.
- Rust build: `cargo build --workspace`. For one crate, use `cargo build -p <crate>`.
- Rust lint, zero warnings: `cargo clippy --workspace --all-targets -- -D warnings`. For one crate, use `-p <crate>` in place of `--workspace`.
- Rust tests: `cargo test --workspace`. For one crate, use `cargo test -p <crate>`.
- Proofs: `lake build` in `proofs/`.
- Atomic transaction fixtures: `lake exe atomic-transactions-fixtures > fixtures/atomic-transactions.tsv` in `proofs/`.
- Claim release fixtures: `lake exe claim-release-fixtures > fixtures/claim-release.tsv` in `proofs/`.
- Bisection fixtures: `lake exe bisection-fixtures > fixtures/bisection.tsv` in `proofs/`.
- Re-check fixtures: `lake exe recheck-fixtures > fixtures/recheck.tsv` in `proofs/`.
- Workspace deletion fixtures: `lake exe workspace-deletion-fixtures > fixtures/workspace-deletion.tsv` in `proofs/`.
- Fixture tables, regenerated from `proofs/` after a model changes: `lake exe landing-fixtures > fixtures/landing.tsv` and `lake exe three-way-merge-fixtures > fixtures/three-way-merge.tsv`.
- CI runs the Rust build, lint and tests on Windows, and the Rust build on Linux.

## Code
- Keep functions under 60 lines. Split anything longer.
- Name functions, types and variables for what they do.
- Comment code whose purpose is not clear from reading it. Explain why, not what.

## Scope
- Work only on the piece you claimed.
- If you find an unrelated problem, file an issue for it. Do not fix it in your PR.
- Add to the Task List only when the user asks. Otherwise, file an issue.
- Update SPEC.md when behaviour changes. Never change a product decision yourself; file an issue when the spec contradicts itself.

## Implementation first
- Build the feature first, end to end, then run it and see it work.
- Implementation is the larger part of every PR. Proofs, tests and docs together stay smaller than the implementation code.
- Only after the feature works, add the proofs, tests or docs it needs in the same PR.
- Lockfiles and generated files count on neither side of the implementation size comparison.
- Never claim only proofs, tests or docs while implementation work in the milestone is unclaimed.

## Proofs
- Write Lean only for features on the proof list in SPEC.md. Every other piece ships with no Lean.
- For a proof-list feature, write the code first. Then write `proofs/<Feature>/Design.lean` in the same PR with exactly three theorems:
  - `correct`: the model meets the SPEC.md section it implements; name that section.
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
- Update SPEC.md when behaviour changes.
- Tests and docs are low priority; add them only after the feature works and only where they pay for themselves.
- Never add a test suite for a whole module or a test that restates the implementation.
- Copy a scout's failing script into `stress/` unchanged when fixing its bug. Never edit it.
- Comment code whose purpose is not clear; explain why. Never add a new Markdown file unless the piece asks for one.

## Coordination (AIRC)
- Before editing, claim your task, branch and every file you expect to touch in AIRC.
- If your claim is refused, pick different unclaimed work.
- Before editing a file another agent claimed, record a share or a wait in AIRC.
- If your work depends on another PR, record a wait; continue and rebase when it merges.
- Message another agent only when a claim, overlap or dependency needs words. Never repeat events Velocity posts.
- Answer AIRC questions addressed to you. Reply to requests with done or declined and a reason. Never reply to inform messages.
- Log discoveries other agents need in the Discoveries ledger.

## Shared files
- Identify generated files and registration lists for this project here as they are established.
- Regenerate generated files after a merge. Never merge them by hand.
- Keep both additions to shared registration lists, inside each module's group.
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
- PR duty gives a PR that touches a path listed here the sensitive review: full review, the full stress suite before merge, and a confidence proof at the strongest level its language allows.
- List one path, folder or glob per bullet, as inline code, followed by why it is sensitive. Add migrations, worktree deletion and auth code as they are established.
- `.github/workflows/`: CI config.
- `crates/rosen-server/src/landing/`: decides when the base moves; a bug lands unchecked code or applies a land twice (SPEC.md R8).
- `crates/rosen-server/src/metadata.rs`: shared SQLite connection settings and schema initialization; a bad migration can corrupt authoritative metadata.
- `crates/rosen-server/src/changes/schema.rs`: SQLite schema and migration for changes and snapshots; a bad migration corrupts or loses metadata.
- `crates/rosen-server/src/conflicts.rs`: SQLite schema and storage for stored conflicts; a bug lets a conflicted change land (SPEC.md R5).
- `crates/rosen-client/src/workspace/lifecycle.rs`: guarded workspace deletion; an unsafe deletion loses local edits (SPEC.md R4).
- `crates/rosen-client/src/workspace/delete.rs`: holds verified files and removes exact objects; a pathname race can lose another workspace's edits.
- `crates/rosen-platform/`: the audited Windows file-disposition boundary; its handle and ABI contract protects verified deletion.
- `crates/rosen-store/`: the authoritative object store; a bug loses or corrupts objects, and objects are never deleted (SPEC.md D5).
- `crates/rosen-protocol/src/transport_windows.rs`: restricts the named pipe to the current user; a bug exposes the server to other users.
- `crates/rosen-merge/`: every land and rebase merges through it; a bug silently drops or duplicates lines (SPEC.md R5).
- `crates/rosen-server/src/machines/`: join-key consumption, enrollment and credential rotation; a bug admits an unauthorized machine (SPEC.md R14).
- `crates/rosen-client/src/enrollment.rs`: TLS-pinned enrollment and Credential Manager persistence; a bug discloses a join key or machine credential (SPEC.md R14).
- `crates/rosen-server/src/network/`: the TLS listener, certificate pinning and machine credential check; a bug admits an unenrolled peer or sends the credential to an impostor (SPEC.md R14).

- `crates/rosen-client/src/workspace/pool_worker.rs`: removes unused ready directories on restart and pool shrink; a wrong root could delete a taken workspace (SPEC.md R3).

## Pull requests
- One piece, one branch, one PR. Name the branch `vel/<task-name>`.
- Use your thread's own worktree. Velocity creates and removes it. Never run `git worktree add`, `git worktree remove` or `git worktree prune` yourself.
- Describe what changed, why, the theorems proved and the Task List item or issue it completes.
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
- The required local checks pass on the merge result and the required proofs check.
- The PR is merged and Velocity has removed its worktree.

## Never
- Never wait for a plan or tool approval. Run with full permissions and the provider's full toolset.
- Never start build work before the project spec is locked.
- Never push directly to the project's base branch.
- Never commit secrets, tokens or `.env` files.
- Never skip, delete or weaken a scout's failing script to make checks pass.
