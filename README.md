# Rosen

Rosen is a version control system built so that many coding agents can work on one project at the same time. The v1 target is 100 concurrent agents spread over several machines; the long-term target is 1000.

Rosen is the source-control layer under [Velocity](https://github.com/mESmaC/velocity), the parallel agentic IDE. It replaces git worktrees, GitHub pull requests and Velocity's local coordination files with one server that every agent's machine talks to.

[SPEC.md](SPEC.md) is the authoritative description of what Rosen does. This README is a map of it.

## Why Rosen

Git and a merge queue stop scaling once dozens of agents land work every few minutes. Rosen is optimised for development velocity: landing must be fast, and it must never lose work.

- **No merge queue or global lock.** A land is a compare-and-swap of the base ref. Unrelated changes never wait on each other.
- **Nothing to push.** Every write to a workspace is snapshotted to the server within about 2 seconds. The server does every merge, rebase and landed commit; the client only materialises, watches and uploads files.
- **Changes, not branches.** Each agent works on its own change, a row in the server's database with a stable ID. The base is the only ref.
- **Conflicts are data.** The server merges three ways automatically. A region that does not merge becomes a stored conflict on the change instead of an error.
- **Fast pre-land, full post-land.** A land needs a receipt for build and lint on the exact merge result. Full tests run on landed commits in batches; a failure is bisected and the culprit is reverted automatically.
- **Coordination in one place.** Path claims, edit regions, overlaps and the project's AIRC channel (piece claims, messages, events and ledgers) live on the server, so every machine sees the same holder.
- **Never lose an edit.** Workspaces are deleted only behind a guard that proves every edit is stored on the server, and local work continues offline through a journal.

The design draws on Anthropic's [Redesigning source control](https://www.anthropic.com/research/redesigning-source-control), [CarterT27/scion](https://github.com/CarterT27/scion) and Google's [Scion](https://github.com/googlecloudplatform/scion). SPEC.md §0 lists what Rosen takes from each.

## How it works

One authoritative server holds each project. Every machine that runs agents for it runs a daemon:

- On the server's machine, the daemon is the server. It holds the object store and a SQLite metadata database (WAL mode).
- On every other machine, the daemon is a tier-1 object cache. It answers object reads locally and forwards every other request to the server over TLS.
- Clients on each machine talk to their own daemon over a named pipe on Windows, or a Unix socket elsewhere.

Objects are stored in git format as loose, content-addressed files, so Rosen imports git repositories and pushes landed history to a git mirror such as GitHub. Each land becomes one commit with `Rosen-Change`, `Rosen-Receipt` and `Rosen-Approved-By` trailers.

A change moves through these states:

```
open ──> ready ──> landed
  │
  └──> abandoned
```

A typical agent flow:

1. Take a workspace and create a change, optionally stacked on another unlanded change.
2. Claim the paths it will edit. The first claim wins; others record a share or a wait.
3. Edit files. Snapshots reach the server automatically.
4. Get an approval for the current head, and a receipt from an external checker for the merge result.
5. Land. The server checks conflicts, approval and receipt, then moves the base by compare-and-swap. If the base moved meanwhile, it merges again and reuses the receipt only when every change landed since is independent of this one.
6. After the land, the server releases claims, rebases stacked and overlapping changes, and pushes the mirror.

Rosen runs no builds itself. Checkers such as Velocity's build queue or PR duty run the configured check sets and submit receipts.

## Repository layout

| Path | Holds |
|---|---|
| `crates/rosen-core` | The data model: changes, stacks, conflicts, receipts, reviews, claims, regions and project configuration. Pure logic, no I/O. |
| `crates/rosen-store` | The storage backend interface and the git-format loose-object store. |
| `crates/rosen-merge` | Three-way file merge, conflict objects and overlap detection. |
| `crates/rosen-protocol` | The versioned wire protocol, framing and the local transport. |
| `crates/rosen-platform` | Narrow OS primitives, including verified Windows file deletion and credential storage. |
| `crates/rosen-server` | The `rosen-server` daemon: metadata, snapshots, rebase, landing, claims, review, events, the AIRC channel, history, mirror, TLS listener and enrollment. |
| `crates/rosen-client` | The library Velocity links: daemon discovery and start, workspaces, the file watcher, snapshots, the offline journal and every server call. |
| `crates/rosen-cli` | The `rosen` binary, a thin layer over `rosen-client`. |
| `crates/rosen-stress` | The SPEC.md §5 stress harness. |
| `crates/rosen-faults` | The fault drill: random kills, partitions and an injected failing commit. |
| `proofs/` | Lean design proofs for the features on the SPEC.md proof list, and the fixture tables the Rust tests check against. |

## Building

Rosen is a Rust workspace. Windows 11 is the shipped platform; Linux and macOS are best-effort and must compile.

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The proofs use Lean 4, pinned in `proofs/lean-toolchain`. Install [elan](https://github.com/leanprover/elan), then:

```sh
cd proofs
lake build
```

## Getting started

These commands run on one machine. Most commands start the daemon on demand.

```sh
rosen server start
rosen import path/to/repo --mirror https://github.com/you/repo.git

rosen change new --workspace ../ws-1
cd ../ws-1
rosen workspace watch &            # snapshot every write within 2 s
rosen claim src/parser.rs
# ...edit files...
rosen commit -m "Parse nested lists"
rosen status

rosen review approve <change> --head <head>
rosen receipt submit <change> --head <head> --base <base>
rosen land <change>
```

`rosen --help` is written so an agent can learn Rosen from it alone. Every command takes `--json` and prints one object per line with a versioned `schema` field such as `rosen.change/v1`; `rosen --json-schema` prints the full catalog offline.

Useful environment variables:

| Variable | Meaning |
|---|---|
| `ROSEN_AGENT` | The agent identity used as owner, author and submitter. Defaults to the OS user name. |
| `ROSEN_CHANGE` | The change that `claim`, `share`, `wait`, `release` and `status` act on by default. |
| `ROSEN_ENDPOINT` | The daemon's pipe name or socket path. |
| `ROSEN_LISTEN` | The TCP address the server listens on for other machines, such as `0.0.0.0:7420`. |

### Adding machines

Machines join with a join key and no interactive step, so provisioning scripts can add them:

```sh
# On an enrolled machine
rosen machine key create --role agent --expires 24h --uses 3

# On the new machine
rosen machine join <key>
```

The joining daemon checks the server's certificate against the fingerprint in the key before sending it. It then stores its own credential encrypted, through Windows Credential Manager, and rotates it before expiry. `rosen machine list`, `approve` and `revoke` manage enrolled machines.

## Project configuration

The server reads `rosen.toml` at the repository root from the base, never from a change, so a change cannot lower its own review tier or rename its check set. Every part is optional, and unknown keys are refused.

```toml
generated = ["Cargo.lock"]                     # regenerated, never merged
registration_lists = ["src/commands.rs"]       # both sides' additions kept
sensitive = ["crates/rosen-store/"]            # sensitive review tier
ignore = ["target/"]                           # snapshots skip these

[checks.pre_land]
name = "build"
commands = ["cargo build --workspace", "cargo clippy --workspace -- -D warnings"]

[checks.post_land]
commands = ["cargo test --workspace"]

[warm_pool]
size = 4

[prepare]
commands = ["cargo build --workspace"]

[mirror]
remote = "https://github.com/you/repo.git"
```

`rosen config show` prints the configuration in force, and why the base's file was refused if it was. SPEC.md §2 describes build units and workspace-wide inputs, which decide when a receipt can be reused.

## Verification

Rosen backs its core guarantees with three layers of checking:

- **Lean design proofs** for the features on the proof list: optimistic landing and auto-revert, path and piece claims, the workspace deletion guard and three-way merge. Each proof states that the model meets its SPEC section, a cost bound and an optimality result, and exports a fixture table under `proofs/fixtures/` that a Rust test checks the implementation against.
- **A differential test** of the three-way merge against `git merge-file`.
- **The stress run and fault drill.** `rosen-stress` drives many agents creating, claiming, receipting and landing changes, then verifies that no edit was lost and no land was applied twice. `rosen-faults drill --seed <n>` kills processes and cuts off machines at random points, and replays exactly from its seed. It also lands an injected failing commit and runs a post-land checker, and checks the server bisected and reverted it; `--no-failing-commit` leaves that out.

AGENTS.md lists the commands for each.

## Status

Rosen v1 is in active development. As of 2026-11-13:

- **Built:** R1–R14 in SPEC.md, covering import and mirror, snapshots and the offline journal, local and server-side workspaces with the prepared warm pool, guarded deletion, merge and stored conflicts cleared by `rosen resolve`, claims and overlaps, review, optimistic landing with receipt reuse, events, the CLI, history queries, post-land bisection and auto-revert, the server-side AIRC channel, and network enrollment.
- **In progress:** the multi-machine §5 stress run, Velocity running its whole flow through `rosen-client`, and `lake build` in CI.
- **Open fixes:** a stacked-change land that can revert other landed edits (#146), and the hardening items on the project Task List.

Out of scope for v1: a hosted server, regional caches, a virtual filesystem, two-way git sync, object garbage collection, per-directory access control and a GUI. SPEC.md §6 has the full list.

## Contributing

Work is coordinated through Velocity and its AIRC channel. Read [AGENTS.md](AGENTS.md) before starting: it sets the build and lint commands, the claim and branch rules, the proof rules and the sensitive paths that get a full review.

## License

Apache License 2.0. See [LICENSE](LICENSE).
