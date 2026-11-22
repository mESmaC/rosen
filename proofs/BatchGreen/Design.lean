/-!
# Design proof: post-land batches (spec/R12-post-land-verification.md R12 "Batches" & "Platforms")

Models `post_land::store::passed`, `post_land::platforms::passed_with`, and the
offer in `post_land.rs`. Positions are positive; zero denotes no green commit.
A passing run of `tested` marks every land up to it green. Aggregate green
advances to the minimum passing position across required platforms; advisory
platforms move only their own checkpoints. Two cost units are proved:
**checker requests** (every poll, including empty `none` offers) and
**full post-land runs** (`1` per `some` offer, `0` per empty `none` offer).
-/
namespace BatchGreen

def advance (green tested : Nat) : Nat := max green tested
def offer (green tip : Nat) : Option Nat := if green < tip then some tip else none

/-- Actual lands use `some`; mirror history entries are `none` (R1). Zero is empty. -/
def latestLanded : List (Option Nat) → Nat
  | [] => 0
  | none :: rest => latestLanded rest
  | some landed :: rest => max landed (latestLanded rest)

/-- Advances one platform's monotone checkpoint without disturbing others. -/
def passPlatform (checkpoints : Nat → Nat) (platform tested : Nat) : Nat → Nat :=
  fun q => if q = platform then advance (checkpoints q) tested else checkpoints q
/-- Aggregate checkpoint across required platforms (R12 "Platforms"): `0` if empty or any required
platform has not passed yet, else the minimum across required platforms. -/
def aggregateGreen : List Nat → (Nat → Nat) → Nat
  | [], _ => 0
  | [p], cp => cp p
  | p :: q :: rest, cp => min (cp p) (aggregateGreen (q :: rest) cp)

private theorem le_aggregateGreen : ∀ (required : List Nat) (cp : Nat → Nat) (pos : Nat),
    required ≠ [] → (pos ≤ aggregateGreen required cp ↔ ∀ p ∈ required, pos ≤ cp p)
  | [], _, _, hne => absurd rfl hne
  | [p], _, _, _ => by simp [aggregateGreen]
  | p :: q :: rest, cp, pos, _ => by
    rw [aggregateGreen, Nat.le_min, le_aggregateGreen (q :: rest) cp pos (List.cons_ne_nil _ _)]
    simp only [List.mem_cons]
    exact ⟨fun ⟨hp, hr⟩ r h => h.elim (· ▸ hp) (hr r),
      fun hall => ⟨hall p (.inl rfl), fun r hr => hall r (.inr hr)⟩⟩
private theorem aggregateGreen_mono : ∀ (required : List Nat) (cp₁ cp₂ : Nat → Nat),
    (∀ p ∈ required, cp₁ p ≤ cp₂ p) → aggregateGreen required cp₁ ≤ aggregateGreen required cp₂
  | [], _, _, _ => Nat.zero_le _
  | [p], _, _, hle => hle p (by simp)
  | p :: q :: rest, cp₁, cp₂, hle => by
    have := hle p (by simp)
    have := aggregateGreen_mono (q :: rest) cp₁ cp₂ (fun r hr => hle r (by simp [hr]))
    simp only [aggregateGreen]; omega
private theorem aggregateGreen_pass_nonrequired : ∀ (required : List Nat) (cp : Nat → Nat)
    (p tested : Nat), p ∉ required →
    aggregateGreen required (passPlatform cp p tested) = aggregateGreen required cp
  | [], _, _, _, _ => rfl
  | [q], _, p, _, hnot => by
    have hne : q ≠ p := fun h => hnot (by simp [h]); simp [aggregateGreen, passPlatform, hne]
  | q :: r :: rest, cp, p, tested, hnot => by
    have hne : q ≠ p := fun h => hnot (by simp [h])
    have ih := aggregateGreen_pass_nonrequired (r :: rest) cp p tested (fun h => hnot (by simp [h]))
    simp [aggregateGreen, passPlatform, hne, ih]

