/-! Design proof for routing a conflicted land to its change (SPEC.md R8
step 2). Models `landing/conflict_routes.rs`, `ConflictSet::record_merge` and
the row-diff `conflicts::save` in `crates/rosen-server`. -/
namespace ConflictRouting

/-- A store of keyed rows; `none` is an absent row. -/
abbrev Store (K V : Type) := K → Option V

/-- One write sets one keyed row: an INSERT, a one-row UPDATE or a DELETE. -/
def put {K V : Type} [DecidableEq K] (s : Store K V) (w : K × Option V) : Store K V :=
  fun k => if k = w.1 then w.2 else s k
def runWrites {K V : Type} [DecidableEq K] (s : Store K V) (ws : List (K × Option V)) :
    Store K V :=
  ws.foldl put s
private theorem changed_written {K V : Type} [DecidableEq K] :
    ∀ (ws : List (K × Option V)) (s : Store K V) (k : K),
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

/-- Every row that must change needs its own write. -/
private theorem writes_lower {K V : Type} [DecidableEq K] (s : Store K V)
    (ws : List (K × Option V)) (ks : List K) (hn : ks.Nodup)
    (hd : ∀ k ∈ ks, runWrites s ws k ≠ s k) : ks.length ≤ ws.length := by
  have hsub : ks ⊆ ws.map Prod.fst := fun k hk => changed_written ws s k (hd k hk)
  simpa using hn.length_le_of_subset hsub

/-- Writing each listed key with its value in `t` leaves `t` there. -/
private theorem run_map {K V : Type} [DecidableEq K] (t : Store K V) (k : K) :
    ∀ (l : List K) (s : Store K V),
      runWrites s (l.map fun j => (j, t j)) k = if k ∈ l then t k else s k
  | [], _ => by simp [runWrites]
  | a :: l, s => by
    show runWrites (put s (a, t a)) (l.map fun j => (j, t j)) k = _
    rw [run_map t k l]
    by_cases hl : k ∈ l <;> by_cases ha : k = a <;> simp_all [put]

/-- Keys in first-seen order without repeats. -/
def uniq {α : Type} [DecidableEq α] : List α → List α
  | [] => []
  | a :: l => if a ∈ uniq l then uniq l else a :: uniq l
private theorem uniq_spec {α : Type} [DecidableEq α] :
    ∀ l : List α, (∀ x, x ∈ uniq l ↔ x ∈ l) ∧ (uniq l).Nodup
  | [] => by simp [uniq]
  | a :: l => by
    obtain ⟨hm, hn⟩ := uniq_spec l
    by_cases h : a ∈ uniq l <;> simp only [uniq, h, ite_true, ite_false]
    · exact ⟨fun x => by rw [hm, List.mem_cons]; exact ⟨Or.inr, fun
        | .inl hx => hx ▸ (hm a).mp h | .inr hx => hx⟩, hn⟩
    · exact ⟨fun x => by simp [hm], List.nodup_cons.mpr ⟨h, hn⟩⟩

/-- A stored conflict region: its file, its hunk in that file's merge, and the
commit each side was read from. The Rust conflict ID is derived from exactly
this content, so a key names one row. -/
structure Region where
  path : Nat
  hunk : Nat
  base : Nat
  ours : Nat
  theirs : Nat
  deriving DecidableEq

/-- A change's conflict rows: `some true` open, `some false` resolved. -/
abbrev Rows := Store Region Bool

/-- The registry and conflict store as one routing transaction sees them;
`others` is every other change's stored state. -/
structure State where
  base : Nat
  head : Nat
  closed : Bool
  conflicts : Rows
  others : List Nat

/-- A refused land's merge: the head it merged, the base it merged from and
onto, the conflicted paths, and per path the conflicting hunks of
`merge_file(base, head, onto)`, re-run in the owner's argument order. -/
structure Merge where
  base : Nat
  onto : Nat
  head : Nat
  paths : List Nat
  hunks : Nat → List Nat

def accepts (sameHead closed : Bool) : Bool := sameHead && !closed

