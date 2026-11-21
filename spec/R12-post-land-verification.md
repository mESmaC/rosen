### R12 · Post-land verification
- The project configures a **post-land check set** (the full tests).
  External checkers run it (D9).
  After-land projects and `recheck = "merge"` projects also run `checks.pre_land` (build and lint) after landing.
  `post_land_next` offers test work; `post_land_build_next` offers build work.
  Jobs carry their check kind and immutable configured commands; results are checked against both.
  The two streams have independent passing prefixes, so a passing build never marks full tests green.
- In `merge` mode, `post_land_next` offers pending build/lint work on the newest tip before full tests.
  Build jobs carry `landing` priority for external checkers to pass to `rosen run --class`; full tests carry `verify` priority.
  Build failures use the same bisection, confirmation and revert rules below.
- **Last built.**
  Successful build/lint runs advance a separate, durable, monotone checkpoint, never the full-test last-green checkpoint.
  A `built` event wakes warm pools.
  In `merge` mode both new unstacked changes and warm pools start at last built; before the first passing build they use the imported starting commit, without claiming it is verified.
  `config show` exposes this starting commit as `workspace_base`.
- **Batches.**
  The server offers checkers the newest untested landed commit.
  One passing run marks every commit up to it **green**, and the server records the last green commit.
- **Failure and bisect.**
  On a failing run, the server offers the commits between the last green and the failure for bisection, until one commit is found to break, the **culprit**.
  - Bisection jobs persist their captured landed interval and a step token.
    Each result must name the exact offered commit, check kind and token; duplicate or stale results are refused.
    The server splits the remaining interval at its midpoint, then requests a second failure of the culprit and a fresh passing run of its parent.
    Contradictory confirmation stops the session for the checker to investigate.
    Sessions resume after restart.
- **Record every confirmed failure.**
  Both build and test culprits are persisted as `post-land-failure` records and events with their check kind and output, queryable through `post_land_failures` and `review failures [--change <change>]` after restart.
  Repeated confirmation is idempotent.
  A recorded test-failure batch is not offered endlessly as untested and is never marked green merely because it was recorded.
  - Records and events are deduplicated by `(culprit commit, check kind)`, including confirmation by different bisection sessions.
    Completed batches remain covered when their culprit already has a record.
    Later test searches use the newer of the test green and the latest recorded, non-reverting test culprit before the failing batch as their lower bound.
    This does not mark the bound green; the new culprit still needs a fresh passing parent confirmation.
  - Later build searches start after the newer of the build checkpoint, recorded build culprits and batches containing only a known failure.
    While a build culprit awaits its inverse, the session captures the latest outstanding culprit's output durably.
    An exact output match counts as no new failure during that search and its fresh parent confirmation; unfamiliar failures still require two failures and that parent confirmation.
    A batch containing only the known failure is covered without marking it built or green.
    Output comparison can distinguish new breaks only when they produce different output.
- **Auto-revert.**
  In after-land mode only a **build** failure requests a revert; a **test** failure stays landed with its failure record for Velocity to file as an issue.
  Before-land projects keep reverting confirmed full-test failures.
  The mode is resolved at the culprit's landed commit, like its check commands; a configuration edit takes effect once it lands.
  The server lands a revert of the culprit through R8, as a server-owned change that needs no receipt or approval.
  The culprit change goes back to `open` with its owner, carrying the failing check output, and gets a `reverted` event.
  Its commits are kept on the server.
  R4 has usually deleted its local workspace by the time it is found, so its owner materialises the change's head into a workspace again to fix it (#135).
  - The inverse waits for the culprit's original post-land work to finish and merges against the current base, preserving later lands; an overlapping inverse that cannot merge cleanly refuses without moving the base.
    A refused inverse durably publishes `revert_refused` with the culprit, attempted base, conflicting paths, overlapping later commits and reason, once per culprit and attempted base.
    Recovery retries pending inverses independently with backoff, waking when a new land moves the base; one refused inverse never blocks another.
    The inverse land, reopened change and durable failure output commit atomically.
    Repeating a confirmed culprit returns the same revert, and its reopened change can land again under the same change ID after repair.
