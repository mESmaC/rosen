- Default from a measured ramp (2026-11-14, one Windows 11 machine with 20 logical cores and 64 GiB, other agents' work running beside it): 1 to 4 clean `cargo build` runs at once, each through the queue in its own `build` slot and target folder.
  Rosen's workspace took 52.9, 63.1, 74.1 and 71.1 s of wall time, 1.0, 1.7, 2.1 and 3.0 times the work per second of one run alone; Velocity's (without the app crate) 43.3, 60.6, 89.8 and 104.8 s, 1.0, 1.4, 1.45 and 1.65 times.
  Throughput still rose at four for both, so that machine gets 4; free memory fell by under 2 GiB per added build, and never below 26 GiB.
  The ramp went no higher than 4, so the default claims nothing past it.
