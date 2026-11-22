import Bisection.Design
open Bisection

private def kindName : CheckKind → String
  | .build => "build"
  | .test => "test"

private def platName : Option String → String
  | none => ""
  | some p => p

private def bit (b : Bool) : String := if b then "1" else "0"

def main : IO Unit := do
  IO.println "low\thigh\tculprit\tmidpoint\tnext_low\tnext_high"
  for lo in List.range 3 do
    for width in List.range 16 do
      let hi := lo + width + 2
      for offset in List.range (hi - lo) do
        let culprit := lo + offset + 1
        let next := step lo hi culprit
        IO.println s!"{lo}\t{hi}\t{culprit}\t{midpoint lo hi}\t{next.1}\t{next.2}"
  -- #373: a recorded culprit at 3 bounds a later failure at 5, after repair at 4.
  let next := step 3 5 5
  IO.println s!"3\t5\t5\t{midpoint 3 5}\t{next.1}\t{next.2}"
  for (lo, hi, mirrors) in [(0, 5, [2, 4]), (1, 4, [2, 3]), (2, 5, [3])] do
    let rows := (List.range (hi - lo)).map fun offset =>
      let commit := lo + offset + 1
      (commit, mirrors.contains commit)
    let cell := fun (ids : List Nat) => String.intercalate "," (ids.map toString)
    IO.println s!"mirror\t{lo}\t{hi}\t{cell mirrors}\t{cell (landedInterval lo rows)}"
  -- #440/#461: (kind, platform)-keyed sessions and scoped culprit boundaries.
  let sTestMac : Scope := { kind := .test, platform := some "macos" }
  let sTestWin : Scope := { kind := .test, platform := some "windows" }
  let sBuildMac : Scope := { kind := .build, platform := some "macos" }
  let sBuildWin : Scope := { kind := .build, platform := some "windows" }
  let sTestNone : Scope := { kind := .test, platform := none }
  let cases : List (Scope × Scope × Nat × Nat × Option (Nat × Scope) × Nat) := [
    (sTestMac, sTestMac, 0, 4, none, 3),
    (sTestMac, sTestWin, 1, 5, some (3, sTestWin), 2),
    (sTestMac, sBuildMac, 1, 5, some (3, sBuildMac), 2),
    (sTestMac, sTestWin, 1, 5, some (3, sTestMac), 5),
    (sBuildWin, sBuildMac, 0, 6, some (2, sBuildWin), 4),
    (sTestMac, sTestNone, 0, 4, some (2, sTestNone), 1)
  ]
  for (s1, s2, cp, failed, rec, culprit) in cases do
    let (id1, tab1) := openSession [] { failed := failed, scope := s1 }
    let (id2, _) := openSession tab1 { failed := failed, scope := s2 }
    let culprits := match rec with | some entry => [entry] | none => []
    let bound := scopedBoundary s1 cp failed culprits
    let mid := midpoint bound failed
    let nxt := step bound failed culprit
    let (recPos, recKind, recPlat) := match rec with
      | some (pos, sc) => (pos, kindName sc.kind, platName sc.platform)
      | none => (0, "", "")
    IO.println s!"platform\t{kindName s1.kind}\t{platName s1.platform}\t{kindName s2.kind}\t{platName s2.platform}\t{cp}\t{failed}\t{recPos}\t{recKind}\t{recPlat}\t{culprit}\t{bit (id1 == id2)}\t{bit (acceptsJob s1 s2)}\t{bound}\t{mid}\t{nxt.1}\t{nxt.2}"

#print axioms main