/-- **correct** (spec/R12-post-land-verification.md R12 Batches & Platforms): a pass makes green
the union of the existing and tested prefixes; offers choose the newest land beyond green ignoring
mirror maintenance (R1); stale passes never move green back; `pos` is aggregate-green iff every
required platform passed at or beyond `pos`; passes are monotone on the aggregate, and advisory
platforms outside `required` never move it. -/
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
  refine ⟨by unfold advance; omega, Nat.le_max_left _ _, Nat.le_max_right _ _,
    by unfold advance; omega, by simp [offer], ?_,
    fun req cp hne => le_aggregateGreen req cp pos hne, ?_,
    fun req cp p hnot => aggregateGreen_pass_nonrequired req cp p tested hnot⟩
  · intro rows bound; induction rows with
    | nil => simp [latestLanded]
    | cons row rest ih => cases row with
      | none => simpa [latestLanded] using ih
      | some landed =>
        rw [latestLanded, Nat.max_le, ih]; simp only [List.mem_cons, Option.some.injEq]
        exact ⟨fun ⟨h, t⟩ n hn => hn.elim (· ▸ h) (t n),
          fun all => ⟨all landed (.inl rfl), fun n hn => all n (.inr hn)⟩⟩
  · intro req cp p; apply aggregateGreen_mono; intro q _
    by_cases hq : q = p <;> simp [passPlatform, hq, advance] <;> omega

/-- An offering policy: from green prefix and history tip, the position to run or `none`. -/
abbrev Policy := Nat → Nat → Option Nat
/-- Ours offers the newest landed tip whenever it is beyond green. -/
def ours : Policy := offer
/-- One checker request whose offered run passes (a failure starts bisection, R12). -/
def serve (policy : Policy) (green tip : Nat) : Nat :=
  match policy green tip with | some tested => advance green tested | none => green
/-- Full post-land runs charged for one offer: `0` for an empty offer (`none`), `1` for `some`. -/
def offerRuns : Option Nat → Nat | none => 0 | some _ => 1

/-- Accumulates per-request `step` cost until `pos` is green; `none` if the schedule ends first. -/
def costUntil (step : Option Nat → Nat) (policy : Policy) (pos : Nat) : Nat → List Nat → Option Nat
  | green, tips =>
    if pos ≤ green then some 0 else
    match tips with
    | [] => none
    | tip :: rest =>
      (costUntil step policy pos (serve policy green tip) rest).map (· + step (policy green tip))
/-- Cost in **checker requests** (every poll until `pos` is green, including empty `none` offers). -/
abbrev requestsUntil := costUntil (fun _ => 1)
/-- Cost in **full post-land runs** (`0` for an empty `none` offer, `1` for `some`) until green. -/
abbrev runsUntil := costUntil offerRuns

/-- Sums a per-platform cost across `required`; `none` if any platform ends before `pos`. -/
def platformSum (cost : Policy → Nat → Nat → List Nat → Option Nat)
    (policy : Nat → Policy) (pos : Nat) (cp : Nat → Nat) (sch : Nat → List Nat) : List Nat → Option Nat
  | [] => some 0
  | p :: rest =>
    match cost (policy p) pos (cp p) (sch p), platformSum cost policy pos cp sch rest with
    | some n, some m => some (n + m)
    | _, _ => none
/-- Total **checker requests** (including empty offers) across every required platform. -/
abbrev platformRequestsUntil := platformSum requestsUntil
/-- Total **full post-land runs** (`0` per empty offer, `1` per run) across required platforms. -/
abbrev platformRunsUntil := platformSum runsUntil

private theorem single_cost_bound (step : Option Nat → Nat) (hstep : ∀ o, step o ≤ 1) (pos : Nat) :
    ∀ (tips : List Nat) (green i : Nat) (h : i < tips.length), pos ≤ tips[i] →
      ∃ n, costUntil step ours pos green tips = some n ∧ n ≤ i + 1
  | [], _, _, h, _ => absurd h (by simp)
  | tip :: rest, green, i, h, hp => by
    unfold costUntil
    by_cases hg : pos ≤ green
    · exact ⟨0, by simp [hg], by omega⟩
    · simp only [hg, ite_false]; cases i with
      | zero =>
        have hpos : pos ≤ serve ours green tip := by
          simp only [List.getElem_cons_zero] at hp
          by_cases hlt : green < tip <;> simp [serve, ours, offer, hlt, advance] <;> omega
        have := hstep (ours green tip); exact ⟨step (ours green tip), by unfold costUntil; simp [hpos], by omega⟩
      | succ j =>
        obtain ⟨n, hn, _⟩ := single_cost_bound step hstep pos rest (serve ours green tip) j
          (by simpa using h) (by simpa using hp)
        have := hstep (ours green tip); exact ⟨n + step (ours green tip), by simp [hn], by omega⟩
