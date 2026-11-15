- Each class has a number of **slots**, and that many of its runs go at once.
  `landing` and `build` get one slot per 5 logical cores or per 8 GiB of memory, whichever gives fewer, and never under 2; `verify` and `prepare` get 1.
  A machine can set its own count for any class (below).
  A run starts when its class has a slot free once the earlier waiters of its class have taken theirs, no more important run is going, and nothing more important is ahead of it in line.
