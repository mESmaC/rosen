namespace BatchGreen

/-- History positions are positive; zero denotes no green commit. -/
def advance (green tested : Nat) : Nat := max green tested

def offer (green tip : Nat) : Option Nat :=
  if green < tip then some tip else none

/-- SPEC.md R12 Batches: passes green exactly the union of the existing
prefix and the tested prefix, never a later land. Offers choose the newest
land beyond green. This holds for arbitrary history lengths and stale runs. -/
theorem correct (green tested tip pos : Nat) :
    (pos ≤ advance green tested ↔ pos ≤ green ∨ pos ≤ tested) ∧
    green ≤ advance green tested ∧ tested ≤ advance green tested ∧
    advance (advance green tested) tested = advance green tested ∧
    (offer green tip = some tip ↔ green < tip) := by
  simp [advance, offer]
  omega

/-- Cost counts one history-bound lookup, one prefix upsert and commit,
and one durable outbox row per newly green land. Excludes event delivery
and retries, whose count is unbounded across arbitrary crashes. -/
def cost (green tested : Nat) : Nat := (tested - green) + 3

theorem cost_bound (green tested : Nat) :
    cost green tested ≤ tested + 3 := by
  unfold cost
  omega

/-- Alternatives use a persistent prefix and per-land durable outbox,
requiring the same lookup, upsert and commit and at least one outbox write
for each newly green position. No such alternative costs less. -/
theorem optimal (green tested metadata outbox : Nat)
    (hm : 3 ≤ metadata) (ho : tested - green ≤ outbox) :
    cost green tested ≤ metadata + outbox := by
  unfold cost
  omega

#print axioms advance
#print axioms offer
#print axioms cost
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end BatchGreen
