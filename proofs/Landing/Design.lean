/-!
# Design proof: optimistic landing (SPEC.md R8)
Models the land decision (`crates/rosen-server/src/landing/rules.rs`) and the
compare-and-swap of the base ref (`ledger.rs`), and proves:
* `correct`: a land moves the base exactly when SPEC.md R8 step 2 allows it,
  from the old base to the landed commit, and never twice for one change;
* `cost_bound`: under the stated cost model, a land costs at most one attempt
  per competing land that beat it, plus one, and never more than the cap;
* `optimal`: among sound re-check policies, ours asks for the fewest new
  receipts on every request history.
`fixtureTable` is exported to `proofs/fixtures/landing.tsv`, which the Rust
test `landing_fixtures` checks the implementation against.
-/
namespace Landing
/-- `active` covers both `open` and `ready`: the decision treats them alike. -/
inductive ChangeState where
  | active | landed | abandoned
  deriving DecidableEq, Repr
inductive Tier where
  | prose | normal | sensitive
  deriving DecidableEq, Repr
inductive ReceiptCheck where
  | missing | invalid | valid
  deriving DecidableEq, Repr
structure Input where
  state : ChangeState
  parentLanded : Bool
  hasConflicts : Bool
  approved : Bool
  tier : Tier
  receipt : ReceiptCheck
  baseMoved : Bool
  landedSinceAllIndependent : Bool
  mergeClean : Bool
  deriving DecidableEq, Repr
inductive Refusal where
  | alreadyLanded | abandoned | parentUnlanded | conflicts | unapproved
  | noReceipt | receiptStale | mergeConflict | needsRecheck
  deriving DecidableEq, Repr
inductive Verdict where
  | land
  | refuse (r : Refusal)
  deriving DecidableEq, Repr
/-- Checks that do not depend on the merge result, in refusal order. -/
def gate (i : Input) : Option Refusal :=
  match i.state with
  | .landed => some .alreadyLanded
  | .abandoned => some .abandoned
  | .active =>
    if !i.parentLanded then some .parentUnlanded
    else if i.hasConflicts then some .conflicts
    else if !i.approved then some .unapproved
    else if i.tier = .prose then none
    else match i.receipt with
      | .missing => some .noReceipt
      | .invalid => some .receiptStale
      | .valid => none
def decideLand (i : Input) : Verdict :=
  match gate i with
  | some r => .refuse r
  | none =>
    if !i.mergeClean then .refuse .mergeConflict
    else if i.tier = .prose ∨ !i.baseMoved ∨ i.landedSinceAllIndependent then .land
    else .refuse .needsRecheck
/-- SPEC.md R8 step 2, with SPEC.md R7 approval and SPEC.md R8 step 3 stack
order: the change is live, its parent has landed, it has no stored
conflicts, its current head is approved, it merges cleanly onto the current
base, and it is prose-only or holds a valid receipt that the re-check rules
let count on this base. -/
def Authorized (i : Input) : Prop :=
  i.state = .active ∧ i.parentLanded = true ∧ i.hasConflicts = false ∧
  i.approved = true ∧ i.mergeClean = true ∧
  (i.tier = .prose ∨
    (i.receipt = .valid ∧ (i.baseMoved = false ∨ i.landedSinceAllIndependent = true)))
instance : DecidablePred Authorized := fun i => by
  unfold Authorized; infer_instance
/-- The metadata store as landing sees it: the base ref and the landed
`(change, commit)` pairs, newest first. -/
structure Ledger where
  base : Nat
  landed : List (Nat × Nat)

def Ledger.hasLanded (l : Ledger) (change : Nat) : Bool :=
  l.landed.any (·.1 == change)

/-- One SQLite transaction: refuse a second land of a change, else move the
base only if it still equals `expected`. -/
def cas (l : Ledger) (expected change commit : Nat) : Option Ledger :=
  if l.hasLanded change then none
  else if l.base = expected then some ⟨commit, (change, commit) :: l.landed⟩
  else none

def landStep (l : Ledger) (i : Input) (expected change commit : Nat) : Option Ledger :=
  match decideLand i with
  | .land => cas l expected change commit
  | .refuse _ => none

