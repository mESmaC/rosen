- A turn passes to the processes it runs: they get `ROSEN_RUN_HELD` set to its class, and `rosen run` beneath one runs at once instead of waiting behind its own ancestor.
  A script that queues its own steps can therefore be run through `rosen run` whole, and a prepare or check command that calls `rosen run` never waits on the turn it is part of.
