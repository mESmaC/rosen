- A waiter keeps room for the first waiter that precedes it and has a free slot of its class.
  It starts only when the live turns, that waiter's budget and its own budget together fit the shared CPU and memory budgets.
  A big turn blocked on memory then waits for running turns to drain, while smaller, later or less important turns that would push it further out of reach wait behind it.
  Waiters precede each other by class, then arrival, and a waiter past its patience ranks with `landing`, so a later arrival of any class keeps room for it.
  Room is kept only for a waiter that would fit once every live turn ends, so memory held outside the queue does not stall the line.
  Each ticket's file name carries the budget its caller asked for, so every process can charge it.
  A waiter held for 5 minutes or more writes what it waits for beside its ticket, for example `waiting for 15.2 GiB, 13.7 GiB available; prepare (3.0 GiB) holds a turn`.
