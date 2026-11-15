- **Applying server rebases.**
  When the server rebases a change (R5, R8, D19), the rebased tree is written into a local workspace only when the workspace's owner asks.
  The client exposes `rebase_change` to compute and persist a server rebase, preserving all snapshot commits; `rosen rebase` runs it and, inside the change's workspace, applies it.
  A rebase that conflicts still moves the change onto the base: its new head holds the conflict markers and its conflicts are stored (R5).
  Snapshots sent before local application name their workspace basis; their edits are merged onto the rebased head while their original commit trees are kept.
  Such a snapshot is never refused for a content conflict: new conflicts hold markers with the snapshot's side as `ours` and are stored (R5).
  For a file whose conflict is already open, the snapshot replaces its head content directly, including deletion, instead of merging into its markers.
  This also repairs a repeated snapshot from an older basis.
  Its conflict records remain open until explicit resolution.
  The client exposes `apply_rebase(root, owner)` to apply the stored tree explicitly.
  Applying requires paused editing tools, refuses unstored edits and pending offline snapshots, and preserves ignored output.
  A watcher can stay open: application takes the same writer lock as snapshots and journal replay, and the watcher's next capture reloads the stored basis, ignore rules and index, including sparse paths absent from disk.
  Velocity asks at the agent's step boundary, or while the agent is parked, so files never change under an agent mid-step (§9).
