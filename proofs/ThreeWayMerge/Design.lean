/-!
# Three-way merge (SPEC R5)

Model of `crates/rosen-merge/src/merge.rs`. Two edit scripts, base → ours and
base → theirs, are walked in step and cut into diff3 chunks: a *stable* chunk
is one base line both sides keep; an *unstable* chunk is the stretch between
two stable chunks, from all three files. Each unstable chunk then resolves to
one side's text or to a conflict carrying base, ours and theirs.

The Rust code reads the scripts as match maps rather than op lists; the
fixture table `proofs/fixtures/three-way-merge.tsv`, exported below, checks
that both cut and resolve the same way.
-/

namespace ThreeWayMerge

/-- One step of a two-way edit script from the base to one side. -/
inductive Op (α : Type) where
  | keep (line : α)
  | del (line : α)
  | ins (line : α)
  deriving Repr

variable {α : Type} [DecidableEq α]

/-- The base lines an op consumes. -/
def Op.base : Op α → List α
  | .keep l => [l]
  | .del l => [l]
  | .ins _ => []

/-- The side lines an op produces. -/
def Op.side : Op α → List α
  | .keep l => [l]
  | .del _ => []
  | .ins l => [l]

def baseOf (script : List (Op α)) : List α := script.flatMap Op.base
def sideOf (script : List (Op α)) : List α := script.flatMap Op.side

inductive Chunk (α : Type) where
  | stable (line : α)
  | unstable (base ours theirs : List α)
  deriving Repr

def Chunk.baseText : Chunk α → List α
  | .stable l => [l]
  | .unstable b _ _ => b

def Chunk.oursText : Chunk α → List α
  | .stable l => [l]
  | .unstable _ o _ => o

def Chunk.theirsText : Chunk α → List α
  | .stable l => [l]
  | .unstable _ _ t => t

/-- The unstable chunk being collected: base, ours and theirs so far. -/
structure Pending (α : Type) where
  base : List α
  ours : List α
  theirs : List α

/-- Closes the pending chunk; an empty one emits nothing. -/
def flush (p : Pending α) : List (Chunk α) :=
  if p.base = [] ∧ p.ours = [] ∧ p.theirs = [] then [] else [.unstable p.base p.ours p.theirs]

/-- Emits `done` chunks before the rest of the walk, counting one step. -/
def tick (done : List (Chunk α)) (rest : List (Chunk α) × Nat) : List (Chunk α) × Nat :=
  (done ++ rest.1, rest.2 + 1)

/-- Cuts the chunks. The `Nat` counts recursive steps: the cost model of
`cost_bound`. A stable chunk needs both scripts to keep the same line;
insertions are consumed before base lines, so a chunk closes at the first base
line both sides keep, as in `partition` in `merge.rs`. -/
def chunk : List (Op α) → List (Op α) → Pending α → List (Chunk α) × Nat
  | .keep x :: os, .keep y :: ts, p =>
    if x = y then tick (flush p ++ [.stable x]) (chunk os ts ⟨[], [], []⟩)
    else tick [] (chunk os ts ⟨p.base ++ [x], p.ours ++ [x], p.theirs ++ [y]⟩)
  | .ins x :: os, ts, p => tick [] (chunk os ts ⟨p.base, p.ours ++ [x], p.theirs⟩)
  | os, .ins y :: ts, p => tick [] (chunk os ts ⟨p.base, p.ours, p.theirs ++ [y]⟩)
  | o :: os, t :: ts, p =>
    tick [] (chunk os ts ⟨p.base ++ o.base, p.ours ++ o.side, p.theirs ++ t.side⟩)
  | o :: os, [], p => tick [] (chunk os [] ⟨p.base ++ o.base, p.ours ++ o.side, p.theirs⟩)
  | [], t :: ts, p => tick [] (chunk [] ts ⟨p.base ++ t.base, p.ours, p.theirs ++ t.side⟩)
  | [], [], p => (flush p, 0)
termination_by os ts => os.length + ts.length

inductive Region (α : Type) where
  | clean (lines : List α)
  | conflict (base ours theirs : List α)
  deriving Repr, DecidableEq

def Region.isConflict : Region α → Bool
  | .clean _ => false
  | .conflict .. => true

/-- `resolve_chunk` in `merge.rs`. -/
def resolve : Chunk α → Region α
  | .stable l => .clean [l]
  | .unstable b o t =>
    if o = b ∨ o = t then .clean t else if t = b then .clean o else .conflict b o t

def merge (ours theirs : List (Op α)) : List (Region α) :=
  (chunk ours theirs ⟨[], [], []⟩).1.map resolve

/-- SPEC R5 for one chunk: a clean region is the whole text of a side whose
counterpart left the chunk as in the base (or made the same edit), so no
kept line is dropped or duplicated; anything else is a conflict holding all
three texts, never a silent pick. -/
def Honest : Chunk α → Region α → Prop
  | .stable l, r => r = .clean [l]
  | .unstable b o t, .clean r => (o = b ∧ r = t) ∨ (t = b ∧ r = o) ∨ (o = t ∧ r = o)
  | .unstable b o t, .conflict b' o' t' =>
    b' = b ∧ o' = o ∧ t' = t ∧ o ≠ b ∧ t ≠ b ∧ o ≠ t

/-! ### Lemmas toward `correct` -/

omit [DecidableEq α] in
theorem flush_texts (p : Pending α) :
    (flush p).flatMap Chunk.baseText = p.base ∧
    (flush p).flatMap Chunk.oursText = p.ours ∧
    (flush p).flatMap Chunk.theirsText = p.theirs := by
  unfold flush
  split <;> simp_all [Chunk.baseText, Chunk.oursText, Chunk.theirsText]

