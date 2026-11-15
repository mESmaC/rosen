- If the compare-and-swap loses a race, the server retries the land itself with backoff, with the mode and gates at the new base (in before-land mode, as long as no new receipt is needed).
  A land is never applied twice.
  Live attempts retry if the base already moved before preparing a layer, after preparing the clean stack, or before writing candidate commits.
  The transaction still rechecks the ref and every mutable input.
  After writing candidate commits, an attempt already on an old base retries before joining the SQLite writer wait.
  After the optimistic attempt budget (12 by default), the request joins a first-in-first-out queue for an exclusive retry turn instead of returning `Contended`.
  Queued retries wait for active optimistic attempts to finish and pause new attempts until the queue drains, including across engine configuration reloads.
  A zero budget queues immediately; the reported attempt count includes exclusive attempts and saturates at its wire type's maximum.
  Every exclusive attempt re-reads the base and mutable gates and uses the same compare-and-swap transaction; snapshot and server-revert races retry with backoff.
  The turn is released on a decision, error or unwind, before any post-land work.
