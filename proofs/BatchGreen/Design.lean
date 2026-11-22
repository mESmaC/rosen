/-!
# Design proof: post-land batches (spec/R12-post-land-verification.md R12 "Batches" & "Platforms")

Models `post_land::store::passed`, `post_land::platforms::passed_with`, and the
offer in `post_land.rs`. History positions are positive and zero denotes no
green commit. A passing run of position `tested` marks every land up to it
green, so each stream's green record is a prefix that only grows. Across
required platforms, the aggregate checkpoint advances only to the minimum
passing position across every required platform; advisory platforms advance
only their own checkpoints. The cost model counts checker requests, each one
full post-land run.
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

/-- Advances one platform's monotone checkpoint without disturbing others. -/
def passPlatform (checkpoints : Nat → Nat) (platform tested : Nat) : Nat → Nat :=
  fun q => if q = platform then advance (checkpoints q) tested else checkpoints q

/-- Aggregate checkpoint across required platforms (R12 "Platforms"): zero when
no required platform is configured or any required platform has not passed yet,
otherwise the minimum passing position across every required platform. -/
def aggregateGreen : List Nat → (Nat → Nat) → Nat
  | [], _ => 0
  | [p], cp => cp p
  | p :: q :: rest, cp => min (cp p) (aggregateGreen (q :: rest) cp)

private theorem le_aggregateGreen (required : List Nat) (cp : Nat → Nat) (pos : Nat)
    (hne : required ≠ []) :
    pos ≤ aggregateGreen required cp ↔ ∀ p ∈ required, pos ≤ cp p := by
  match required with
  | [] => contradiction
  | [p] => simp [aggregateGreen]
  | p :: q :: rest =>
    have ih := le_aggregateGreen (q :: rest) cp pos (List.cons_ne_nil _ _)
    rw [aggregateGreen, Nat.le_min, ih]
    simp only [List.mem_cons]
    exact ⟨fun ⟨hp, hr⟩ r h => h.elim (· ▸ hp) (hr r),
      fun hall => ⟨hall p (.inl rfl), fun r hr => hall r (.inr hr)⟩⟩

private theorem aggregateGreen_mono (required : List Nat) (cp₁ cp₂ : Nat → Nat)
    (hle : ∀ p ∈ required, cp₁ p ≤ cp₂ p) :
    aggregateGreen required cp₁ ≤ aggregateGreen required cp₂ := by
  match required with
  | [] => simp [aggregateGreen]
  | [p] => exact hle p (by simp)
  | p :: q :: rest =>
    have h1 := hle p (by simp)
    have h2 := aggregateGreen_mono (q :: rest) cp₁ cp₂ (fun r hr => hle r (by simp [hr]))
    simp only [aggregateGreen]; omega

private theorem aggregateGreen_pass_nonrequired (required : List Nat) (cp : Nat → Nat)
    (p tested : Nat) (hnot : p ∉ required) :
    aggregateGreen required (passPlatform cp p tested) = aggregateGreen required cp := by
  match required with
  | [] => rfl
  | [q] =>
    have hne : q ≠ p := fun h => hnot (by simp [h])
    simp [aggregateGreen, passPlatform, hne]
  | q :: r :: rest =>
    have hne : q ≠ p := fun h => hnot (by simp [h])
    have ih := aggregateGreen_pass_nonrequired (r :: rest) cp p tested (fun h => hnot (by simp [h]))
    simp [aggregateGreen, passPlatform, hne, ih]

