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

/-- One layer in a bottom-first stack: its change ID, its gate input evaluated
on its completed tree, and its merge+regeneration step given the tree below it. -/
structure StackLayer (α : Type) where
  change : Nat
  input : Nat → α
  merge : Nat → LayerRegeneration

/-- Bottom-first stack resolution: each layer merges onto the previous layer's
completed tree and finishes regeneration before the next layer continues (R8). -/
def resolveStack (onto : Nat) :
    List (StackLayer α) → Except RegenerationRefusal (List (StackLayer α × Nat))
  | [] => .ok []
  | layer :: rest =>
    match finishRegeneration (layer.merge onto) with
    | .error err => .error err
    | .ok tree =>
      match resolveStack tree rest with
      | .error err => .error err
      | .ok tail => .ok ((layer, tree) :: tail)

/-- Specification of bottom-first tree propagation: layer 0 merges onto `onto`,
and every subsequent layer merges onto the preceding layer's completed tree. -/
inductive ResolvedStack : Nat → List (StackLayer α) → List (StackLayer α × Nat) → Prop where
  | nil (onto : Nat) : ResolvedStack onto [] []
  | cons {onto tree : Nat} {layer : StackLayer α} {rest : List (StackLayer α)}
      {tail : List (StackLayer α × Nat)} :
      finishRegeneration (layer.merge onto) = .ok tree →
      ResolvedStack tree rest tail →
      ResolvedStack onto (layer :: rest) ((layer, tree) :: tail)

/-- Read-only R8 land check for a bottom-first stack in after-land mode, with
preceding layer gates satisfied. Stops at the first uncached layer needing
regeneration: complete plans report `pendingAtLand` at any layer (not only the
top), while incomplete plans report `manual` (R8, #454, #468). -/
def checkStackRegeneration (onto : Nat) (layers : List (StackLayer α)) : RegenerationCheck :=
  match resolveStack onto layers with
  | .ok _ => .landable
  | .error .missing => .manual
  | .error .pending | .error .failed => .pendingAtLand

/-- Read-only R8 land check for a stack with earlier layer gates checked first:
an earlier gate refusal is preserved and never masked as pending regeneration. -/
def checkStackGate {ρ : Type} (earlierRefusal : Option ρ) (onto : Nat)
    (layers : List (StackLayer α)) : Except ρ RegenerationCheck :=
  match earlierRefusal with
  | some refusal => .error refusal
  | none => .ok (checkStackRegeneration onto layers)

/-- Checked merge preview for a stack: returns the top layer's completed tree
only when the stack is nonempty and every layer finishes regeneration. -/
def previewStack (baseTree : Nat) (layers : List (StackLayer α)) : Option Nat :=
  match resolveStack baseTree layers with
  | .ok resolved => resolved.getLast?.map Prod.snd
  | .error _ => none

/-- Builds each layer's candidate `(input, change, commit)` from its completed
tree, chaining each written commit as the next layer's parent. -/
def candidatesOf (writeCommit : Nat → Nat → Nat → Nat) (parent : Nat) :
    List (StackLayer α × Nat) → List (α × Nat × Nat)
  | [] => []
  | (layer, tree) :: rest =>
    let commit := writeCommit layer.change parent tree
    (layer.input tree, layer.change, commit) :: candidatesOf writeCommit commit rest

theorem finish_correct (commands : List (Option Nat)) (finished : Option (Option Nat))
    (tree : Nat) :
    finishRegeneration (.required commands finished) = .ok tree ↔
      commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true) ∧ finished = some (some tree) := by
  rw [← and_assoc]
  by_cases ready : commands ≠ [] ∧ (∀ c ∈ commands, c.isSome = true)
  · rw [and_iff_right ready]
    cases finished with
    | none => simp only [finishRegeneration, ite_eq_left ready] <;> simp
    | some result => cases result <;> simp only [finishRegeneration, ite_eq_left ready] <;> simp
  · cases finished with
    | none => simp [finishRegeneration, ready]
    | some result => cases result <;> simp [finishRegeneration, ready]

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

