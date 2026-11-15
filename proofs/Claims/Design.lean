/-!
# Design proof: path claims (spec/R6-path-claims-and-edit-tracking.md R6)

Models `crates/rosen-server/src/claims/store.rs` and the symmetric overlap
rule in `crates/rosen-core/src/claim.rs`. `claim` is exclusive; `notice` is default.
* `correct`: exclusive safety and waiter wakeups hold with notices interleaved.
* `cost_bound`: overlap tests and record reads are bounded by table sizes.
* `optimal`: safe exclusive deciders cannot refuse fewer requests in mixed states.
Shares cannot block or wake anyone and are omitted. `Claims.Export` supplies
`proofs/fixtures/claims.tsv` for the Rust test `claims_match_the_lean_table`.
-/

namespace Claims

variable {P : Type}

/-- A stored record; the mode matters only for claims, not waits. -/
structure Rec (P : Type) where
  change : Nat
  pat : P
  exclusive : Bool

/-- The live claims and the stored waits. -/
structure State (P : Type) where
  claims : List (Rec P)
  waits : List (Rec P)

/-- A live claim on another change overlaps `p`. -/
def blocked (ov : P → P → Bool) (claims : List (Rec P)) (c : Nat) (p : P) : Bool :=
  claims.any fun k => k.change != c && ov k.pat p

/-- First claim wins: grant exactly when no requested pattern is blocked. -/
def grants (ov : P → P → Bool) (s : State P) (c : Nat) (ps : List P) : Bool :=
  !(ps.any (blocked ov s.claims c))

def grant (s : State P) (c : Nat) (ps : List P) (exclusive : Bool := true) : State P :=
  { s with claims := ps.map (fun p => ⟨c, p, exclusive⟩) ++ s.claims }

/-- Exclusive requests are all or nothing: one blocked pattern refuses them. -/
def claim (ov : P → P → Bool) (s : State P) (c : Nat) (ps : List P) : Option (State P) :=
  if grants ov s c ps then some (grant s c ps) else none

/-- Default R6 notices record intent without consulting blockers. -/
def notice (s : State P) (c : Nat) (ps : List P) : Option (State P) := some (grant s c ps false)

/-- A wait is stored only while something blocks it. -/
def wait (ov : P → P → Bool) (s : State P) (c : Nat) (p : P) : State P :=
  if blocked ov s.claims c p then { s with waits := ⟨c, p, false⟩ :: s.waits } else s

/-- Drops the records `drop` selects (a change's paths on `rosen release`,
or all of them on land or abandon), then wakes every unblocked wait.
Returns the new state and the woken waits. -/
def release (ov : P → P → Bool) (s : State P) (drop : Rec P → Bool) :
    State P × List (Rec P) :=
  let claims := s.claims.filter (fun k => !drop k)
  let waits := s.waits.filter (fun w => !drop w)
  ({ claims, waits := waits.filter (fun w => blocked ov claims w.change w.pat) },
   waits.filter (fun w => !blocked ov claims w.change w.pat))

/-- Only pairs of exclusive holders must be disjoint. R6 permits a later
notice to overlap even an exclusive holder, so excluding all such pairs
would not be an invariant of the implementation. -/
def Exclusive (ov : P → P → Bool) (s : State P) : Prop :=
  ∀ a ∈ s.claims, a.exclusive = true → ∀ b ∈ s.claims, b.exclusive = true →
    a.change ≠ b.change → ov a.pat b.pat = false

/-- No waiter is stranded: every stored wait has a live holder to wake it. -/
def WaitsBlocked (ov : P → P → Bool) (s : State P) : Prop :=
  ∀ w ∈ s.waits, blocked ov s.claims w.change w.pat = true

def Inv (ov : P → P → Bool) (s : State P) : Prop := Exclusive ov s ∧ WaitsBlocked ov s

def Symmetric (ov : P → P → Bool) : Prop := ∀ p q, ov p q = ov q p

private theorem blocked_true {ov : P → P → Bool} {claims : List (Rec P)} {c : Nat} {p : P} :
    blocked ov claims c p = true ↔ ∃ k ∈ claims, k.change ≠ c ∧ ov k.pat p = true := by
  simp [blocked, List.any_eq_true]