private theorem decideLand_eq_land (i : Input) : decideLand i = .land ↔ Authorized i := by
  obtain ⟨s, p, c, a, t, r, b, q, m⟩ := i
  cases s <;> cases p <;> cases c <;> cases a <;> cases t <;> cases r <;>
    cases b <;> cases q <;> cases m <;> decide

/-- **correct** (SPEC.md R8 "Optimistic landing", step 2, with SPEC.md R7
approval and SPEC.md R8 step 3 stack order): a land step succeeds
only when the land is authorized, the base still equals the expected commit,
and the change has not landed; it then moves the base to the landed commit
and records the land. Conversely every such land succeeds. -/
-- SPEC.md §R8
theorem correct (l : Ledger) (i : Input) (expected change commit : Nat) :
    (∀ l', landStep l i expected change commit = some l' →
      Authorized i ∧ expected = l.base ∧ l.hasLanded change = false ∧
      l' = ⟨commit, (change, commit) :: l.landed⟩) ∧
    (Authorized i → expected = l.base → l.hasLanded change = false →
      landStep l i expected change commit = some ⟨commit, (change, commit) :: l.landed⟩) := by
  constructor
  · intro l' h
    unfold landStep at h
    split at h
    · rename_i hv
      unfold cas at h
      split at h
      · contradiction
      · split at h
        · rename_i hl hb
          cases h
          exact ⟨(decideLand_eq_land i).mp hv, hb.symm, by simpa using hl, rfl⟩
        · contradiction
    · contradiction
  · intro ha hb hl
    have hv := (decideLand_eq_land i).mpr ha
    simp [landStep, hv, cas, hl, hb]

/-- Cost model: one attempt costs one merge, one commit write and one
compare-and-swap, and cost counts attempts. `races[j]` is true when a
competing land moved the base during attempt `j`, so its swap lost and the
loop retries; `cap` is `max_attempts`. -/
def attempts : Nat → List Bool → Nat
  | 0, _ => 0
  | _ + 1, [] => 1
  | _ + 1, false :: _ => 1
  | n + 1, true :: rest => 1 + attempts n rest

/-- **cost_bound**: under the cost model of `attempts`, a land never costs
more than `cap` attempts, nor more than one attempt per competing land that
beat it plus one. -/
-- SPEC.md §R8
theorem cost_bound (cap : Nat) (races : List Bool) :
    attempts cap races ≤ cap ∧ attempts cap races ≤ races.count true + 1 := by
  induction cap generalizing races with
  | zero => simp [attempts]
  | succ n ih =>
    cases races with
    | nil => simp [attempts]
    | cons race rest =>
      cases race
      · simp [attempts]
      · have := ih rest
        simp [attempts]
        omega

/-- A re-check policy says, for a land request, whether it must wait for a
new receipt. Its cost on a request history is how many requests wait. -/
def recheckCost (policy : Input → Bool) (requests : List Input) : Nat :=
  requests.countP policy

/-- Our policy: exactly the requests `decideLand` refuses with `needsRecheck`. -/
def oursRecheck (i : Input) : Bool :=
  decideLand i == .refuse .needsRecheck

/-- The class of alternatives: policies that never let a receipt count across
a dependent land for a non-prose change that otherwise passes every check.
Reusing it there can land code no check built (the struct literal case in R8). -/
def SoundPolicy (policy : Input → Bool) : Prop :=
  ∀ i, gate i = none → i.mergeClean = true → i.tier ≠ .prose →
    i.baseMoved = true → i.landedSinceAllIndependent = false → policy i = true

private theorem oursRecheck_unsafe (i : Input) (h : oursRecheck i = true) :
    gate i = none ∧ i.mergeClean = true ∧ i.tier ≠ .prose ∧
      i.baseMoved = true ∧ i.landedSinceAllIndependent = false := by
  obtain ⟨s, p, c, a, t, r, b, q, m⟩ := i
  revert h
  cases s <;> cases p <;> cases c <;> cases a <;> cases t <;> cases r <;>
    cases b <;> cases q <;> cases m <;> decide

private theorem countP_le_of_imp (p q : Input → Bool) (hpq : ∀ i, p i = true → q i = true) :
    ∀ l : List Input, l.countP p ≤ l.countP q
  | [] => by simp
  | x :: xs => by
    have ih := countP_le_of_imp p q hpq xs
    by_cases hp : p x = true
    · simp [hp, hpq x hp]; omega
    · simp only [List.countP_cons, Bool.not_eq_true] at hp ⊢
      simp [hp]; split <;> omega

/-- **optimal**: our re-check policy is sound, and no sound policy asks for
fewer new receipts on any history of land requests. -/
-- SPEC.md §R8
theorem optimal :
    SoundPolicy oursRecheck ∧
    ∀ policy, SoundPolicy policy → ∀ requests : List Input,
      recheckCost oursRecheck requests ≤ recheckCost policy requests := by
  constructor
  · intro i hg hm ht hb hq
    obtain ⟨s, p, c, a, t, r, b, q, m⟩ := i
    simp only at hg hm ht hb hq
    subst hm hb hq
    simp [oursRecheck, decideLand, hg, ht]
  · intro policy hp requests
    apply countP_le_of_imp
    intro i hi
    obtain ⟨hg, hm, ht, hb, hq⟩ := oursRecheck_unsafe i hi
    exact hp i hg hm ht hb hq

/-! ## Fixture table -/

def bit (b : Bool) : String := if b then "1" else "0"

def ChangeState.code : ChangeState → String
  | .active => "active" | .landed => "landed" | .abandoned => "abandoned"

def Tier.code : Tier → String
  | .prose => "prose" | .normal => "normal" | .sensitive => "sensitive"

def ReceiptCheck.code : ReceiptCheck → String
  | .missing => "missing" | .invalid => "invalid" | .valid => "valid"

def Verdict.code : Verdict → String
  | .land => "land"
  | .refuse .alreadyLanded => "already_landed"
  | .refuse .abandoned => "abandoned"
  | .refuse .parentUnlanded => "parent_unlanded"
  | .refuse .conflicts => "conflicts"
  | .refuse .unapproved => "unapproved"
  | .refuse .noReceipt => "no_receipt"
  | .refuse .receiptStale => "receipt_stale"
  | .refuse .mergeConflict => "merge_conflict"
  | .refuse .needsRecheck => "needs_recheck"

/-- Every input, except that `landedSinceAllIndependent` is pinned to true when the
base has not moved: nothing landed since B, so the flag is vacuously true. -/
def allInputs : List Input :=
  [ChangeState.active, .landed, .abandoned].flatMap fun s =>
  [false, true].flatMap fun p =>
  [false, true].flatMap fun c =>
  [false, true].flatMap fun a =>
  [Tier.prose, .normal, .sensitive].flatMap fun t =>
  [ReceiptCheck.missing, .invalid, .valid].flatMap fun r =>
  [(false, true), (true, false), (true, true)].flatMap fun (b, q) =>
  [false, true].map fun m => ⟨s, p, c, a, t, r, b, q, m⟩

def fixtureRow (i : Input) : String :=
  "\t".intercalate [i.state.code, bit i.parentLanded, bit i.hasConflicts,
    bit i.approved, i.tier.code, i.receipt.code, bit i.baseMoved,
    bit i.landedSinceAllIndependent, bit i.mergeClean, (decideLand i).code]

def fixtureTable : String :=
  "state\tparent_landed\thas_conflicts\tapproved\ttier\treceipt\tbase_moved\t" ++
    "landed_since_all_independent\tmerge_clean\tverdict\n" ++
    String.join (allInputs.map (fixtureRow · ++ "\n"))


#print axioms correct
#print axioms cost_bound
#print axioms optimal
#print axioms gate
#print axioms decideLand
#print axioms Authorized
#print axioms Ledger.hasLanded
#print axioms cas
#print axioms landStep
#print axioms decideLand_eq_land
#print axioms attempts
#print axioms recheckCost
#print axioms oursRecheck
#print axioms SoundPolicy
#print axioms oursRecheck_unsafe
#print axioms countP_le_of_imp
#print axioms bit
#print axioms ChangeState.code
#print axioms Tier.code
#print axioms ReceiptCheck.code
#print axioms Verdict.code
#print axioms allInputs
#print axioms fixtureRow
#print axioms fixtureTable
end Landing
