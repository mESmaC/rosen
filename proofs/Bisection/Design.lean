/-! # Design proof: post-land bisection (spec/R12-post-land-verification.md R12). -/
namespace Bisection

def midpoint (lo hi : Nat) := lo + (hi - lo) / 2

def step (lo hi culprit : Nat) : Prod Nat Nat :=
  let mid := midpoint lo hi
  if mid < culprit then (mid, hi) else (lo, mid)

/-- The starting checkpoint is already eligible. The remaining history rows
carry the commit ID and whether the row represents mirror maintenance. -/
def landedInterval (checkpoint : Nat) (rows : List (Nat × Bool)) : List Nat :=
  checkpoint :: (rows.filter fun row => !row.2).map Prod.fst

inductive CheckKind where
  | build | test
  deriving DecidableEq, Repr

/-- Bisection stream identity (R12 "Platforms"): each `(kind, platform)` pair
keeps independent sessions and culprit search boundaries. -/
structure Scope where
  kind : CheckKind
  platform : Option String
  deriving DecidableEq, Repr

structure SessionKey where
  failed : Nat
  scope : Scope
  deriving DecidableEq, Repr

def lookupSession : List SessionKey → SessionKey → Option Nat
  | [], _ => none
  | k :: rest, key =>
    if k = key then some 0
    else (lookupSession rest key).map (· + 1)

/-- Opens a session for `key` if none exists, returning its session ID and the
updated session list. Matches `UNIQUE(ref_name, failed, kind, platform)`. -/
def openSession (sessions : List SessionKey) (key : SessionKey) : Nat × List SessionKey :=
  match lookupSession sessions key with
  | some id => (id, sessions)
  | none => (sessions.length, sessions ++ [key])

private def advanceBoundary (scope : Scope) (failed : Nat)
    (records : List (Nat × Scope)) (acc : Nat) : Nat :=
  (records.filter fun c => c.2 == scope && c.1 < failed).foldl (fun a c => max a c.1) acc

/-- Lower search boundary for `scope`: the newest of its own checkpoint,
recorded culprits, and batches covered by a known failure strictly before
`failed` in the same `(kind, platform)` scope (R12). -/
def scopedBoundary (scope : Scope) (checkpoint failed : Nat)
    (culprits knownBatches : List (Nat × Scope)) : Nat :=
  advanceBoundary scope failed knownBatches (advanceBoundary scope failed culprits checkpoint)

/-- Bisection job submission requires exact `(kind, platform)` match. -/
def acceptsJob (expected actual : Scope) : Bool := expected == actual

private theorem lookup_lt : ∀ (s : List SessionKey) (k : SessionKey) (i : Nat),
    lookupSession s k = some i → i < s.length
  | [], _, _, h => by simp [lookupSession] at h
  | k :: rest, key, i, h => by
    unfold lookupSession at h; split at h
    · cases h; simp
    · obtain ⟨j, hj, rfl⟩ := Option.map_eq_some_iff.mp h
      have := lookup_lt rest key j hj; simp; omega