/-- The regions the owner-order merge produces for `path`. -/
def produced (m : Merge) (path : Nat) : List Region :=
  (m.hunks path).map fun h => ⟨path, h, m.base, m.head, m.onto⟩

/-- `record_merge` on one row: produced regions are open, a resolved region
the merge no longer produces stays resolved, an open one is dropped. -/
def cell (hit : Bool) (v : Option Bool) : Option Bool :=
  if hit then some true else if v = some false then some false else none
def recordMerge (m : Merge) (path : Nat) (rows : Rows) : Rows :=
  fun k => if k.path = path then cell (decide (k ∈ produced m path)) (rows k) else rows k
def mergePaths (m : Merge) : List Nat → Rows → Rows
  | [], rows => rows
  | p :: ps, rows => mergePaths m ps (recordMerge m p rows)
def route (s : State) (m : Merge) : State :=
  if accepts (s.head == m.head) s.closed then
    { s with conflicts := mergePaths m m.paths s.conflicts }
  else s
private theorem cell_cell (hit : Bool) (v : Option Bool) : cell hit (cell hit v) = cell hit v := by
  cases hit <;> cases v <;> simp [cell]
private theorem mergePaths_apply (m : Merge) (k : Region) :
    ∀ (ps : List Nat) (rows : Rows), mergePaths m ps rows k =
      if k.path ∈ ps then cell (decide (k ∈ produced m k.path)) (rows k) else rows k
  | [], _ => by simp [mergePaths]
  | p :: ps, rows => by
    simp only [mergePaths, mergePaths_apply m k ps, recordMerge, List.mem_cons]
    by_cases hp : k.path = p
    · subst hp; by_cases hs : k.path ∈ ps <;> simp [hs, cell_cell]
    · by_cases hs : k.path ∈ ps <;> simp [hp, hs]
private theorem mergePaths_twice (m : Merge) (rows : Rows) :
    mergePaths m m.paths (mergePaths m m.paths rows) = mergePaths m m.paths rows := by
  funext k
  simp only [mergePaths_apply]
  by_cases h : k.path ∈ m.paths <;> simp [h, cell_cell]

/-- **correct** (SPEC.md §R8 step 2, conflict bullet): a change whose head
moved or that closed gets nothing. Otherwise only its conflict rows change:
on each routed path a row is open exactly when the merge produced it, a
resolved region the merge no longer produces stays resolved, and every
produced region holds the base, the change's own head as `ours` and the
incoming base as `theirs`. A repeat request routes to the same state. -/
theorem correct (s : State) (m : Merge) :
    (route s m).base = s.base ∧ (route s m).head = s.head ∧
    (route s m).closed = s.closed ∧ (route s m).others = s.others ∧
    (accepts (s.head == m.head) s.closed = false → (route s m).conflicts = s.conflicts) ∧
    (accepts (s.head == m.head) s.closed = true →
      (∀ k, (route s m).conflicts k =
        if k.path ∈ m.paths then
          (if k ∈ produced m k.path then some true
           else if s.conflicts k = some false then some false else none)
        else s.conflicts k) ∧
      ∀ k, k ∈ produced m k.path → k.base = m.base ∧ k.ours = s.head ∧ k.theirs = m.onto) ∧
    route (route s m) m = route s m := by
  by_cases ha : accepts (s.head == m.head) s.closed = true
  · have hh : s.head = m.head := by
      simp only [accepts, Bool.and_eq_true, beq_iff_eq] at ha; exact ha.1
    have hr : route s m = { s with conflicts := mergePaths m m.paths s.conflicts } := by
      simp [route, ha]
    have hr2 : route (route s m) m = route s m := by
      rw [hr]; simp [route, ha, mergePaths_twice]
    rw [hr2, hr]
    refine ⟨rfl, rfl, rfl, rfl, fun h => absurd ha (by simp [h]), fun _ => ⟨?_, ?_⟩, rfl⟩
    · intro k
      show mergePaths m m.paths s.conflicts k = _
      rw [mergePaths_apply]
      by_cases hp : k.path ∈ m.paths <;> by_cases hq : k ∈ produced m k.path <;> simp [hp, hq, cell]
    · intro k hk
      obtain ⟨_, _, he⟩ := List.mem_map.mp hk
      exact he ▸ ⟨rfl, hh.symm, rfl⟩
  · simp only [Bool.not_eq_true] at ha
    simp [route, ha]

