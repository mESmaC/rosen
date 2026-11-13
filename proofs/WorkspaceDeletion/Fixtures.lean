import WorkspaceDeletion.Design

namespace WorkspaceDeletion.Fixtures

def bit (b : Bool) : String := if b then "1" else "0"

/-- Edit versions 0/1/2 witness dirty writes, journal entries and disk
mismatches independently. Only disk version 2 can be ignored. -/
def evidence (dirty journal mismatch ignored : Bool) : Evidence Nat :=
  { dirty := if dirty then [0] else []
    journal := if journal then [1] else []
    disk := if mismatch then [2] else []
    ignored := fun edit => ignored && edit == 2 }

def row (dirty journal mismatch ignored : Bool) : String :=
  let mayDelete := (pending (evidence dirty journal mismatch ignored) []).isEmpty
  "\t".intercalate [bit dirty, bit journal, bit mismatch, bit ignored, bit mayDelete]

def table : String :=
  "dirty\tjournal\tmismatch\tignored\tdelete\n" ++
    String.join ([false, true].flatMap fun d => [false, true].flatMap fun j =>
      [false, true].flatMap fun m => [false, true].map fun i => row d j m i ++ "\n")

#print axioms bit
#print axioms evidence
#print axioms row
#print axioms table

end WorkspaceDeletion.Fixtures

def main : IO Unit := IO.print WorkspaceDeletion.Fixtures.table
