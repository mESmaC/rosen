- The caller first flushes a final snapshot, then applies the rebased files, including any remaining conflict markers (R5).
  If another writer advances the workspace basis while the target tree is fetched, application refuses and the owner retries.
