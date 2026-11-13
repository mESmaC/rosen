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

#print axioms main
