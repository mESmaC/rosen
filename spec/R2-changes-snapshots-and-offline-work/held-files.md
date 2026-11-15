- **Held files.**
  A file the watcher cannot read because another process holds it open, such as a running executable or an exclusively opened database, does not hold back other writes.
  The snapshot keeps that file's last stored content, reports it, and the watcher reads it again each second until it can.
  A file larger than one 16 MiB frame, which is hashed and then sent from disk, is treated the same way when it is still changing after being hashed a few times, as a growing log or a database written without pause would be; its last stored content stands until it holds still for one read.
  `rosen commit` and workspace deletion (R4) read every file, so a held file fails them with its path.
