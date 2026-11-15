- If the compare-and-swap loses a race, the server retries the land itself with backoff, with the mode and gates at the new base (in before-land mode, as long as no new receipt is needed).
  A land is never applied twice.
  Live attempts retry if the base already moved before preparing a layer, after preparing the clean stack, or before writing candidate commits.
  The transaction still rechecks the ref and every mutable input.
  After writing candidate commits, an attempt already on an old base retries before joining the SQLite writer wait.
