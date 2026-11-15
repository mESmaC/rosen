- The server remembers the parent tree each child was based on.
  A parent snapshot or server rebase automatically merges its new tree into live descendants in stack order, retaining each dependency, change ID and authored commit.
  Conflicts are stored on the child and stop propagation through that child until resolved.
  A restart resumes pending parent updates from the saved tree; workspaces apply the resulting head only when their owners ask (R2).
