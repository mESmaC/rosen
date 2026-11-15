- **Prepared workspaces.**
  Project configuration lists **prepare commands**.
  These are steps whose results don't depend on the workspace's path or its agent, such as installing pinned toolchains, fetching dependency caches (`lake exe cache get`) and a prebuild (`cargo build --workspace`).
  The daemon runs them in each ready workspace, so an agent's first build compiles only what it changes.
