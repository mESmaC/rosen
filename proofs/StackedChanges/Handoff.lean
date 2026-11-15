import StackedChanges.Design

-- #209: bits represent independent parent, child and intervening file edits.
-- Export the final child head from the proved bottom-to-top propagation pass.
def main : IO Unit := do
  IO.println "intervening\tchild_after_answer"
  for intervening in [4] do
    let parent : StackedChanges.Node := ⟨0, [0], 1 + intervening, 0, false, false⟩
    let child : StackedChanges.Node := ⟨1, [1], 3, 1, false, false⟩
    let merge := fun basis onto head => some (Nat.xor (Nat.xor head basis) onto)
    let result := StackedChanges.pass merge parent [child]
    let head := ((result.head?).map StackedChanges.Node.head).getD 0
    IO.println s!"{intervening}\t{head}"
