- `rosen run --slots` prints each class's count, thread allocation, memory budget and process priority on this machine, plus current available memory, live reservations and the reserve, then an idle stall notice if the head waiter has waited 5 minutes or more with no turn running (with any `reserve-gib` setting that would unblock it), then what each waiter held 5 minutes or more waits for; `rosen run --slots build=6,landing=3` sets this machine's own counts, and `CLASS=default` goes back to the default.
  `rosen run --slots reserve-gib=12` sets a whole-GiB reserve of at least 4 GiB.
  `reserve-gib=default` restores the default.
  Counts and reserve live in the queue folder, so they hold for every project and process of this user on the machine.
  Waiting runs see a change on their next look, and a lowered count stops no run already going.
