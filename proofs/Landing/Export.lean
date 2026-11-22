import Landing.Design

namespace Landing

/-! ## Fixture table -/
/-- With no snapshot or inverse racing the exclusive turn, it needs one
attempt after the optimistic budget is exhausted (R8 retry; #393). -/
def contentionAttempts (cap losses : Nat) : Nat :=
  attempts cap (List.replicate losses true) + (if cap ≤ losses then 1 else 0)
def bit (b : Bool) : String := if b then "1" else "0"
def ChangeState.code : ChangeState → String
  | .active => "active" | .landed => "landed" | .abandoned => "abandoned"
def Tier.code : Tier → String
  | .prose => "prose" | .normal => "normal" | .sensitive => "sensitive"
def ReviewMode.code : ReviewMode → String
  | .beforeLand => "before-land" | .afterLand => "after-land"
def ReceiptCheck.code : ReceiptCheck → String
  | .missing => "missing" | .invalid => "invalid" | .valid => "valid" | .sameTree => "same_tree"
def Verdict.code : Verdict → String
  | .land => "land"
  | .refuse .alreadyLanded => "already_landed"
  | .refuse .abandoned => "abandoned"
  | .refuse .parentUnlanded => "parent_unlanded"
  | .refuse .conflicts => "conflicts"
  | .refuse .unapproved => "unapproved"
  | .refuse .noReceipt => "no_receipt"
  | .refuse .receiptStale => "receipt_stale"
  | .refuse .mergeConflict => "merge_conflict"
  | .refuse .needsRecheck => "needs_recheck"
/-- Every input, except that `landedSinceAllIndependent` is pinned to true when the
base has not moved: nothing landed since B, so the flag is vacuously true. -/
def allInputs : List Input :=
  [ChangeState.active, .landed, .abandoned].flatMap fun s =>
  [false, true].flatMap fun p =>
  [false, true].flatMap fun c =>
  [false, true].flatMap fun a =>
  [ReviewMode.beforeLand, .afterLand].flatMap fun v =>
  [false, true].flatMap fun x =>
  [Tier.prose, .normal, .sensitive].flatMap fun t =>
  [ReceiptCheck.missing, .invalid, .valid, .sameTree].flatMap fun r =>
  [(false, true), (true, false), (true, true)].flatMap fun (b, q) =>
  [false, true].map fun m => ⟨s, p, c, a, v, x, t, r, b, q, m⟩
def fixtureRow (i : Input) : String :=
  "\t".intercalate [i.state.code, bit i.parentLanded, bit i.hasConflicts,
    bit i.approved, i.review.code, (if i.mergeMode then "merge" else "reach"), i.tier.code, i.receipt.code, bit i.baseMoved,
    bit i.landedSinceAllIndependent, bit i.mergeClean, (decideLand i).code]
def RegenerationCheck.code : RegenerationCheck → String
  | .landable => "landable"
  | .pendingAtLand => "pending_at_land"
  | .manual => "regeneration_required"
def regenerationRow (name : String) (commands : List (Option Nat))
    (finished : Option (Option Nat)) : String :=
  let layer := LayerRegeneration.required commands finished
  let (verdict, checked) := match finishRegeneration layer with
    | .ok _ => ("land", true)
    | .error .missing => ("regeneration_required", false)
    | .error .failed => ("regeneration_failed", false)
    | .error .pending => ("pending", false)
  let initial := checkTopRegeneration (.required commands none)
  let afterPreview := checkTopRegeneration layer
  s!"regeneration\t{name}\t{verdict}\t{bit checked}\t{initial.code}\t{afterPreview.code}\n"

def stackPassingInput : Input :=
  ⟨.active, true, false, false, .afterLand, false, .normal, .missing, true, false, true⟩
def stackTreeCode : Nat → String
  | 4 => "parent_provisional"
  | 52 => "parent_regenerated"
  | 14 => "child_on_provisional"
  | 62 => "child_on_regenerated"
  | 64 => "child_provisional"
  | 72 => "child_regenerated"
  | _ => "other"
def stackWriteCommit (_change _parent tree : Nat) : Nat := 100 + tree
def stackCommitTree (commit : Nat) : Nat := commit - 100
def stackRefusalCode : RegenerationRefusal → String
  | .missing => "regeneration_required"
  | .failed => "regeneration_failed"
  | .pending => "pending"
def regenerationStackRow (name : String) (layers : List (StackLayer Input)) : String :=
  let preview := match previewStack 1 layers with
    | some tree => stackTreeCode tree
    | none => "-"
  let (verdict, parentTree, childTree) :=
    match regeneratedStackCas ⟨1, []⟩ 1 1 stackWriteCommit layers with
    | some ledger =>
      let parentCommit := (ledger.landed.find? (fun (p : Nat × Nat) => p.1 == 10)).map Prod.snd |>.getD 0
      ("land", stackTreeCode (stackCommitTree parentCommit),
        stackTreeCode (stackCommitTree ledger.base))
    | none =>
      let reason := match resolveStack 1 layers with
        | .error err => stackRefusalCode err
        | .ok _ => "refused"
      (reason, "-", "-")
  s!"regeneration_stack\t{name}\t{verdict}\t{preview}\t{parentTree}\t{childTree}\n"
def stackLayers (parent : LayerRegeneration) (childOnRegen : LayerRegeneration) :
    List (StackLayer Input) :=
  [ ⟨10, fun _ => stackPassingInput, fun onto => if onto = 1 then parent else .clean 4⟩,
    ⟨20, fun _ => stackPassingInput, fun onto => if onto = 52 then childOnRegen else .clean 14⟩ ]

def fixtureTable : String :=
  "state\tparent_landed\thas_conflicts\tapproved\treview\trecheck\ttier\treceipt\tbase_moved\t" ++
    "landed_since_all_independent\tmerge_clean\tverdict\n" ++
    String.join (allInputs.map (fixtureRow · ++ "\n")) ++
    String.join ([ReviewMode.beforeLand, .afterLand].flatMap fun review =>
      [false, true].flatMap fun build =>
        [("unscoped", false), ("required", false), ("advisory", true)].map fun (role, advisory) =>
          s!"failure\t{review.code}\t{bit build}\t{role}\t{bit (failureReverts review build advisory)}\n") ++
    String.join ([0, 1, 12].map fun cap =>
      s!"contention\t{cap}\t{cap}\t{contentionAttempts cap cap}\n") ++
    regenerationRow "partial" [some 0, none] none ++
    regenerationRow "failure" [some 0, some 0] (some none) ++
    regenerationRow "success" [some 0, some 0] (some (some 1)) ++
    regenerationStackRow "parent_regenerated_child_clean"
      (stackLayers (.required [some 7] (some (some 52))) (.clean 62)) ++
    regenerationStackRow "parent_regenerated_child_regenerated"
      (stackLayers (.required [some 7] (some (some 52))) (.required [some 8] (some (some 72)))) ++
    regenerationStackRow "parent_missing_child_clean"
      (stackLayers (.required [some 7, none] none) (.clean 62)) ++
    regenerationStackRow "parent_failed_child_clean"
      (stackLayers (.required [some 7] (some none)) (.clean 62)) ++
    regenerationStackRow "parent_regenerated_child_failed"
      (stackLayers (.required [some 7] (some (some 52))) (.required [some 8] (some none)))
end Landing

def main : IO Unit := IO.print Landing.fixtureTable
