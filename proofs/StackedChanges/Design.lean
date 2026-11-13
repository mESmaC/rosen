/-! Design proof: stacked changes (SPEC.md R8 step 3). Models parent-update
propagation through a stack (`stacks.rs` `reconcile`/`step`) and the land gate
that makes a stack land bottom first. `eligible` is exported to
`fixtures/stacked-changes.tsv`, checked by `stack_selection_matches_proved_fixture`. -/
namespace StackedChanges
/-- The selection rule of `stacks::eligible` and the `step` query. -/
def eligible (closed parentClosed conflicted sameBasis : Bool) : Bool :=
  !closed && !parentClosed && !conflicted && !sameBasis
/-- One change in a stack: its ID and authored commits never change; `basis`
is the parent tree the change was last merged onto (`stack_bases`). -/
structure Node where
  id : Nat
  commits : List Nat
  head : Nat
  basis : Nat
  conflicted : Bool
  closed : Bool
  deriving DecidableEq
/-- The edge from parent `q` to child `c` needs a merge. -/
def pending (q c : Node) : Bool :=
  eligible c.closed q.closed c.conflicted (c.basis == q.head)
section
-- `merge basis onto head` is the tree merge; `none` is a conflict.
variable (merge : Nat → Nat → Nat → Option Nat)
/-- `finish_step`: a clean merge moves head and basis together; a conflict
is stored on the child and leaves its head and basis where they were. -/
def mergeNode (q c : Node) : Node :=
  match merge c.basis q.head c.head with
  | some tree => { c with head := tree, basis := q.head }
  | none => { c with conflicted := true }
def settle (q c : Node) : Node := if pending q c then mergeNode merge q c else c
/-- One `step`: the stack is listed bottom to top under its root `q`, and a
parent is created before its children, so `ORDER BY created_at` takes the
lowest pending edge. `none` when no edge is pending. -/
def step (q : Node) : List Node → Option (List Node)
  | [] => none
  | c :: cs => if pending q c then some (mergeNode merge q c :: cs)
      else (step c cs).map (c :: ·)
/-- `reconcile` with `n` iterations of fuel: the stack, merges and
selection queries run. Each iteration runs one selection query. -/
def loop (q : Node) : Nat → List Node → List Node × Nat × Nat
  | 0, cs => (cs, 0, 0)
  | n + 1, cs =>
    match step merge q cs with
    | none => (cs, 0, 1)
    | some cs' => ((loop q n cs').1, (loop q n cs').2.1 + 1, (loop q n cs').2.2 + 1)
/-- The stack after propagation: each child settles against its parent's
final node. -/
def pass (q : Node) : List Node → List Node
  | [] => []
  | c :: cs => settle merge q c :: pass (settle merge q c) cs
def merges (q : Node) : List Node → Nat
  | [] => 0
  | c :: cs => (if pending q c then 1 else 0) + merges (settle merge q c) cs
/-- Positions, from `off`, of the children that `pass` merges. -/
def changedAt (q : Node) (off : Nat) : List Node → List Nat
  | [] => []
  | c :: cs => (if pending q c then [off] else []) ++ changedAt (settle merge q c) (off + 1) cs
private theorem settle_settled (q c : Node) : pending q (settle merge q c) = false := by
  unfold settle
  split
  · unfold mergeNode
    split <;> simp [pending, eligible]
  · simpa using ‹¬pending q c = true›
private theorem settle_skip (q c : Node) (h : pending q c = false) : settle merge q c = c := by
  simp [settle, h]
private theorem settle_changes (q c : Node) (h : pending q c = true) : settle merge q c ≠ c := by
  simp only [settle, h, ite_true, mergeNode]
  simp only [pending, eligible, Bool.and_eq_true, Bool.not_eq_true', beq_eq_false_iff_ne] at h
  split
  · exact fun he => h.2 (congrArg Node.basis he).symm
  · intro he; have := congrArg Node.conflicted he; simp_all
private theorem loop_skip (q c : Node) (h : pending q c = false) :
    ∀ n cs, loop merge q n (c :: cs) = (c :: (loop merge c n cs).1, (loop merge c n cs).2)
  | 0, _ => rfl
  | n + 1, cs => by
    simp only [loop, step, h]
    cases hs : step merge c cs with
    | none => simp
    | some cs' => simp [loop_skip q c h n cs']
