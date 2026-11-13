import BatchGreen.Design

def main : IO Unit := do
  IO.println "green\ttested\tnext"
  for green in [0, 1, 3] do
    for tested in [1, 2, 3] do
      IO.println s!"{green}\t{tested}\t{BatchGreen.advance green tested}"

#print axioms main
