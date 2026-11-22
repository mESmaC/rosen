import BatchGreen.Design

private def platformName : Nat → String
  | 0 => "windows"
  | 1 => "macos"
  | _ => "linux"

private def formatPass (entry : Nat × Nat) : String :=
  s!"{platformName entry.1}:{entry.2}"

private def applyPasses (passes : List (Nat × Nat)) : Nat → Nat :=
  passes.foldl (fun cp entry => BatchGreen.passPlatform cp entry.1 entry.2) (fun _ => 0)

def main : IO Unit := do
  IO.println "green\ttested\tnext"
  for green in [0, 1, 3] do
    for tested in [1, 2, 3] do
      IO.println s!"{green}\t{tested}\t{BatchGreen.advance green tested}"
  for mirrors in [[2], [2, 3], [1, 2, 3]] do
    let rows := [1, 2, 3].map fun pos => if mirrors.contains pos then none else some pos
    let offered := (BatchGreen.offer 0 (BatchGreen.latestLanded rows)).getD 0
    IO.println s!"offer\t{String.intercalate "," (mirrors.map toString)}\t{offered}"
  let scenarios : List (List Nat × List (Nat × Nat)) := [
    ([0, 1], [(0, 2)]),
    ([0, 1], [(0, 2), (1, 1)]),
    ([0, 1], [(0, 2), (1, 1), (0, 1)]),
    ([0, 1], [(0, 2), (1, 3)]),
    ([0, 1], [(0, 2), (1, 3), (0, 3)]),
    ([0, 1], [(0, 2), (2, 3)]),
    ([0, 1], [(0, 2), (1, 1), (2, 3)]),
    ([0], [(0, 2), (1, 1)])
  ]
  for (required, passes) in scenarios do
    let cp := applyPasses passes
    let agg := BatchGreen.aggregateGreen required cp
    let reqCell := String.intercalate "," (required.map platformName)
    let passCell := String.intercalate "," (passes.map formatPass)
    let cpCell := String.intercalate "," ([0, 1, 2].map fun p => s!"{platformName p}:{cp p}")
    IO.println s!"platform\t{reqCell}\t{passCell}\t{agg}\t{cpCell}"

#print axioms main