private theorem loop_pass : ∀ (cs : List Node) (q : Node) (n : Nat), cs.length < n →
    loop merge q n cs = (pass merge q cs, merges merge q cs, merges merge q cs + 1)
  | _, _, 0, hn => absurd hn (Nat.not_lt_zero _)
  | [], _, _ + 1, _ => by simp [loop, step, pass, merges]
  | c :: cs, q, n + 1, hn => by
    have hlen : cs.length < n := by simp at hn; omega
    by_cases hp : pending q c = true
    · have hs : mergeNode merge q c = settle merge q c := by simp [settle, hp]
      simp only [loop, step, hp, ite_true, hs,
        loop_skip merge q _ (settle_settled merge q c), loop_pass cs _ n hlen]
      simp [pass, merges, hp]
      omega
    · have hp' : pending q c = false := by simpa using hp
      rw [loop_skip merge q c hp', loop_pass cs c (n + 1) (by omega)]
      simp [pass, merges, settle_skip merge q c hp', hp']
private theorem pass_settled : ∀ (cs : List Node) (q : Node), step merge q (pass merge q cs) = none
  | [], _ => rfl
  | c :: cs, q => by simp [pass, step, settle_settled, pass_settled cs]
private theorem pass_chain : ∀ (cs : List Node) (q : Node),
    pass merge q cs = List.zipWith (settle merge) (q :: pass merge q cs) cs
  | [], _ => by simp [pass]
  | c :: cs, q => by
    rw [pass, List.zipWith_cons_cons]
    exact congrArg _ (pass_chain cs _)
private theorem pass_ids : ∀ (cs : List Node) (q : Node),
    (pass merge q cs).map (fun c => (c.id, c.commits)) = cs.map (fun c => (c.id, c.commits))
  | [], _ => rfl
  | c :: cs, q => by
    have hc : (settle merge q c).id = c.id ∧ (settle merge q c).commits = c.commits := by
      unfold settle mergeNode; split
      · split <;> simp
      · simp
    simp [pass, hc, pass_ids cs]
private theorem merges_le : ∀ (cs : List Node) (q : Node), merges merge q cs ≤ cs.length
  | [], _ => Nat.le_refl 0
  | c :: cs, q => by
    have := merges_le cs (settle merge q c)
    simp only [merges, List.length_cons]
    split <;> omega
private theorem changedAt_ge : ∀ (cs : List Node) (q : Node) (off k : Nat),
    k ∈ changedAt merge q off cs → off ≤ k
  | [], _, _, _, h => by simp [changedAt] at h
  | c :: cs, q, off, k, h => by
    simp only [changedAt, List.mem_append] at h
    rcases h with h | h
    · by_cases hp : pending q c = true <;> simp [hp] at h <;> omega
    · have := changedAt_ge cs _ (off + 1) k h
      omega
private theorem changedAt_nodup : ∀ (cs : List Node) (q : Node) (off : Nat),
    (changedAt merge q off cs).Nodup
  | [], _, _ => by simp [changedAt]
  | c :: cs, q, off => by
    have ih := changedAt_nodup cs (settle merge q c) (off + 1)
    by_cases hp : pending q c = true
    · simp only [changedAt, hp, ite_true, List.singleton_append, List.nodup_cons]
      exact ⟨fun h => by have := changedAt_ge merge cs _ _ _ h; omega, ih⟩
    · simpa [changedAt, hp] using ih
private theorem changedAt_length : ∀ (cs : List Node) (q : Node) (off : Nat),
    (changedAt merge q off cs).length = merges merge q cs
  | [], _, _ => rfl
  | c :: cs, q, off => by
    have ih := changedAt_length cs (settle merge q c) (off + 1)
    by_cases hp : pending q c = true <;> simp [changedAt, merges, hp, ih] <;> omega
/-- A position is listed exactly when propagation changes the child there. -/
private theorem changedAt_iff : ∀ (cs : List Node) (q : Node) (off k : Nat),
    k + off ∈ changedAt merge q off cs ↔ (pass merge q cs)[k]? ≠ cs[k]?
  | [], _, _, _ => by simp [changedAt, pass]
  | c :: cs, q, off, 0 => by
    by_cases hp : pending q c = true
    · simp [changedAt, pass, hp, settle_changes merge q c hp]
    · have hp' : pending q c = false := by simpa using hp
      simp only [changedAt, pass, hp', settle_skip merge q c hp']
      simp only [Bool.false_eq_true, ite_false, List.nil_append, Nat.zero_add,
        List.getElem?_cons_zero, ne_eq, not_true_eq_false, iff_false]
      intro h; have := changedAt_ge merge cs _ _ _ h; omega
  | c :: cs, q, off, k + 1 => by
    have ih := changedAt_iff cs (settle merge q c) (off + 1) k
    rw [show k + (off + 1) = k + 1 + off by omega] at ih
    have hne : k + 1 + off ≠ off := by omega
    by_cases hp : pending q c = true <;>
      simp only [changedAt, pass, hp, List.mem_append, List.getElem?_cons_succ, ← ih,
        ite_true, ite_false, Bool.false_eq_true, List.mem_singleton, hne, false_or,
        List.not_mem_nil]
end
/-- Landed positions of a stack, bottom at `0`. -/
def Prefix (landed : Nat → Bool) : Prop := ∀ i j, i ≤ j → landed j = true → landed i = true
def land (landed : Nat → Bool) (i : Nat) : Nat → Bool := fun j => if j = i then true else landed j
private theorem land_prefix (l : Nat → Bool) (i : Nat) (hp : Prefix l)
    (gate : i = 0 ∨ l (i - 1) = true) : Prefix (land l i) := by
  intro a b hab hb
  simp only [land] at hb ⊢
  by_cases ha : a = i
  · simp [ha]
  · simp only [ha, ite_false]
    by_cases hbi : b = i
    · rcases gate with h0 | hg
      · omega
      · exact hp a (i - 1) (by omega) hg
    · simp only [hbi, ite_false] at hb
      exact hp a b hab hb
/-- **correct** (SPEC.md R8 step 3: stack order, bottom-first landing): with more
iterations than children, the loop ends in `pass` after `merges` merges and a
final empty query, leaving no pending edge; each child settles against its
parent's final node; changed children are distinct and number the merges; IDs
and commits are kept; a conflict keeps the child's head, so propagation stops
there; and landing only after the parent keeps the landed set a prefix. -/
-- SPEC.md §R8
theorem correct (merge : Nat → Nat → Nat → Option Nat) (q : Node) (cs : List Node) :
    (∀ n, cs.length < n →
      loop merge q n cs = (pass merge q cs, merges merge q cs, merges merge q cs + 1)) ∧
    step merge q (pass merge q cs) = none ∧
    pass merge q cs = List.zipWith (settle merge) (q :: pass merge q cs) cs ∧
    (changedAt merge q 0 cs).Nodup ∧ (changedAt merge q 0 cs).length = merges merge q cs ∧
    (pass merge q cs).map (fun c => (c.id, c.commits)) = cs.map (fun c => (c.id, c.commits)) ∧
    (∀ p c : Node, merge c.basis p.head c.head = none → (settle merge p c).head = c.head) ∧
    (∀ (landed : Nat → Bool) (i : Nat), Prefix landed → (i = 0 ∨ landed (i - 1) = true) →
      Prefix (land landed i)) := by
  refine ⟨fun n hn => loop_pass merge cs q n hn, pass_settled merge cs q, pass_chain merge cs q,
    changedAt_nodup merge cs q 0, changedAt_length merge cs q 0, pass_ids merge cs q, ?_,
    land_prefix⟩
  intro p c hm
  unfold settle mergeNode
  split
  · split <;> simp_all
  · rfl
/-- **cost_bound** (SPEC.md R8 step 3). Cost model: `loop`'s merges, each writing
a fixed set of rows (change and `stack_bases`, or conflicts, plus one
`stack_delivery` row), and selection queries; merge CPU and objects excluded.
A pass merges at most once per child and runs one query more than it merges. -/
-- SPEC.md §R8
theorem cost_bound (merge : Nat → Nat → Nat → Option Nat) (q : Node) (cs : List Node)
    (n : Nat) (hn : cs.length < n) :
    (loop merge q n cs).2.1 ≤ cs.length ∧
    (loop merge q n cs).2.2 = (loop merge q n cs).2.1 + 1 := by
  rw [loop_pass merge cs q n hn]
  exact ⟨merges_le merge cs q, rfl⟩
/-- Change records keyed by stack position; `none` is no record. -/
abbrev Store := Nat → Option Node
/-- One write sets one change's record. -/
def put (s : Store) (w : Nat × Option Node) : Store := fun k => if k = w.1 then w.2 else s k
def runWrites (s : Store) (ws : List (Nat × Option Node)) : Store := ws.foldl put s
def toStore (cs : List Node) : Store := fun i => cs[i]?
/-- Our writes: one per merged child, setting its propagated record. -/
def oursWrites (merge : Nat → Nat → Nat → Option Nat) (q : Node) (cs : List Node) :
    List (Nat × Option Node) :=
  (changedAt merge q 0 cs).map fun k => (k, toStore (pass merge q cs) k)
private theorem changed_written :
    ∀ (ws : List (Nat × Option Node)) (s : Store) (k : Nat),
      runWrites s ws k ≠ s k → k ∈ ws.map Prod.fst
  | [], _, _, h => absurd rfl h
  | w :: ws, s, k, h => by
    by_cases hk : k = w.1
    · simp [hk]
    · have := changed_written ws (put s w) k (by
        intro he; apply h
        show runWrites (put s w) ws k = s k
        rw [he]; simp [put, hk])
      simp [this]
private theorem writes_lower (s : Store) (ws : List (Nat × Option Node)) (ks : List Nat)
    (hn : ks.Nodup) (hd : ∀ k ∈ ks, runWrites s ws k ≠ s k) : ks.length ≤ ws.length := by
  have hsub : ks ⊆ ws.map Prod.fst := fun k hk => changed_written ws s k (hd k hk)
  simpa using hn.length_le_of_subset hsub
private theorem run_targets (t : Store) : ∀ (ks : List Nat) (s : Store) (k : Nat),
    runWrites s (ks.map fun k => (k, t k)) k = if k ∈ ks then t k else s k
  | [], _, _ => by simp [runWrites]
  | k' :: ks, s, k => by
    show runWrites (put s (k', t k')) (ks.map fun k => (k, t k)) k = _
    rw [run_targets t ks]
    by_cases h1 : k ∈ ks
    · simp [h1]
    · by_cases h2 : k = k' <;> simp [h1, h2, put]
/-- **optimal** (SPEC.md R8 step 3). Class of alternatives: any sequence of
writes that each set one change's record (in any order, batched or not)
and turn the stack's records into the propagated stack. Our writes are in
the class and number exactly our merges; no write sequence in the class is
shorter, because every merged child's record differs and needs a write. -/
-- SPEC.md §R8
theorem optimal (merge : Nat → Nat → Nat → Option Nat) (q : Node) (cs : List Node) :
    runWrites (toStore cs) (oursWrites merge q cs) = toStore (pass merge q cs) ∧
    (oursWrites merge q cs).length = merges merge q cs ∧
    ∀ ws : List (Nat × Option Node),
      runWrites (toStore cs) ws = toStore (pass merge q cs) → merges merge q cs ≤ ws.length := by
  have hiff := fun k => changedAt_iff merge cs q 0 k
  refine ⟨?_, by simp [oursWrites, changedAt_length], ?_⟩
  · funext k
    rw [oursWrites, run_targets]
    by_cases h : k ∈ changedAt merge q 0 cs
    · simp [h]
    · have heq : (pass merge q cs)[k]? = cs[k]? := by
        have := mt (hiff k).mpr (by simpa using h)
        simpa using this
      simp [h, toStore, heq]
  · intro ws hw
    rw [← changedAt_length merge cs q 0]
    apply writes_lower (toStore cs) ws _ (changedAt_nodup merge cs q 0)
    intro k hk
    rw [hw]
    exact (hiff k).mp (by simpa using hk)
#print axioms eligible
#print axioms Node
#print axioms pending
#print axioms mergeNode
#print axioms settle
#print axioms step
#print axioms loop
#print axioms pass
#print axioms merges
#print axioms changedAt
#print axioms Prefix
#print axioms land
#print axioms put
#print axioms runWrites
#print axioms toStore
#print axioms oursWrites
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end StackedChanges
