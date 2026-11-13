import StackedChanges.Design

def main : IO Unit := do
  IO.println "closed\tparent_closed\tconflicted\tsame_basis\tselected"
  for c in [false, true] do
    for p in [false, true] do
      for f in [false, true] do
        for s in [false, true] do
          let bit := fun b : Bool => if b then "1" else "0"
          IO.println s!"{bit c}\t{bit p}\t{bit f}\t{bit s}\t{bit (StackedChanges.eligible c p f s)}"

#print axioms main
