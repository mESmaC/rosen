namespace StackedChanges

def eligible (closed parentClosed conflicted sameBasis : Bool) : Bool :=
  !closed && !parentClosed && !conflicted && !sameBasis

structure Edge where
  parent : Nat
  basis : Nat
  head : Nat
  commits : List Nat
  deriving DecidableEq

-- A clean merge replaces the live tree and remembered parent tree together.
-- Authored commits and the dependency identity are immutable (SPEC R2, R8).
def advance (e : Edge) (parentTree mergedTree : Nat) : Edge :=
  { e with basis := parentTree, head := mergedTree }

-- SPEC.md R8: exactly live, conflict-free edges with a changed parent are
-- selected. Their clean transition preserves history and stack order.
theorem correct (c p f s : Bool) (e : Edge) (onto merged : Nat) :
    (eligible c p f s = true ↔ c = false ∧ p = false ∧ f = false ∧ s = false) ∧
    (advance e onto merged).parent = e.parent ∧
    (advance e onto merged).commits = e.commits ∧
    (advance e onto merged).basis = onto ∧ (advance e onto merged).head = merged := by
  cases c <;> cases p <;> cases f <;> cases s <;> simp [eligible, advance]

-- Cost model: changes-row and basis-row writes for one successful edge transition.
-- A normalized changes row and separate basis row are written once each;
-- Immutable objects, notification outbox writes and merge CPU are outside this model.
def writes (selected : Bool) : Nat := if selected then 2 else 0

theorem cost_bound (selected : Bool) : writes selected ≤ 2 := by
  cases selected <;> decide

-- Class: alternatives using the same two separately addressed durable rows,
-- each requiring a write when selected. Replay without a change is free.
theorem optimal (selected : Bool) (changeWrites basisWrites : Nat)
    (changeRequired : selected = true → 1 ≤ changeWrites)
    (basisRequired : selected = true → 1 ≤ basisWrites) :
    writes selected ≤ changeWrites + basisWrites := by
  cases selected with
  | false => simp [writes]
  | true =>
    have hc := changeRequired rfl
    have hb := basisRequired rfl
    simp only [writes, ite_true]
    omega

#print axioms eligible
#print axioms Edge
#print axioms advance
#print axioms writes
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end StackedChanges
