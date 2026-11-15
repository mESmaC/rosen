- Read-only landing lookups use bounded read-only WAL connections, separately from receipt and compare-and-swap writes.
  The compare-and-swap transaction still rechecks every mutable gate on the writer connection before moving the ref.
  In-memory ledgers retain their single connection.
