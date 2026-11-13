/-!
# Design proof: piece claims (SPEC.md R13)

Models the `channel_pieces` table (`crates/rosen-server/src/channel/pieces.rs`)
as a list of rows and the first-claim-wins rule (`rosen_core::piece::decide`),
and proves:

* `correct`: SPEC.md R13 "Piece claims". From a table where each piece has
  at most one row, a claim is refused exactly when another member holds the
  piece, and the refusal names that holder; a granted claimant holds the
  piece afterwards and no other piece's holder moves; two serialised claims
  by different members on one piece never both succeed; a release removes
  exactly the releasing member's selected pieces and keeps every other
  holder; every operation keeps one row per piece.
* `cost_bound`: under the stated cost model, a claim and a release each read
  at most every stored row once, and a claim adds at most one row.
* `optimal`: among deciders that never give one piece two holders, first
  claim wins refuses the fewest claims on every request history.

A quarantined member's claims are frozen (Velocity F14): the server refuses
its claims and releases before this rule runs. That only refuses more and
changes no row, so it cannot break any property here, and it is left out.

`fixtureTable` in `PieceClaims/Export.lean` is exported to
`proofs/fixtures/piece-claims.tsv`, which the Rust test
`piece_claim_fixtures` replays against the server's channel store.
-/

namespace PieceClaims

variable {K : Type} [DecidableEq K]

/-- One stored row: a piece and the member holding it. -/
structure Row (K : Type) where
  piece : K
  member : Nat

abbrev Table (K : Type) := List (Row K)

/-- The member holding piece `k`: the first matching row. -/
def holder : Table K → K → Option Nat
  | [], _ => none
  | r :: t, k => if r.piece = k then some r.member else holder t k

/-- One row per piece: what the table's primary key enforces. -/
def Unique : Table K → Prop
  | [] => True
  | r :: t => holder t r.piece = none ∧ Unique t

inductive Outcome where
  | granted (extended : Bool)
  | refused (holder : Nat)
  deriving DecidableEq

/-- First claim wins: take a free piece, extend one's own, or name the holder. -/
def claim (t : Table K) (m : Nat) (k : K) : Outcome × Table K :=
  match holder t k with
  | none => (.granted false, ⟨k, m⟩ :: t)
  | some h => if h = m then (.granted true, t) else (.refused h, t)

/-- Drops member `m`'s rows on the pieces `sel` selects (one piece, or all). -/
def release (t : Table K) (m : Nat) (sel : K → Bool) : Table K :=
  t.filter fun r => !(r.member == m && sel r.piece)

private theorem holder_cons_ne {r : Row K} {t : Table K} {k : K} (h : r.piece ≠ k) :
    holder (r :: t) k = holder t k := by
  simp [holder, h]

private theorem holder_release (t : Table K) (m : Nat) (sel : K → Bool) (hu : Unique t)
    (k : K) :
    holder (release t m sel) k =
      if holder t k = some m ∧ sel k = true then none else holder t k := by
  induction t with
  | nil => simp [release, holder]
  | cons r t ih =>
    obtain ⟨hfresh, hu⟩ := hu
    have ih := ih hu
    by_cases hk : r.piece = k
    · subst hk
      by_cases hkeep : r.member = m ∧ sel r.piece = true
      · have : release (r :: t) m sel = release t m sel := by
          simp [release, hkeep.1, hkeep.2]
        rw [this, ih, hfresh]
        simp [holder, hkeep.1, hkeep.2]
      · have : release (r :: t) m sel = r :: release t m sel := by
          simp only [release, List.filter_cons]
          by_cases hm : r.member = m <;> simp_all
        rw [this]
        simp only [holder, ↓reduceIte, Option.some.injEq]
        by_cases hm : r.member = m <;> simp_all
    · by_cases hkeep : r.member = m ∧ sel r.piece = true
      · have : release (r :: t) m sel = release t m sel := by
          simp [release, hkeep.1, hkeep.2]
        rw [this, ih, holder_cons_ne hk]
      · have : release (r :: t) m sel = r :: release t m sel := by
          simp only [release, List.filter_cons]
          by_cases hm : r.member = m <;> simp_all
        rw [this, holder_cons_ne hk, ih, holder_cons_ne hk]

