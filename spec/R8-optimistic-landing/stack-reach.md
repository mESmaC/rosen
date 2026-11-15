- in `reach` mode, reach covers all stack layers; build units and workspace-wide input globs are read from the current base, so a change cannot widen its own independence.
  Reach compares the base with the clean merge result, and each intervening land with its parent, using §2's configuration-key and Cargo dependency rules.
  A dependency the configuration misses is caught after landing by post-land verification (R12), with reversion following the project's review mode and failure kind;
