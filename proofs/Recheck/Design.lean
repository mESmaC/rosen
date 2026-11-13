/-! SPEC.md §2 Reach and R8 step 2: receipt reuse over a complete interval
of landed reaches, including lands subsequently reverted. Inputs are finite
sets of unit IDs represented by lists; repeated IDs do not affect safety. -/
namespace Recheck

def reusable (ours : List Nat) (lands : List (List Nat)) : Bool :=
  lands.all fun theirs => !(ours.any fun u => theirs.contains u)

/-- SPEC R8: reuse exactly when every intervening reach is disjoint. -/
theorem correct (ours : List Nat) (lands : List (List Nat)) :
    reusable ours lands = true ↔
      ∀ theirs ∈ lands, ∀ u ∈ ours, u ∉ theirs := by
  simp [reusable]

/-- Cost model: one unit equality probe for each candidate pair, without
early exit. Actual short-circuit scans use at most this many probes. -/
def probeCost (ours : List Nat) (lands : List (List Nat)) : Nat :=
  (lands.map fun theirs => ours.length * theirs.length).sum

theorem cost_bound (ours : List Nat) (lands : List (List Nat)) :
    probeCost ours lands ≤ ours.length * lands.flatten.length := by
  induction lands with
  | nil => simp [probeCost]
  | cons head tail ih =>
    simp only [probeCost, List.map_cons, List.sum_cons, List.flatten_cons,
      List.length_append, Nat.mul_add] at *
    omega

/-- Alternatives may reuse only when no unit is shared. Cost is the number
of fresh receipts required (zero for reuse, one otherwise). No sound policy
can reuse an interval our policy refuses. -/
theorem optimal (policy : List Nat → List (List Nat) → Bool)
    (sound : ∀ ours lands, policy ours lands = true →
      ∀ theirs ∈ lands, ∀ u ∈ ours, u ∉ theirs)
    (ours : List Nat) (lands : List (List Nat)) :
    (if reusable ours lands then 0 else 1 : Nat) ≤
      (if policy ours lands then 0 else 1 : Nat) := by
  by_cases hp : policy ours lands = true
  · have hr := (correct ours lands).mpr (sound ours lands hp)
    simp [hp, hr]
  · simp [hp]; split <;> omega

#print axioms reusable
#print axioms probeCost
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end Recheck
