import Recheck.Design

def sets : List (List Nat) := [[], [0], [1], [0, 1], [2], [0, 1, 2]]
def cell (xs : List Nat) : String := String.intercalate "," (xs.map toString)
def main : IO Unit := do
  IO.println "ours\tfirst\tsecond\treuse"
  for ours in sets do
    for first in sets do
      for second in sets do
        IO.println (String.intercalate "\t" [cell ours, cell first, cell second,
          if Recheck.reusable ours [first, second] then "1" else "0"])
