import Bisection.Design
open Bisection

def main : IO Unit := do
  IO.println "low\thigh\tculprit\tmidpoint\tnext_low\tnext_high"
  for lo in List.range 3 do
    for width in List.range 16 do
      let hi := lo + width + 2
      for offset in List.range (hi - lo) do
        let culprit := lo + offset + 1
        let next := step lo hi culprit
        IO.println s!"{lo}\t{hi}\t{culprit}\t{midpoint lo hi}\t{next.1}\t{next.2}"
  -- #373: a recorded culprit at 3 bounds a later failure at 5, after repair at 4.
  let next := step 3 5 5
  IO.println s!"3\t5\t5\t{midpoint 3 5}\t{next.1}\t{next.2}"
  for (lo, hi, mirrors) in [(0, 5, [2, 4]), (1, 4, [2, 3]), (2, 5, [3])] do
    let rows := (List.range (hi - lo)).map fun offset =>
      let commit := lo + offset + 1
      (commit, mirrors.contains commit)
    let cell := fun (ids : List Nat) => String.intercalate "," (ids.map toString)
    IO.println s!"mirror\t{lo}\t{hi}\t{cell mirrors}\t{cell (landedInterval lo rows)}"

#print axioms main