private theorem multi_cost_bound (cost : Policy → Nat → Nat → List Nat → Option Nat)
    (pos : Nat) (cp : Nat → Nat) (sch : Nat → List Nat) (idx : Nat → Nat)
    (single : ∀ tips green i (h : i < tips.length), pos ≤ tips[i] →
      ∃ n, cost ours pos green tips = some n ∧ n ≤ i + 1) :
    ∀ req : List Nat, (∀ p ∈ req, ∃ h : idx p < (sch p).length, pos ≤ (sch p)[idx p]) →
      ∃ total, platformSum cost (fun _ => ours) pos cp sch req = some total ∧
        total ≤ (req.map fun p => idx p + 1).sum
  | [], _ => ⟨0, rfl, Nat.zero_le _⟩
  | p :: rest, hall => by
    obtain ⟨hp_lt, hp_ge⟩ := hall p (by simp)
    obtain ⟨n, hn, _⟩ := single (sch p) (cp p) (idx p) hp_lt hp_ge
    obtain ⟨m, hm, _⟩ := multi_cost_bound cost pos cp sch idx single rest (fun q hq => hall q (by simp [hq]))
    exact ⟨n + m, by simp [platformSum, hn, hm], by simp; omega⟩
private theorem runs_le_requests (policy : Policy) (pos : Nat) :
    ∀ (tips : List Nat) (green n : Nat), requestsUntil policy pos green tips = some n →
      ∃ r, runsUntil policy pos green tips = some r ∧ r ≤ n
  | tips, green, n, h => by
    by_cases hp : pos ≤ green
    · unfold costUntil at h; simp [hp] at h; exact ⟨0, by unfold costUntil; simp [hp], by omega⟩
    · cases tips with
      | nil => unfold costUntil at h; simp [hp] at h
      | cons tip rest =>
        unfold costUntil at h; simp only [hp, ite_false, Option.map_eq_some_iff] at h
        obtain ⟨k, hk, rfl⟩ := h
        obtain ⟨r, hr, _⟩ := runs_le_requests policy pos rest (serve policy green tip) k hk
        have : offerRuns (policy green tip) ≤ 1 := by cases policy green tip <;> simp [offerRuns]
        exact ⟨r + offerRuns (policy green tip), by unfold costUntil; simp [hp, hr], by omega⟩

/-- **cost_bound** (spec/R12-post-land-verification.md R12 Batches & Platforms). Two explicit cost
units while runs pass: **checker requests** (`requestsUntil`, `platformRequestsUntil`, counting
every poll including empty `none` offers) and **full post-land runs** (`runsUntil`,
`platformRunsUntil`, charging `1` per `some` offer and `0` per empty `none` offer). Both stay
within `i + 1` (single-stream) and `∑ (idx p + 1)` (multi-platform); an empty offer adds `0` runs
and `1` request, and full runs never exceed checker requests. -/
-- spec/R12-post-land-verification.md R12
theorem cost_bound (pos : Nat) :
    (∀ (tips : List Nat) (green i : Nat) (h : i < tips.length), pos ≤ tips[i] →
      (∃ n, requestsUntil ours pos green tips = some n ∧ n ≤ i + 1) ∧
      (∃ r, runsUntil ours pos green tips = some r ∧ r ≤ i + 1)) ∧
    (∀ (req : List Nat) (cp : Nat → Nat) (sch : Nat → List Nat) (idx : Nat → Nat),
      (∀ p ∈ req, ∃ h : idx p < (sch p).length, pos ≤ (sch p)[idx p]) →
      (∃ t, platformRequestsUntil (fun _ => ours) pos cp sch req = some t ∧ t ≤ (req.map (idx · + 1)).sum) ∧
      (∃ t, platformRunsUntil (fun _ => ours) pos cp sch req = some t ∧ t ≤ (req.map (idx · + 1)).sum)) ∧
    (∀ (policy : Policy) (green tip : Nat) (rest : List Nat), ¬pos ≤ green → policy green tip = none →
      runsUntil policy pos green (tip :: rest) = runsUntil policy pos green rest ∧
      requestsUntil policy pos green (tip :: rest) = (requestsUntil policy pos green rest).map (· + 1)) ∧
    (∀ (policy : Policy) (tips : List Nat) (green n : Nat),
      requestsUntil policy pos green tips = some n →
      ∃ r, runsUntil policy pos green tips = some r ∧ r ≤ n) := by
  have h1 : ∀ o : Option Nat, (fun _ : Option Nat => 1) o ≤ 1 := fun _ => Nat.le_refl 1
  have hr : ∀ o : Option Nat, offerRuns o ≤ 1 := fun o => by cases o <;> simp [offerRuns]
  refine ⟨fun tips g i h hp => ⟨single_cost_bound _ h1 pos tips g i h hp, single_cost_bound _ hr pos tips g i h hp⟩,
    fun req cp sch idx h => ⟨multi_cost_bound _ pos cp sch idx (single_cost_bound _ h1 pos) req h,
      multi_cost_bound _ pos cp sch idx (single_cost_bound _ hr pos) req h⟩,
    ?_, fun p => runs_le_requests p pos⟩
  intro p g t rest hpos hnone
  have hstep : ∀ s, costUntil s p pos g (t :: rest) = (costUntil s p pos g rest).map (· + s none) := by
    intro s; conv => lhs; rw [costUntil]
    simp [hpos, serve, hnone]
  refine ⟨?_, hstep (fun _ => 1)⟩
  change costUntil offerRuns p pos g (t :: rest) = costUntil offerRuns p pos g rest
  rw [hstep offerRuns]; cases costUntil offerRuns p pos g rest <;> rfl

