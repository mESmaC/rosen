namespace LiveEdits

-- Metadata and binary detection are from the immutable parent/head pair.
-- This model covers region classification, not the line-diff algorithm.
def textual (parent head sameMode regular binary : Bool) : Bool :=
  parent && head && sameMode && regular && !binary
def whole (parent head sameMode regular binary : Bool) : Bool :=
  !textual parent head sameMode regular binary

-- SPEC.md §R6: absent, mode-changed, non-regular and binary files cover
-- every line; only a pair of regular text files uses line regions.
theorem correct (p h m r b : Bool) :
    whole p h m r b = false ↔
      p = true ∧ h = true ∧ m = true ∧ r = true ∧ b = false := by
  cases p <;> cases h <;> cases m <;> cases r <;> cases b <;> decide

-- SPEC.md §R6. Cost model: count line-diff calls per changed path;
-- metadata tests and binary detection are excluded. At most one is needed.
def diffCost (p h m r b : Bool) : Nat := if whole p h m r b then 0 else 1
theorem cost_bound (p h m r b : Bool) : diffCost p h m r b ≤ 1 := by
  simp only [diffCost]; split <;> omega

-- SPEC.md §R6. Alternatives compute exact text regions through the same
-- diff primitive, invoking it at least once for every textual pair. Under
-- the line-diff-call cost model no alternative in this class costs less.
theorem optimal (alternative : Bool → Bool → Bool → Bool → Bool → Nat)
    (exactLines : ∀ p h m r b, textual p h m r b = true →
      1 ≤ alternative p h m r b) (p h m r b : Bool) :
    diffCost p h m r b ≤ alternative p h m r b := by
  cases ht : textual p h m r b
  · simp [diffCost, whole, ht]
  · simpa [diffCost, whole, ht] using exactLines p h m r b ht

#print axioms correct
#print axioms textual
#print axioms whole
#print axioms diffCost
#print axioms cost_bound
#print axioms optimal
end LiveEdits
