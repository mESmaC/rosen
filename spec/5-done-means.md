## 5. Done means (v1)

- R1–R14 are built and run on Windows 11.
  The Linux build compiles.
- Each GitHub release provides `rosen` and `rosen-server` with the suffix `-x86_64-pc-windows-msvc.exe` for Windows x86-64 and `-x86_64-unknown-linux-gnu` for Linux x86-64.
  `SHA256SUMS.txt` lists the four binary hashes, and `rosen-client-commit.txt` contains the full Rosen source commit SHA used to build them.
- **The stress run passes on at least three Rosen machines**, enrolled with one join key and no interactive step, one of them serving the project.
  The machines are separate Windows 11 computers where those are available.
  Otherwise they are isolated on one Windows 11 computer, each with its own data directory and endpoint, and every machine but the serving one reaches the server only over its network listener (#193).
  The run covers all of this:
  - 100 concurrent agents spread across the machines, each creating a change (some stacked), editing, claiming, submitting receipts and landing, over at least 1000 lands;
  - a new workspace is ready in under 2 s (p95) with the warm pool filled;
  - a land with a valid receipt commits in under 200 ms (p95);
  - automatic snapshot latency is reported as samples, p50, p95, maximum and the count over R2's 2 s budget, separately from pass/fail checks; each new stress change attempts to start its watcher and waits for its initial scan before measuring edits, then verifies the acknowledged snapshot's immutable commit contains every expected edit;
  - watcher startup, measurement and shutdown errors or timeouts are counted as `snapshot_failures` with diagnostics, without aborting the change; a valid sample is kept even if shutdown fails, and an explicit snapshot still makes the change's edits durable before landing;
  - no edit is lost, no land is applied twice, and the base moves only by a clean land (plus matching receipt and approval in before-land mode), or a server revert;
  - the harness retries a receipt or approval refused after a server head move by reading a fresh merge result;
  - in the seeded before-land stress project, verification matches each receipt trailer's head, base, check set and qualified checker to a recorded receipt and its immutable checked tree, and each approval trailer to a run review of a head whose checked merge covers the landed tree;
    Exact checked-tree reuse and reuse across independent seed units remain valid, while a different tree in the change's unit or an unrelated approval fails verification;
    Older reports without checked-tree and checker evidence are readable but cannot establish this guarantee, and need a new run;
  - an injected failing commit is bisected and reverted, and its change reopens with its work intact;
  - no two agents ever hold the same piece or exclusive path claim, on any machine;
  - the run is repeated with the server, machine daemons and clients killed at random points, and with the server unreachable from some machines for a period, and still meets those guarantees.
- Velocity can create workspaces, claim pieces and paths, message agents, stack changes, review, submit receipts and land through `rosen-client` alone, with no git worktree, GitHub PR or local AIRC files in the loop.
- The proofs on the proof list check, and each is tested against its fixture table.
  Merge is also checked against `git merge-file` on a differential corpus.
- `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass, as does `lake build` in `proofs/`.

---
