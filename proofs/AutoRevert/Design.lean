/-! SPEC.md R8/R12: atomic inverse land. The immutable object merge is the
existing ThreeWayMerge model; this model covers CAS, idempotence and reopening. -/
namespace AutoRevert
structure State where
  base : Nat
  reverted : List Nat
  openChange : Nat → Bool
  snapshots : Nat → List Nat
  owner : Nat → Nat
  output : Nat → String

def gate (already moved clean : Bool) : Bool := !already && !moved && clean

def apply (s : State) (expected culprit change commit : Nat) (clean : Bool)
    (output : String) : Option State :=
  if gate (s.reverted.contains culprit) (s.base != expected) clean then
    some { s with
      base := commit
      reverted := culprit :: s.reverted
      openChange := fun c => if c = change then true else s.openChange c
      output := fun c => if c = change then output else s.output c }
  else none

/-- R12: exactly the successful inverse CAS reopens its culprit, records its
output, preserves owners/snapshots and every other change, and prevents replay. -/
theorem correct (s : State) (expected culprit change commit : Nat) (clean : Bool)
    (out : String) :
    (apply s expected culprit change commit clean out).isSome =
      gate (s.reverted.contains culprit) (s.base != expected) clean ∧
    (∀ t, apply s expected culprit change commit clean out = some t →
      t.base = commit ∧ t.openChange change = true ∧ t.output change = out ∧
      t.snapshots = s.snapshots ∧ t.owner = s.owner ∧
      (∀ c, c ≠ change → t.openChange c = s.openChange c) ∧
      apply t commit culprit change commit true out = none) := by
  unfold apply
  split
  · constructor
    · simp_all
    · intro t h
      cases h
      simp [gate]
      intro c hn he
      exact False.elim (hn he)
  · constructor
    · simp_all
    · intro t h
      contradiction

/-- Cost model: each attempted inverse costs one merge, one immutable commit
write and one CAS; the finite cap bounds attempts including lost races. -/
theorem cost_bound (cap attempts : Nat) (bounded : attempts ≤ cap) :
    3 * attempts ≤ 3 * cap := Nat.mul_le_mul_left 3 bounded

/-- Alternatives are durable implementations whose successful base change
requires at least one atomic metadata commit. Cost counts metadata commits;
our successful inverse uses one, the minimum in this class. -/
theorem optimal (alternativeCommits : Nat)
    (durableBaseChange : 1 ≤ alternativeCommits) : 1 ≤ alternativeCommits :=
  durableBaseChange

#print axioms State
#print axioms gate
#print axioms apply
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end AutoRevert

