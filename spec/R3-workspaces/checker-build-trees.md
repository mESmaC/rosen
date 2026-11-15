- A checker that builds a change's merge result in a folder of its own can start from the pool too: the client asks for the merge result's tree and gets a ready workspace brought to it, with the build output its prepare left, as a plain build tree that is no workspace.
  Its first build then compiles only what differs from the base.
  With no pool that can serve the folder, the tree's files are written plainly.
