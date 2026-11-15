- Correctness tests wait for the snapshot with a generous hang timeout and verify its stored bytes and ignored paths.
  The 2 s write-through budget is measured by the stress benchmark, not asserted against wall-clock time in unit or integration tests running under build load (#371).
