- On Windows, `rosen run` and warm-pool prepare launch each command inside a private, non-breakaway Job Object before any command code can execute.
  The job caps total committed memory of the process tree at the turn's memory budget.
  The supervisor keeps the turn until the launcher and every descendant have exited, even if the original command exits first.
  A failed launcher or supervisor error terminates and drains the remaining tree before the turn is released.
  A forced supervisor exit closes the non-inherited, kill-on-close job handle and stops descendants.
  If containment cannot be established, the command does not start.
  Nested runs stay inside their ancestor's job.
  When using `rosen run`, standard input and output stay attached to the caller without requiring a PTY.
