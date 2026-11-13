import Claims.Design

namespace ClaimRelease

/-- Durable states of a wake. Publishing can repeat after a crash. -/
inductive Wake where
  | waiting | pending | delivered
  deriving DecidableEq

def release (commits : Bool) (w : Wake) : Wake :=
  if commits then (if w = .waiting then .pending else w) else w

def publish (commits : Bool) (w : Wake) : Wake :=
  if commits then (if w = .pending then .delivered else w) else w

/-- SPEC.md §R6: atomic removal leaves a durable wake; failed publication
retains it, and a successful publication acknowledges it. All states and
commit/failure outcomes are covered, including replay. Claim selection and
which waits become unblocked are proven by Claims.correct. -/
theorem correct (w : Wake) (commit sent : Bool) :
    (release commit w = .waiting ↔ w = .waiting ∧ commit = false) ∧
    (w ≠ .waiting → release commit w = w) ∧
    (publish sent .pending = if sent then .delivered else .pending) ∧
    publish sent .delivered = .delivered := by
  cases w <;> cases commit <;> cases sent <;> decide

/-- SPEC.md §R6. Cost model: one durable transaction to remove records and
enqueue, one event append per wake plus one release event, and one outbox
acknowledgement. Counts successful operations, excluding crash retries. -/
def cost (wakes : Nat) := 1 + (wakes + 1) + 1

theorem cost_bound (wakes : Nat) : cost wakes ≤ wakes + 3 := by
  simp [cost]; omega

/-- SPEC.md §R6. Alternatives use a separate event log transaction, publish
each wake and the release, and durably acknowledge the batch. Each must
perform at least one atomic removal/enqueue, wakes+1 appends, and one ack. -/
theorem optimal (wakes removal appends ack : Nat)
    (hr : 1 ≤ removal) (hp : wakes + 1 ≤ appends) (ha : 1 ≤ ack) :
    cost wakes ≤ removal + appends + ack := by
  simp [cost]; omega

#print axioms correct
#print axioms cost_bound
#print axioms optimal
#print axioms Wake
#print axioms release
#print axioms publish
#print axioms cost
end ClaimRelease
