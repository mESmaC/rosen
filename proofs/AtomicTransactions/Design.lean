/-!
# Design proof: the land transaction (SPEC.md R8 step 2 and section 3)

Models `Ledger::compare_and_swap` in `crates/rosen-server/src/landing/ledger.rs`.
The metadata store is a set of keyed rows. One SQLite transaction reads every
gate `check_attempt` reads (the landed history, the registry row and its head,
a valid approval, open conflicts and the base ref) and then writes the ref,
history and registry rows. A rollback or crash publishes none of them. The
model assumes SQLite transaction isolation and durability, not its internals.
-/
namespace AtomicTransactions

/-- One metadata row, keyed by the change it belongs to where it has one. -/
inductive Key where
  | ref
  | history (change : Nat)
  | registry (change : Nat)
  | approval (change : Nat)
  | conflict (change : Nat)
  deriving DecidableEq

/-- Row contents: a commit, a registry state with its head, or a present flag
(a valid approval, an open conflict). `live` stands for `open` and `ready`. -/
inductive Val where
  | commit (id : Nat)
  | live (head : Nat)
  | landed (head : Nat)
  | present
  deriving DecidableEq

/-- The store; `none` is an absent row. -/
abbrev State := Key → Option Val

/-- One write sets one row: an INSERT, a single-row UPDATE or a DELETE. -/
abbrev Write := Key × Option Val

def put (s : State) (w : Write) : State :=
  fun k => if k = w.1 then w.2 else s k

def runWrites (s : State) (ws : List Write) : State := ws.foldl put s

