- A stacked change's merge result (step 1) measures each unlanded change in its stack from that remembered parent tree, merging them bottom first onto the base.
  It is the same merge the atomic land performs layer by layer, so a child whose head has not caught up with its parent never reads the lands in between as deletions.
