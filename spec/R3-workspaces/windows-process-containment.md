- On Windows, `rosen run` creates its command launcher inside a private, non-breakaway Job Object before it can execute.
  The supervising client keeps the queue turn until the launcher and every descendant have exited, even if the original command exits first.
  A failed launcher or a client error terminates and drains the remaining tree before the turn is released; a forced client exit closes the non-inherited, kill-on-close job handle and stops its descendants.
  If containment cannot be established, the command does not start.
  This also applies to nested runs and when the queue is unavailable.
  Input and output stay attached to the caller without requiring a PTY.
