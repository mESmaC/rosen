import AutoRevert.Design

def main : IO Unit := do
  IO.println "already\tmoved\tclean\tland"
  for already in [false, true] do
    for moved in [false, true] do
      for clean in [false, true] do
        let n (b : Bool) := if b then 1 else 0
        IO.println s!"{n already}\t{n moved}\t{n clean}\t{n (AutoRevert.gate already moved clean)}"
#print axioms main