- **Who ran each result.**
  `post_land_passed`, `post_land_failed` and `bisect_submit` record their submitter and time, as receipts do (R8 step 1, R14 "Agents").
  The request names only the checker's nick, without `@`; the server appends the machine from the connection's credential, or `server` on the local transport.
  A request with no nick records the machine alone.
  Last built, last green, each accepted bisection step and each confirmed failure carry it, as do the `built`, `green` and `post-land-failure` events.
  Results submitted before this was recorded show no submitter.
- **Status.**
  `post_land_status` and `review status [--json]` (schema `rosen.post_land_status/v1`) read post-land progress without changing it, on the read-only connections.
  It never offers a job or moves a session.
  It returns the base tip, last built and last green with their submitters and times, and the open bisection sessions.
  A session is open while it searches or confirms, or while it stopped as flaky and its failing batch is still above its kind's checkpoint.
  Each session gives its kind, lower bound, failing commit, interval length in commits, `planned_steps` (the halving steps for the interval plus the two confirmations) and `flaky`.
  Its steps are the accepted results (`passed` or `failed`, with submitter and time), the current job as `offered`, and the steps still planned as `waiting`, with no commit yet.
  It lists each pending inverse, a confirmed culprit whose required inverse has not landed, with its latest attempted base, conflicting paths, overlapping later commits, reason and first refusal time.
- **Progress events.**
  Opening a session and each accepted bisection result publish one `bisect` event, staged with the change that causes it so it survives a restart.
  Its detail holds the session, kind, step, commit, result, submitter and time, and `progress`: `opened`, `step`, `culprit` or `flaky`.
- **Platforms.**
  `[checks.pre_land]` and `[checks.post_land]` may name `platforms = ["windows", "macos", "linux"]` (or `<os>-<arch>` targets) and optional `[checks.<set>.platform_commands]` appended for matching platforms; omitting `platforms` defaults to any platform.
  Checkers report their OS and architecture via `post_land_next_for`, and the server offers each platform's jobs only to matching checkers.
  Each required platform keeps its own monotone checkpoint, bisection sessions and culprit records; the aggregate base built or green checkpoint advances only to the minimum passing commit across every required platform.
  `rosen checker run [--platform <platform>]` runs a long-lived headless checker on an enrolled machine without Velocity, running each job's commands through `rosen run` (with the `verify` class for full tests) and posting batch and bisection results.
  `post_land_status`, `rosen server status`, `rosen review status` and the Collapse state feed (`platforms`) show each required platform's checkpoint and report when no matching checker is connected (for example `macOS: no checker`).
- **Deploy steps.**
  `rosen.toml` may declare `[deploy.<name>]` steps, each with a single-line `command`, optional `paths` globs, `secrets` (names only, never values) and `platforms`.
  When a full-test green checkpoint advances across a landed range touching a step's `paths` (or any range when `paths` is empty), the server queues the matching steps from the configuration at that green commit in declaration order, atomically with the checkpoint.
  Steps for one project run one at a time and in order: the server publishes `deploy_due` for the next pending step only when none is running, and marks any pending older run of a step skipped when a newer green already covers it (or removes the step).
  External checkers report each step's outcome (`passed`, `failed` or `skipped`) through `deploy_result`, which records the submitter and time, publishes a `deploy_result` event and dispatches the next pending step.
- A flaky result is the checker's concern: a failure is confirmed only after the culprit fails twice and its parent passes.
  Required build reverts run immediately after confirmation; recovery retries an interrupted inverse.
- **Accept when:** an after-land test failure is bisected, recorded and left landed; an after-land build failure is recorded and reverted, its change reopens with its work intact, and later independent work is preserved.
  Before-land full-test failures retain their existing automatic revert behavior.
  When an initial build culprit cannot be reverted, a later independent break with different output is still found, recorded and reverted, and the first refusal is visible as an event.
