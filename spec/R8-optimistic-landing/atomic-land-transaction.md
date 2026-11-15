- The ref move, every layer's landed-history insertion and every registry transition to `landed` commit in one SQLite transaction.
  The transaction rechecks every layer's head, state, parent, remembered merge basis, conflicts and required approval; a missing or moved input retries the attempt.
  A failed layer never lands only a prefix.
  Snapshots after that commit are refused, including before post-land recovery finishes.
