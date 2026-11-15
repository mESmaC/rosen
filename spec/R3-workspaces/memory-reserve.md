- Before granting any new turn, the queue also requires current free memory to be strictly above a **reserve**, by default 20% of RAM.
  On Windows this is free system commit (commit limit minus committed bytes), on Linux `MemAvailable`, and on macOS free plus inactive pages.
  At or below the reserve a turn waits as though its slot were busy, regardless of class or priority aging; running turns continue.
  This is admission based on a current sample, not a limit on a command's later allocations.
  If the OS cannot report memory, scheduling continues and the unavailable reading is shown.
