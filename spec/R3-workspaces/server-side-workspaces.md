- **Server-side workspaces.**
  Files live on the server, and clients read, write, list and diff them through the API, with no checkout.
  Writes become snapshots directly.
  `workspace_read` reads a file or lists a directory on the current change head; `workspace_diff` compares it with its parent.
  `workspace_write` writes bytes (new files are regular files and existing modes are preserved), or deletes a file with `None`, and returns its stored snapshot.
  Parent directories are created as needed and empty directories are pruned on deletion.
  Files of any size are carried: a write too large for one protocol frame uploads its blob in slices first and then names it, and a read of such a file, here or through the R11 file read at any revision, fetches it in slices and checks it against its ID.
  Concurrent writes each build on the preceding head.
  Reads and writes follow server rebases directly.
  Rosen recommends them for jobs that touch few files, such as reviewers, check runners and doc edits.
