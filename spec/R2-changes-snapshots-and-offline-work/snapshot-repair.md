- Before acknowledging a snapshot, including an unchanged tree, the server checks its objects and returns missing or damaged IDs as repair hints.
  The client resends matching prepared or workspace bytes, including unchanged blobs, then retries.
  Recovered bytes stay with a pending journal record if sending or replay fails.
  Newer workspace bytes are never substituted for an older ID; unavailable repair bytes leave an error and the path is read again on the next attempt.
  Large-object verification and repair stream their bytes (#363).
