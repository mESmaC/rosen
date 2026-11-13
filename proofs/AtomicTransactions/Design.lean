/-! SPEC R8 and section 3: a SQLite commit publishes ref, history and registry
closure together. Uncommitted work (including a crash) publishes none.
The model assumes SQLite transaction isolation/durability, not its internals. -/
namespace AtomicTransactions
structure State where
  base : Nat
  history : List Nat
  closed : List Nat
  deriving DecidableEq

def allowed (s : State) (expected change : Nat) : Bool :=
  s.base == expected && !s.history.contains change

def staged (s : State) (change commit : Nat) : State :=
  ⟨commit, change :: s.history, change :: s.closed⟩

def publish (s : State) (expected change commit : Nat) (committed : Bool) : State :=
  if committed && allowed s expected change then staged s change commit else s

/-- SPEC R8/section 3: every outcome is the complete old or complete new state;
publication requires a matching base and a change absent from history. -/
theorem correct (s : State) (expected change commit : Nat) (committed : Bool) :
    publish s expected change commit committed = s ∨
      (publish s expected change commit committed = staged s change commit ∧
       s.base = expected ∧ change ∉ s.history) := by
  unfold publish
  split
  · rename_i h
    right
    simp [allowed] at h
    exact ⟨rfl, h.2⟩
  · exact Or.inl rfl

/-- Cost model counts durable row mutations: ref update, history insert,
registry update. Gate reads, object writes and SQLite page I/O are excluded. -/
def writes (accepted : Bool) : Nat := if accepted then 3 else 0

theorem cost_bound (accepted : Bool) : writes accepted ≤ 3 := by
  cases accepted <;> decide

/-- Alternatives are row-mutation implementations that persist the new ref,
new history row and registry closure in three distinct records. Each must
write each record at least once. Under this model none costs less. -/
theorem optimal (refWrites historyWrites registryWrites : Nat)
    (hr : 1 ≤ refWrites) (hh : 1 ≤ historyWrites) (hc : 1 ≤ registryWrites) :
    writes true ≤ refWrites + historyWrites + registryWrites := by
  simp [writes]; omega

#print axioms State
#print axioms allowed
#print axioms staged
#print axioms publish
#print axioms writes
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end AtomicTransactions
