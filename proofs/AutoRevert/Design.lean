/-!
# Design proof: auto-revert (SPEC.md R12)
Atomic inverse land. The immutable object merge is the
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

/-- **correct** (SPEC.md R12 Auto-revert): exactly the successful inverse CAS reopens its culprit, records its
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

structure Attempt where
  state : State
  expected : Nat
  clean : Bool

/-- Each observation includes any competing land that preceded its CAS.
The loop actually calls apply, stops at success, and retries refusal up to cap.
Charging three units even for early refusal conservatively covers one inverse
merge, one immutable commit write and one CAS. -/
def retry (culprit change commit : Nat) (out : String) : Nat → List Attempt → Option State × Nat
  | 0, _ => (none, 0)
  | _, [] => (none, 0)
  | cap + 1, a :: rest =>
    match apply a.state a.expected culprit change commit a.clean out with
    | some t => (some t, 3)
    | none =>
      let next := retry culprit change commit out cap rest
      (next.1, 3 + next.2)

private theorem retry_bound (culprit change commit : Nat) (out : String)
    (cap : Nat) (observations : List Attempt) :
    (retry culprit change commit out cap observations).2 ≤ 3 * cap := by
  induction cap generalizing observations with
  | zero => simp [retry]
  | succ cap ih =>
    cases observations with
    | nil => simp [retry]
    | cons a rest =>
      simp only [retry]
      split
      · omega
      · have := ih rest
        dsimp
        omega

/-- **cost_bound**: under the merge/write/CAS cost model in retry, the actual
modeled loop, capped at 12 as in revert.rs, spends at most 36 operation units,
for all competing-base observations and all apply success/refusal outcomes. -/
theorem cost_bound (culprit change commit : Nat) (out : String)
    (observations : List Attempt) :
    (retry culprit change commit out 12 observations).2 ≤ 36 :=
  retry_bound culprit change commit out 12 observations

/-- Class of alternatives: finite sequences of durable atomic metadata
transactions. Each operation may transform the entire State, so the class
allows every atomic implementation, including ones with stronger primitives.
An empty sequence performs no durable writes. Cost is the sequence length. -/
def execute (operations : List (State → State)) (s : State) : State :=
  operations.foldl (fun state operation => operation state) s

def metadataCost (result : Option State) : Nat := if result.isSome then 1 else 0

/-- **optimal**: for every successful apply which moves the base, among all
sequences of durable metadata transactions achieving its R12 result, apply's
metadata cost is minimal. The singleton transaction attaining that cost is
explicitly constructed; correct supplies the full preservation postcondition. -/
theorem optimal (s t : State) (expected culprit change commit : Nat)
    (clean : Bool) (out : String)
    (success : apply s expected culprit change commit clean out = some t)
    (movesBase : commit ≠ s.base) (operations : List (State → State))
    (achieves : execute operations s = t) :
    metadataCost (apply s expected culprit change commit clean out) ≤ operations.length ∧
    execute [fun _ => t] s = t := by
  constructor
  · have target := (correct s expected culprit change commit clean out).2 t success
    cases operations with
    | nil =>
      have unchanged : s = t := achieves
      have same : commit = s.base := by rw [unchanged]; exact target.1.symm
      exact False.elim (movesBase same)
    | cons operation rest => simp [metadataCost, success]
  · rfl

#print axioms State
#print axioms gate
#print axioms apply
#print axioms Attempt
#print axioms retry
#print axioms retry_bound
#print axioms execute
#print axioms metadataCost
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end AutoRevert

