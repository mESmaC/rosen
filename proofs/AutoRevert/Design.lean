/-!
# Design proof: auto-revert (spec/R12-post-land-verification.md R12)
Atomic inverse land, mode/advisory disposition, and platform-specific failure
records sharing a culprit-keyed inverse. The immutable object merge is the
existing ThreeWayMerge model; this model covers CAS, idempotence, reopening,
disposition gating, and culprit-keyed inverse reuse across platforms. -/
namespace AutoRevert
structure State where
  base : Nat
  reverted : List Nat
  openChange : Nat → Bool
  snapshots : Nat → List Nat
  owner : Nat → Nat
  output : Nat → String

def gate (already moved clean : Bool) : Bool := !already && !moved && clean

/-- Reopening waits until the original land's last post-land mutation. -/
def readyGate (already moved clean recovered : Bool) : Bool :=
  recovered && gate already moved clean

/-- R12 disposition: advisory platforms record failures without reverting;
required/unscoped platforms revert builds in all modes and tests in before-land mode. -/
def requiresRevert (beforeLand isBuild isAdvisory : Bool) : Bool :=
  !isAdvisory && (beforeLand || isBuild)

def apply (s : State) (expected culprit change commit : Nat) (clean : Bool)
    (output : String) : Option State :=
  if gate (s.reverted.contains culprit) (s.base != expected) clean then
    some { s with
      base := commit
      reverted := culprit :: s.reverted
      openChange := fun c => if c = change then true else s.openChange c
      output := fun c => if c = change then output else s.output c }
  else none

structure FailureKey where
  culprit : Nat
  isBuild : Bool
  platform : String
  deriving DecidableEq, Repr

structure FailureRecord where
  key : FailureKey
  requiresRevert : Bool
  revert : Option Nat
  deriving DecidableEq, Repr

structure PlatformState where
  revert : State
  inverses : List (Nat × Nat)
  records : List FailureRecord

def lookupInverse : List (Nat × Nat) → Nat → Option Nat
  | [], _ => none
  | (c, inv) :: rest, culprit => if c = culprit then some inv else lookupInverse rest culprit

def hasFailureKey (records : List FailureRecord) (key : FailureKey) : Bool :=
  records.any (·.key == key)

def insertFailure (records : List FailureRecord) (key : FailureKey) (req : Bool) :
    List FailureRecord :=
  if hasFailureKey records key then records
  else records ++ [{ key := key, requiresRevert := req, revert := none }]

def linkInverse (records : List FailureRecord) (culprit inv : Nat) : List FailureRecord :=
  records.map fun r =>
    if r.key.culprit = culprit ∧ r.requiresRevert = true then { r with revert := some inv } else r

/-- Records one `(culprit, kind, platform)` failure and either reuses an existing
culprit-keyed inverse, lands a new inverse once, or leaves the base untouched.
The returned boolean is `true` iff a new inverse commit moved the base. -/
def recordPlatformFailure (ps : PlatformState) (beforeLand isBuild isAdvisory : Bool)
    (platform : String) (expected culprit change commit : Nat)
    (clean recovered : Bool) (out : String) : PlatformState × Bool :=
  let req := requiresRevert beforeLand isBuild isAdvisory
  let key : FailureKey := { culprit := culprit, isBuild := isBuild, platform := platform }
  let records₁ := insertFailure ps.records key req
  if !req then ({ ps with records := records₁ }, false)
  else match lookupInverse ps.inverses culprit with
    | some inv => ({ ps with records := linkInverse records₁ culprit inv }, false)
    | none =>
      if recovered then
        match apply ps.revert expected culprit change commit clean out with
        | some t =>
          ({ revert := t, inverses := (culprit, commit) :: ps.inverses,
             records := linkInverse records₁ culprit commit }, true)
        | none => ({ ps with records := records₁ }, false)
      else ({ ps with records := records₁ }, false)

private theorem apply_correct (s : State) (expected culprit change commit : Nat) (clean : Bool)
    (out : String) :
    (apply s expected culprit change commit clean out).isSome =
      gate (s.reverted.contains culprit) (s.base != expected) clean ∧
    (∀ t, apply s expected culprit change commit clean out = some t →
      t.base = commit ∧ t.openChange change = true ∧ t.output change = out ∧
      t.snapshots = s.snapshots ∧ t.owner = s.owner ∧
      (∀ c, c ≠ change → t.openChange c = s.openChange c) ∧
      apply t commit culprit change commit true out = none) := by
  unfold apply
  split
  · constructor
    · simp_all
    · intro t h; cases h; simp [gate]; intro c hn he; exact False.elim (hn he)
  · constructor
    · simp_all
    · intro t h; contradiction

