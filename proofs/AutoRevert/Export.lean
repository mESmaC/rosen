import AutoRevert.Design

def main : IO Unit := do
  IO.println "already\tmoved\tclean\trecovered\tland"
  for already in [false, true] do
    for moved in [false, true] do
      for clean in [false, true] do
        for recovered in [false, true] do
          let n (b : Bool) := if b then 1 else 0
          IO.println s!"{n already}\t{n moved}\t{n clean}\t{n recovered}\t{n (AutoRevert.readyGate already moved clean recovered)}"
#print axioms main
