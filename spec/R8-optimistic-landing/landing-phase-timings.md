- Per-change request, decision, attempt and commit phase totals are always recorded on the server for `rosen stats`, as counts and durations only.
- Setting `ROSEN_LAND_TIMINGS` also logs those per-change phase timings to the server log.
  Timings separate registry reads, merge preparation, receipt validation, commit writes, compare-and-swap and the stack phase.