/-- **correct** (spec/R12-post-land-verification.md R12 Batches & Platforms): a pass makes green
exactly the union of the existing prefix and the tested prefix, never a later land. Offers
choose the newest land beyond green, disregarding mirror maintenance
(spec/R1-repository-import-and-git-mirror.md R1). A stale passing run cannot move green back.
Across required platforms, `pos` is aggregate-green iff every required platform has passed at
or beyond `pos`; platform passes are monotone on the aggregate, and advisory platforms
outside `required` never move the aggregate checkpoint. -/
-- spec/R12-post-land-verification.md R12
theorem correct (green tested tip pos : Nat) :
    (pos ≤ advance green tested ↔ pos ≤ green ∨ pos ≤ tested) ∧
    green ≤ advance green tested ∧ tested ≤ advance green tested ∧
    advance (advance green tested) tested = advance green tested ∧
    (offer green tip = some tip ↔ green < tip) ∧
    (∀ rows bound, latestLanded rows ≤ bound ↔
      ∀ landed, some landed ∈ rows → landed ≤ bound) ∧
    (∀ required cp, required ≠ [] →
      (pos ≤ aggregateGreen required cp ↔ ∀ p ∈ required, pos ≤ cp p)) ∧
    (∀ required cp p,
      aggregateGreen required cp ≤ aggregateGreen required (passPlatform cp p tested)) ∧
    (∀ required cp p, p ∉ required →
      aggregateGreen required (passPlatform cp p tested) = aggregateGreen required cp) := by
  have covered : pos ≤ advance green tested ↔ pos ≤ green ∨ pos ≤ tested := by
    unfold advance; omega
  refine ⟨covered, Nat.le_max_left green tested, Nat.le_max_right green tested, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · unfold advance; omega
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
        exact ⟨fun ⟨h, t⟩ n hn => hn.elim (· ▸ h) (t n),
          fun all => ⟨all landed (.inl rfl), fun n hn => all n (.inr hn)⟩⟩
  · intro required cp hne; exact le_aggregateGreen required cp pos hne
  · intro required cp p
    apply aggregateGreen_mono
    intro q _
    by_cases hq : q = p <;> simp [passPlatform, hq, advance] <;> omega
  · intro required cp p hnot; exact aggregateGreen_pass_nonrequired required cp p tested hnot

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

/-- Total checker runs across every required platform until `pos` is aggregate-green. -/
def platformRequestsUntil (policy : Nat → Policy) (pos : Nat)
    (checkpoints : Nat → Nat) (schedules : Nat → List Nat) : List Nat → Option Nat
  | [] => some 0
  | p :: rest =>
    match requestsUntil (policy p) pos (checkpoints p) (schedules p),
          platformRequestsUntil policy pos checkpoints schedules rest with
    | some n, some m => some (n + m)
    | _, _ => none

private theorem single_cost_bound (pos : Nat) :
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
        obtain ⟨n, hn, hle⟩ := single_cost_bound pos rest (serve ours green tip) j
          (by simpa using h) (by simpa using hp)
        exact ⟨n + 1, by simp [hn], by omega⟩

private theorem multi_cost_bound (pos : Nat) (checkpoints : Nat → Nat)
    (schedules : Nat → List Nat) (idx : Nat → Nat) :
    ∀ required : List Nat,
      (∀ p ∈ required, ∃ h : idx p < (schedules p).length, pos ≤ (schedules p)[idx p]) →
      ∃ total, platformRequestsUntil (fun _ => ours) pos checkpoints schedules required = some total ∧
        total ≤ (required.map fun p => idx p + 1).sum
  | [], _ => ⟨0, rfl, Nat.zero_le _⟩
  | p :: rest, hall => by
    obtain ⟨hp_lt, hp_ge⟩ := hall p (by simp)
    obtain ⟨n, hn, hn_le⟩ := single_cost_bound pos (schedules p) (checkpoints p) (idx p) hp_lt hp_ge
    obtain ⟨m, hm, hm_le⟩ :=
      multi_cost_bound pos checkpoints schedules idx rest (fun q hq => hall q (by simp [hq]))
    exact ⟨n + m, by simp [platformRequestsUntil, hn, hm], by simp; omega⟩

/-- **cost_bound** (spec/R12-post-land-verification.md R12 Batches & Platforms). Cost model:
checker requests, each one full post-land test run, until a land is green; runs pass.
Single-stream: if request `i` sees a tip at or beyond `pos`, at most `i + 1` runs.
Multi-platform: across required platforms, if platform `p`'s request `idx p` sees a tip
at or beyond `pos`, aggregate green is reached in at most `∑ (idx p + 1)` total runs. -/
-- spec/R12-post-land-verification.md R12
theorem cost_bound (pos : Nat) :
    (∀ (tips : List Nat) (green i : Nat) (h : i < tips.length), pos ≤ tips[i] →
      ∃ n, requestsUntil ours pos green tips = some n ∧ n ≤ i + 1) ∧
    (∀ (required : List Nat) (checkpoints : Nat → Nat) (schedules : Nat → List Nat) (idx : Nat → Nat),
      (∀ p ∈ required, ∃ h : idx p < (schedules p).length, pos ≤ (schedules p)[idx p]) →
      ∃ total, platformRequestsUntil (fun _ => ours) pos checkpoints schedules required = some total ∧
        total ≤ (required.map fun p => idx p + 1).sum) :=
  ⟨single_cost_bound pos, fun required cp schedules idx h =>
    multi_cost_bound pos cp schedules idx required h⟩

