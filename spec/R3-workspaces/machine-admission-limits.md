- Machine admission is shared by clients and pool workers through OS file leases under the daemon's data directory.
  Defaults are 4 simultaneous materialisations and a 100 GiB logical workspace-content budget; the server defaults to 8 concurrent workspace I/O operations.
  The library can initialize other positive limits.
  `rosen workspace limits` shows the machine's two caps, the materialisations running and the bytes charged, and with `--materialisations` or `--disk-gib` changes either cap; the change persists for every client and the pool, and requests already waiting see it on their next attempt.
  A lowered slot cap stops no running copy.
