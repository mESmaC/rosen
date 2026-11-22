namespace Landing

/-- R8 clean merge gate. Trees are immutable IDs. -/
inductive LayerRegeneration where
  | clean (tree : Nat)
  | required (commands : List (Option Nat)) (finished : Option (Option Nat))
inductive RegenerationRefusal where
  | missing | pending | failed

def finishRegeneration : LayerRegeneration → Except RegenerationRefusal Nat
  | .clean tree => .ok tree
  | .required commands finished =>
    if commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) then match finished with
      | none => .error .pending
      | some none => .error .failed
      | some (some tree) => .ok tree
    else .error .missing

inductive RegenerationCheck where
  | landable | pendingAtLand | manual

/-- Read-only R8 land check for the top layer in after-land mode, with the
preceding gates satisfied. Only a successful result can be cached. -/
def checkTopRegeneration (layer : LayerRegeneration) : RegenerationCheck :=
  match finishRegeneration layer with
  | .ok _ => .landable
  | .error .missing => .manual
  | .error .pending | .error .failed => .pendingAtLand

theorem check_top_required (commands : List (Option Nat)) (finished : Option (Option Nat)) :
    checkTopRegeneration (.required commands finished) =
      if commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) then
        if finished.join.isSome then .landable else .pendingAtLand
      else .manual := by
  by_cases complete : commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true)
  · simp only [checkTopRegeneration, finishRegeneration, ite_eq_left complete]
    cases finished with
    | none => rfl
    | some result => cases result <;> rfl
  · simp only [checkTopRegeneration, finishRegeneration, ite_eq_right complete]

end Landing
