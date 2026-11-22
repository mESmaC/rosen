import Landing.Regeneration

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
def regenerationReady (r : LayerRegeneration) : Bool :=
  match finishRegeneration r with
  | .ok _ => true
  | .error _ => false
def regeneratedStackCas (l : Ledger) (expected : Nat) (layers : List (Input × Nat × Nat))
    (regeneration : List LayerRegeneration) : Option Ledger :=
  if layers.length = regeneration.length ∧ regeneration.all regenerationReady = true then
    stackCas l expected layers
  else none
private theorem stack_correct (l : Ledger) (expected : Nat) (layers : List (Input × Nat × Nat))
    (result : Ledger) : stackCas l expected layers = some result ↔
      expected = l.base ∧ stackReady l layers ∧ result = stackResult l layers := by
  unfold stackCas; split <;> simp_all [eq_comm]

private theorem finish_correct (commands : List (Option Nat)) (finished : Option (Option Nat))
    (tree : Nat) :
    finishRegeneration (.required commands finished) = .ok tree ↔
      commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) ∧ finished = some (some tree) := by
  rw [← and_assoc]
  by_cases ready : commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true)
  · rw [and_iff_right ready]
    cases finished with
    | none => simp only [finishRegeneration, ite_eq_left ready] <;> simp
    | some result => cases result <;> simp only [finishRegeneration, ite_eq_left ready] <;> simp
  · cases finished with
    | none => simp [finishRegeneration, ready]
    | some result => cases result <;> simp [finishRegeneration, ready]
private theorem decideLand_eq_land (i : Input) : decideLand i = .land ↔ Authorized i := by
  obtain ⟨s, p, c, a, v, x, t, r, b, q, m⟩ := i
  cases s <;> cases p <;> cases c <;> cases a <;> cases v <;> cases x <;> cases t <;> cases r <;>
    cases b <;> cases q <;> cases m <;> decide
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
/-- R12 Platforms: a confirmed failure on an advisory target cannot request an inverse. -/
def failureReverts (review : ReviewMode) (build advisory : Bool) : Bool :=
  !advisory && (review == .beforeLand || build)
structure Verification where
  ledger : Ledger
  failures : List (Nat × Bool × String)
/-- Recording precedes any requested inverse: the record itself never moves the base. -/
def reportFailure (s : Verification) (review : ReviewMode) (build advisory : Bool)
    (culprit : Nat) (out : String) : Bool × Verification :=
  (failureReverts review build advisory, {s with failures := (culprit, build, out) :: s.failures})
/-- **correct** (spec/R8-optimistic-landing/README.md R8, clean-merge-and-conflict-markers.md, land-check.md,
and spec/R12-post-land-verification.md R12): receipts and approvals gate
before-land projects. Every confirmed failure is recorded. Advisory failures request no inverse.
An unresolved regeneration blocks the stack CAS, and a complete uncached top plan reads as pending. -/
theorem correct (l : Ledger) (i : Input) (expected change commit : Nat) :
    ((∀ l', landStep l i expected change commit = some l' →
      Authorized i ∧ expected = l.base ∧ l.hasLanded change = false ∧
      l' = ⟨commit, (change, commit) :: l.landed⟩) ∧
    (Authorized i → expected = l.base → l.hasLanded change = false →
      landStep l i expected change commit = some ⟨commit, (change, commit) :: l.landed⟩)) ∧
    (∀ s review build advisory culprit out,
      (reportFailure s review build advisory culprit out).2.ledger = s.ledger ∧
      (reportFailure s review build advisory culprit out).2.failures = (culprit, build, out) :: s.failures ∧
      ((reportFailure s review build advisory culprit out).1 = true ↔
        advisory = false ∧ (review = .beforeLand ∨ build = true))) ∧
    (∀ expected layers result, stackCas l expected layers = some result ↔
      expected = l.base ∧ stackReady l layers ∧ result = stackResult l layers) ∧
    (∀ commands finished tree, finishRegeneration (.required commands finished) = .ok tree ↔
      commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) ∧ finished = some (some tree)) ∧
    (∀ expected layers regeneration result,
      regeneratedStackCas l expected layers regeneration = some result ↔
      layers.length = regeneration.length ∧ (∀ r ∈ regeneration, regenerationReady r = true) ∧
      expected = l.base ∧ stackReady l layers ∧ result = stackResult l layers) ∧
    (∀ tree, checkTopRegeneration (.clean tree) = .landable) ∧
    (∀ commands finished, checkTopRegeneration (.required commands finished) =
      if commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) then
        if finished.join.isSome then .landable else .pendingAtLand
      else .manual) := by
  refine ⟨land_correct l i expected change commit, ?_, stack_correct l, finish_correct, ?_,
    fun _ => rfl, check_top_required⟩
  · intro s review build advisory culprit out
    refine ⟨rfl, rfl, ?_⟩
    cases review <;> cases build <;> cases advisory <;> simp [reportFailure, failureReverts]
  · intro expected layers regeneration result
    simp only [regeneratedStackCas, List.all_eq_true]
    split
    · rename_i ready
      rw [stack_correct]
      exact ⟨fun h => ⟨ready.1, ready.2, h⟩, fun h => h.2.2⟩
    · rename_i blocked
      simp only [reduceCtorEq, false_iff]
      exact fun h => blocked ⟨h.1, h.2.1⟩
