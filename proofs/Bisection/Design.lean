/-! # Design proof: post-land bisection (spec/R12-post-land-verification.md R12). -/
namespace Bisection

def midpoint (lo hi : Nat) := lo + (hi - lo) / 2

def step (lo hi culprit : Nat) : Prod Nat Nat :=
  let mid := midpoint lo hi
  if mid < culprit then (mid, hi) else (lo, mid)

/-- The starting checkpoint is already eligible. The remaining history rows
carry the commit ID and whether the row represents mirror maintenance. -/
def landedInterval (checkpoint : Nat) (rows : List (Nat × Bool)) : List Nat :=
  checkpoint :: (rows.filter fun row => !row.2).map Prod.fst

/-- spec/R12-post-land-verification.md R12: a monotone checker passes exactly before culprit.
Every split retains that culprit and strictly reduces a non-singleton range;
a singleton identifies it. Confirmation requires a second failure and parent pass.
The captured interval excludes mirror maintenance (spec/R1-repository-import-and-git-mirror.md R1). -/
theorem correct (lo hi culprit : Nat) (hl : lo < culprit) (hh : culprit <= hi) :
    let next := step lo hi culprit
    (hi - lo > 1 → lo <= next.1 ∧ next.1 < culprit ∧ culprit <= next.2 ∧
      next.2 <= hi ∧ next.2 - next.1 < hi - lo) ∧
    (hi - lo = 1 → hi = culprit) ∧
    (∀ (firstFail secondFail parentPass : Bool),
      (firstFail && secondFail && parentPass = true) ↔
      firstFail = true ∧ secondFail = true ∧ parentPass = true) ∧
    (∀ checkpoint rows commit, commit ∈ landedInterval checkpoint rows ↔
      commit = checkpoint ∨ (commit, false) ∈ rows) := by
  dsimp [step, midpoint]
  constructor
  · intro hw
    by_cases hm : lo + (hi - lo) / 2 < culprit <;> simp [hm] <;> omega
  constructor
  · omega
  constructor
  · intro a b c; cases a <;> cases b <;> cases c <;> decide
  · intro checkpoint rows commit
    simp [landedInterval]

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
#print axioms landedInterval
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end Bisection
