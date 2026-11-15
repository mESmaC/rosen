- **Ignored paths.**
  Snapshots skip paths matched by the workspace's `.gitignore` files and by the project's ignore list (project configuration), such as build output (`target/`, `node_modules/`).
  An ignored path is never uploaded, never counts as an edit or an overlap (R6), and never blocks deletion (R4).
  Writes to ignored paths do not delay a snapshot.
  For untracked paths, event filtering and snapshot scans check ancestors first: below an excluded directory they do not probe event-path metadata or read nested `.gitignore` files, avoiding transient handles inside build trees that tools rename on Windows.
  A nested rule cannot re-include a path until its excluded ancestors are re-included; already tracked files remain captured.
