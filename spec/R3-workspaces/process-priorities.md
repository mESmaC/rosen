- A less important run that is already going is never stopped, and never holds a more important one back.
  With enough free memory, a landing check starts at once beside a prepare.
  On Windows, `rosen run` starts `landing` commands at `NORMAL_PRIORITY_CLASS`, `build` at `BELOW_NORMAL_PRIORITY_CLASS`, and `verify` and `prepare` at `IDLE_PRIORITY_CLASS`.
  Elsewhere it uses `nice -n 0`, `5` or `10` respectively.
  Children inherit the priority; a nested run keeps its ancestor's class and does not add niceness again.
