- A block clone is checked by hashing one cloned file against its blob before the rest are cloned.
  On a mismatch, or any clone failure, the workspace is written by plain copy instead.
  Untracked output the clone carries at a path the new tree tracks gives way to the tracked file or directory; it is not a clone failure.
