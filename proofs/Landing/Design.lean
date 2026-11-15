/-! Optimistic landing, stack atomicity and failure disposition (spec/R8-optimistic-landing/README.md R8 and spec/R12-post-land-verification.md R12; #365/#366/#368). -/
namespace Landing
/-- `active` covers both `open` and `ready`: the decision treats them alike. -/
inductive ChangeState where
  | active | landed | abandoned
  deriving DecidableEq, Repr
inductive Tier where
  | prose | normal | sensitive
  deriving DecidableEq, Repr
/-- The project's `review` setting at the attempt's base (R7): `beforeLand`
makes an approval for the current head gate the land; `afterLand` does not. -/
inductive ReviewMode where
  | beforeLand | afterLand
  deriving DecidableEq, Repr
inductive ReceiptCheck where
  /-- `valid` binds the current head and configured check set; `sameTree`
  verifies a trusted receipt's checked tree equals the current result (R8). -/
  | missing | invalid | valid | sameTree
  deriving DecidableEq, Repr
structure Input where
  state : ChangeState
  parentLanded : Bool
  hasConflicts : Bool
  approved : Bool
  review : ReviewMode
  mergeMode : Bool
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
    else if i.review = .beforeLand ∧ !i.approved then some .unapproved
    else if i.review = .afterLand ∨ i.tier = .prose then none
    else match i.receipt with
      | .missing => some .noReceipt
      | .invalid => some .receiptStale
      | .valid | .sameTree => none
def decideLand (i : Input) : Verdict :=
  match gate i with
  | some r => .refuse r
  | none =>
    if !i.mergeClean then .refuse .mergeConflict
    else if i.review = .afterLand ∨ i.tier = .prose ∨ i.receipt = .sameTree ∨ i.mergeMode = true ∨
        !i.baseMoved ∨ i.landedSinceAllIndependent then .land
    else .refuse .needsRecheck
/-- spec/R8-optimistic-landing/README.md R8 step 2, with spec/R7-review.md R7 review modes and spec/R8-optimistic-landing/README.md R8 step 3
stack order: the change is live, its ancestors can join the request, it has no stored
conflicts, its current head is approved unless the project reviews after the
land, it merges cleanly onto the current base, and only before-land code
needs a receipt that the reach/merge re-check rules let count on this base. -/
def Authorized (i : Input) : Prop :=
  i.state = .active ∧ i.parentLanded = true ∧ i.hasConflicts = false ∧
  (i.review = .afterLand ∨ i.approved = true) ∧ i.mergeClean = true ∧
  (i.review = .afterLand ∨ i.tier = .prose ∨
    i.receipt = .sameTree ∨
    (i.receipt = .valid ∧ (i.mergeMode = true ∨ i.baseMoved = false ∨ i.landedSinceAllIndependent = true)))
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
/-- Bottom-first layers: every layer passes its gates, only the top carries
the receipt, and IDs are distinct. One transaction publishes the whole chain. -/
def stackReady (l : Ledger) (layers : List (Input × Nat × Nat)) : Prop :=
  layers ≠ [] ∧ (layers.map (·.2.1)).Nodup ∧
    (∀ x ∈ layers, x.1.state = .active ∧ x.1.hasConflicts = false ∧
      (x.1.review = .afterLand ∨ x.1.approved = true) ∧ x.1.mergeClean = true ∧
      l.hasLanded x.2.1 = false) ∧
    ((layers.getLast?).any fun top => decide (Authorized top.1)) = true
instance : Decidable (stackReady l layers) := by unfold stackReady; infer_instance
def stackResult (l : Ledger) (layers : List (Input × Nat × Nat)) : Ledger :=
  ⟨((layers.getLast?).map (·.2.2)).getD l.base, layers.reverse.map (·.2) ++ l.landed⟩
def stackCas (l : Ledger) (expected : Nat) (layers : List (Input × Nat × Nat)) : Option Ledger :=
  if l.base = expected ∧ stackReady l layers then some (stackResult l layers) else none
private theorem stack_correct (l : Ledger) (expected : Nat) (layers : List (Input × Nat × Nat))
    (result : Ledger) : stackCas l expected layers = some result ↔
      expected = l.base ∧ stackReady l layers ∧ result = stackResult l layers := by
  unfold stackCas; split <;> simp_all [eq_comm]
private theorem decideLand_eq_land (i : Input) : decideLand i = .land ↔ Authorized i := by
  obtain ⟨s, p, c, a, v, x, t, r, b, q, m⟩ := i
  cases s <;> cases p <;> cases c <;> cases a <;> cases v <;> cases x <;> cases t <;> cases r <;>
    cases b <;> cases q <;> cases m <;> decide
/-- Original CAS and stack guarantee, with the R7/R8 mode-dependent gates. -/
private theorem land_correct (l : Ledger) (i : Input) (expected change commit : Nat) :
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
/-- R12: both kinds record a failure; only this action also requests an inverse. -/
def failureReverts (review : ReviewMode) (build : Bool) : Bool :=
  review == .beforeLand || build
structure Verification where
  ledger : Ledger
  failures : List (Nat × Bool × String)
/-- Recording precedes any requested inverse: the record itself never moves the base. -/
def reportFailure (s : Verification) (review : ReviewMode) (build : Bool)
    (culprit : Nat) (out : String) : Bool × Verification :=
  (failureReverts review build, {s with failures := (culprit, build, out) :: s.failures})
/-- **correct** (spec/R8-optimistic-landing/README.md R8 and R12, owner decision #365): the original CAS,
stack and preservation requirements remain; receipts/approvals gate only
before-land projects. Every confirmed failure is recorded, and only builds
request a revert after land. The statement changes because the spec changed. -/
theorem correct (l : Ledger) (i : Input) (expected change commit : Nat) :
    ((∀ l', landStep l i expected change commit = some l' →
      Authorized i ∧ expected = l.base ∧ l.hasLanded change = false ∧
      l' = ⟨commit, (change, commit) :: l.landed⟩) ∧
    (Authorized i → expected = l.base → l.hasLanded change = false →
      landStep l i expected change commit = some ⟨commit, (change, commit) :: l.landed⟩)) ∧
    (∀ s review build culprit out,
      (reportFailure s review build culprit out).2.ledger = s.ledger ∧
      (reportFailure s review build culprit out).2.failures = (culprit, build, out) :: s.failures ∧
      ((reportFailure s review build culprit out).1 = true ↔ review = .beforeLand ∨ build = true)) ∧
    (∀ expected layers result, stackCas l expected layers = some result ↔
      expected = l.base ∧ stackReady l layers ∧ result = stackResult l layers) := by
  refine ⟨land_correct l i expected change commit, ?_, stack_correct l⟩
  intro s review build culprit out
  refine ⟨rfl, rfl, ?_⟩
  cases review <;> cases build <;> simp [reportFailure, failureReverts]
/-- Cost model: an attempt merges and writes each layer, and performs one
compare-and-swap; `attempts` counts optimistic attempts. `races[j]` is true
when a competing land moved the base during attempt `j`, so its swap lost.
`cap` is `max_attempts`; exhausted requests then take an exclusive turn. -/
def attempts : Nat → List Bool → Nat
  | 0, _ => 0
  | _ + 1, [] => 1
  | _ + 1, false :: _ => 1
  | n + 1, true :: rest => 1 + attempts n rest
/-- **cost_bound**: under the cost model of `attempts`, the optimistic phase
never costs more than `cap` attempts, nor more than one attempt per competing
land that beat it plus one. -/
-- spec/R8-optimistic-landing/README.md R8
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
/-- Alternatives must recheck dependent lands in reach mode; merge mode
permits that reuse by spec/R8-optimistic-landing/README.md R8 (#366). Exact-tree receipts need no recheck. -/
def SoundPolicy (policy : Input → Bool) : Prop :=
  ∀ i, gate i = none → i.mergeClean = true → i.review = .beforeLand → i.mergeMode = false → i.tier ≠ .prose →
    i.baseMoved = true → i.landedSinceAllIndependent = false → i.receipt ≠ .sameTree → policy i = true
private theorem oursRecheck_unsafe (i : Input) (h : oursRecheck i = true) :
    gate i = none ∧ i.mergeClean = true ∧ i.review = .beforeLand ∧ i.mergeMode = false ∧ i.tier ≠ .prose ∧
      i.baseMoved = true ∧ i.landedSinceAllIndependent = false ∧ i.receipt ≠ .sameTree := by
  obtain ⟨s, p, c, a, v, x, t, r, b, q, m⟩ := i
  revert h
  cases s <;> cases p <;> cases c <;> cases a <;> cases v <;> cases x <;> cases t <;> cases r <;>
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
-- spec/R8-optimistic-landing/README.md R8
theorem optimal :
    SoundPolicy oursRecheck ∧
    ∀ policy, SoundPolicy policy → ∀ requests : List Input,
      recheckCost oursRecheck requests ≤ recheckCost policy requests := by
  constructor
  · intro i hg hm hv hx ht hb hq hr
    obtain ⟨s, p, c, a, v, x, t, r, b, q, m⟩ := i
    simp only at hg hm hv hx ht hb hq hr
    subst hv hx hm hb hq
    simp [oursRecheck, decideLand, hg, ht, hr]
  · intro policy hp requests
    apply countP_le_of_imp
    intro i hi
    obtain ⟨hg, hm, hv, hx, ht, hb, hq, hr⟩ := oursRecheck_unsafe i hi
    exact hp i hg hm hv hx ht hb hq hr
/-! ## Fixture table -/
/-- With no snapshot or inverse racing the exclusive turn, it needs one
attempt after the optimistic budget is exhausted (R8 retry; #393). -/
def contentionAttempts (cap losses : Nat) : Nat :=
  attempts cap (List.replicate losses true) + (if cap ≤ losses then 1 else 0)
def bit (b : Bool) : String := if b then "1" else "0"
def ChangeState.code : ChangeState → String
  | .active => "active" | .landed => "landed" | .abandoned => "abandoned"
def Tier.code : Tier → String
  | .prose => "prose" | .normal => "normal" | .sensitive => "sensitive"
def ReviewMode.code : ReviewMode → String
  | .beforeLand => "before-land" | .afterLand => "after-land"
def ReceiptCheck.code : ReceiptCheck → String
  | .missing => "missing" | .invalid => "invalid" | .valid => "valid" | .sameTree => "same_tree"
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
  [ReviewMode.beforeLand, .afterLand].flatMap fun v =>
  [false, true].flatMap fun x =>
  [Tier.prose, .normal, .sensitive].flatMap fun t =>
  [ReceiptCheck.missing, .invalid, .valid, .sameTree].flatMap fun r =>
  [(false, true), (true, false), (true, true)].flatMap fun (b, q) =>
  [false, true].map fun m => ⟨s, p, c, a, v, x, t, r, b, q, m⟩
def fixtureRow (i : Input) : String :=
  "\t".intercalate [i.state.code, bit i.parentLanded, bit i.hasConflicts,
    bit i.approved, i.review.code, (if i.mergeMode then "merge" else "reach"), i.tier.code, i.receipt.code, bit i.baseMoved,
    bit i.landedSinceAllIndependent, bit i.mergeClean, (decideLand i).code]
def fixtureTable : String :=
  "state\tparent_landed\thas_conflicts\tapproved\treview\trecheck\ttier\treceipt\tbase_moved\t" ++
    "landed_since_all_independent\tmerge_clean\tverdict\n" ++
    String.join (allInputs.map (fixtureRow · ++ "\n")) ++
    String.join ([ReviewMode.beforeLand, .afterLand].flatMap fun review =>
      [false, true].map fun build =>
        "failure\t" ++ review.code ++ "\t" ++ bit build ++ "\t" ++ bit (failureReverts review build) ++ "\n") ++
    String.join ([0, 1, 12].map fun cap =>
      s!"contention\t{cap}\t{cap}\t{contentionAttempts cap cap}\n")
#print axioms correct
#print axioms cost_bound
#print axioms optimal
#print axioms stack_correct
end Landing
