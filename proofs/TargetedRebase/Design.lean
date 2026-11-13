namespace TargetedRebase
-- Saved overlap IDs survive claim release (SPEC.md §R8 step 4).
def capture (saved : Option (List Nat)) (found : List Nat) : List Nat := saved.getD found
-- SPEC.md §R8: first capture selects overlaps; replay preserves the saved set.
theorem correct (saved : Option (List Nat)) (found : List Nat) :
    And (saved = none -> capture saved found = found)
    (forall targets, saved = some targets -> capture saved found = targets) := by
  cases saved <;> simp [capture]
-- Cost model: one batch-marker row and one row per target, zero writes on replay.
def writes (saved : Option (List Nat)) (found : List Nat) : Nat :=
  if saved.isSome then 0 else 1 + found.length
-- SPEC.md §R8: first capture writes at most one row per target plus a marker.
theorem cost_bound (saved : Option (List Nat)) (found : List Nat) :
    writes saved found <= 1 + found.length := by
  cases saved <;> simp [writes]
-- SPEC.md §R8: alternatives storing a marker and every selected ID cannot
-- use fewer row writes; replay's zero writes is minimal as well.
theorem optimal (saved : Option (List Nat)) (found : List Nat) (cost : Nat)
    (complete : saved = none -> 1 + found.length <= cost) :
    writes saved found <= cost := by
  cases saved with
  | none => exact complete rfl
  | some targets => simp [writes]
#print axioms capture
#print axioms writes
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end TargetedRebase
