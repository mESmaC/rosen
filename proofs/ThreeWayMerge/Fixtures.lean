import ThreeWayMerge.Design

/-!
Exports `proofs/fixtures/three-way-merge.tsv` from the model:

    lake env lean ThreeWayMerge/Fixtures.lean > fixtures/three-way-merge.tsv

Each character is one line. Columns: base, ours, theirs, merged; a conflict
is written `<ours|base|theirs>`. Cases are chosen so the longest common
subsequence fixes the alignment, so any optimal diff gives the same chunks.
-/

namespace ThreeWayMerge

def keepCount : List (Op Char) → Nat
  | [] => 0
  | .keep _ :: rest => keepCount rest + 1
  | _ :: rest => keepCount rest

/-- A longest-common-subsequence edit script, by brute force: fixtures are a
few lines long. -/
def lcsScript : List Char → List Char → List (Op Char)
  | [], ys => ys.map .ins
  | xs, [] => xs.map .del
  | x :: xs, y :: ys =>
    if x = y then .keep x :: lcsScript xs ys
    else
      let dropX := .del x :: lcsScript xs (y :: ys)
      let addY := .ins y :: lcsScript (x :: xs) ys
      if keepCount addY > keepCount dropX then addY else dropX
termination_by xs ys => xs.length + ys.length

def renderRegion : Region Char → String
  | .clean lines => String.ofList lines
  | .conflict b o t => s!"<{String.ofList o}|{String.ofList b}|{String.ofList t}>"

def fixtureRow (base ours theirs : String) : String :=
  let b := base.toList
  let merged := merge (lcsScript b ours.toList) (lcsScript b theirs.toList)
  s!"{base}\t{ours}\t{theirs}\t{String.join (merged.map renderRegion)}"

def fixtureCases : List (String × String × String) := [
  ("abc", "abc", "abc"),
  ("abc", "aXc", "abc"),
  ("abc", "abc", "aYc"),
  ("abc", "aXc", "aXc"),
  ("abc", "aXc", "aYc"),
  ("abcde", "aXcde", "abcYe"),
  ("abcd", "aXcd", "abYd"),
  ("abc", "ac", "aYc"),
  ("abc", "ac", "ac"),
  ("abc", "Xabc", "abcY"),
  ("abc", "Xabc", "Yabc"),
  ("abc", "Xabc", "Xabc"),
  ("", "ab", "ab"),
  ("", "ab", "cd"),
  ("", "", "ab"),
  ("ab", "", "ab"),
  ("ab", "", "aX"),
  ("abcdefg", "aXcdefZ", "abcYefg"),
  ("abcdefg", "aXcdeQg", "aWcdeRg"),
  ("abc", "abXYc", "abc"),
  ("abc", "abXc", "abYc"),
  ("abcd", "ad", "abcd"),
  ("abcd", "ad", "abYd"),
  ("abcdef", "abcdefXY", "Wabcdef"),
  ("abcabc", "abcXabc", "abcabc")
]

#eval IO.print (String.intercalate "\n" (fixtureCases.map fun (b, o, t) => fixtureRow b o t) ++ "\n")

end ThreeWayMerge
