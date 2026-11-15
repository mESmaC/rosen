- A rename works only within one volume, so ready workspaces live on the volume workspaces do.
  The daemon keeps them in its data directory unless started with `--warm-pool-dir <DIR>` (default `$ROSEN_WARM_POOL_DIR`), which names a directory on the workspaces' volume, such as the Dev Drive.
  The daemon uses only a subdirectory of it named for its data directory, so daemons of several projects can share one setting and the daemon never removes anything else there.
  A take into another volume than the pool's is not served: the client materialises the workspace itself, and its report says why the pool was not used.