/-- What the routing transaction must leave in the change's conflict rows,
for each answer of the binding read (head unchanged and change open). -/
def spec (s : State) (m : Merge) (accepted : Bool) : Rows :=
  if accepted then mergePaths m m.paths s.conflicts else s.conflicts

/-- A routine in one transaction: write one row, or read the change's binding.
The binding cannot change inside the transaction, so every read sees the same
answer. Cost model: one unit per binding read and per row written; reading
the stored conflict set is not counted. -/
inductive Prog where
  | done
  | write (k : Region) (v : Option Bool) (next : Prog)
  | observe (next : Bool → Prog)

def Prog.writes : Prog → Bool → List (Region × Option Bool)
  | .done, _ => []
  | .write k v p, b => (k, v) :: p.writes b
  | .observe f, b => (f b).writes b
def Prog.reads : Prog → Bool → Nat
  | .done, _ => 0
  | .write _ _ p, b => p.reads b
  | .observe f, b => 1 + (f b).reads b
def Prog.cost (p : Prog) (b : Bool) : Nat := p.reads b + (p.writes b).length
def writeAll : List (Region × Option Bool) → Prog
  | [] => .done
  | (k, v) :: ws => .write k v (writeAll ws)

/-- The rows that differ, among the stored keys `keys` and the produced
regions: `save` writes exactly these. -/
def diffKeys (s : State) (m : Merge) (keys : List Region) : List Region :=
  (uniq (keys ++ m.paths.flatMap (produced m))).filter fun k =>
    mergePaths m m.paths s.conflicts k ≠ s.conflicts k

/-- Ours: read the binding once; if it holds, write the differing rows. -/
def ours (s : State) (m : Merge) (keys : List Region) : Prog :=
  .observe fun b => if b then
    writeAll ((diffKeys s m keys).map fun k => (k, mergePaths m m.paths s.conflicts k))
  else .done
private theorem writeAll_eq (ws : List (Region × Option Bool)) (b : Bool) :
    (writeAll ws).writes b = ws ∧ (writeAll ws).reads b = 0 := by
  induction ws with
  | nil => simp [writeAll, Prog.writes, Prog.reads]
  | cons w ws ih => obtain ⟨k, v⟩ := w; simp [writeAll, Prog.writes, Prog.reads, ih]
private theorem ours_cost (s : State) (m : Merge) (keys : List Region) (b : Bool) :
    (ours s m keys).cost b = 1 + if b then (diffKeys s m keys).length else 0 := by
  cases b <;> simp [ours, Prog.cost, Prog.writes, Prog.reads, writeAll_eq]
private theorem mem_diffKeys (s : State) (m : Merge) (keys : List Region) (k : Region) :
    k ∈ diffKeys s m keys ↔ (k ∈ keys ∨ ∃ p ∈ m.paths, k ∈ produced m p) ∧
      mergePaths m m.paths s.conflicts k ≠ s.conflicts k := by
  simp [diffKeys, (uniq_spec _).1]

/-- **cost_bound** (SPEC.md §R8 step 2, conflict bullet): under the cost model
of `Prog.cost` (one unit per binding read and per row written; reading the
stored set is free), with `keys` the change's stored rows, routing costs at
most one read plus one write per stored row on a routed path and per
produced region. -/
theorem cost_bound (s : State) (m : Merge) (keys : List Region) (b : Bool) :
    (ours s m keys).cost b ≤ 1 + (keys.filter fun k => k.path ∈ m.paths).length +
      (m.paths.flatMap (produced m)).length := by
  have hsub : diffKeys s m keys ⊆
      (keys.filter fun k => k.path ∈ m.paths) ++ m.paths.flatMap (produced m) := by
    intro k hk'
    obtain ⟨hin, hne⟩ := (mem_diffKeys s m keys k).mp hk'
    have hp : k.path ∈ m.paths := Classical.byContradiction fun hp => by
      rw [mergePaths_apply] at hne; simp [hp] at hne
    rcases hin with hin | hin
    · simp [hin, hp]
    · simp only [List.mem_append, List.mem_flatMap]; exact Or.inr hin
  have : (diffKeys s m keys).length ≤ _ :=
    (List.Sublist.nodup List.filter_sublist (uniq_spec _).2).length_le_of_subset hsub
  rw [ours_cost]; rw [List.length_append] at this
  cases b <;> simp only [Bool.false_eq_true, ite_false, ite_true] <;> omega

