- The default memory reserve is 20% of installed RAM or 4 GiB, whichever is higher.
  The queue keeps a minimum reserve of 4 GiB even when the machine overrides the default.
  On Windows available memory is the smaller of available physical RAM and free system commit, on Linux it is `MemAvailable`, and on macOS it is free plus inactive pages.
  The latest available memory sample must cover the reserve, all live turns' full memory budgets, and the incoming turn's budget.
  The same admission check requires enough unreserved logical cores for the new turn.
  An unavailable memory sample holds new turns until a reading is available, and running turns continue.
