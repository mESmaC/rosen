- Commands started by `rosen run` default `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS` and `LEAN_NUM_THREADS` independently to `max(2, logical cores / heavy slots)`, rounding division down.
  Heavy slots are the machine's configured `landing` plus `build` counts.
  Each value the caller set is preserved; defaults are calculated when the command starts.
