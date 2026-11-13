/-!
# Design proof: saved overlap targets (SPEC.md R8 step 4)

Models `load_or_capture` in `crates/rosen-server/src/landing/overlap_targets.rs`
and the post-land order in `landing/service.rs`: capture the overlap targets,
release the landed change's claims, then rebase each target. A crash can stop
the run after any step; recovery replays the whole run. The stored rows for
one land are a marker for an empty selection, or one row per target. No rows
means no selection yet. The overlap query (R6) is any function of the live
claims, so the result holds for every overlap rule.
-/
namespace TargetedRebase

/-- One stored row for a land: the empty-selection marker (`none`) or one
target change ID (`some id`), as the two tables hold them. -/
abbrev Row := Option Nat

/-- What `load` reads: no rows is no selection; otherwise the stored targets. -/
def decode (rows : List Row) : Option (List Nat) :=
  if rows = [] then none else some (rows.filterMap id)

/-- What the capture transaction writes for a selection. -/
def encode (targets : List Nat) : List Row :=
  if targets = [] then [none] else targets.map some

/-- Post-land state of one land: the live claims as (change, path) pairs,
the stored rows, and every rebase performed so far, in order. -/
structure State where
  claims : List (Nat × Nat)
  rows : List Row
  rebased : List Nat

/-- Selects with the overlap query only when nothing is stored; a replay and
a concurrent recovery that captured first both reuse the stored set. -/
def capture (overlaps : List (Nat × Nat) → List Nat) (s : State) : State :=
  if decode s.rows = none then { s with rows := encode (overlaps s.claims) } else s

def release (landed : Nat) (s : State) : State :=
  { s with claims := s.claims.filter (·.1 != landed) }

def rebase (s : State) : State :=
  { s with rebased := s.rebased ++ (decode s.rows).getD [] }

/-- The post-land steps in order. A crash runs a prefix of them. -/
def steps (overlaps : List (Nat × Nat) → List Nat) (landed : Nat) : List (State → State) :=
  [capture overlaps, release landed, rebase]

def runPrefix (overlaps : List (Nat × Nat) → List Nat) (landed : Nat) (k : Nat)
    (s : State) : State :=
  ((steps overlaps landed).take k).foldl (fun s f => f s) s

/-- Crashed runs, one prefix length each, then the run that completes. -/
def recover (overlaps : List (Nat × Nat) → List Nat) (landed : Nat) (crashes : List Nat)
    (s : State) : State :=
  runPrefix overlaps landed 3 (crashes.foldl (fun s k => runPrefix overlaps landed k s) s)

private theorem decode_encode (l : List Nat) : decode (encode l) = some l := by
  unfold decode encode
  by_cases h : l = []
  · simp [h]
  · simp only [h, ite_false]
    have : l.map some ≠ [] := by simpa using h
    simp only [this, ite_false, Option.some.injEq]
    induction l with
    | nil => rfl
    | cons a t ih => simp

/-- Before the claims are released nothing is stored; afterwards the stored
selection is the overlap set of the claims as they stood at capture. -/
def Saved (overlaps : List (Nat × Nat) → List Nat) (claims : List (Nat × Nat))
    (s : State) : Prop :=
  (decode s.rows = none ∧ s.claims = claims) ∨ decode s.rows = some (overlaps claims)

private theorem capture_saved {overlaps : List (Nat × Nat) → List Nat}
    {claims : List (Nat × Nat)} {s : State} (h : Saved overlaps claims s) :
    decode (capture overlaps s).rows = some (overlaps claims) := by
  rcases h with ⟨hn, hc⟩ | hd
  · simp [capture, hn, hc, decode_encode]
  · simp [capture, hd]

/-- Every crashed run keeps the invariant: capture runs first, so release
never runs before the selection is stored. -/
private theorem saved_prefix (overlaps : List (Nat × Nat) → List Nat) (landed : Nat)
    (claims : List (Nat × Nat)) (k : Nat) (s : State) (h : Saved overlaps claims s) :
    Saved overlaps claims (runPrefix overlaps landed k s) := by
  match k with
  | 0 => simpa [runPrefix] using h
  | 1 => exact Or.inr (by simpa [runPrefix, steps] using capture_saved h)
  | 2 => exact Or.inr (by simpa [runPrefix, steps, release] using capture_saved h)
  | _ + 3 => exact Or.inr (by simpa [runPrefix, steps, release, rebase] using capture_saved h)

