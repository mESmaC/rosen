- A less important run that is already going keeps its resource reservation until it finishes.
  A landing check can start beside a prepare when both fit the shared budget.
  On Windows, `landing` work runs at `NORMAL_PRIORITY_CLASS`, and every other class runs at `BELOW_NORMAL_PRIORITY_CLASS`.
  On other platforms, `landing` uses `nice -n 0`, `build` uses `nice -n 5`, and `verify` and `prepare` use `nice -n 10`.
  Children inherit the priority; a nested run keeps its ancestor's class and does not add niceness again.
