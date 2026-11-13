import ClaimRelease.Design
open ClaimRelease

/-- A holder's claim (record 0) and a waiter's wait (record 1); releasing the
claim removes both and owes the waiter's wake (event 1). `sent` is whether a
restart's recovery publishes the pending note. -/
def main : IO Unit := do
  IO.println "commit\tsent\tremoved\tpending"
  let st : Store := fun | .record _ => some [] | .note _ => none
  for commit in [false, true] do
    for sent in [false, true] do
      let released := release (start st) [0, 1] [1] commit
      let final := if sent then replay released else released
      let removed := (released.store (.record 0)).isNone
      let pending := (List.range final.next).countP fun n => (final.store (.note n)).isSome
      let bit := fun b : Bool => if b then 1 else 0
      IO.println s!"{bit commit}\t{bit sent}\t{bit removed}\t{pending}"

#print axioms main
