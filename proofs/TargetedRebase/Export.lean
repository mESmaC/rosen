import TargetedRebase.Design
open TargetedRebase
-- Row: stored selection (none, - for empty, or its target), the overlap query's
-- fresh target, and the target the capture selects (- for none).
def main : IO Unit := do
  for saved in [none, some [], some [1]] do
    let rows := match saved with | none => [] | some xs => encode xs
    let captured := capture (fun _ => [2]) ⟨[], rows, []⟩
    let code := fun xs : List Nat => if xs.isEmpty then "-" else toString xs.head!
    let cached := match saved with | none => "none" | some xs => code xs
    IO.println (cached ++ "\t2\t" ++ code ((decode captured.rows).getD []))
#print axioms main
