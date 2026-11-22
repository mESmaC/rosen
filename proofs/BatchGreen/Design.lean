/-!
# Design proof: post-land batches (spec/R12-post-land-verification.md R12 "Batches")

Models `post_land::store::passed` and the offer in `post_land.rs`. History
positions are positive and zero denotes no green commit. A passing run of
position `tested` marks every land up to it green, so the green record is a
prefix that only grows. The server offers checkers the newest landed commit
beyond green. The cost that batching saves is full post-land test runs, so
the cost model counts checker requests, each one full run.
-/
namespace BatchGreen

def advance (green tested : Nat) : Nat := max green tested

def offer (green tip : Nat) : Option Nat :=
  if green < tip then some tip else none

/-- Positions of actual lands use `some`. A mirror history entry is `none`
because R1 excludes it from landed-change checks. Zero is the empty tip. -/
def latestLanded : List (Option Nat) → Nat
  | [] => 0
  | none :: rest => latestLanded rest
  | some landed :: rest => max landed (latestLanded rest)

/-- **correct** (spec/R12-post-land-verification.md R12 Batches): a pass makes green exactly the union
of the existing prefix and the tested prefix, never a later land. Offers
choose the newest land beyond green, disregarding mirror maintenance
(spec/R1-repository-import-and-git-mirror.md R1). A stale passing run cannot
move green back. -/
-- spec/R12-post-land-verification.md R12
theorem correct (green tested tip pos : Nat) :
    (pos ≤ advance green tested ↔ pos ≤ green ∨ pos ≤ tested) ∧
    green ≤ advance green tested ∧ tested ≤ advance green tested ∧
    advance (advance green tested) tested = advance green tested ∧
    (offer green tip = some tip ↔ green < tip) ∧
    (∀ rows bound, latestLanded rows ≤ bound ↔
      ∀ landed, some landed ∈ rows → landed ≤ bound) := by
  have covered : pos ≤ advance green tested ↔ pos ≤ green ∨ pos ≤ tested := by
    unfold advance
    omega
  refine ⟨covered, Nat.le_max_left green tested, Nat.le_max_right green tested, ?_, ?_, ?_⟩
  · unfold advance
    omega
  · simp [offer]
  · intro rows bound
    induction rows with
    | nil => simp [latestLanded]
    | cons row rest ih =>
      cases row with
      | none => simpa [latestLanded] using ih
      | some landed =>
        rw [latestLanded, Nat.max_le, ih]
        simp only [List.mem_cons, Option.some.injEq]
        constructor
        · rintro ⟨head, tail⟩ n (rfl | hn)
          · exact head
          · exact tail n hn
        · intro all
          exact ⟨all landed (.inl rfl), fun n hn => all n (.inr hn)⟩

/-- An offering policy: from the green prefix and the history tip at a
checker's request, the position to run, or none. -/
abbrev Policy := Nat → Nat → Option Nat

/-- Ours offers the tip when it is beyond green. -/
def ours : Policy := offer

/-- One checker request, whose run passes (the batch case; a failing run
starts bisection, spec/R12-post-land-verification.md R12 "Failure and bisect"). -/
def serve (policy : Policy) (green tip : Nat) : Nat :=
  match policy green tip with
  | some tested => advance green tested
  | none => green

/-- Cost: the requests served, given the tip seen at each, until position
`pos` is green; `none` if the schedule ends first. -/
def requestsUntil (policy : Policy) (pos : Nat) : Nat → List Nat → Option Nat
  | green, tips =>
    if pos ≤ green then some 0 else
    match tips with
    | [] => none
    | tip :: rest => (requestsUntil policy pos (serve policy green tip) rest).map (· + 1)

/-- **cost_bound** (spec/R12-post-land-verification.md R12 Batches). Cost model: checker requests,
each one full post-land test run, until a land is green; runs pass. A land
at position `pos` is green after the first request that sees it landed:
if request `i` sees a tip at or beyond `pos`, at most `i + 1` runs. -/
-- spec/R12-post-land-verification.md R12
theorem cost_bound (pos : Nat) :
    ∀ (tips : List Nat) (green i : Nat) (h : i < tips.length), pos ≤ tips[i] →
      ∃ n, requestsUntil ours pos green tips = some n ∧ n ≤ i + 1
  | [], _, _, h, _ => absurd h (by simp)
  | tip :: rest, green, i, h, hp => by
    unfold requestsUntil
    by_cases hg : pos ≤ green
    · exact ⟨0, by simp [hg], by omega⟩
    · simp only [hg, ite_false]
      cases i with
      | zero =>
        have : pos ≤ serve ours green tip := by
          simp only [List.getElem_cons_zero] at hp
          by_cases hlt : green < tip <;> simp [serve, ours, offer, hlt, advance] <;> omega
        exact ⟨1, by unfold requestsUntil; simp [this], by omega⟩
      | succ j =>
        obtain ⟨n, hn, hle⟩ := cost_bound pos rest (serve ours green tip) j
          (by simpa using h) (by simpa using hp)
        exact ⟨n + 1, by simp [hn], by omega⟩

/-- The class of alternatives: policies that offer only commits already
landed, since no checker can test a commit beyond the tip. -/
def Landed (policy : Policy) : Prop :=
  ∀ green tip tested, policy green tip = some tested → tested ≤ tip

private theorem serve_le (alt : Policy) (h : Landed alt) (g g' tip : Nat) (hg : g' ≤ g) :
    serve alt g' tip ≤ serve ours g tip := by
  have hours : serve ours g tip = max g tip := by
    by_cases hlt : g < tip <;> simp [serve, ours, offer, hlt, advance] <;> omega
  rw [hours]
  unfold serve
  split
  · rename_i tested ht
    have := h g' tip tested ht
    simp [advance]; omega
  · omega

/-- **optimal** (spec/R12-post-land-verification.md R12 Batches). Cost model as in `cost_bound`. For
every schedule of checker requests and every land, no policy that offers
only landed commits makes that land green in fewer runs than offering the
newest land: ours keeps a green prefix at least as long after every request. -/
-- spec/R12-post-land-verification.md R12
theorem optimal (alt : Policy) (landed : Landed alt) (pos : Nat) :
    ∀ (tips : List Nat) (green green' n : Nat), green' ≤ green →
      requestsUntil alt pos green' tips = some n →
      ∃ m, requestsUntil ours pos green tips = some m ∧ m ≤ n
  | tips, green, green', n, hg, ha => by
    by_cases hp : pos ≤ green
    · exact ⟨0, by unfold requestsUntil; simp [hp], Nat.zero_le _⟩
    · have hp' : ¬pos ≤ green' := by omega
      cases tips with
      | nil => unfold requestsUntil at ha; simp [hp'] at ha
      | cons tip rest =>
        unfold requestsUntil at ha
        simp only [hp', ite_false, Option.map_eq_some_iff] at ha
        obtain ⟨k, hk, rfl⟩ := ha
        obtain ⟨m, hm, hle⟩ := optimal alt landed pos rest (serve ours green tip)
          (serve alt green' tip) k (serve_le alt landed green green' tip hg) hk
        exact ⟨m + 1, by unfold requestsUntil; simp [hp, hm], by omega⟩

#print axioms advance
#print axioms offer
#print axioms latestLanded
#print axioms ours
#print axioms serve
#print axioms requestsUntil
#print axioms Landed
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end BatchGreen
