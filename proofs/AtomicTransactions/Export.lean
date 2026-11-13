import AtomicTransactions.Design
open AtomicTransactions
def main : IO Unit := do
  IO.println "duplicate\tbase_matches\tcommitted\tlanded"
  for duplicate in [false, true] do
    for baseMatches in [false, true] do
      for committed in [false, true] do
        let s : State := ⟨0, if duplicate then [1] else [], []⟩
        let result := publish s (if baseMatches then 0 else 9) 1 2 committed
        let bit := fun b : Bool => if b then "1" else "0"
        IO.println ("\t".intercalate [bit duplicate, bit baseMatches, bit committed,
          bit (result.base == 2)])

#print axioms main