private theorem lookup_append_some : ∀ (s : List SessionKey) (k k' : SessionKey) (i : Nat),
    lookupSession s k = some i → lookupSession (s ++ [k']) k = some i
  | [], _, _, _, h => by simp [lookupSession] at h
  | x :: rest, k, k', i, h => by
    unfold lookupSession at h; split at h
    · rename_i heq; simp [List.cons_append, lookupSession, heq, h]
    · rename_i hne; obtain ⟨j, hj, rfl⟩ := Option.map_eq_some_iff.mp h
      simp [List.cons_append, lookupSession, hne, lookup_append_some rest k k' j hj]

private theorem lookup_append_self : ∀ (s : List SessionKey) (k : SessionKey),
    lookupSession s k = none → lookupSession (s ++ [k]) k = some s.length
  | [], k, _ => by simp [lookupSession]
  | x :: rest, k, h => by
    unfold lookupSession at h; split at h
    · contradiction
    · rename_i hne
      have hrest : lookupSession rest k = none := by
        cases h' : lookupSession rest k <;> simp [h'] at h; rfl
      simp [List.cons_append, lookupSession, hne, lookup_append_self rest k hrest]

private theorem lookup_append_other : ∀ (s : List SessionKey) (k₁ k₂ : SessionKey),
    k₁ ≠ k₂ → lookupSession s k₂ = none → lookupSession (s ++ [k₁]) k₂ = none
  | [], _, _, hne, _ => by simp [lookupSession, hne]
  | x :: rest, k₁, k₂, hne, h => by
    unfold lookupSession at h; split at h
    · contradiction
    · rename_i hxk
      have hrest : lookupSession rest k₂ = none := by
        cases hr : lookupSession rest k₂ <;> simp [hr] at h; rfl
      simp [List.cons_append, lookupSession, hxk, lookup_append_other rest k₁ k₂ hne hrest]

private theorem lookup_inj : ∀ (s : List SessionKey) (k₁ k₂ : SessionKey) (i : Nat),
    lookupSession s k₁ = some i → lookupSession s k₂ = some i → k₁ = k₂
  | [], _, _, _, h1, _ => by simp [lookupSession] at h1
  | x :: rest, k₁, k₂, i, h1, h2 => by
    unfold lookupSession at h1 h2; split at h1 <;> split at h2
    · rename_i e1 e2; exact e1.symm.trans e2
    · cases h1; simp at h2
    · cases h2; simp at h1
    · obtain ⟨j1, hj1, rfl⟩ := Option.map_eq_some_iff.mp h1
      obtain ⟨j2, hj2, heq⟩ := Option.map_eq_some_iff.mp h2
      have : j1 = j2 := by omega
      subst this; exact lookup_inj rest k₁ k₂ j1 hj1 hj2

private theorem openSession_idempotent (sessions : List SessionKey) (key : SessionKey) :
    let r := openSession sessions key
    openSession r.2 key = r := by
  dsimp [openSession]
  cases h : lookupSession sessions key with
  | some id => simp [h]
  | none => simp [lookup_append_self sessions key h]

private theorem openSession_isolated (sessions : List SessionKey) (k₁ k₂ : SessionKey)
    (hne : k₁ ≠ k₂) :
    let r₁ := openSession sessions k₁
    let r₂ := openSession r₁.2 k₂
    r₁.1 ≠ r₂.1 := by
  dsimp [openSession]
  cases h1 : lookupSession sessions k₁ with
  | some id1 =>
    cases h2 : lookupSession sessions k₂ with
    | some id2 => intro heq; subst heq; exact hne (lookup_inj sessions k₁ k₂ id1 h1 h2)
    | none => dsimp only; have hlt := lookup_lt sessions k₁ id1 h1; omega
  | none =>
    cases h2 : lookupSession (sessions ++ [k₁]) k₂ with
    | some id2 =>
      cases hs : lookupSession sessions k₂ with
      | some j =>
        rw [lookup_append_some sessions k₂ k₁ j hs] at h2; cases h2
        dsimp only; have hlt := lookup_lt sessions k₂ id2 hs; omega
      | none => rw [lookup_append_other sessions k₁ k₂ hne hs] at h2; contradiction
    | none => simp

private theorem foldl_max_ge (l : List (Nat × Scope)) (acc : Nat) :
    acc ≤ l.foldl (fun a c => max a c.1) acc := by
  induction l generalizing acc with
  | nil => exact Nat.le_refl _
  | cons c rest ih => exact Nat.le_trans (Nat.le_max_left acc c.1) (ih (max acc c.1))

private theorem foldl_max_mem (l : List (Nat × Scope)) (acc : Nat) :
    ∀ c ∈ l, c.1 ≤ l.foldl (fun a x => max a x.1) acc := by
  induction l generalizing acc with
  | nil => intro c hc; cases hc
  | cons x rest ih =>
    intro c hc; simp only [List.mem_cons] at hc
    rcases hc with rfl | hr
    · exact Nat.le_trans (Nat.le_max_right acc c.1) (foldl_max_ge rest (max acc c.1))
    · exact ih (max acc x.1) c hr

private theorem foldl_max_eq (l : List (Nat × Scope)) (acc : Nat) :
    let m := l.foldl (fun a x => max a x.1) acc
    m = acc ∨ ∃ c ∈ l, m = c.1 := by
  induction l generalizing acc with
  | nil => left; rfl
  | cons x rest ih =>
    rcases ih (max acc x.1) with h | ⟨c, hc, hm⟩
    · by_cases hle : acc ≤ x.1
      · right; exact ⟨x, by simp, by simpa [Nat.max_eq_right hle] using h⟩
      · left; simpa [Nat.max_eq_left (Nat.le_of_not_le hle)] using h
    · right; exact ⟨c, by simp [hc], hm⟩

private theorem advanceBoundary_eq (scope : Scope) (failed acc : Nat)
    (records : List (Nat × Scope)) :
    let m := advanceBoundary scope failed records acc
    m = acc ∨ ((m, scope) ∈ records ∧ m < failed) := by
  dsimp only [advanceBoundary]
  rcases foldl_max_eq (records.filter fun c => c.2 == scope && c.1 < failed) acc with
      h | ⟨⟨p, sc⟩, hc, hm⟩
  · exact Or.inl h
  · right; simp only [List.mem_filter, Bool.and_eq_true, beq_iff_eq, decide_eq_true_eq] at hc
    rcases hc with ⟨hmem, rfl, hlt⟩; rw [hm]; exact ⟨hmem, hlt⟩

private theorem scopedBoundary_exact (scope : Scope) (checkpoint failed : Nat)
    (culprits knownBatches : List (Nat × Scope)) :
    let b := scopedBoundary scope checkpoint failed culprits knownBatches
    b = checkpoint ∨ ((b, scope) ∈ culprits ∧ b < failed) ∨
      ((b, scope) ∈ knownBatches ∧ b < failed) := by
  dsimp only [scopedBoundary]
  rcases advanceBoundary_eq scope failed _ knownBatches with h2 | h2
  · rw [h2]
    rcases advanceBoundary_eq scope failed checkpoint culprits with h1 | h1
    · exact Or.inl h1
    · exact Or.inr (Or.inl h1)
  · exact Or.inr (Or.inr h2)

/-- spec/R12-post-land-verification.md R12: a monotone checker passes exactly before culprit.
Every split retains that culprit and strictly reduces a non-singleton range;
a singleton identifies it. Confirmation requires a second failure and parent pass.
The captured interval excludes mirror maintenance (spec/R1-repository-import-and-git-mirror.md R1).
Sessions are keyed by `(failed, kind, platform)` (idempotent on duplicates, isolated across
different kinds or platforms), lower search boundaries advance only from culprits or batches
covered by a known failure in the same `(kind, platform)` scope while remaining strictly below
`failed`, and submitted jobs must match their session's `(kind, platform)`. -/
theorem correct (lo hi culprit : Nat) (hl : lo < culprit) (hh : culprit <= hi) :
    let next := step lo hi culprit
    (hi - lo > 1 → lo <= next.1 ∧ next.1 < culprit ∧ culprit <= next.2 ∧
      next.2 <= hi ∧ next.2 - next.1 < hi - lo) ∧
    (hi - lo = 1 → hi = culprit) ∧
    (∀ (firstFail secondFail parentPass : Bool),
      (firstFail && secondFail && parentPass = true) ↔
      firstFail = true ∧ secondFail = true ∧ parentPass = true) ∧
    (∀ checkpoint rows commit, commit ∈ landedInterval checkpoint rows ↔
      commit = checkpoint ∨ (commit, false) ∈ rows) ∧
    (∀ sessions key, let r := openSession sessions key; openSession r.2 key = r) ∧
    (∀ sessions k₁ k₂, k₁ ≠ k₂ →
      let r₁ := openSession sessions k₁
      let r₂ := openSession r₁.2 k₂
      r₁.1 ≠ r₂.1) ∧
    (∀ scope checkpoint failed culprits knownBatches,
      let b := scopedBoundary scope checkpoint failed culprits knownBatches
      checkpoint ≤ b ∧
      (∀ pos, (pos, scope) ∈ culprits → pos < failed → pos ≤ b) ∧
      (∀ pos, (pos, scope) ∈ knownBatches → pos < failed → pos ≤ b) ∧
      (b = checkpoint ∨ ((b, scope) ∈ culprits ∧ b < failed) ∨
        ((b, scope) ∈ knownBatches ∧ b < failed)) ∧
      (checkpoint < failed → b < failed) ∧
      (∀ pos other, other ≠ scope →
        scopedBoundary scope checkpoint failed ((pos, other) :: culprits) knownBatches = b ∧
        scopedBoundary scope checkpoint failed culprits ((pos, other) :: knownBatches) = b)) ∧
    (∀ expected actual, acceptsJob expected actual = true ↔ expected = actual) := by
  dsimp [step, midpoint]
  refine ⟨?_, ?_, ?_, ?_, openSession_idempotent, openSession_isolated, ?_, ?_⟩
  · intro hw; by_cases hm : lo + (hi - lo) / 2 < culprit <;> simp [hm] <;> omega
  · omega
  · intro a b c; cases a <;> cases b <;> cases c <;> decide
  · intro checkpoint rows commit; simp [landedInterval]
  · intro scope checkpoint failed culprits knownBatches
    refine ⟨Nat.le_trans (foldl_max_ge _ _) (foldl_max_ge _ _), ?_, ?_,
      scopedBoundary_exact scope checkpoint failed culprits knownBatches, ?_, ?_⟩
    · intro pos hmem hlt
      exact Nat.le_trans (foldl_max_mem _ checkpoint (pos, scope) (by simp [List.mem_filter, hmem, hlt])) (foldl_max_ge _ _)
    · intro pos hmem hlt
      exact foldl_max_mem _ _ (pos, scope) (by simp [List.mem_filter, hmem, hlt])
    · intro hlt
      rcases scopedBoundary_exact scope checkpoint failed culprits knownBatches with h | ⟨_, h⟩ | ⟨_, h⟩
      · rw [h]; exact hlt
      · exact h
      · exact h
    · intro pos other hne; simp [scopedBoundary, advanceBoundary, hne]
  · intro expected actual; simp [acceptsJob]

/-- Cost model: each external checker query costs one; measure remaining
candidates after that query. Balanced bisection leaves at most ceil(n/2),
and narrowing the lower bound from same-scope culprits or covered batches never
increases the remaining candidate count. Each distinct `(failed, kind, platform)`
key opens at most one session. Two more queries confirm the retained failing
endpoint and its parent. -/
theorem cost_bound (lo hi culprit : Nat) (h : lo < hi) :
    (let next := step lo hi culprit; next.2 - next.1 <= (hi - lo + 1) / 2) ∧
    (∀ scope culprits knownBatches,
      let lo' := scopedBoundary scope lo hi culprits knownBatches
      let next := step lo' hi culprit
      next.2 - next.1 <= (hi - lo + 1) / 2) ∧
    (∀ sessions key, (openSession sessions key).2.length <= sessions.length + 1) := by
  refine ⟨?_, ?_, ?_⟩
  · dsimp [step, midpoint]; by_cases hm : lo + (hi - lo) / 2 < culprit <;> simp [hm] <;> omega
  · intro scope culprits knownBatches
    have hge : lo ≤ scopedBoundary scope lo hi culprits knownBatches :=
      Nat.le_trans (foldl_max_ge _ _) (foldl_max_ge _ _)
    have hlt : scopedBoundary scope lo hi culprits knownBatches < hi := by
      rcases scopedBoundary_exact scope lo hi culprits knownBatches with h' | ⟨_, h'⟩ | ⟨_, h'⟩
      · rw [h']; exact h
      · exact h'
      · exact h'
    generalize scopedBoundary scope lo hi culprits knownBatches = lo' at hge hlt ⊢
    dsimp [step, midpoint]; by_cases hm : lo' + (hi - lo') / 2 < culprit <;> simp [hm] <;> omega
  · intro sessions key; unfold openSession; cases lookupSession sessions key <;> simp

/-- Alternatives are ordered binary comparisons splitting n candidates
into k and n-k; cost is the worst remaining candidate count after one query.
No such split has smaller worst-case cost than this balanced split. -/
theorem optimal (n k : Nat) (hk : k <= n) :
    max (midpoint 0 n) (n - midpoint 0 n) <= max k (n - k) := by
  dsimp [midpoint]
  have := Nat.le_max_left k (n - k); have := Nat.le_max_right k (n - k)
  exact Nat.max_le.mpr ⟨by omega, by omega⟩

#print axioms midpoint
#print axioms step
#print axioms landedInterval
#print axioms CheckKind
#print axioms Scope
#print axioms SessionKey
#print axioms lookupSession
#print axioms openSession
#print axioms scopedBoundary
#print axioms acceptsJob
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end Bisection
