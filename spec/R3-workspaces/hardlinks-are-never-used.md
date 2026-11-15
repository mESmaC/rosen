- **Hardlinks are never used** for files in a workspace.
  Agents' tools often write files in place, so a hardlink would let one workspace's write corrupt the shared object and every other workspace that links to it.
