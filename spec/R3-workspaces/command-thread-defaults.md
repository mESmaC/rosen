- Each turn gets at least one job and up to half of the machine's logical cores, rounded down, capped at two jobs for prepare and five for all other classes.
  The turn sets `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, and `LEAN_NUM_THREADS` to this allocation, and nested runs inherit it.
  The queue charges one GiB per job plus one GiB for other processes in the turn.
  A caller can ask for a smaller budget, of at least one job and one GiB, which the queue holds within the class's cap.
  The queue admits and charges that turn on the budget it asked for, records it so other waiters count it, and sets the thread variables to its jobs.
