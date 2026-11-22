/-!
# Workspace deletion guard (spec/R4-workspace-lifecycle-and-deletion.md R4)

An Edit identifies both a filesystem object and its content version; a path
alone cannot distinguish a replacement from the object previously verified.
Evidence contains watcher writes, pending journal records and observations
from final content verification against acknowledged authoritative snapshots.
Ignored status is fixed at that guard observation; ignored output lies
outside the durability contract.

The removal primitive must keep held bytes stable through verification and
object disposal, delete only that exact verified object, and retain objects
outside its verified set. Nonrecursive directory removal preserves later
additions. `removed` is any completed prefix of such object removals; `later`
represents concurrent additions and replacements retained by the primitive.
No whole-directory stability or absence of concurrent writers is assumed.
The theorem covers partial cleanup, refusal and interruption, not just success.

The model does not verify the OS handle API, hash function, transport or
filesystem traversal. Object identity, held-byte exclusion and preservation
of unverified objects are implementation obligations. Its server list means
acknowledged authoritative snapshots, never a cache or workspace marker.
-/
namespace WorkspaceDeletion

structure Evidence (Edit : Type) where
  dirty : List Edit
  journal : List Edit
  disk : List Edit
  ignored : Edit → Bool

/-- Versions whose durability must be checked, including a final hash scan. -/
def observations (e : Evidence Edit) : List Edit := e.dirty ++ e.journal ++ e.disk

/-- The observed relevant versions absent from the server. -/
def pending [DecidableEq Edit] (e : Evidence Edit) (server : List Edit) : List Edit :=
  (observations e).filter fun edit => !e.ignored edit && decide (edit ∉ server)

/-- Disk evidence on a staged retry. A missing tracked file may already have
been disposed of. For an ignore file removed during a partial deletion, use
its stored rules at the same precedence as the live rules. `live` and `stored`
model the resolved matches after choosing the source of each ignore file.
Journal and watcher evidence are handled separately by the guard. -/
def stagedDisk (disk : List Edit) (present tracked : Edit → Bool)
    (live : Edit → Option Bool) (stored : Edit → Bool) : Evidence Edit :=
  { dirty := []
    journal := []
    disk := disk.filter present
    ignored := fun edit => !(tracked edit || !((live edit).getD (stored edit))) }

/-- A refusal reports the full count and samples the ordered path list. -/
def refusalSummary (paths : List Edit) : Nat × List Edit :=
  (paths.length, paths.take 5)

/-- Only verified originals can be removed. Every completed removal prefix
keeps unselected originals and concurrent later versions; refusals keep both. -/
def remaining [DecidableEq Edit] (e : Evidence Edit)
    (server originals removed later : List Edit) : List Edit :=
  if pending e server = [] then
    originals.filter (fun edit => decide (edit ∉ removed)) ++ later
  else originals ++ later

/-- Classify a markerless staged root's entries without following links.
`stateDir` is reserved for a plain `.rosen` directory. Its flag records
whether it is empty. All other entries, including links, are `other`. -/
inductive ShellEntry where
  | stateDir (empty : Bool)
  | other

def removableShell : List ShellEntry → Bool
  | [] | [.stateDir true] => true
  | _ => false

/-- A registration can be cleared once the boundary has been checked, the
scan accepts the shell, and nonrecursive removal of the root has succeeded.
The last observation can fail even after an accepted scan, for example if a
concurrent write makes the directory nonempty. -/
def markerlessFinished (boundaryClear : Bool) (entries : List ShellEntry)
    (rootRemoved : Bool) : Bool :=
  boundaryClear && removableShell entries && rootRemoved