theorem chunk_ours (os ts : List (Op α)) (p : Pending α) :
    (chunk os ts p).1.flatMap Chunk.oursText = p.ours ++ sideOf os := by
  fun_induction chunk os ts p <;>
    simp_all [tick, sideOf, Op.side, Chunk.oursText, (flush_texts _).2.1]

theorem chunk_theirs (os ts : List (Op α)) (p : Pending α) :
    (chunk os ts p).1.flatMap Chunk.theirsText = p.theirs ++ sideOf ts := by
  fun_induction chunk os ts p <;>
    simp_all [tick, sideOf, Op.side, Chunk.theirsText, (flush_texts _).2.2]

theorem chunk_base (os ts : List (Op α)) (p : Pending α) (h : baseOf os = baseOf ts) :
    (chunk os ts p).1.flatMap Chunk.baseText = p.base ++ baseOf os := by
  fun_induction chunk os ts p
  case case5 o os t ts p _ _ _ ih =>
    cases o <;> cases t <;> simp_all [tick, baseOf, Op.base, Op.side]
  all_goals simp_all [tick, baseOf, Op.base, Chunk.baseText, (flush_texts _).1]

theorem resolve_honest (c : Chunk α) : Honest c (resolve c) := by
  cases c with
  | stable l => rfl
  | unstable b o t =>
    simp only [resolve]
    split
    · rename_i h; rcases h with h | h <;> simp_all [Honest]
    · split <;> simp_all [Honest, eq_comm]

/-! ### Theorems -/

/-- **Correctness (SPEC R5, "Automatic merge and conflicts").** For any two
edit scripts from one base: the chunks partition base, ours and theirs exactly
(no line dropped or duplicated), and each output region is `Honest` for its
chunk. -/
theorem correct (os ts : List (Op α)) (h : baseOf os = baseOf ts) :
    let cs := (chunk os ts ⟨[], [], []⟩).1
    cs.flatMap Chunk.baseText = baseOf os ∧
    cs.flatMap Chunk.oursText = sideOf os ∧
    cs.flatMap Chunk.theirsText = sideOf ts ∧
    (merge os ts).length = cs.length ∧
    ∀ i (hc : i < cs.length) (hr : i < (merge os ts).length),
      Honest cs[i] (merge os ts)[i] := by
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · simpa using chunk_base os ts ⟨[], [], []⟩ h
  · simpa using chunk_ours os ts ⟨[], [], []⟩
  · simpa using chunk_theirs os ts ⟨[], [], []⟩
  · simp [merge]
  · intro i hc hr
    simp only [merge, List.getElem_map]
    exact resolve_honest _

/-- **Cost bound.** Cost model: one unit per recursive step of `chunk`, each of
which reads at most one op from each script (the Rust partition advances its
three line cursors the same way). Cutting the chunks costs at most the total
length of the two edit scripts. -/
theorem cost_bound (os ts : List (Op α)) (p : Pending α) :
    (chunk os ts p).2 ≤ os.length + ts.length := by
  fun_induction chunk os ts p <;> simp_all [tick] <;> omega

/-- The number of conflict regions in a merge result. -/
def conflictCount (regions : List (Region α)) : Nat := regions.countP (·.isConflict)

omit [DecidableEq α] in
/-- Lemma toward `optimal`: a pointwise "conflict implies conflict" between two
resolvers bounds their conflict counts. -/
theorem conflictCount_map_mono {β : Type} (cs : List β) (f g : β → Region α)
    (step : ∀ c, (f c).isConflict = true → (g c).isConflict = true) :
    conflictCount (cs.map f) ≤ conflictCount (cs.map g) := by
  induction cs with
  | nil => simp [conflictCount]
  | cons c cs ih =>
    unfold conflictCount at ih ⊢
    rw [List.map_cons, List.map_cons, List.countP_cons, List.countP_cons]
    have := step c
    by_cases hf : (f c).isConflict = true <;> by_cases hg : (g c).isConflict = true <;>
      simp only [hf, hg, ↓reduceIte] <;> simp_all <;> omega

/-- Lemma toward `optimal`: `resolve` reports a conflict only where every
sound resolver must. -/
theorem resolve_conflict_forced (other : Chunk α → Region α)
    (sound : ∀ c r, other c = .clean r → Honest c (.clean r)) (c : Chunk α)
    (hr : (resolve c).isConflict = true) : (other c).isConflict = true := by
  cases ho : other c with
  | conflict => rfl
  | clean r =>
    have hon := sound c r ho
    cases c with
    | stable l => simp [resolve, Region.isConflict] at hr
    | unstable b o t =>
      simp only [resolve] at hr
      split at hr
      · simp [Region.isConflict] at hr
      · split at hr
        · simp [Region.isConflict] at hr
        · simp_all [Honest]

/-- **Optimality.** Cost model: the number of conflict regions an agent must
resolve. Class of alternatives: every chunk-wise resolver over the same chunks
that never picks silently, i.e. whose clean regions are all `Honest`. No such
resolver reports fewer conflicts than `resolve`. -/
theorem optimal (cs : List (Chunk α)) (other : Chunk α → Region α)
    (sound : ∀ c r, other c = .clean r → Honest c (.clean r)) :
    conflictCount (cs.map resolve) ≤ conflictCount (cs.map other) :=
  conflictCount_map_mono cs resolve other (resolve_conflict_forced other sound)

end ThreeWayMerge
