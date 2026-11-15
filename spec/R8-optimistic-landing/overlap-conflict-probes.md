- the server probes changes whose edit regions overlap the landed change by merging onto the new base outside the change registry lock.
  Conflicts are stored and announced with a `conflict` event, without moving the head or parent.
  A clean probe installs nothing and emits no `rebased` event; each change moves at its own land or when its owner runs `rosen rebase`;
