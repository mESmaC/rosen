namespace ConflictRouting

def accepts (sameHead closed : Bool) : Bool := sameHead && !closed

structure State where
  base : Nat
  head : Nat
  conflicts : List Nat
  otherChanges : List (List Nat)
  deriving DecidableEq

def route (s : State) (sameHead closed : Bool) (regions : List Nat) : State :=
  if accepts sameHead closed then { s with conflicts := regions } else s

-- SPEC.md §R8: refusing a conflicted land preserves base, head and other
-- changes; only the owning live change with the same head gets the regions.
theorem correct (s : State) (sameHead closed : Bool) (regions : List Nat) :
    (route s sameHead closed regions).base = s.base ∧
    (route s sameHead closed regions).head = s.head ∧
    (route s sameHead closed regions).otherChanges = s.otherChanges ∧
    (route s sameHead closed regions).conflicts =
      (if sameHead = true ∧ closed = false then regions else s.conflicts) := by
  cases sameHead <;> cases closed <;> simp [route, accepts]

-- Cost model counts one guard and one write per supplied conflict region.
def cost (sameHead closed : Bool) (regions : List Nat) : Nat :=
  1 + if accepts sameHead closed then regions.length else 0

-- SPEC.md §R8. Under this model, routing costs at most one guard plus n writes.
theorem cost_bound (sameHead closed : Bool) (regions : List Nat) :
    cost sameHead closed regions ≤ 1 + regions.length := by
  cases sameHead <;> cases closed <;> simp [cost, accepts]

-- SPEC.md §R8. Alternatives must check the binding and, when accepted,
-- durably write every region. No algorithm in that class costs less.
theorem optimal (sameHead closed : Bool) (regions : List Nat) (alternative : Nat)
    (guard : 1 ≤ alternative)
    (writes : accepts sameHead closed = true → 1 + regions.length ≤ alternative) :
    cost sameHead closed regions ≤ alternative := by
  cases h : accepts sameHead closed
  · simpa [cost, h] using guard
  · simpa [cost, h] using writes h

#print axioms correct
#print axioms cost_bound
#print axioms optimal

def fixtures : String :=
  "same_head\tclosed\taccepted\n" ++ String.join (
    [false, true].flatMap fun same => [false, true].map fun closed =>
      s!"{if same then 1 else 0}\t{if closed then 1 else 0}\t{if accepts same closed then 1 else 0}\n")
end ConflictRouting

#print axioms ConflictRouting.accepts
#print axioms ConflictRouting.route
#print axioms ConflictRouting.cost
#print axioms ConflictRouting.fixtures