theorem resolveStack_iff (onto : Nat) (layers : List (StackLayer α))
    (resolved : List (StackLayer α × Nat)) :
    resolveStack onto layers = .ok resolved ↔ ResolvedStack onto layers resolved := by
  induction layers generalizing onto resolved with
  | nil =>
    constructor
    · intro h; simp [resolveStack] at h; subst h; exact .nil onto
    · intro h; cases h; rfl
  | cons layer rest ih =>
    constructor
    · intro h
      simp only [resolveStack] at h
      cases hf : finishRegeneration (layer.merge onto) with
      | error err => simp [hf] at h
      | ok tree =>
        simp only [hf] at h
        cases hr : resolveStack tree rest with
        | error err => simp [hr] at h
        | ok tail =>
          simp only [hr, Except.ok.injEq] at h
          subst h
          exact .cons hf ((ih tree tail).mp hr)
    · intro h
      cases h with
      | cons hf hr =>
        simp [resolveStack, hf, (ih _ _).mpr hr]

theorem ResolvedStack.length_eq {onto : Nat} {layers : List (StackLayer α)}
    {resolved : List (StackLayer α × Nat)} (h : ResolvedStack onto layers resolved) :
    resolved.length = layers.length := by
  induction h with
  | nil _ => rfl
  | cons _ _ ih => simp [ih]

theorem previewStack_iff (baseTree : Nat) (layers : List (StackLayer α)) (tree : Nat) :
    previewStack baseTree layers = some tree ↔
      ∃ resolved, ResolvedStack baseTree layers resolved ∧
        resolved.getLast?.map Prod.snd = some tree := by
  unfold previewStack
  cases hr : resolveStack baseTree layers with
  | error err =>
    simp only [reduceCtorEq, false_iff, not_exists, not_and]
    intro resolved hres
    have h' := (resolveStack_iff baseTree layers resolved).mpr hres
    rw [hr] at h'
    cases h'
  | ok resolved =>
    constructor
    · intro h
      exact ⟨resolved, (resolveStack_iff baseTree layers resolved).mp hr, h⟩
    · rintro ⟨resolved', hres, hlast⟩
      have h' := (resolveStack_iff baseTree layers resolved').mpr hres
      rw [hr] at h'
      cases h'
      exact hlast

def withResolvedStack {β : Type} (onto : Nat) (layers : List (StackLayer α))
    (f : List (StackLayer α × Nat) → Option β) : Option β :=
  match resolveStack onto layers with
  | .error _ => none
  | .ok resolved => f resolved

theorem withResolvedStack_some {β : Type} (onto : Nat) (layers : List (StackLayer α))
    (f : List (StackLayer α × Nat) → Option β) (result : β) :
    withResolvedStack onto layers f = some result ↔
      ∃ resolved, ResolvedStack onto layers resolved ∧ f resolved = some result := by
  unfold withResolvedStack
  cases hr : resolveStack onto layers with
  | error err =>
    simp only [reduceCtorEq, false_iff, not_exists, not_and]
    intro resolved hres
    have h' := (resolveStack_iff onto layers resolved).mpr hres
    rw [hr] at h'
    cases h'
  | ok resolved =>
    constructor
    · intro h
      exact ⟨resolved, (resolveStack_iff onto layers resolved).mp hr, h⟩
    · rintro ⟨resolved', hres, h⟩
      have h' := (resolveStack_iff onto layers resolved').mpr hres
      rw [hr] at h'
      cases h'
      exact h

theorem candidatesOf_length (writeCommit : Nat → Nat → Nat → Nat) (parent : Nat)
    (resolved : List (StackLayer α × Nat)) :
    (candidatesOf writeCommit parent resolved).length = resolved.length := by
  induction resolved generalizing parent with
  | nil => rfl
  | cons head tail ih =>
    rcases head with ⟨layer, tree⟩
    simp [candidatesOf, ih]

theorem candidatesOf_trees (writeCommit : Nat → Nat → Nat → Nat) (commitTree : Nat → Nat)
    (hct : ∀ c p t, commitTree (writeCommit c p t) = t) (parent : Nat)
    (resolved : List (StackLayer α × Nat)) :
    (candidatesOf writeCommit parent resolved).map (fun x => commitTree x.2.2) =
      resolved.map Prod.snd := by
  induction resolved generalizing parent with
  | nil => rfl
  | cons head tail ih =>
    rcases head with ⟨layer, tree⟩
    simp [candidatesOf, hct, ih]