/-- spec/R4-workspace-lifecycle-and-deletion.md R4: for any prefix of removals of verified object versions,
every relevant original or later concurrent version stays server-stored or
local. Only observed objects may enter that prefix; unobserved objects and
later additions/replacements are retained. Thus complete workspace evidence
is required for full cleanup, but not for safety of partial cleanup.
The refusal reports the full count and the first five pending versions, or
all of them if there are fewer. Every example is relevant and unstored.
A staged retry requires durability for the tracked or unignored versions
still on disk, with live ignore rules taking precedence over stored rules.
After marker removal, registered cleanup can finish only after boundary
validation and successful nonrecursive removal of an empty shell. -/
theorem correct [DecidableEq Edit] (e : Evidence Edit)
    (server originals removed later : List Edit)
    (verified : ∀ edit ∈ removed, edit ∈ observations e) :
    (∀ edit ∈ originals ++ later, e.ignored edit = false →
      edit ∈ server ∨ edit ∈ remaining e server originals removed later) ∧
    (∀ edit ∈ later, edit ∈ remaining e server originals removed later) ∧
    (∀ edit ∈ pending e server, edit ∈ observations e ∧
      e.ignored edit = false ∧ edit ∉ server) ∧
    ((refusalSummary (pending e server)).1 = (pending e server).length ∧
      (refusalSummary (pending e server)).2.length = min 5 (pending e server).length ∧
      List.IsPrefix (refusalSummary (pending e server)).2 (pending e server)) ∧
    (∀ present tracked live stored edit,
      edit ∈ pending (stagedDisk e.disk present tracked live stored) server ↔
      edit ∈ e.disk ∧ present edit = true ∧
        (tracked edit = true ∨ (live edit).getD (stored edit) = false) ∧ edit ∉ server) ∧
    (∀ boundaryClear entries rootRemoved,
      markerlessFinished boundaryClear entries rootRemoved = true ↔
      boundaryClear = true ∧ (entries = [] ∨ entries = [.stateDir true]) ∧
        rootRemoved = true) := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro edit held hi
    rcases List.mem_append.mp held with original | added
    · by_cases accepted : pending e server = []
      · by_cases hs : edit ∈ server
        · exact Or.inl hs
        · have notRemoved : edit ∉ removed := by
            intro hr
            have ho := verified edit hr
            have hp : edit ∈ pending e server := by simp [pending, ho, hi, hs]
            simp [accepted] at hp
          exact Or.inr (by simp [remaining, accepted, original, notRemoved])
      · exact Or.inr (by simp [remaining, accepted, original])
    · apply Or.inr
      by_cases accepted : pending e server = [] <;> simp [remaining, accepted, added]
  · intro edit added
    by_cases accepted : pending e server = [] <;> simp [remaining, accepted, added]
  · intro edit hp
    simpa [pending, List.mem_filter, Bool.and_eq_true] using hp
  · exact ⟨rfl, List.length_take, List.take_prefix _ _⟩
  · intro present tracked live stored edit
    simp [pending, observations, stagedDisk, List.mem_filter, Bool.and_eq_true,
      Bool.or_eq_true, and_left_comm, and_comm]
  · intro boundaryClear entries rootRemoved
    cases entries with
    | nil => simp [markerlessFinished, removableShell]
    | cons entry tail =>
      cases tail with
      | nil =>
        cases entry with
        | other => simp [markerlessFinished, removableShell]
        | stateDir empty => cases empty <;> simp [markerlessFinished, removableShell]
      | cons next rest =>
        cases entry <;> simp [markerlessFinished, removableShell]
/-- One unit per dirty/journal record checked; one unit plus its bytes per
hash-scanned file, for one evidence-validation pass. This primitive model
excludes set/list lookup, traversal, OS metadata/handle operations, actual
object/directory disposal, network flushes, retries and their latency. -/
def scanCost (e : Evidence Edit) (bytes : Edit → Nat) : Nat :=
  e.dirty.length + e.journal.length + (e.disk.map fun edit => bytes edit + 1).sum

private theorem hash_cost_le (disk : List Edit) (bytes : Edit → Nat) (limit : Nat)
    (bounded : ∀ edit ∈ disk, bytes edit ≤ limit) :
    (disk.map fun edit => bytes edit + 1).sum ≤ disk.length * (limit + 1) := by
  induction disk with
  | nil => simp
  | cons head tail ih =>
    have hh := bounded head (by simp)
    have ht := ih (fun edit he => bounded edit (by simp [he]))
    simp only [List.map_cons, List.sum_cons, List.length_cons, Nat.succ_mul]
    omega

