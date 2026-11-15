- Prepare commands run for one workspace at a time per machine, at below-normal priority, so agents' own builds go first.
  A machine runs one daemon per project, so each prepare takes a turn in the machine's run queue, in its last class (Run queue below).
