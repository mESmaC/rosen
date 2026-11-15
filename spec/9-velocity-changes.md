## 9. Velocity changes Rosen needs

Rosen depends on these changes to Velocity.
They are built in Velocity's repository, not here.

- **Worktrees.**
  Threads get workspaces from `rosen-client`.
  The worktree warm pool (F5), its prebuild worker and `git worktree` calls go.
  Rosen prepares pool workspaces (R3).
  The project's prepare commands take over Velocity's toolchain, Lean cache and prebuild steps.
  Velocity keeps the per-agent steps after a take: copying untracked files, linking skills and its after-take setup command.
  A swarm launch raises the pool size on the machines it uses.
- **PR watcher.**
  GitHub polling every 15 s (F6) is replaced by the Rosen event stream.
- **PR duty.**
  Before-land duty agents review, approve, submit receipts and land through `rosen-client`.
  After-land projects land named changes without those gates; review/scout agents follow landed events and record coverage.
  Branch updates (F6) become conflict probes for overlapping edits; stack restacks remain server-side rebases (D19).
- **AIRC.**
  Velocity's AIRC tools and view call the Rosen server (R13) instead of local files.
  File claims, shares, waits and overlap notices become Rosen path claims (R6).
  Task claims, events, messages, presence and ledgers become channel state on the server.
- **Rebases.**
  Velocity asks `rosen-client` to apply a pending server rebase at the agent's step boundary, or while it is parked, in place of `airc_rebase` (R2).
- **Enrollment and dispatch.**
  Velocity creates join keys and lists, approves and revokes machines through R14.
  Its cross-system dispatch uses the same join key, so one command adds a headless machine to both Velocity and Rosen.
- **Dependency waits and parking.**
  Most waits become stacked changes.
  Parking remains only for path-claim waits.
- **Build queue.**
  Velocity keeps its build queue.
  Before-land projects produce pre-land receipts with priority; after-land projects build after landing.
  Both run full tests and typed bisection (R12), filing recorded test failures and promptly processing required build reverts.
