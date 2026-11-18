- `rosen run [--class <class>] -- <command>` waits for the turn, runs the command and exits with its status; the class defaults to `build`.
  While waiting it prints its class, thread allocation, memory budget and priority alongside the runs ahead, plus current available memory, the reserve, the current reservations and whether memory is holding it back.
  A checker takes a turn through `rosen-client` for the whole of one check.
  If the queue cannot grant admission because it cannot be opened or read, the command exits with an error.
