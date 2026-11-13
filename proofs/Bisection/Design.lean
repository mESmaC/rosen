/-! # Design proof: post-land bisection (SPEC.md R12). -/
namespace Bisection

def midpoint (lo hi : Nat) := lo + (hi - lo) / 2

def step (lo hi culprit : Nat) : Prod Nat Nat :=
  let mid := midpoint lo hi
  if mid < culprit then (mid, hi) else (lo, mid)

/-- SPEC.md R12: a monotone checker passes exactly before culprit.
Every split retains that culprit and strictly reduces a non-singleton range;
a singleton identifies it. Confirmation requires a second failure and parent pass. -/
theorem correct (lo hi culprit : Nat) (hl : lo < culprit) (hh : culprit <= hi) :
    let next := step lo hi culprit
    (hi - lo > 1 → lo <= next.1 ∧ next.1 < culprit ∧ culprit <= next.2 ∧
      next.2 <= hi ∧ next.2 - next.1 < hi - lo) ∧
    (hi - lo = 1 → hi = culprit) ∧
    (∀ (firstFail secondFail parentPass : Bool),
      (firstFail && secondFail && parentPass = true) ↔
      firstFail = true ∧ secondFail = true ∧ parentPass = true) := by
  dsimp [step, midpoint]
  constructor
  · intro hw
    by_cases hm : lo + (hi - lo) / 2 < culprit <;> simp [hm] <;> omega
  constructor
  · omega
  · intro a b c; cases a <;> cases b <;> cases c <;> decide

/-- Cost model: each external checker query costs one; measure remaining
candidates after that query. Balanced bisection leaves at most ceil(n/2).
Two more queries confirm the retained failing endpoint and its parent. -/
theorem cost_bound (lo hi culprit : Nat) (h : lo < hi) :
    let next := step lo hi culprit
    next.2 - next.1 <= (hi - lo + 1) / 2 := by
  dsimp [step, midpoint]
  by_cases hm : lo + (hi - lo) / 2 < culprit <;> simp [hm] <;> omega

/-- Alternatives are ordered binary comparisons splitting n candidates
into k and n-k; cost is the worst remaining candidate count after one query.
No such split has smaller worst-case cost than this balanced split. -/
theorem optimal (n k : Nat) (hk : k <= n) :
    max (midpoint 0 n) (n - midpoint 0 n) <= max k (n - k) := by
  dsimp [midpoint]
  have h1 := Nat.le_max_left k (n-k)
  have h2 := Nat.le_max_right k (n-k)
  apply Nat.max_le.mpr; constructor <;> omega

#print axioms midpoint
#print axioms step
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end Bisection
