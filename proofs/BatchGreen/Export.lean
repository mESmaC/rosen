import BatchGreen.Design

def main : IO Unit := do
  IO.println "green\ttested\tnext"
  for green in [0, 1, 3] do
    for tested in [1, 2, 3] do
      IO.println s!"{green}\t{tested}\t{BatchGreen.advance green tested}"
  for mirrors in [[2], [2, 3], [1, 2, 3]] do
    let rows := [1, 2, 3].map fun pos => if mirrors.contains pos then none else some pos
    let offered := (BatchGreen.offer 0 (BatchGreen.latestLanded rows)).getD 0
    IO.println s!"offer\t{String.intercalate "," (mirrors.map toString)}\t{offered}"

#print axioms main
