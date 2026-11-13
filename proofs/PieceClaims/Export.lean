import PieceClaims.Design

/-!
# Fixture table for piece claims

Every history of up to three operations by members 1 and 2 on pieces `a`
and `b`, replayed through the design model from an empty table. Operations:
`c<m><k>` claims piece `k` for member `m`, `r<m><k>` releases it, and
`r<m>*` releases all of member `m`'s pieces. Each row gives the outcome of
each operation (`g` granted, `e` extended, `r<h>` refused naming holder `h`,
or the number of rows a release dropped) and the final holder of each piece.
-/

namespace PieceClaims.Fixtures

inductive Op where
  | claim (m : Nat) (k : String)
  | release (m : Nat) (k : Option String)

def ops : List Op :=
  [.claim 1 "a", .claim 1 "b", .claim 2 "a", .claim 2 "b",
    .release 1 (some "a"), .release 2 (some "a"), .release 1 none, .release 2 none]

def Op.name : Op → String
  | .claim m k => s!"c{m}{k}"
  | .release m (some k) => s!"r{m}{k}"
  | .release m none => s!"r{m}*"

def outcomeText : Outcome → String
  | .granted false => "g"
  | .granted true => "e"
  | .refused h => s!"r{h}"

/-- Runs one operation, returning its outcome text and the new table. -/
def step (t : Table String) : Op → String × Table String
  | .claim m k =>
    let (outcome, t') := claim t m k
    (outcomeText outcome, t')
  | .release m sel =>
    let t' := release t m fun k => sel.all (· == k)
    (toString (t.length - t'.length), t')

def run (history : List Op) : List String × Table String :=
  history.foldl (fun (out, t) op => let (o, t') := step t op; (out ++ [o], t')) ([], [])

def holderText (t : Table String) (k : String) : String :=
  match holder t k with
  | some m => s!"{k}={m}"
  | none => s!"{k}=-"

def histories : Nat → List (List Op)
  | 0 => [[]]
  | n + 1 => histories n ++ ((histories n).filter (·.length == n)).flatMap fun h =>
      ops.map fun op => h ++ [op]

def fixtureRow (history : List Op) : String :=
  let (outcomes, t) := run history
  "\t".intercalate [" ".intercalate (history.map Op.name), " ".intercalate outcomes,
    s!"{holderText t "a"} {holderText t "b"}"]

def fixtureTable : String :=
  "history\toutcomes\tholders\n" ++
    String.join ((histories 3).filter (!·.isEmpty) |>.map (fixtureRow · ++ "\n"))

end PieceClaims.Fixtures

def main : IO Unit := IO.print PieceClaims.Fixtures.fixtureTable
