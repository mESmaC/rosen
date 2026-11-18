- Each class has a maximum number of live turns, called slots, and all classes share a single admission budget for logical cores and memory.
  The default for `landing` and `build` is one slot per five logical cores, rounded up, and the default for `verify` and `prepare` is one.
  A machine can set its own count for any class.
  A run starts when its class has room after earlier waiters of its class have taken their slots, it is entitled to start under the priority rules, and its reservation fits the shared budgets.
  The queue takes one admission lock across classes while it counts live reservations and grants a new turn.