/-- **optimal** (SPEC.md §R8 step 2, conflict bullet): the class is every
routine of `Prog` that, whichever way the binding read answers, leaves the
rows `spec` requires. Ours is in the class, and whenever accepting the merge
changes a stored row, no routine in the class costs less on either answer:
a routine that never reads the binding writes the same rows on both answers,
and every row that must change takes its own write. -/
theorem optimal (s : State) (m : Merge) (keys : List Region)
    (hk : ∀ k, s.conflicts k ≠ none → k ∈ keys)
    (changes : mergePaths m m.paths s.conflicts ≠ s.conflicts) :
    (∀ b, runWrites s.conflicts ((ours s m keys).writes b) = spec s m b) ∧
    ∀ p : Prog, (∀ b, runWrites s.conflicts (p.writes b) = spec s m b) →
      ∀ b, (ours s m keys).cost b ≤ p.cost b := by
  have hmap : ∀ k, runWrites s.conflicts
      ((diffKeys s m keys).map fun j => (j, mergePaths m m.paths s.conflicts j)) k =
      mergePaths m m.paths s.conflicts k := by
    intro k
    rw [run_map]
    by_cases hd : k ∈ diffKeys s m keys
    · simp [hd]
    · simp only [hd, ite_false]
      refine Classical.byContradiction fun hne => hd ((mem_diffKeys s m keys k).mpr ⟨?_, Ne.symm hne⟩)
      rw [mergePaths_apply] at hne
      by_cases hp : k.path ∈ m.paths
      · by_cases hq : k ∈ produced m k.path
        · exact Or.inr ⟨k.path, hp, hq⟩
        · left; apply hk; intro hn; simp [hp, hq, hn, cell] at hne
      · simp [hp] at hne
  refine ⟨fun b => ?_, fun p hp b => ?_⟩
  · cases b
    · simp [ours, Prog.writes, spec, runWrites]
    · funext k; simp [ours, Prog.writes, spec, writeAll_eq, hmap]
  · have blind : ∀ q : Prog, q.reads b = 0 → ∀ c, q.writes c = q.writes b := by
      intro q
      induction q with
      | done => simp [Prog.writes]
      | write k v q ih => intro h c; simp [Prog.writes, Prog.reads] at h ⊢; exact ih h c
      | observe f _ => intro h; simp [Prog.reads] at h
    have hr : 1 ≤ p.reads b := by
      refine Nat.succ_le_of_lt (Nat.pos_of_ne_zero fun h0 => ?_)
      have hw := blind p h0
      have := (hp true).symm.trans ((congrArg _ (hw true)).trans
        ((congrArg _ (hw false)).symm.trans (hp false)))
      exact changes (by simpa [spec] using this)
    rw [ours_cost, Prog.cost]
    cases b
    · simp; omega
    · have := writes_lower s.conflicts (p.writes true) (diffKeys s m keys)
        (List.Sublist.nodup List.filter_sublist (uniq_spec _).2) (fun k hk' => by
          rw [hp true]; simpa [spec] using ((mem_diffKeys s m keys k).mp hk').2)
      simp; omega

#print axioms correct
#print axioms cost_bound
#print axioms optimal

def fixtures : String :=
  "same_head\tclosed\taccepted\n" ++ String.join (
    [false, true].flatMap fun same => [false, true].map fun closed =>
      s!"{if same then 1 else 0}\t{if closed then 1 else 0}\t{if accepts same closed then 1 else 0}\n")
end ConflictRouting

#print axioms ConflictRouting.fixtures
