import StackedChanges.Design

-- Trees encode shared's whole-file value (0 absent, 1 original, 2 parent,
-- 3 child) plus 4 when the unrelated landed file is present. Whole-file
-- edits use R5's unchanged-side and identical-side rules, else conflict.
def mergeValue (base onto head : Nat) : Option Nat :=
  if onto == base then some head
  else if head == base || onto == head then some onto else none

def mergeTree (base onto head : Nat) : Option Nat := do
  let shared ← mergeValue (base % 4) (onto % 4) (head % 4)
  let unrelated ← mergeValue (base / 4) (onto / 4) (head / 4)
  return shared + 4 * unrelated

def node (basis head : Nat) : StackedChanges.Node := ⟨0, [], head, basis, false, false⟩

def main : IO Unit := do
  IO.println "closed\tparent_closed\tconflicted\tsame_basis\tselected"
  for c in [false, true] do
    for p in [false, true] do
      for f in [false, true] do
        for s in [false, true] do
          let bit := fun b : Bool => if b then "1" else "0"
          IO.println s!"{bit c}\t{bit p}\t{bit f}\t{bit s}\t{bit (StackedChanges.eligible c p f s)}"
  -- merge rows: name, current base, parent basis/head, child basis/head,
  -- result tree (- for conflict). #146's root B1=4 differs from H0=1.
  for (name, root, pb, ph, cb, ch) in [
      ("issue146-stale-conflict", 4, 4, 6, 1, 3),
      ("stale-clean-keeps-land", 4, 4, 5, 1, 3),
      ("parent-preview-keeps-land", 4, 0, 1, 1, 3),
      ("unchanged-basis", 4, 4, 5, 5, 7),
      ("no-intervening-land", 0, 0, 1, 1, 3),
      ("parent-conflict-stops", 7, 1, 2, 2, 3)] do
    let child := { node cb ch with conflicted := name == "issue146-stale-conflict" }
    let result := StackedChanges.mergeResult mergeTree root [node pb ph, child]
    let code := result.map toString |>.getD "-"
    IO.println s!"merge\t{name}\t{root}\t{pb}\t{ph}\t{cb}\t{ch}\t{code}"

#print axioms mergeValue
#print axioms mergeTree
#print axioms node
#print axioms main
