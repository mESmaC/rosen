- **Run queue.**
  Heavy commands on one machine take turns, a few at a time, so they keep the cores busy and finish sooner than many running side by side.
  The queue is one folder per machine and user, with operating system file locks: it needs no daemon, and a crashed waiter or runner leaves the line when its process dies.
