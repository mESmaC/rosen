import ClaimRelease.Design

def main : IO Unit := do
  IO.println "commit\tsent\tremoved\tpending"
  for commit in [false, true] do
    for sent in [false, true] do
      let released := ClaimRelease.release commit .waiting
      let final := ClaimRelease.publish sent released
      IO.println s!"{if commit then 1 else 0}\t{if sent then 1 else 0}\t{if released != .waiting then 1 else 0}\t{if final == .pending then 1 else 0}"

#print axioms main
