/-
Axiom audit for the Lean proofs, run by audit-axioms.sh after `lake build`.
Usage: lake env lean --run AxiomAudit.lean <module>...

Each module is imported on its own because every fixture exporter defines
`main`, so two exporters cannot share one environment.
-/
import Lean

open Lean

def allowedAxioms : NameSet :=
  NameSet.empty |>.insert ``propext |>.insert ``Classical.choice |>.insert ``Quot.sound

/-- Declarations defined in `module` itself, not in what it imports. -/
def declarationsOf (env : Environment) (module : Name) : Array Name :=
  match env.getModuleIdx? module with
  | none => #[]
  | some idx => env.header.moduleData[idx.toNat]!.constNames

/-- One line per declaration of `module` that depends on a disallowed axiom,
and the number of declarations checked. -/
def auditModule (module : Name) : IO (Array String × Nat) := do
  let env ← importModules #[{ module }] {}
  let declarations := declarationsOf env module
  let context : Core.Context := { fileName := "<axiom audit>", fileMap := default }
  let (violations, _) ← (do
      let mut violations := #[]
      for name in declarations do
        let disallowed := (← collectAxioms name).filter (!allowedAxioms.contains ·)
        unless disallowed.isEmpty do
          violations := violations.push s!"{module}: {name} depends on {disallowed.toList}"
      return violations : CoreM (Array String)).toIO context { env }
  return (violations, declarations.size)

def main (modules : List String) : IO UInt32 := do
  initSearchPath (← findSysroot)
  let mut violations := #[]
  let mut checked := 0
  for module in modules do
    -- importModules throws on an unknown module, so a wrong name cannot pass silently.
    let (found, count) ← auditModule module.toName
    violations := violations ++ found
    checked := checked + count
  for violation in violations do
    IO.eprintln violation
  if violations.isEmpty then
    IO.println s!"axiom audit passed: {checked} declarations in {modules.length} modules"
    return 0
  IO.eprintln s!"axiom audit failed: {violations.size} violations"
  return 1
