- Each turn gets at least one job and up to half of the machine's logical cores, rounded down, capped at two jobs for prepare and five for all other classes.
  The turn sets `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, and `LEAN_NUM_THREADS` to this allocation, and nested runs inherit it.
  The queue charges one GiB per job plus one GiB for other processes in the turn.
