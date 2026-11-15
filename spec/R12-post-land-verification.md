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
- **Auto-revert.**
  In after-land mode only a **build** failure requests a revert; a **test** failure stays landed with its failure record for Velocity to file as an issue.
  Before-land projects keep reverting confirmed full-test failures.
  The mode is resolved at the culprit's landed commit, like its check commands; a configuration edit takes effect once it lands.
  The server lands a revert of the culprit through R8, as a server-owned change that needs no receipt or approval.
  The culprit change goes back to `open` with its owner, carrying the failing check output, and gets a `reverted` event.
  Its commits are kept on the server.
  R4 has usually deleted its local workspace by the time it is found, so its owner materialises the change's head into a workspace again to fix it (#135).
  - The inverse waits for the culprit's original post-land work to finish and merges against the current base, preserving later lands; an overlapping inverse that cannot merge cleanly refuses without moving the base.
    The inverse land, reopened change and durable failure output commit atomically.
    Repeating a confirmed culprit returns the same revert, and its reopened change can land again under the same change ID after repair.
- A flaky result is the checker's concern: a failure is confirmed only after the culprit fails twice and its parent passes.
  Required build reverts run immediately after confirmation; recovery retries an interrupted inverse.
- **Accept when:** an after-land test failure is bisected, recorded and left landed; an after-land build failure is recorded and reverted, its change reopens with its work intact, and later independent work is preserved.
  Before-land full-test failures retain their existing automatic revert behavior.