/-- The gates `check_attempt` evaluates inside the transaction (#122, #130):
the change has not landed, its registry row is live at the land's head, an
approval still holds, it has no open conflict, and the base is unmoved. -/
def allowed (s : State) (expected change head : Nat) : Bool :=
  s (.history change) == none &&
  s (.registry change) == some (.live head) &&
  s (.approval change) == some .present &&
  s (.conflict change) == none &&
  s .ref == some (.commit expected)

/-- The writes of an accepted land: ref UPDATE, history INSERT, registry UPDATE. -/
def landWrites (change head commit : Nat) : List Write :=
  [(.ref, some (.commit commit)), (.history change, some (.commit commit)),
   (.registry change, some (.landed head))]

/-- The writes the transaction commits: all three, or none when a gate refuses
or the transaction does not commit. -/
def writes (s : State) (expected change head commit : Nat) (committed : Bool) : List Write :=
  if committed && allowed s expected change head then landWrites change head commit else []

def publish (s : State) (expected change head commit : Nat) (committed : Bool) : State :=
  runWrites s (writes s expected change head commit committed)

/-- Every gate of `allowed`, as propositions. -/
def Gates (s : State) (expected change head : Nat) : Prop :=
  s (.history change) = none ∧ s (.registry change) = some (.live head) ∧
  s (.approval change) = some .present ∧ s (.conflict change) = none ∧
  s .ref = some (.commit expected)

private theorem allowed_iff (s : State) (expected change head : Nat) :
    allowed s expected change head = true ↔ Gates s expected change head := by
  simp [allowed, Gates, and_assoc]

private theorem landed_rows (s : State) (change head commit : Nat) (k : Key) :
    runWrites s (landWrites change head commit) k =
      if k = .registry change then some (.landed head)
      else if k = .history change then some (.commit commit)
      else if k = .ref then some (.commit commit) else s k := by
  simp only [runWrites, landWrites, List.foldl, put]

/-- **correct** (SPEC.md R8 step 2 and section 3): every outcome is the
complete old store or the complete new one. The new one exists exactly when
the transaction commits with every gate holding: the change has not landed,
its registry row is live at the land's head, an approval for it still holds,
it has no open conflict, and the base equals the expected commit. It moves
the ref to the landed commit, records the land in history, marks the
registry row landed, and leaves every other row as it was. -/
-- SPEC.md §R8
theorem correct (s : State) (expected change head commit : Nat) (committed : Bool) :
    (publish s expected change head commit committed = s ∧
        ¬(committed = true ∧ Gates s expected change head)) ∨
      (committed = true ∧ Gates s expected change head ∧
        publish s expected change head commit committed .ref = some (.commit commit) ∧
        publish s expected change head commit committed (.history change) =
          some (.commit commit) ∧
        publish s expected change head commit committed (.registry change) =
          some (.landed head) ∧
        ∀ k, k ≠ .ref → k ≠ .history change → k ≠ .registry change →
          publish s expected change head commit committed k = s k) := by
  unfold publish writes
  cases hc : committed <;> cases ha : allowed s expected change head
  · exact Or.inl ⟨rfl, by simp⟩
  · exact Or.inl ⟨rfl, by simp⟩
  · exact Or.inl ⟨rfl, by simp [← allowed_iff, ha]⟩
  · refine Or.inr ⟨rfl, (allowed_iff _ _ _ _).mp ha, ?_⟩
    simp only [Bool.and_self, ite_true, landed_rows]
    refine ⟨by simp, by simp, by simp, fun k h1 h2 h3 => by simp [h1, h2, h3]⟩

/-- **cost_bound** (SPEC.md R8 step 2). Cost model: durable row writes in
the land transaction (INSERT, single-row UPDATE or DELETE). Gate reads,
object writes and SQLite page I/O are excluded. A land writes at most three
rows, and a refused or rolled-back attempt writes none. -/
-- SPEC.md §R8
theorem cost_bound (s : State) (expected change head commit : Nat) (committed : Bool) :
    (writes s expected change head commit committed).length ≤ 3 ∧
    ((committed && allowed s expected change head) = false →
      writes s expected change head commit committed = []) := by
  unfold writes
  constructor
  · split <;> simp [landWrites]
  · intro h; simp [h]

private theorem changed_written :
    ∀ (ws : List Write) (s : State) (k : Key), runWrites s ws k ≠ s k → k ∈ ws.map Prod.fst
  | [], _, _, h => absurd rfl h
  | w :: ws, s, k, h => by
    by_cases hk : k = w.1
    · simp [hk]
    · have := changed_written ws (put s w) k (by
        intro he; apply h
        show runWrites (put s w) ws k = s k
        rw [he]; simp [put, hk])
      simp [this]

private theorem writes_lower (s : State) (ws : List Write) (ks : List Key) (hn : ks.Nodup)
    (hd : ∀ k ∈ ks, runWrites s ws k ≠ s k) : ks.length ≤ ws.length := by
  have hsub : ks ⊆ ws.map Prod.fst := fun k hk => changed_written ws s k (hd k hk)
  simpa using hn.length_le_of_subset hsub

/-- **optimal** (SPEC.md R8 step 2). Cost model as in `cost_bound`. The
class of alternatives: any sequence of single-row writes, in any order and
over any rows, that leaves the store exactly as this transaction publishes
it. A land makes a new commit, so it differs from the base it was made on.
No alternative writes fewer rows: the ref, history and registry rows each
change, and a single-row write changes one row. -/
-- SPEC.md §R8
theorem optimal (s : State) (expected change head commit : Nat) (committed : Bool)
    (fresh : commit ≠ expected) (alternative : List Write)
    (same : runWrites s alternative = publish s expected change head commit committed) :
    (writes s expected change head commit committed).length ≤ alternative.length := by
  by_cases ha : (committed && allowed s expected change head) = true
  · have hg := (allowed_iff s expected change head).mp (Bool.and_eq_true _ _ ▸ ha).2
    obtain ⟨hh, hr, -, -, hb⟩ := hg
    have hw : writes s expected change head commit committed = landWrites change head commit :=
      by simp [writes, ha]
    have hp : ∀ k, runWrites s alternative k = runWrites s (landWrites change head commit) k :=
      fun k => by rw [same, publish, hw]
    have key := writes_lower s alternative [.ref, .history change, .registry change]
      (by simp) (by
        intro k hk
        simp only [List.mem_cons, List.not_mem_nil, or_false] at hk
        rw [hp, landed_rows]
        rcases hk with rfl | rfl | rfl <;> simp_all)
    simpa [hw, landWrites] using key
  · simp [writes, Bool.not_eq_true _ ▸ ha]

#print axioms Key
#print axioms Val
#print axioms put
#print axioms runWrites
#print axioms allowed
#print axioms landWrites
#print axioms writes
#print axioms publish
#print axioms Gates
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end AtomicTransactions
