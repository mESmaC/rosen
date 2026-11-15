import Recheck.Design

def sets : List (List Nat) := [[], [0], [1], [0, 1], [2], [0, 1, 2]]
def cell (xs : List Nat) : String := String.intercalate "," (xs.map toString)
def inputCases : List (String × Recheck.Input) := [
  ("ci", .ci), ("config_operations", .config false true),
  ("config_checks", .config true true), ("config_units", .config true true),
  ("config_inputs", .config true true), ("config_generated", .config true true),
  ("config_registration", .config true true), ("config_invalid", .config false false),
  ("lock_independent", .lockfile [1] true), ("lock_shared", .lockfile [0] true),
  ("lock_invalid", .lockfile [] false), ("workspace_alias", .lockfile [1] true),
  ("member_manifest", .lockfile [1] true), ("toolchain", .shared),
  ("ci_input", .shared), ("lock_transitive", .lockfile [1] true),
  ("lock_versions", .lockfile [1] true)]

def ownershipCases : List (String × List Nat × Recheck.Input) := [
  ("configured_unit_names", [0], .lockfile [0] true),
  ("configured_multiple_holders", [0, 1], .lockfile [0, 1] true),
  ("implicit_workspace_member", [0], .lockfile [0, 1] true)]

def inputRow (name : String) (ours : List Nat) (input : Recheck.Input) : String :=
  let reached := Recheck.inputReach [0, 1, 2] input
  String.intercalate "\t" [cell ours, cell reached, "",
    if Recheck.reusable ours [reached] then "1" else "0", name]

def main : IO Unit := do
  IO.println "ours\tfirst\tsecond\treuse\tcase"
  for ours in sets do
    for first in sets do
      for second in sets do
        IO.println (String.intercalate "\t" [cell ours, cell first, cell second,
          if Recheck.reusable ours [first, second] then "1" else "0", ""])
  for (name, input) in inputCases do
    IO.println (inputRow name [0] input)
  for (name, ours, input) in ownershipCases do
    IO.println (inputRow name ours input)
