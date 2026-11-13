import AtomicTransactions.Design
open AtomicTransactions

/-- Change 1 is live at head 4 on base 0; each column toggles one gate. -/
def start (duplicate approved conflicted : Bool) : State
  | .ref => some (.commit 0)
  | .history 1 => if duplicate then some (.commit 5) else none
  | .registry 1 => some (.live 4)
  | .approval 1 => if approved then some .present else none
  | .conflict 1 => if conflicted then some .present else none
  | _ => none

def main : IO Unit := do
  IO.println "duplicate\thead_matches\tapproval_holds\topen_conflict\tbase_matches\tcommitted\tlanded"
  let bits := [false, true]
  for duplicate in bits do
    for headMatches in bits do
      for approved in bits do
        for conflicted in bits do
          for baseMatches in bits do
            for committed in bits do
              let s := start duplicate approved conflicted
              let result := publish s (if baseMatches then 0 else 9) 1
                (if headMatches then 4 else 7) 2 committed
              let bit := fun b : Bool => if b then "1" else "0"
              IO.println ("\t".intercalate [bit duplicate, bit headMatches, bit approved,
                bit conflicted, bit baseMatches, bit committed,
                bit (result .ref == some (.commit 2))])

#print axioms start
#print axioms main
