- Read-only landing lookups use bounded read-only WAL connections, separately from receipt and compare-and-swap writes.
  The compare-and-swap transaction still rechecks every mutable gate on the writer connection before moving the ref.
  Registry facts, approval lookups and child selection also use bounded read-only WAL connections rather than waiting behind the registry writer mutex.
  Each registry lookup reads one consistent SQLite snapshot.
  In-memory ledgers and registries retain their single connection.
