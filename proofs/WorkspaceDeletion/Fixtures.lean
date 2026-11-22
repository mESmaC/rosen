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

/-- Mix the two forms of ignored build output with genuinely unstored paths. -/
def reportRow (scratchCount : Nat) : String :=
  let caches := ["proofs/.lake/output.bin", "configured-cache/output.bin"]
  let scratch := (List.range scratchCount).map fun i =>
    if i < 10 then s!"scratch/edit-0{i}" else s!"scratch/edit-{i}"
  let observed : Evidence String := ⟨[], [], caches ++ scratch, caches.contains⟩
  let (count, sample) := refusalSummary (pending observed [])
  let qualifier := if count > sample.length then "e.g. " else ""
  let diagnostic := s!"unstored files ({count}): {qualifier}{", ".intercalate sample}"
  "\t".intercalate ["report", toString scratchCount, diagnostic]

def table : String :=
  "dirty\tjournal\tmismatch\tignored\tdelete\n" ++
    "#375 regression: a durable Unix workspace (0 0 0 0) must be removable\n" ++
    "#388 regression: ignored junction output never requires storing its target\n" ++
    row false false true true ++ "\n" ++
    "#413 regression: ignored build output and unignored scratch have different durability requirements\n" ++
    row false false true true ++ "\n" ++
    row false false true false ++ "\n" ++
    String.join ([false, true].flatMap fun d => [false, true].flatMap fun j =>
      [false, true].flatMap fun m => [false, true].map fun i => row d j m i ++ "\n") ++
    "#413 regression: report rows give the scratch count and exact refusal text\n" ++
    reportRow 12 ++ "\n"

#print axioms bit
#print axioms evidence
#print axioms row
#print axioms reportRow
#print axioms table

end WorkspaceDeletion.Fixtures

def main : IO Unit := IO.print WorkspaceDeletion.Fixtures.table