private theorem blocked_false {ov : P → P → Bool} {claims : List (Rec P)} {c : Nat} {p : P} :
    blocked ov claims c p = false ↔ ∀ k ∈ claims, k.change ≠ c → ov k.pat p = false := by
  simp [blocked, List.any_eq_false]

private theorem grants_true {ov : P → P → Bool} {s : State P} {c : Nat} {ps : List P} :
    grants ov s c ps = true ↔ ∀ p ∈ ps, blocked ov s.claims c p = false := by
  simp [grants, List.any_eq_false]

private theorem grant_exclusive {ov : P → P → Bool} (hsym : Symmetric ov) {s : State P}
    {c : Nat} {ps : List P} (hx : Exclusive ov s) (hg : grants ov s c ps = true) :
    Exclusive ov (grant s c ps) := by
  rw [grants_true] at hg
  intro a ha he b hb hf hne
  simp only [grant, List.mem_append, List.mem_map] at ha hb
  rcases ha with ⟨p, hp, rfl⟩ | ha <;> rcases hb with ⟨q, hq, rfl⟩ | hb
  · exact absurd rfl hne
  · rw [hsym]; exact blocked_false.mp (hg p hp) b hb (Ne.symm hne)
  · exact blocked_false.mp (hg q hq) a ha hne
  · exact hx a ha he b hb hf hne

private theorem notice_exclusive {ov : P → P → Bool} {s : State P} {c : Nat} {ps : List P}
    (hx : Exclusive ov s) : Exclusive ov (grant s c ps false) := by
  intro a ha he b hb hf hne
  simp only [grant, List.mem_append, List.mem_map] at ha hb
  rcases ha with ⟨p, hp, rfl⟩ | ha <;> rcases hb with ⟨q, hq, rfl⟩ | hb
  · cases he
  · cases he
  · cases hf
  · exact hx a ha he b hb hf hne