theorem candidatesOf_last_tree (writeCommit : Nat → Nat → Nat → Nat) (commitTree : Nat → Nat)
    (hct : ∀ c p t, commitTree (writeCommit c p t) = t) (parent : Nat)
    (resolved : List (StackLayer α × Nat)) :
    ((candidatesOf writeCommit parent resolved).getLast?.map (fun x => commitTree x.2.2)) =
      resolved.getLast?.map Prod.snd := by
  induction resolved generalizing parent with
  | nil => rfl
  | cons head tail ih =>
    rcases head with ⟨layer, tree⟩
    cases tail with
    | nil => simp [candidatesOf, hct]
    | cons next rest =>
      simp only [candidatesOf, List.getLast?_cons_cons]
      exact ih (writeCommit layer.change parent tree)

theorem candidatesOf_published_trees (writeCommit : Nat → Nat → Nat → Nat) (commitTree : Nat → Nat)
    (hct : ∀ c p t, commitTree (writeCommit c p t) = t) (parent base : Nat)
    (existing : List (Nat × Nat)) (resolved : List (StackLayer α × Nat))
    (hne : candidatesOf writeCommit parent resolved ≠ []) :
    let cands := candidatesOf writeCommit parent resolved
    ∃ topTree, resolved.getLast?.map Prod.snd = some topTree ∧
      commitTree ((cands.getLast?.map (·.2.2)).getD base) = topTree ∧
      ((cands.reverse.map (·.2) ++ existing).take resolved.length).reverse.map
        (fun x => commitTree x.2) = resolved.map Prod.snd := by
  dsimp only
  cases hlast : (candidatesOf writeCommit parent resolved).getLast? with
  | none =>
    exact absurd (List.getLast?_eq_none_iff.mp hlast) hne
  | some topCand =>
    refine ⟨commitTree topCand.2.2, ?_, ?_, ?_⟩
    · have h := candidatesOf_last_tree writeCommit commitTree hct parent resolved
      simpa [hlast] using h.symm
    · rfl
    · have hlen : ((candidatesOf writeCommit parent resolved).reverse.map (·.2)).length =
          resolved.length := by
        simp [candidatesOf_length]
      rw [List.take_left' hlen, ← List.map_reverse, List.reverse_reverse, List.map_map]
      exact candidatesOf_trees writeCommit commitTree hct parent resolved

theorem checkStackRegeneration_singleton (onto : Nat) (layer : StackLayer α) :
    checkStackRegeneration onto [layer] = checkTopRegeneration (layer.merge onto) := by
  unfold checkStackRegeneration checkTopRegeneration resolveStack
  cases finishRegeneration (layer.merge onto) with
  | ok _ => rfl
  | error err => cases err <;> rfl

theorem checkStackRegeneration_spec (onto : Nat) (layers : List (StackLayer α)) :
    (checkStackRegeneration onto layers = .landable ↔
      ∃ resolved, ResolvedStack onto layers resolved) ∧
    (checkStackRegeneration onto layers = .manual ↔
      resolveStack onto layers = .error .missing) ∧
    (checkStackRegeneration onto layers = .pendingAtLand ↔
      resolveStack onto layers = .error .pending ∨
      resolveStack onto layers = .error .failed) := by
  unfold checkStackRegeneration
  cases hr : resolveStack onto layers with
  | ok resolved =>
    refine ⟨⟨fun _ => ⟨resolved, (resolveStack_iff onto layers resolved).mp hr⟩, fun _ => rfl⟩,
      by simp, by simp⟩
  | error err =>
    cases err <;>
      refine ⟨⟨by simp, fun ⟨resolved, hres⟩ => by
        have h := (resolveStack_iff onto layers resolved).mpr hres
        rw [hr] at h; cases h⟩, by simp, by simp⟩

theorem checkStackGate_spec {ρ : Type} (earlier : Option ρ) (onto : Nat)
    (layers : List (StackLayer α)) :
    (∀ r, earlier = some r → checkStackGate earlier onto layers = .error r) ∧
    (earlier = none →
      (checkStackGate earlier onto layers = .ok .landable ↔
        ∃ resolved, ResolvedStack onto layers resolved) ∧
      (checkStackGate earlier onto layers = .ok .manual ↔
        resolveStack onto layers = .error .missing) ∧
      (checkStackGate earlier onto layers = .ok .pendingAtLand ↔
        resolveStack onto layers = .error .pending ∨
        resolveStack onto layers = .error .failed)) := by
  constructor
  · intro r hr; subst hr; rfl
  · intro hnone; subst hnone
    simpa [checkStackGate] using checkStackRegeneration_spec onto layers

end Landing
