- `rosen run [--class <class>] [--jobs <n>] [--memory-gib <m>] [--full] -- <command>` waits for the turn, runs the command and exits with its status.
  The class defaults to `build`.
  `--jobs` and `--memory-gib` ask for a smaller turn than the class's full allocation; a missing jobs count takes the class's cap and missing memory takes one GiB per job plus one.
  By default it saves the command's standard output and standard error together in a unique log under the workspace's `.rosen/run-logs`, or under the system temporary directory outside a workspace.
  It prints the command, exit status and duration, followed on failure by any Cargo diagnostic blocks and Rust test failures with their output, or the last 40 lines when those details are unavailable.
  The summary ends with the log path.
  With `--full` or the global `--json` flag, the command inherits the caller's output streams directly.
  While waiting it reports its class, thread allocation, memory budget and priority alongside the runs ahead, plus current available memory, the reserve, the current reservations and whether memory is holding it back.
  After the first progress message it reports again only when the running commands or the number of waiting commands ahead change.
  After 5 minutes of waiting, and every 5 minutes after, it also says what holds it: a slot, more important runs, or the cores or memory it needs against what is available, the turns holding them and the waiter it keeps room for, reporting an idle stall (with any `reserve-gib` setting that would unblock it) when no turn is running.
  A checker takes a turn through `rosen-client` for the whole of one check.
  If the queue cannot grant admission because it cannot be opened or read, the command exits with an error.