private theorem linkInverse_binds (records : List FailureRecord) (culprit inv : Nat) :
    ∀ rec ∈ linkInverse records culprit inv,
      rec.key.culprit = culprit → rec.requiresRevert = true → rec.revert = some inv := by
  intro rec hmem hculp hreq
  simp only [linkInverse, List.mem_map] at hmem
  obtain ⟨orig, _, rfl⟩ := hmem
  split at hculp
  · split <;> simp_all
  · split at hreq
    · contradiction
    · rename_i hneg _; exact False.elim (hneg ⟨hculp, hreq⟩)

/-- **correct** (spec/R12-post-land-verification.md R12 Auto-revert & Platforms): the original
preservation/replay guarantee holds, the preparation gate permits apply only after post-land
recovery, advisory/non-reverting failures never move the base, failure records deduplicate by
`(culprit, kind, platform)` while keeping distinct platforms separate, and a second platform
confirming an already-reverted culprit reuses the culprit-keyed inverse without moving the base. -/
theorem correct (s : State) (expected culprit change commit : Nat) (clean : Bool)
    (out : String) :
    (apply s expected culprit change commit clean out).isSome =
      gate (s.reverted.contains culprit) (s.base != expected) clean ∧
    (∀ t, apply s expected culprit change commit clean out = some t →
      t.base = commit ∧ t.openChange change = true ∧ t.output change = out ∧
      t.snapshots = s.snapshots ∧ t.owner = s.owner ∧
      (∀ c, c ≠ change → t.openChange c = s.openChange c) ∧
      apply t commit culprit change commit true out = none) ∧
    (∀ recovered, readyGate (s.reverted.contains culprit) (s.base != expected) clean recovered =
      (recovered && (apply s expected culprit change commit clean out).isSome)) ∧
    (∀ beforeLand isBuild isAdvisory,
      requiresRevert beforeLand isBuild isAdvisory = true ↔
      isAdvisory = false ∧ (beforeLand = true ∨ isBuild = true)) ∧
    (∀ ps beforeLand isBuild isAdvisory platform recovered,
      requiresRevert beforeLand isBuild isAdvisory = false →
      let r := recordPlatformFailure ps beforeLand isBuild isAdvisory platform expected culprit change commit clean recovered out
      r.2 = false ∧ r.1.revert = ps.revert ∧ r.1.inverses = ps.inverses) ∧
    (∀ ps beforeLand isBuild isAdvisory platform recovered inv,
      requiresRevert beforeLand isBuild isAdvisory = true →
      lookupInverse ps.inverses culprit = some inv →
      let r := recordPlatformFailure ps beforeLand isBuild isAdvisory platform expected culprit change commit clean recovered out
      r.2 = false ∧ r.1.revert = ps.revert ∧ r.1.inverses = ps.inverses ∧
      ∀ rec ∈ r.1.records, rec.key.culprit = culprit → rec.requiresRevert = true →
        rec.revert = some inv) ∧
    (∀ records key req, hasFailureKey records key = true → insertFailure records key req = records) ∧
    (∀ records key req, hasFailureKey records key = false →
      (insertFailure records key req).length = records.length + 1) := by
  have h := apply_correct s expected culprit change commit clean out
  refine ⟨h.1, h.2, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro recovered; rw [h.1]; rfl
  · intro beforeLand isBuild isAdvisory
    cases beforeLand <;> cases isBuild <;> cases isAdvisory <;> decide
  · intro ps beforeLand isBuild isAdvisory platform recovered hno
    simp [recordPlatformFailure, hno]
  · intro ps beforeLand isBuild isAdvisory platform recovered inv hyes hinv
    refine ⟨by simp [recordPlatformFailure, hyes, hinv],
      by simp [recordPlatformFailure, hyes, hinv],
      by simp [recordPlatformFailure, hyes, hinv], ?_⟩
    intro rec hmem hculp hreq
    simp only [recordPlatformFailure, hyes, Bool.not_true, Bool.false_eq_true, ite_false, hinv] at hmem
    exact linkInverse_binds _ culprit inv rec hmem hculp hreq
  · intro records key req hhas; simp [insertFailure, hhas]
  · intro records key req hhas; simp [insertFailure, hhas]

structure Attempt where
  state : State
  expected : Nat
  clean : Bool

/-- Each observation includes any competing land that preceded its CAS.
The loop actually calls apply, stops at success, and retries refusal up to cap.
Charging three units even for early refusal conservatively covers one inverse
merge, one immutable commit write and one CAS. -/
def retry (culprit change commit : Nat) (out : String) : Nat → List Attempt → Option State × Nat
  | 0, _ => (none, 0)
  | _, [] => (none, 0)
  | cap + 1, a :: rest =>
    match apply a.state a.expected culprit change commit a.clean out with
    | some t => (some t, 3)
    | none =>
      let next := retry culprit change commit out cap rest
      (next.1, 3 + next.2)

