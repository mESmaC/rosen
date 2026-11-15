- **Write-through snapshots.**
  The client watches the workspace (`ReadDirectoryChangesW` on Windows) and uploads a snapshot within 2 s of the last write, plus on `rosen commit`.
  A snapshot sends only the changed blobs.
  Files of any size are carried: a blob too large for one protocol frame is uploaded, and fetched by a machine's cache, in slices, and the server stores it only once its bytes hash to its ID.
  The client hashes such a file as it reads it and sends it from disk in slices, and the offline journal stores and replays it the same way; a file written again while its snapshot is sent or journaled fails that snapshot's hash check, and the next snapshot takes it at its new content.
  The server and caches stream such a blob through disk, never holding it whole in memory: the server hashes and stores an upload from its staging file, serves slices from one checked copy instead of rereading the object per slice, and a cache stores a fetched blob slice by slice.