/-- Policies that offer only commits already landed (`tested ≤ tip`). -/
def Landed (policy : Policy) : Prop :=
  ∀ green tip tested, policy green tip = some tested → tested ≤ tip
/-- Work-conserving policies: whenever untested commits exist (`green < tip`), the policy offers
a run rather than deferring with `none`. -/
def WorkConserving (policy : Policy) : Prop :=
  ∀ green tip, green < tip → ∃ tested, policy green tip = some tested

private theorem serve_le (alt : Policy) (h : Landed alt) (g g' tip : Nat) (hg : g' ≤ g) :
    serve alt g' tip ≤ serve ours g tip := by
  have hours : serve ours g tip = max g tip := by
    by_cases hlt : g < tip <;> simp [serve, ours, offer, hlt, advance] <;> omega
  rw [hours]; unfold serve; split
  · rename_i tested ht; have := h g' tip tested ht; simp [advance]; omega
  · omega
private theorem single_optimal (step : Option Nat → Nat) (alt : Policy) (landed : Landed alt)
    (hstep : ∀ g g' tip, g' ≤ g → step (ours g tip) ≤ step (alt g' tip)) (pos : Nat) :
    ∀ (tips : List Nat) (green green' n : Nat), green' ≤ green →
      costUntil step alt pos green' tips = some n →
      ∃ m, costUntil step ours pos green tips = some m ∧ m ≤ n
  | tips, green, green', n, hg, ha => by
    by_cases hp : pos ≤ green
    · exact ⟨0, by unfold costUntil; simp [hp], Nat.zero_le _⟩
    · have hp' : ¬pos ≤ green' := by omega
      cases tips with
      | nil => unfold costUntil at ha; simp [hp'] at ha
      | cons tip rest =>
        unfold costUntil at ha; simp only [hp', ite_false, Option.map_eq_some_iff] at ha
        obtain ⟨k, hk, rfl⟩ := ha
        obtain ⟨m, hm, _⟩ := single_optimal step alt landed hstep pos rest
          (serve ours green tip) (serve alt green' tip) k (serve_le alt landed green green' tip hg) hk
        have := hstep green green' tip hg; exact ⟨m + step (ours green tip), by unfold costUntil; simp [hp, hm], by omega⟩
private theorem multi_optimal (cost : Policy → Nat → Nat → List Nat → Option Nat)
    (P : Policy → Prop) (alt : Nat → Policy) (pos : Nat) (cp cp' : Nat → Nat) (sch : Nat → List Nat)
    (single : ∀ a, P a → ∀ tips g g' n, g' ≤ g → cost a pos g' tips = some n →
      ∃ m, cost ours pos g tips = some m ∧ m ≤ n) :
    ∀ req n, (∀ p ∈ req, P (alt p)) → (∀ p ∈ req, cp' p ≤ cp p) →
      platformSum cost alt pos cp' sch req = some n →
      ∃ m, platformSum cost (fun _ => ours) pos cp sch req = some m ∧ m ≤ n
  | [], n, _, _, h => by simp [platformSum] at h; exact ⟨0, rfl, by omega⟩
  | p :: rest, n, hp, hle, h => by
    simp only [platformSum] at h
    cases h1 : cost (alt p) pos (cp' p) (sch p) <;> cases h2 : platformSum cost alt pos cp' sch rest <;>
      simp [h1, h2] at h
    obtain ⟨m1, hm1, _⟩ := single (alt p) (hp p (by simp)) (sch p) (cp p) (cp' p) _ (hle p (by simp)) h1
    obtain ⟨m2, hm2, _⟩ := multi_optimal cost P alt pos cp cp' sch single rest _
      (fun q hq => hp q (by simp [hq])) (fun q hq => hle q (by simp [hq])) h2
    exact ⟨m1 + m2, by simp [platformSum, hm1, hm2], by omega⟩

/-- **optimal** (spec/R12-post-land-verification.md R12 Batches & Platforms). Cost units as in
`cost_bound`. In **checker requests** (`requestsUntil`, `platformRequestsUntil`), no `Landed`
policy beats `ours`. In **full post-land runs** (`runsUntil`, `platformRunsUntil`), no `Landed`
and `WorkConserving` policy beats `ours` (deferring policies that return `none` when `green < tip`
are excluded because they trade latency for fewer runs). `ours` is both `Landed` and
`WorkConserving`. -/
-- spec/R12-post-land-verification.md R12
theorem optimal (pos : Nat) :
    (∀ (alt : Policy), Landed alt → ∀ (tips : List Nat) (g g' n : Nat), g' ≤ g →
      requestsUntil alt pos g' tips = some n → ∃ m, requestsUntil ours pos g tips = some m ∧ m ≤ n) ∧
    (∀ (alt : Nat → Policy) (req : List Nat) (cp cp' : Nat → Nat) (sch : Nat → List Nat) (n : Nat),
      (∀ p ∈ req, Landed (alt p)) → (∀ p ∈ req, cp' p ≤ cp p) →
      platformRequestsUntil alt pos cp' sch req = some n →
      ∃ m, platformRequestsUntil (fun _ => ours) pos cp sch req = some m ∧ m ≤ n) ∧
    (∀ (alt : Policy), Landed alt → WorkConserving alt → ∀ (tips : List Nat) (g g' n : Nat), g' ≤ g →
      runsUntil alt pos g' tips = some n → ∃ m, runsUntil ours pos g tips = some m ∧ m ≤ n) ∧
    (∀ (alt : Nat → Policy) (req : List Nat) (cp cp' : Nat → Nat) (sch : Nat → List Nat) (n : Nat),
      (∀ p ∈ req, Landed (alt p) ∧ WorkConserving (alt p)) → (∀ p ∈ req, cp' p ≤ cp p) →
      platformRunsUntil alt pos cp' sch req = some n →
      ∃ m, platformRunsUntil (fun _ => ours) pos cp sch req = some m ∧ m ≤ n) ∧
    (Landed ours ∧ WorkConserving ours) := by
  have hreq := fun alt hl => single_optimal (fun _ => 1) alt hl (fun _ _ _ _ => Nat.le_refl 1) pos
  have hrun := fun alt hl (hwc : WorkConserving alt) => single_optimal offerRuns alt hl (fun g g' tip hg => by
    by_cases hlt : g < tip
    · obtain ⟨t, ht⟩ := hwc g' tip (by omega); simp [ours, offer, hlt, ht, offerRuns]
    · simp [ours, offer, hlt, offerRuns]) pos
  exact ⟨hreq,
    fun alt req cp cp' sch n hl hle h => multi_optimal _ Landed alt pos cp cp' sch hreq req n hl hle h,
    hrun,
    fun alt req cp cp' sch n hp hle h =>
      multi_optimal _ (fun a => Landed a ∧ WorkConserving a) alt pos cp cp' sch (fun a ha => hrun a ha.1 ha.2) req n hp hle h,
    ⟨fun g t x h => by by_cases hlt : g < t <;> simp [ours, offer, hlt] at h; omega,
     fun g t hlt => ⟨t, by simp [ours, offer, hlt]⟩⟩⟩

#print axioms correct
#print axioms cost_bound
#print axioms optimal
end BatchGreen