/-- Merge/write/CAS units spent when a platform confirms `culprit`: zero when
the failure does not require a revert or reuses an existing culprit-keyed inverse. -/
def platformRetryCost (ps : PlatformState) (beforeLand isBuild isAdvisory : Bool)
    (culprit change commit : Nat) (out : String) (observations : List Attempt) : Nat :=
  if !requiresRevert beforeLand isBuild isAdvisory || (lookupInverse ps.inverses culprit).isSome then 0
  else (retry culprit change commit out 12 observations).2

private theorem retry_bound (culprit change commit : Nat) (out : String)
    (cap : Nat) (observations : List Attempt) :
    (retry culprit change commit out cap observations).2 ≤ 3 * cap := by
  induction cap generalizing observations with
  | zero => simp [retry]
  | succ cap ih =>
    cases observations with
    | nil => simp [retry]
    | cons a rest =>
      simp only [retry]
      split
      · omega
      · have := ih rest; dsimp; omega

/-- **cost_bound**: under the merge/write/CAS cost model in retry, the actual
modeled loop, capped at 12 as in revert.rs, spends at most 36 operation units,
for all competing-base observations and all apply success/refusal outcomes.
A non-reverting platform or a subsequent platform reusing an existing culprit
inverse spends 0 inverse operation units. -/
theorem cost_bound (culprit change commit : Nat) (out : String)
    (observations : List Attempt) :
    (retry culprit change commit out 12 observations).2 ≤ 36 ∧
    (∀ ps beforeLand isBuild isAdvisory,
      platformRetryCost ps beforeLand isBuild isAdvisory culprit change commit out observations ≤ 36 ∧
      (∀ inv, lookupInverse ps.inverses culprit = some inv →
        platformRetryCost ps beforeLand isBuild isAdvisory culprit change commit out observations = 0)) := by
  have hb := retry_bound culprit change commit out 12 observations
  refine ⟨hb, fun ps beforeLand isBuild isAdvisory => ⟨?_, ?_⟩⟩
  · unfold platformRetryCost; split <;> omega
  · intro inv hinv; simp [platformRetryCost, hinv]

/-- Class of alternatives: finite sequences of durable atomic metadata
transactions. Each operation may transform the entire State, so the class
allows every atomic implementation, including ones with stronger primitives.
An empty sequence performs no durable writes. Cost is the sequence length. -/
def execute (operations : List (State → State)) (s : State) : State :=
  operations.foldl (fun state operation => operation state) s

def metadataCost (result : Option State) : Nat := if result.isSome then 1 else 0

/-- **optimal**: for every successful apply which moves the base, among all
sequences of durable metadata transactions achieving its R12 result, apply's
metadata cost is minimal; reusing a culprit-keyed inverse for additional
platforms adds 0 base-moving transactions. -/
theorem optimal (s t : State) (expected culprit change commit : Nat)
    (clean : Bool) (out : String)
    (success : apply s expected culprit change commit clean out = some t)
    (movesBase : commit ≠ s.base) (operations : List (State → State))
    (achieves : execute operations s = t) :
    metadataCost (apply s expected culprit change commit clean out) ≤ operations.length ∧
    execute [fun _ => t] s = t ∧
    (∀ ps beforeLand isBuild isAdvisory platform recovered inv,
      requiresRevert beforeLand isBuild isAdvisory = true →
      lookupInverse ps.inverses culprit = some inv →
      (if (recordPlatformFailure ps beforeLand isBuild isAdvisory platform expected culprit change commit clean recovered out).2 then 1 else 0) = 0) := by
  refine ⟨?_, rfl, ?_⟩
  · have target := (correct s expected culprit change commit clean out).2.1 t success
    cases operations with
    | nil =>
      have unchanged : s = t := achieves
      have same : commit = s.base := by rw [unchanged]; exact target.1.symm
      exact False.elim (movesBase same)
    | cons operation rest => simp [metadataCost, success]
  · intro ps beforeLand isBuild isAdvisory platform recovered inv hyes hinv
    simp [recordPlatformFailure, hyes, hinv]

#print axioms State
#print axioms gate
#print axioms readyGate
#print axioms requiresRevert
#print axioms apply
#print axioms FailureKey
#print axioms FailureRecord
#print axioms PlatformState
#print axioms lookupInverse
#print axioms hasFailureKey
#print axioms insertFailure
#print axioms linkInverse
#print axioms recordPlatformFailure
#print axioms apply_correct
#print axioms Attempt
#print axioms retry
#print axioms platformRetryCost
#print axioms retry_bound
#print axioms execute
#print axioms metadataCost
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end AutoRevert