private theorem saved_crashes (overlaps : List (Nat × Nat) → List Nat) (landed : Nat)
    (claims : List (Nat × Nat)) :
    ∀ (crashes : List Nat) (s : State), Saved overlaps claims s →
      Saved overlaps claims (crashes.foldl (fun s k => runPrefix overlaps landed k s) s)
  | [], _, h => h
  | k :: ks, s, h => saved_crashes overlaps landed claims ks _ (saved_prefix _ _ _ k s h)

/-- **correct** (SPEC.md R8 step 4): however many times post-land work
crashes after any step, the completed run stores exactly the overlap set of
the claims as they stood before release, keeps that set across replays,
including an empty selection, releases the landed change's claims, and
rebases exactly that set in its final pass. -/
-- SPEC.md §R8
theorem correct (overlaps : List (Nat × Nat) → List Nat) (landed : Nat)
    (crashes : List Nat) (claims : List (Nat × Nat)) (done : List Nat) :
    let t := recover overlaps landed crashes ⟨claims, [], done⟩
    decode t.rows = some (overlaps claims) ∧
    (∀ c ∈ t.claims, c.1 ≠ landed) ∧
    ∃ before, t.rebased = before ++ overlaps claims := by
  intro t
  have ht : t = rebase (release landed (capture overlaps
      (crashes.foldl (fun s k => runPrefix overlaps landed k s) ⟨claims, [], done⟩))) := rfl
  have hc := capture_saved (saved_crashes overlaps landed claims crashes ⟨claims, [], done⟩
    (Or.inl ⟨by simp [decode], rfl⟩))
  generalize crashes.foldl (fun s k => runPrefix overlaps landed k s) ⟨claims, [], done⟩ = s
    at hc ht
  refine ⟨by rw [ht]; simpa [rebase, release] using hc, ?_, ?_⟩
  · intro c hc'
    rw [ht] at hc'
    simp only [rebase, release, List.mem_filter, bne_iff_ne, ne_eq] at hc'
    exact hc'.2
  · refine ⟨(capture overlaps s).rebased, ?_⟩
    rw [ht]
    simp [rebase, release, hc]

/-- Rows the capture transaction writes: the encoded selection on the first
capture, nothing on a replay. -/
def writes (overlaps : List (Nat × Nat) → List Nat) (s : State) : Nat :=
  if decode s.rows = none then (encode (overlaps s.claims)).length else 0

/-- **cost_bound** (SPEC.md R8 step 4). Cost model: durable rows written
when saving the selection; reads of claims and edits are excluded. The first
capture writes one row per target and one marker only for an empty
selection, so at most `max 1 n` rows for `n` targets. A replay writes none. -/
-- SPEC.md §R8
theorem cost_bound (overlaps : List (Nat × Nat) → List Nat) (s : State) :
    writes overlaps s ≤ max 1 (overlaps s.claims).length ∧
    (decode s.rows ≠ none → writes overlaps s = 0) := by
  unfold writes encode
  constructor
  · split
    · split
      · exact Nat.le_max_left _ _
      · simpa using Nat.le_max_right _ _
    · simp
  · intro h; simp [h]

/-- **optimal** (SPEC.md R8 step 4). Cost model as in `cost_bound`. The
class of alternatives: any row representation of a selection where each row
holds at most one change ID (`Row`), that reads an empty store as "no
selection yet" (the state before capture), that recovers every selection
exactly, including the empty one, and whose decoded IDs each appear in their
own row (IDs are stored, not packed or computed). No such representation
stores a selection of distinct targets in fewer rows. -/
-- SPEC.md §R8
theorem optimal (enc : List Nat → List Row) (dec : List Row → Option (List Nat))
    (empty : dec [] = none) (exact : ∀ l, dec (enc l) = some l)
    (stored : ∀ rows, ∀ t ∈ (dec rows).getD [], some t ∈ rows)
    (targets : List Nat) (distinct : targets.Nodup) :
    (encode targets).length ≤ (enc targets).length := by
  unfold encode
  split
  · rename_i h
    subst h
    have : enc [] ≠ [] := fun he => by simpa [he, empty] using exact []
    exact List.length_pos_iff.mpr this
  · have hsub : targets.map some ⊆ enc targets := by
      intro r hr
      obtain ⟨t, ht, rfl⟩ := List.mem_map.mp hr
      simpa [exact] using stored (enc targets) t (by simpa [exact] using ht)
    have hn : (targets.map some).Nodup :=
      List.Pairwise.map some (fun _ _ h he => h (Option.some.inj he)) distinct
    simpa using hn.length_le_of_subset hsub

#print axioms decode
#print axioms encode
#print axioms capture
#print axioms release
#print axioms rebase
#print axioms steps
#print axioms runPrefix
#print axioms recover
#print axioms Saved
#print axioms writes
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end TargetedRebase
