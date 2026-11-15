- `rosen run [--class <class>] -- <command>` waits for the turn, runs the command and exits with its status; the class defaults to `build`.
  While waiting it prints its class, thread settings and priority alongside the runs ahead, plus current free memory, the reserve and whether memory is holding it back.
  A checker takes a turn through `rosen-client` for the whole of one check.
  The queue only schedules: a caller that cannot use it runs its command anyway.
