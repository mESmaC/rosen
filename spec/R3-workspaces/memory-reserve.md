- The default memory reserve is 20% of installed RAM or 4 GiB, whichever is higher.
  The queue keeps a minimum reserve of 4 GiB even when the machine overrides the default.
  On Windows available memory takes the larger of available physical RAM and system cache (clamped to installed physical RAM) capped by free system commit, on Linux it is `MemAvailable`, and on macOS it is free plus inactive pages.
  The latest available memory sample must cover the reserve, all live turns' full memory budgets, the budget of the waiter it keeps room for under head-of-line reservation, and the incoming turn's budget.
  When no turn is running, the head waiter is admitted against the 4 GiB floor instead of a larger reserve, so the reserve throttles concurrency without deadlocking an idle machine.
  The same admission check requires enough unreserved logical cores for the new turn.
  An unavailable memory sample holds new turns until a reading is available, and running turns continue.
