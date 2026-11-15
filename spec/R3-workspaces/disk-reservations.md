- Disk admission reserves, before copying, the most logical bytes the build can leave: the target files, or for a block clone the source-template content plus the target files its tree does not hold unchanged, whichever is larger.
  A source on a volume that cannot block clone into the destination is not charged.
  Admission counts existing workspace files (including ignored output) and crash staging files, and keeps pool workspaces charged after a take.
  Deletion frees their budget.
  These limits control Rosen's workspace creation; agents' later filesystem writes are counted on the next admission scan.
