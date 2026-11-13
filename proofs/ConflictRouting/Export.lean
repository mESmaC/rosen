import ConflictRouting.Design
open ConflictRouting

/-! Rows: the binding read (head unchanged, change closed) and its answer;
then two regions of one file, as stored before (`-` absent, `o` open, `r`
resolved) and as produced by the re-merge (`1` produced), and what the
routing transaction leaves: the stored rows after it and the rows `ours`
writes. -/

/-- Region `h` of file 0 for the merge of head 2 onto 1 from base 0. -/
def region (h : Nat) : Region := ⟨0, h, 0, 2, 1⟩
def merge (produced : List Nat) : Merge := ⟨0, 1, 2, [0], fun _ => produced⟩

def stored (a b : Option Bool) : Rows :=
  fun k => if k = region 0 then a else if k = region 1 then b else none
def code : Option Bool → String
  | none => "-" | some true => "o" | some false => "r"

def row (same closed : Bool) (a b : Option Bool) (p0 p1 : Bool) : String :=
  let s : State := ⟨0, if same then 2 else 3, closed, stored a b, []⟩
  let m := merge ((if p0 then [0] else []) ++ (if p1 then [1] else []))
  let accepted := accepts same closed
  let keys := (if a.isSome then [region 0] else []) ++ (if b.isSome then [region 1] else [])
  let after := spec s m accepted
  let bit := fun x : Bool => if x then "1" else "0"
  s!"{bit same}\t{bit closed}\t{bit accepted}\t{code a}{code b}\t{bit p0}{bit p1}\t" ++
    s!"{code (after (region 0))}{code (after (region 1))}\t{((ours s m keys).writes accepted).length}\n"

def main : IO Unit := do
  let states := [none, some true, some false]
  IO.print ("same_head\tclosed\taccepted\tstored\tproduced\trows\twrites\n" ++ String.join (
    [false, true].flatMap fun same => [false, true].flatMap fun closed =>
      states.flatMap fun a => states.flatMap fun b =>
        [false, true].flatMap fun p0 => [false, true].map fun p1 =>
          row same closed a b p0 p1))

#print axioms main
