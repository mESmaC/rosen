- **Held files.**
  A file the watcher cannot read because another process holds it open, such as a running executable or an exclusively opened database, does not hold back other writes.
  The snapshot keeps that file's last stored content, reports it, and the watcher reads it again each second until it can.
  A file larger than one 16 MiB frame, which is hashed and then sent from disk, is treated the same way when it is still changing after being hashed a few times, as a growing log or a database written without pause would be; its last stored content stands until it holds still for one read.
  `rosen commit` uses the stat cache: it rereads new files, tracked files without a trusted cached stat, and files whose size or modification time changed; it skips tracked files whose stats still match, even when another process holds them open.
  A held file that commit needs to reread fails the commit with its path; a skipped held file keeps its cached content.
  Workspace deletion (R4) bypasses the stat cache and reads every relevant file, so a held file fails deletion with its path.
  For a full-read commit, pause the watcher, remove `.rosen/stat-cache.json`, then run `rosen commit` before restarting the watcher. The new commit writer has no cached stats and rereads every relevant file.