/-- Under scanCost's evidence-validation model (record probes plus hash bytes,
excluding traversal, lookup, OS metadata/handles, disposal and network I/O),
one pass costs at most d+j+n(B+1), with d dirty records, j journal records
and n files of at most B bytes. Repeating that same validation p times costs
at most p times this bound; this is not a bound on whole cleanup runtime. -/
theorem cost_bound (e : Evidence Edit) (bytes : Edit → Nat) (maxBytes : Nat)
    (bounded : ∀ edit ∈ e.disk, bytes edit ≤ maxBytes) :
    (scanCost e bytes ≤ e.dirty.length + e.journal.length + e.disk.length * (maxBytes + 1)) ∧
    (∀ passes : Nat, passes * scanCost e bytes ≤
      passes * (e.dirty.length + e.journal.length + e.disk.length * (maxBytes + 1))) := by
  have hashes := hash_cost_le e.disk bytes maxBytes bounded
  have bound : scanCost e bytes ≤
      e.dirty.length + e.journal.length + e.disk.length * (maxBytes + 1) := by
    unfold scanCost
    omega
  exact ⟨bound, fun passes => Nat.mul_le_mul_left passes bound⟩
/-- Three independent black-box evidence categories after filtering ignored output. -/
structure Flags where
  dirty : Bool
  journal : Bool
  mismatch : Bool
  deriving DecidableEq

def clear : Flags := ⟨false, false, false⟩

def permits (f : Flags) : Bool := !f.dirty && !f.journal && !f.mismatch

/-- Unqueried categories reveal no information. A mask describes the fixed
queries of an alternative certificate procedure; arbitrary computation of
observed values is free in this cost model. -/
def observe (mask value : Flags) : Flags :=
  ⟨mask.dirty && value.dirty, mask.journal && value.journal,
   mask.mismatch && value.mismatch⟩

def probeCost (mask : Flags) : Nat :=
  (if mask.dirty then 1 else 0) + (if mask.journal then 1 else 0) +
  (if mask.mismatch then 1 else 0)

/-- Class of alternatives: fixed-mask black-box certificate procedures that
accept every safe state and never accept an unsafe state. No relationships
between dirty, journal and mismatch observations may be assumed. -/
def ExactCertificate (mask : Flags) (decideFrom : Flags → Bool) : Prop :=
  ∀ value, decideFrom (observe mask value) = permits value

/-- Cost model: one unit per category query, ignoring the cost within a
category. Among exact fixed-mask black-box certificate procedures, ours
uses three queries, and no alternative has a smaller query cost. Omitting
any category makes an unsafe singleton indistinguishable from the clear state. -/
theorem optimal :
    ExactCertificate ⟨true, true, true⟩ permits ∧
    probeCost ⟨true, true, true⟩ = 3 ∧
    ∀ mask decideFrom, ExactCertificate mask decideFrom → 3 ≤ probeCost mask := by
  refine ⟨?_, rfl, ?_⟩
  · intro value
    simp [observe]
  · intro mask decideFrom certificate
    have hc := certificate clear
    have hd := certificate ⟨true, false, false⟩
    have hj := certificate ⟨false, true, false⟩
    have hm := certificate ⟨false, false, true⟩
    rcases mask with ⟨dirty, journal, mismatch⟩
    cases dirty <;> cases journal <;> cases mismatch <;>
      simp_all [observe, clear, permits, probeCost]

#print axioms observations
#print axioms pending
#print axioms stagedDisk
#print axioms refusalSummary
#print axioms remaining
#print axioms ShellEntry
#print axioms removableShell
#print axioms markerlessFinished
#print axioms correct
#print axioms scanCost
#print axioms hash_cost_le
#print axioms cost_bound
#print axioms clear
#print axioms permits
#print axioms observe
#print axioms probeCost
#print axioms ExactCertificate
#print axioms optimal

end WorkspaceDeletion