/-- The class of alternatives: policies that offer only commits already
landed, since no checker can test a commit beyond the tip. -/
def Landed (policy : Policy) : Prop :=
  ∀ green tip tested, policy green tip = some tested → tested ≤ tip

private theorem serve_le (alt : Policy) (h : Landed alt) (g g' tip : Nat) (hg : g' ≤ g) :
    serve alt g' tip ≤ serve ours g tip := by
  have hours : serve ours g tip = max g tip := by
    by_cases hlt : g < tip <;> simp [serve, ours, offer, hlt, advance] <;> omega
  rw [hours]; unfold serve
  split
  · rename_i tested ht; have := h g' tip tested ht; simp [advance]; omega
  · omega

private theorem single_optimal (alt : Policy) (landed : Landed alt) (pos : Nat) :
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
        obtain ⟨m, hm, hle⟩ := single_optimal alt landed pos rest (serve ours green tip)
          (serve alt green' tip) k (serve_le alt landed green green' tip hg) hk
        exact ⟨m + 1, by unfold requestsUntil; simp [hp, hm], by omega⟩

private theorem multi_optimal (alt : Nat → Policy) (pos : Nat)
    (checkpoints checkpoints' : Nat → Nat) (schedules : Nat → List Nat) :
    ∀ required n,
      (∀ p ∈ required, Landed (alt p)) →
      (∀ p ∈ required, checkpoints' p ≤ checkpoints p) →
      platformRequestsUntil alt pos checkpoints' schedules required = some n →
      ∃ m, platformRequestsUntil (fun _ => ours) pos checkpoints schedules required = some m ∧ m ≤ n
  | [], n, _, _, h => by simp [platformRequestsUntil] at h; subst h; exact ⟨0, rfl, Nat.zero_le _⟩
  | p :: rest, n, hland, hle, h => by
    simp only [platformRequestsUntil] at h
    cases h1 : requestsUntil (alt p) pos (checkpoints' p) (schedules p) with
    | none => simp [h1] at h
    | some k1 =>
      cases h2 : platformRequestsUntil alt pos checkpoints' schedules rest with
      | none => simp [h1, h2] at h
      | some k2 =>
        simp only [h1, h2, Option.some.injEq] at h; subst h
        obtain ⟨m1, hm1, hle1⟩ := single_optimal (alt p) (hland p (by simp)) pos (schedules p)
          (checkpoints p) (checkpoints' p) k1 (hle p (by simp)) h1
        obtain ⟨m2, hm2, hle2⟩ := multi_optimal alt pos checkpoints checkpoints' schedules rest k2
          (fun q hq => hland q (by simp [hq])) (fun q hq => hle q (by simp [hq])) h2
        exact ⟨m1 + m2, by simp [platformRequestsUntil, hm1, hm2], by omega⟩

/-- **optimal** (spec/R12-post-land-verification.md R12 Batches & Platforms). Cost model as in
`cost_bound`. No landed-only policy makes a land green in fewer runs than offering the newest
land, both for a single stream and summed across every required platform. -/
-- spec/R12-post-land-verification.md R12
theorem optimal (pos : Nat) :
    (∀ (alt : Policy), Landed alt →
      ∀ (tips : List Nat) (green green' n : Nat), green' ≤ green →
        requestsUntil alt pos green' tips = some n →
        ∃ m, requestsUntil ours pos green tips = some m ∧ m ≤ n) ∧
    (∀ (alt : Nat → Policy) (required : List Nat) (checkpoints checkpoints' : Nat → Nat)
        (schedules : Nat → List Nat) (n : Nat),
      (∀ p ∈ required, Landed (alt p)) →
      (∀ p ∈ required, checkpoints' p ≤ checkpoints p) →
      platformRequestsUntil alt pos checkpoints' schedules required = some n →
      ∃ m, platformRequestsUntil (fun _ => ours) pos checkpoints schedules required = some m ∧ m ≤ n) :=
  ⟨fun alt landed => single_optimal alt landed pos,
   fun alt required cp cp' schedules n hland hle h =>
     multi_optimal alt pos cp cp' schedules required n hland hle h⟩

#print axioms advance
#print axioms offer
#print axioms latestLanded
#print axioms passPlatform
#print axioms aggregateGreen
#print axioms ours
#print axioms serve
#print axioms requestsUntil
#print axioms platformRequestsUntil
#print axioms Landed
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end BatchGreen
