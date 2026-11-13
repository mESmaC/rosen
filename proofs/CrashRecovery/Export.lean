import CrashRecovery.Design
open CrashRecovery

/-- Lands `a` (position 1) and `b` (2) are on the base and the overlapping change
sits on `b`; `a`'s post-land work may be pending. The server dies at `crash` while
landing `c` and restarts; recovery replays, and the requester asks again. -/
def row (aPending : Bool) (crash : Nat) : String :=
  let w : World := ⟨[⟨2, 2⟩, ⟨1, 1⟩], if aPending then [⟨1, 1⟩] else [], 2⟩
  let v := crashAt w 3 crash
  let r := recover v v.pending
  let final := restart w 3 crash
  let name := fun (t : Nat) => if t == 1 then "a" else if t == 2 then "b" else "c"
  "\t".intercalate [if aPending then "1" else "0", toString crash, name r.target,
    if landed r 3 then "already_landed" else "landed",
    toString (final.history.filter (·.change == 3)).length, toString final.pending.length,
    name final.target]

def main : IO Unit := do
  IO.println "a_pending\tcrash\ttarget_after_recovery\tretry\tc_in_history\tpending_after\ttarget_after"
  for aPending in [false, true] do
    for crash in [0, 1, 2, 3] do
      IO.println (row aPending crash)

#print axioms row
#print axioms main
