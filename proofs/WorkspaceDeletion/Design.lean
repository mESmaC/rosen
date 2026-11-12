/-!
# Workspace deletion guard (SPEC.md R4)

An edit is a version, not merely a path: storing an earlier version does not
make a later write durable. Evidence is collected after the final flush and
ordered journal replay. The complete-evidence assumption says every relevant
local version is either authoritative-server stored or represented among
watcher writes, journal records or final content-hash observations.
Stability from this observation through removal is an explicit model
precondition. The snapshot writer lease does not exclude arbitrary tools
writing files. Ignored disk output is outside the durability contract.

The model does not prove the OS watcher, hash function, transport, locking or
filesystem traversal correct. Its server list must describe acknowledged
snapshots on the authoritative server, not a cache or a workspace marker.
-/
namespace WorkspaceDeletion

structure Evidence (Edit : Type) where
  dirty : List Edit
  journal : List Edit
  disk : List Edit
  ignored : Edit → Bool

/-- Versions whose durability must be checked, including a final hash scan. -/
def observations (e : Evidence Edit) : List Edit := e.dirty ++ e.journal ++ e.disk

/-- Refusals name each observed relevant version absent from the server. -/
def pending [DecidableEq Edit] (e : Evidence Edit) (server : List Edit) : List Edit :=
  (observations e).filter fun edit => !e.ignored edit && decide (edit ∉ server)

/-- Deletion keeps server objects forever and removes local files only on success. -/
def remaining [DecidableEq Edit] (e : Evidence Edit) (server workspaceEdits : List Edit) : List Edit :=
  if pending e server = [] then [] else workspaceEdits

/-- SPEC.md R4: after any accepted or refused deletion, every relevant edit
that existed locally remains either on the server or in the workspace.
The evidence must cover every unstored local version after the final flush;
crashes and offline periods may leave any distribution between these lists.
Every reported refusal is a relevant version absent from the server. -/
theorem correct [DecidableEq Edit] (e : Evidence Edit) (server workspaceEdits : List Edit)
    (complete : ∀ edit ∈ workspaceEdits, e.ignored edit = false →
      edit ∈ server ∨ edit ∈ observations e) :
    (∀ edit ∈ workspaceEdits, e.ignored edit = false →
      edit ∈ server ∨ edit ∈ remaining e server workspaceEdits) ∧
    (∀ edit ∈ pending e server, edit ∈ observations e ∧
      e.ignored edit = false ∧ edit ∉ server) := by
  constructor
  · intro edit hl hi
    by_cases accepted : pending e server = []
    · by_cases hs : edit ∈ server
      · exact Or.inl hs
      · have ho := (complete edit hl hi).resolve_left hs
        have hp : edit ∈ pending e server := by simp [pending, ho, hi, hs]
        have impossible : False := by simp [accepted] at hp
        exact False.elim impossible
    · exact Or.inr (by simp [remaining, accepted, hl])
  · intro edit hp
    simpa [pending, List.mem_filter, Bool.and_eq_true] using hp

/-- One unit per dirty/journal record checked; one unit plus its bytes per
hash-scanned file. This excludes network flush latency and filesystem calls. -/
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

/-- Under the record-and-byte cost model stated in `scanCost`, a final guard
costs at most d+j+n(B+1), where d watcher records, j journal records and n
files are examined and each file contains at most B bytes. -/
theorem cost_bound (e : Evidence Edit) (bytes : Edit → Nat) (maxBytes : Nat)
    (bounded : ∀ edit ∈ e.disk, bytes edit ≤ maxBytes) :
    scanCost e bytes ≤ e.dirty.length + e.journal.length + e.disk.length * (maxBytes + 1) := by
  have := hash_cost_le e.disk bytes maxBytes bounded
  unfold scanCost
  omega

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
#print axioms remaining
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





