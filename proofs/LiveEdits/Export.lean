import LiveEdits.Design

def main : IO Unit := do
  IO.println "parent\thead\tsame_mode\tregular\tbinary\twhole"
  for p in [false, true] do
    for h in [false, true] do
      for m in [false, true] do
        for r in [false, true] do
          for b in [false, true] do
            IO.println s!"{p}\t{h}\t{m}\t{r}\t{b}\t{LiveEdits.whole p h m r b}"

#print axioms main