/-- `attempts` counts optimistic attempts (merge, write and CAS); each true race lost its swap. -/
def attempts : Nat → List Bool → Nat
  | 0, _ => 0
  | _ + 1, [] => 1
  | _ + 1, false :: _ => 1
  | n + 1, true :: rest => 1 + attempts n rest
/-- **cost_bound**: optimistic attempts are bounded by the cap and lost races;
R12 inverse cost charges one request per confirmed failure that requires it. -/
theorem cost_bound (cap : Nat) (races : List Bool) :
    (attempts cap races ≤ cap ∧ attempts cap races ≤ races.count true + 1) ∧
    (∀ review build advisory, (failureReverts review build advisory).toNat ≤ 1) := by
  refine ⟨?_, fun review build advisory => by cases failureReverts review build advisory <;> decide⟩
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
  simp only [oursRecheck, beq_iff_eq] at h
  unfold decideLand at h
  split at h
  · rename_i refusal hg
    have hr : refusal = .needsRecheck := Verdict.refuse.inj h
    subst refusal; unfold gate at hg
    repeat first | contradiction | split at hg
  · repeat first | contradiction | split at h
    all_goals cases hv : i.review <;> simp_all
private theorem countP_le_of_imp (p q : Input → Bool) (hpq : ∀ i, p i = true → q i = true) :
    ∀ l : List Input, l.countP p ≤ l.countP q
  | [] => by simp
  | x :: xs => by
    have ih := countP_le_of_imp p q hpq xs
    by_cases hp : p x = true
    · simp [hp, hpq x hp]; omega
    · simp only [List.countP_cons, Bool.not_eq_true] at hp ⊢
      simp [hp]; split <;> omega
/-- **optimal**: minimal new receipts among sound re-check policies, and minimal
inverse requests among dispositions that must request each R12-required inverse. -/
theorem optimal :
    (∀ (policy : ReviewMode → Bool → Bool → Bool),
      (∀ review build, review = .beforeLand ∨ build = true → policy review build false = true) →
      ∀ review build advisory,
        (failureReverts review build advisory).toNat ≤ (policy review build advisory).toNat) ∧
    (SoundPolicy oursRecheck ∧ ∀ policy, SoundPolicy policy → ∀ requests : List Input,
      recheckCost oursRecheck requests ≤ recheckCost policy requests) := by
  refine ⟨?_, ?_⟩
  · intro policy required review build advisory
    cases advisory <;> cases review <;> cases build <;> simp [failureReverts, required]
    exact Nat.zero_le _
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
#print axioms correct
#print axioms cost_bound
#print axioms optimal
#print axioms stack_correct
end Landing