private theorem unique_release (t : Table K) (m : Nat) (sel : K → Bool) (hu : Unique t) :
    Unique (release t m sel) := by
  induction t with
  | nil => simp [release, Unique]
  | cons r t ih =>
    obtain ⟨hfresh, hu'⟩ := hu
    by_cases hkeep : r.member = m ∧ sel r.piece = true
    · have : release (r :: t) m sel = release t m sel := by
        simp [release, hkeep.1, hkeep.2]
      rw [this]; exact ih hu'
    · have : release (r :: t) m sel = r :: release t m sel := by
        simp only [release, List.filter_cons]
        by_cases hm : r.member = m <;> simp_all
      rw [this]
      refine ⟨?_, ih hu'⟩
      rw [holder_release t m sel hu' r.piece, hfresh]
      simp

private theorem claim_cases (t : Table K) (m : Nat) (k : K) :
    (holder t k = none ∧ claim t m k = (.granted false, ⟨k, m⟩ :: t)) ∨
    (holder t k = some m ∧ claim t m k = (.granted true, t)) ∨
    (∃ h, h ≠ m ∧ holder t k = some h ∧ claim t m k = (.refused h, t)) := by
  unfold claim
  cases hh : holder t k with
  | none => exact Or.inl ⟨rfl, rfl⟩
  | some h =>
    by_cases hm : h = m
    · subst hm; exact Or.inr (Or.inl ⟨rfl, by simp⟩)
    · exact Or.inr (Or.inr ⟨h, hm, rfl, by simp [hm]⟩)

/-- **correct** (SPEC.md §R13, "Piece claims"): from a table with one row
per piece,
1. a claim is refused exactly when another member holds the piece, and the
   refusal names that holder; a refusal changes nothing;
2. a granted claimant holds the piece afterwards, and no other piece's
   holder moves;
3. of two claims on one piece by different members, serialised by the
   store's transaction, the second is refused naming the first: they never
   both succeed;
4. a release drops exactly the releasing member's selected pieces and keeps
   every other holder;
5. claims and releases keep one row per piece. -/
-- SPEC.md §R13
theorem correct (t : Table K) (hu : Unique t) :
    (∀ m k h, (claim t m k).1 = .refused h ↔ holder t k = some h ∧ h ≠ m) ∧
    (∀ m k h, (claim t m k).1 = .refused h → (claim t m k).2 = t) ∧
    (∀ m k e, (claim t m k).1 = .granted e →
        holder (claim t m k).2 k = some m ∧
        ∀ k', k' ≠ k → holder (claim t m k).2 k' = holder t k') ∧
    (∀ m₁ m₂ k e, (claim t m₁ k).1 = .granted e → m₁ ≠ m₂ →
        (claim (claim t m₁ k).2 m₂ k).1 = .refused m₁) ∧
    (∀ m sel k, holder (release t m sel) k =
        if holder t k = some m ∧ sel k = true then none else holder t k) ∧
    (∀ m k, Unique (claim t m k).2) ∧ (∀ m sel, Unique (release t m sel)) := by
  have granted_holds : ∀ m k e, (claim t m k).1 = .granted e →
      holder (claim t m k).2 k = some m ∧
      ∀ k', k' ≠ k → holder (claim t m k).2 k' = holder t k' := by
    intro m k e hg
    rcases claim_cases t m k with ⟨_, hc⟩ | ⟨hh, hc⟩ | ⟨h, _, _, hc⟩
    · rw [hc]
      refine ⟨by simp [holder], fun k' hk' => ?_⟩
      exact holder_cons_ne (r := ⟨k, m⟩) (fun h => hk' h.symm)
    · rw [hc]; exact ⟨hh, fun _ _ => rfl⟩
    · rw [hc] at hg; cases hg
  refine ⟨?_, ?_, granted_holds, ?_, fun m sel => holder_release t m sel hu, ?_,
    fun m sel => unique_release t m sel hu⟩
  · intro m k h
    rcases claim_cases t m k with ⟨hh, hc⟩ | ⟨hh, hc⟩ | ⟨h', hne, hh, hc⟩
    · simp [hc, hh]
    · simp only [hc, hh, reduceCtorEq, Option.some.injEq, false_iff, not_and]
      exact fun h hm => hm h.symm
    · rw [hc, hh]
      simp only [Outcome.refused.injEq, Option.some.injEq]
      constructor
      · rintro rfl; exact ⟨rfl, hne⟩
      · rintro ⟨rfl, _⟩; rfl
  · intro m k h hr
    rcases claim_cases t m k with ⟨_, hc⟩ | ⟨_, hc⟩ | ⟨_, _, _, hc⟩
    · rw [hc] at hr; cases hr
    · rw [hc] at hr; cases hr
    · rw [hc]
  · intro m₁ m₂ k e hg hne
    have hh := (granted_holds m₁ k e hg).1
    rcases claim_cases (claim t m₁ k).2 m₂ k with ⟨h0, _⟩ | ⟨h0, _⟩ | ⟨h, _, h0, hc⟩
    · rw [hh] at h0; cases h0
    · rw [hh] at h0; cases h0; exact absurd rfl hne
    · rw [hh] at h0; cases h0; rw [hc]
  · intro m k
    rcases claim_cases t m k with ⟨hh, hc⟩ | ⟨_, hc⟩ | ⟨_, _, _, hc⟩
    · rw [hc]; exact ⟨hh, hu⟩
    · rw [hc]; exact hu
    · rw [hc]; exact hu

/-! ## Cost -/

/-- Rows a scan for piece `k` reads: it stops at the first match. -/
def holderCost : Table K → K → Nat
  | [], _ => 0
  | r :: t, k => if r.piece = k then 1 else 1 + holderCost t k

/-- Cost model: one unit per stored row read. A claim scans for its piece's
row until it finds it; a release reads every row once to decide which to
drop. Writes are counted separately as rows added. -/
def claimCost (t : Table K) (k : K) : Nat := holderCost t k

def releaseCost (t : Table K) : Nat := t.length

private theorem holderCost_le : ∀ (t : Table K) (k : K), holderCost t k ≤ t.length
  | [], _ => by simp [holderCost]
  | r :: t, k => by
    have := holderCost_le t k
    simp only [holderCost, List.length_cons]
    split <;> omega

/-- **cost_bound**: under the cost model above, with `n` stored rows, a claim
reads at most `n` rows and leaves at most `n + 1`, and a release reads `n`
rows and leaves at most `n`. -/
-- SPEC.md §R13
theorem cost_bound (t : Table K) (m : Nat) (k : K) (sel : K → Bool) :
    claimCost t k ≤ t.length ∧ (claim t m k).2.length ≤ t.length + 1 ∧
    releaseCost t ≤ t.length ∧ (release t m sel).length ≤ t.length := by
  refine ⟨holderCost_le t k, ?_, Nat.le_refl _, List.length_filter_le _ _⟩
  rcases claim_cases t m k with ⟨_, hc⟩ | ⟨_, hc⟩ | ⟨_, _, _, hc⟩ <;> rw [hc] <;> simp

/-! ## Optimality -/

/-- What granting a claim stores: nothing new for the holder's own piece,
otherwise a row for the claimant. -/
def take (t : Table K) (m : Nat) (k : K) : Table K :=
  if holder t k = some m then t else ⟨k, m⟩ :: t

def grants (t : Table K) (m : Nat) (k : K) : Bool :=
  match (claim t m k).1 with
  | .granted _ => true
  | .refused _ => false

/-- A decider's cost on a request history: how many claims it refuses. -/
def refusals (d : Table K → Nat → K → Bool) (requests : List (Table K × Nat × K)) : Nat :=
  requests.countP fun r => !d r.1 r.2.1 r.2.2

/-- The class of alternatives: deciders that, from a table with one row per
piece, never grant a claim whose stored result has two rows for a piece. -/
def Safe (d : Table K → Nat → K → Bool) : Prop :=
  ∀ t m k, Unique t → d t m k = true → Unique (take t m k)

/-- **optimal**: first claim wins is safe, and no safe decider refuses fewer
claims on any history of requests made from tables with one row per piece. -/
-- SPEC.md §R13
theorem optimal :
    Safe (grants (K := K)) ∧
    ∀ d, Safe d → ∀ requests : List (Table K × Nat × K),
      (∀ r ∈ requests, Unique r.1) →
      refusals grants requests ≤ refusals d requests := by
  refine ⟨?_, ?_⟩
  · intro t m k hu hg
    rcases claim_cases t m k with ⟨hh, _⟩ | ⟨hh, _⟩ | ⟨_, _, _, hc⟩
    · simp [take, hh]; exact ⟨hh, hu⟩
    · simp [take, hh]; exact hu
    · simp [grants, hc] at hg
  · intro d hd requests hreq
    apply List.countP_mono_left
    intro ⟨t, m, k⟩ hr hours
    simp only [Bool.not_eq_true'] at hours ⊢
    rcases claim_cases t m k with ⟨_, hc⟩ | ⟨_, hc⟩ | ⟨h, hne, hh, _⟩
    · simp [grants, hc] at hours
    · simp [grants, hc] at hours
    · cases hdg : d t m k
      · rfl
      · have hu := hd t m k (hreq _ hr) hdg
        have hnm : holder t k ≠ some m := by rw [hh]; simpa using hne
        simp only [take, hnm, ↓reduceIte, Unique] at hu
        rw [hh] at hu
        cases hu.1

end PieceClaims
