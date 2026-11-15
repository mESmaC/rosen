- **Per-agent setup stays with the owner.**
  Steps that depend on the agent, its credentials or its final path run after the take, by whoever took the workspace.
  Velocity copies untracked files such as `.env`, links skills and runs its after-take setup command (§9).
  Rosen never stores those files.
