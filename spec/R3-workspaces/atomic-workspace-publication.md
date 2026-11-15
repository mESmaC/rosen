- A workspace is built in a hidden directory beside its destination and renamed into place when complete, so a crash never leaves a partial workspace.
  Publication and warm-pool takes use an atomic no-replace rename: a file, directory or symlink created at the destination during preparation is preserved and publication fails.
  If the platform or filesystem cannot perform that operation, publication fails without a replacing fallback.
