/-! SPEC.md R8 (step 2, step 4 and "Accept when"): a server crash at any point of a
land loses no land and applies none twice, and recovery finishes every interrupted
post-land run without moving a change behind a base it already reached.

SQLite transactions are atomic and durable (AtomicTransactions proves the land
transaction). One land's post-land work is a forward-only rebase of one overlapping
change, `target`, then clearing the land's pending flag. Positions are places in the
base history: the n-th land sits at position n. -/
namespace CrashRecovery

structure Land where
  change : Nat
  pos : Nat
  deriving DecidableEq, Repr

/-- What survives a crash: the base history, the lands whose post-land work has not
finished, and the base position the overlapping change sits on. -/
structure World where
  history : List Land
  pending : List Land
  target : Nat
  deriving DecidableEq, Repr

def landed (w : World) (c : Nat) : Bool := w.history.any (·.change == c)

/-- The rebase moves the target only forward; the land stops being pending. -/
def postLand (w : World) (l : Land) : World :=
  { w with target := max w.target l.pos, pending := w.pending.erase l }

def recover (w : World) (order : List Land) : World := order.foldl postLand w

def next (w : World) (c : Nat) : Land := ⟨c, w.history.length + 1⟩

def committed (w : World) (c : Nat) : World :=
  { w with history := next w c :: w.history, pending := next w c :: w.pending }

/-- What is durable when the server dies landing `c`: `0` before the land
transaction commits, `1` after it commits, `2` after the post-land rebase but before
the land is marked finished, larger: no crash. A landed change never lands again. -/
def crashAt (w : World) (c crash : Nat) : World :=
  if landed w c then w
  else match crash with
    | 0 => w
    | 1 => committed w c
    | 2 => { committed w c with target := max w.target (next w c).pos }
    | _ => postLand (committed w c) (next w c)

/-- A land request that does not crash. -/
def land (w : World) (c : Nat) : World := crashAt w c 3

/-- The restart replays every pending land, then the requester, whose connection
dropped, asks to land `c` again. -/
def restart (w : World) (c crash : Nat) : World :=
  let v := crashAt w c crash
  land (recover v v.pending) c

def raise (t : Nat) (l : Land) : Nat := max t l.pos

theorem recover_fields (w : World) (o : List Land) :
    (recover w o).history = w.history ∧ (recover w o).pending = o.foldl List.erase w.pending ∧
    (recover w o).target = o.foldl raise w.target := by
  induction o generalizing w with
  | nil => simp [recover]
  | cons a o ih => exact ih (postLand w a)

theorem erase_self (p : List Land) : p.foldl List.erase p = [] := by
  induction p with
  | nil => rfl
  | cons a p ih => simpa [List.erase_cons_head] using ih

theorem raise_swap (p : List Land) (t n : Nat) :
    p.foldl raise (max t n) = max (p.foldl raise t) n := by
  induction p generalizing t with
  | nil => rfl
  | cons a p ih =>
    simp only [List.foldl_cons, raise, ← ih]
    congr 1; omega

/-- Once the land has committed, recovery finishes it and the repeated ask
changes nothing. -/
theorem finish (v : World) (c : Nat) (done : landed v c = true) :
    land (recover v v.pending) c = ⟨v.history, [], v.pending.foldl raise v.target⟩ := by
  obtain ⟨hh, hp, ht⟩ := recover_fields v v.pending
  have again : landed (recover v v.pending) c = true := by simpa [landed, hh] using done
  simp only [land, crashAt, again, ↓reduceIte]
  generalize recover v v.pending = r at hh hp ht ⊢
  cases r
  simp_all [erase_self]

/-- SPEC.md R8 "Accept when" and steps 2 and 4: whatever point the server dies at,
the restart ends in the world a land that never crashed reaches: the change is in
the base history exactly once, no post-land work is left, and the overlapping change
sits on the newest base any land moved it to, never behind it. -/
theorem correct (w : World) (c crash : Nat) (fresh : landed w c = false) :
    restart w c crash =
      ⟨next w c :: w.history, [], max (w.pending.foldl raise w.target) (next w c).pos⟩ := by
  have once : ∀ p t, landed ⟨next w c :: w.history, p, t⟩ c = true := by
    simp [landed, next]
  match crash with
  | 0 =>
    obtain ⟨hh, hp, ht⟩ := recover_fields w w.pending
    have again : landed (recover w w.pending) c = false := by simpa [landed, hh] using fresh
    simp [restart, land, crashAt, fresh, again, postLand, committed, next, hh, hp, ht,
      erase_self]
  | 1 | 2 | _ + 3 =>
    simp only [restart, crashAt, fresh, committed, postLand, Bool.false_eq_true, ↓reduceIte,
      List.erase_cons_head]
    rw [finish _ c (once _ _)]
    simp [raise, raise_swap, Nat.max_assoc]

/-- Cost model: post-land runs after the restart, counting each replay and the run
of a land the repeated ask makes; merges and SQLite I/O inside a run are excluded.
Bound: one run per land left pending before the crash, plus this land. -/
def restartRuns (w : World) (c crash : Nat) : Nat :=
  let v := crashAt w c crash
  v.pending.length + (if landed (recover v v.pending) c then 0 else 1)

theorem cost_bound (w : World) (c crash : Nat) (fresh : landed w c = false) :
    restartRuns w c crash ≤ w.pending.length + 1 := by
  have ran : ∀ p t, landed (recover ⟨next w c :: w.history, p, t⟩ p) c = true := by
    simp [landed, next, (recover_fields _ _).1]
  unfold restartRuns
  match crash with
  | 0 => simp only [show crashAt w c 0 = w by simp [crashAt, fresh]]; split <;> omega
  | 1 | 2 | _ + 3 =>
    simp only [crashAt, fresh, committed, postLand, Bool.false_eq_true, ↓reduceIte,
      List.erase_cons_head, ran, List.length_cons] <;> omega

theorem erase_length (o p : List Land) :
    p.length ≤ (o.foldl List.erase p).length + o.length := by
  induction o generalizing p with
  | nil => simp
  | cons a o ih =>
    have step := ih (p.erase a)
    have shrink : p.length ≤ (p.erase a).length + 1 := by
      rw [List.length_erase]; split <;> omega
    simp only [List.foldl_cons, List.length_cons]; omega

/-- Alternatives: any sequence of post-land runs, in any order and with any repeats
or omissions, that leaves no land pending. Each run finishes at most one land, so
none takes fewer runs than replaying each pending land once, as recovery does. -/
theorem optimal (w : World) (alt : List Land) (finishes : (recover w alt).pending = []) :
    (recover w w.pending).pending = [] ∧ w.pending.length ≤ alt.length := by
  have bound := erase_length alt w.pending
  rw [(recover_fields w alt).2.1] at finishes
  rw [finishes] at bound
  exact ⟨by rw [(recover_fields _ _).2.1, erase_self], by simpa using bound⟩

#print axioms recover_fields
#print axioms erase_self
#print axioms raise_swap
#print axioms finish
#print axioms correct
#print axioms cost_bound
#print axioms erase_length
#print axioms optimal
end CrashRecovery
