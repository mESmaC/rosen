import TargetedRebase.Design
open TargetedRebase
-- Row: cached target (or -), fresh target, selected target (or -).
def main : IO Unit := do
  for saved in [none, some [], some [1]] do
    let result := capture saved [2]
    let code := fun xs : List Nat => if xs.isEmpty then "-" else toString xs.head!
    let cached := match saved with | none => "none" | some xs => code xs
    IO.println (cached ++ "\t2\t" ++ code result)
#print axioms main