private theorem blocked_grant {ov : P → P → Bool} {s : State P} {c c' : Nat} {ps : List P}
    {exclusive : Bool} {q : P} (h : blocked ov s.claims c' q = true) :
    blocked ov (grant s c ps exclusive).claims c' q = true := by
  obtain ⟨k, hk, hne, hov⟩ := blocked_true.mp h
  exact blocked_true.mpr ⟨k, by simp [grant, hk], hne, hov⟩

private theorem claim_refused (ov : P → P → Bool) (s : State P) (c : Nat) (ps : List P) :
    claim ov s c ps = none ↔
      ∃ p ∈ ps, ∃ k ∈ s.claims, k.change ≠ c ∧ ov k.pat p = true := by
  by_cases hg : grants ov s c ps = true
  · simp only [claim, hg, ↓reduceIte, reduceCtorEq, false_iff, not_exists, not_and]
    intro p hp k hk hne hov
    have := blocked_false.mp (grants_true.mp hg p hp) k hk hne
    simp_all
  · have hg' : grants ov s c ps = false := by simpa using hg
    simp only [claim, hg', Bool.false_eq_true, ↓reduceIte, true_iff]
    simp only [grants, Bool.not_eq_false', List.any_eq_true] at hg'
    obtain ⟨p, hp, hb⟩ := hg'
    obtain ⟨k, hk, hne, hov⟩ := blocked_true.mp hb
    exact ⟨p, hp, k, hk, hne, hov⟩

private theorem wait_inv (ov : P → P → Bool) (s : State P) (c : Nat) (p : P)
    (hinv : Inv ov s) : Inv ov (wait ov s c p) := by
  obtain ⟨hx, hw⟩ := hinv
  unfold wait
  split
  · refine ⟨hx, ?_⟩
    intro w hw'
    simp only [List.mem_cons] at hw'
    rcases hw' with rfl | hw'
    · assumption
    · exact hw w hw'
  · exact ⟨hx, hw⟩

/-- **correct** (spec/R6-path-claims-and-edit-tracking.md R6): mixed states
preserve exclusive safety and blocked waits; new exclusives respect all live
claims and races refuse the second request. Release partitions waits into
blocked and woken. Notices never refuse and preserve the invariants, so these
guarantees can be iterated with arbitrary notices interleaved. -/
theorem correct (ov : P → P → Bool) (hsym : Symmetric ov) (s : State P) (hinv : Inv ov s) :
    (∀ c ps, claim ov s c ps = none ↔
        ∃ p ∈ ps, ∃ k ∈ s.claims, k.change ≠ c ∧ ov k.pat p = true) ∧
    (∀ c ps s', claim ov s c ps = some s' → Inv ov s' ∧
      ∀ p ∈ ps, blocked ov s.claims c p = false) ∧
    (∀ c₁ ps s₁ c₂ qs p q, claim ov s c₁ ps = some s₁ → c₁ ≠ c₂ → p ∈ ps → q ∈ qs →
        ov p q = true → claim ov s₁ c₂ qs = none) ∧
    (∀ c p, Inv ov (wait ov s c p)) ∧
    (∀ drop, Inv ov (release ov s drop).1 ∧
        (∀ w ∈ s.waits, drop w = false →
          w ∈ (release ov s drop).1.waits ∨ w ∈ (release ov s drop).2) ∧
        (∀ w ∈ (release ov s drop).2,
          blocked ov (release ov s drop).1.claims w.change w.pat = false)) ∧
    (∀ state : State P, ∀ c ps, notice state c ps ≠ none) ∧
    (∀ c ps s', notice s c ps = some s' → Inv ov s') := by
  obtain ⟨hx, hw⟩ := hinv
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · exact claim_refused ov s
  · intro c ps s' h
    unfold claim at h
    split at h
    · cases h
      exact ⟨⟨grant_exclusive hsym hx (by assumption),
        fun w hw' => blocked_grant (hw w hw')⟩, grants_true.mp (by assumption)⟩
    · cases h
  · intro c₁ ps s₁ c₂ qs p q h₁ hne hp hq hov
    unfold claim at h₁
    split at h₁
    · cases h₁
      have hb : blocked ov (grant s c₁ ps).claims c₂ q = true :=
        blocked_true.mpr ⟨⟨c₁, p, true⟩, by simp [grant, hp], hne, hov⟩
      have : grants ov (grant s c₁ ps) c₂ qs = false := by
        cases hg : grants ov (grant s c₁ ps) c₂ qs
        · rfl
        · have := grants_true.mp hg q hq; simp_all
      simp [claim, this]
    · cases h₁
  · exact fun c p => wait_inv ov s c p ⟨hx, hw⟩
  · intro drop
    refine ⟨⟨?_, ?_⟩, ?_, ?_⟩
    · intro a ha he b hb hf hne
      simp only [release, List.mem_filter] at ha hb
      exact hx a ha.1 he b hb.1 hf hne
    · intro w hw'
      simp only [release, List.mem_filter] at hw'
      exact hw'.2
    · intro w hw' hd
      simp only [release, List.mem_filter, hw', hd, Bool.not_false, and_self, true_and]
      cases blocked ov (s.claims.filter fun k => !drop k) w.change w.pat <;> simp
    · intro w hw'
      simp only [release, List.mem_filter, Bool.not_eq_true'] at hw' ⊢
      exact hw'.2
  · intro state c ps; simp [notice]
  · intro c ps s' h
    cases h
    exact ⟨notice_exclusive hx, fun w hw' => blocked_grant (hw w hw')⟩

/-- Overlap tests a short-circuiting scan makes: it stops at the first hit. -/
def anyCost (f : α → Bool) : List α → Nat
  | [] => 0
  | x :: xs => if f x then 1 else 1 + anyCost f xs

/-- Cost model: one unit per overlap test and per record a release reads.
Claims test all pattern/claim pairs to name every holder; waits stop at a
blocker. Release reads all records, then scans remaining claims for each
remaining wait until one blocks it. -/
def claimCost (s : State P) (ps : List P) : Nat :=
  (ps.map fun _ => s.claims.length).sum

def waitCost (ov : P → P → Bool) (s : State P) (c : Nat) (p : P) : Nat :=
  anyCost (fun k => k.change != c && ov k.pat p) s.claims

def releaseCost (ov : P → P → Bool) (s : State P) (drop : Rec P → Bool) : Nat :=
  let claims := s.claims.filter (fun k => !drop k)
  let waits := s.waits.filter (fun w => !drop w)
  s.claims.length + s.waits.length +
    (waits.map fun w => anyCost (fun k => k.change != w.change && ov k.pat w.pat) claims).sum

private theorem anyCost_le (f : α → Bool) : ∀ l : List α, anyCost f l ≤ l.length
  | [] => by simp [anyCost]
  | x :: xs => by
    have := anyCost_le f xs
    simp only [anyCost, List.length_cons]
    split <;> omega

private theorem sum_map_le (f : α → Nat) (bound : Nat) (hf : ∀ x, f x ≤ bound) :
    ∀ l : List α, (l.map f).sum ≤ l.length * bound
  | [] => by simp
  | x :: xs => by
    have := sum_map_le f bound hf xs
    have := hf x
    simp only [List.map_cons, List.sum_cons, List.length_cons, Nat.succ_mul]
    omega

/-- **cost_bound**: under the cost model above, with `n` live claims, `m`
stored waits and `r` requested patterns, a claim costs at most `r·n`, a wait
at most `n`, and a release at most `n + m + m·n`. -/
-- spec/R6-path-claims-and-edit-tracking.md R6
theorem cost_bound (ov : P → P → Bool) (s : State P) (c : Nat) (ps : List P) (p : P)
    (drop : Rec P → Bool) :
    claimCost s ps ≤ ps.length * s.claims.length ∧
    waitCost ov s c p ≤ s.claims.length ∧
    releaseCost ov s drop ≤
      s.claims.length + s.waits.length + s.waits.length * s.claims.length := by
  refine ⟨sum_map_le _ _ (fun _ => Nat.le_refl _) ps, anyCost_le _ _, ?_⟩
  simp only [releaseCost]
  have hc := List.length_filter_le (fun k => !drop k) s.claims
  have hw := List.length_filter_le (fun w => !drop w) s.waits
  have hsum := sum_map_le
    (fun w : Rec P => anyCost (fun k => k.change != w.change && ov k.pat w.pat)
      (s.claims.filter fun k => !drop k))
    s.claims.length (fun w => Nat.le_trans (anyCost_le _ _) hc)
    (s.waits.filter fun w => !drop w)
  have := Nat.mul_le_mul_right s.claims.length hw
  omega

/-- A claim decider says, for a state, a claimant and the requested patterns,
whether to grant. Its cost on a request history is how many it refuses. -/
def refusals (d : State P → Nat → List P → Bool)
    (requests : List (State P × Nat × List P)) : Nat :=
  requests.countP fun r => !d r.1 r.2.1 r.2.2

/-- Alternatives must preserve exclusive-holder safety and grant a new
exclusive request only when no other claim (including a notice) overlaps it.
Pairwise exclusive safety alone would incorrectly allow ignoring notices. -/
def Safe (ov : P → P → Bool) (d : State P → Nat → List P → Bool) : Prop :=
  ∀ s c ps, Exclusive ov s → d s c ps = true →
    Exclusive ov (grant s c ps) ∧ ∀ p ∈ ps, blocked ov s.claims c p = false

/-- **optimal**: exclusive first-claim-wins is safe, and no safe decider refuses fewer
claims on any history of mixed notice/exclusive states with disjoint exclusive holders. -/
-- spec/R6-path-claims-and-edit-tracking.md R6
theorem optimal (ov : P → P → Bool) (hsym : Symmetric ov) :
    Safe ov (grants ov) ∧
    ∀ d, Safe ov d → ∀ requests : List (State P × Nat × List P),
      (∀ r ∈ requests, Exclusive ov r.1) →
      refusals (grants ov) requests ≤ refusals d requests := by
  refine ⟨fun s c ps hx hg => ⟨grant_exclusive hsym hx hg, grants_true.mp hg⟩, ?_⟩
  intro d hd requests hreq
  apply List.countP_mono_left
  intro ⟨s, c, ps⟩ hr hours
  simp only [Bool.not_eq_true'] at hours ⊢
  simp only [grants, Bool.not_eq_false', List.any_eq_true] at hours
  obtain ⟨p, hp, hb⟩ := hours
  obtain ⟨k, hk, hne, hov⟩ := blocked_true.mp hb
  cases hdg : d s c ps
  · rfl
  · have hx := blocked_false.mp ((hd s c ps (hreq _ hr) hdg).2 p hp) k hk hne
    simp_all

end Claims
