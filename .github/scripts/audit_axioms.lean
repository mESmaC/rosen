/-
CI audit for AGENTS.md "Proofs": every declaration in `proofs/` may depend only
on `propext`, `Classical.choice` and `Quot.sound`.

The `#print axioms` lines in each Design.lean list only what their author chose
to print, so this audit walks every declaration of every module instead. `sorry`
and `admit` surface as `sorryAx`, `native_decide` as an auxiliary axiom of the
declaration that uses it (`<name>._native.native_decide.ax_1_1` on the pinned
toolchain), and a stray `axiom` as itself, so one allow-list check covers all
four bans. Each module is audited in its own `lean` process (see `spawnAudit`).

Run from `proofs/` after `lake build`:
  lake env lean --run ../.github/scripts/audit_axioms.lean
-/
import Lean

open Lean

def allowedAxioms : NameSet :=
  NameSet.empty |>.insert ``propext |>.insert ``Classical.choice |>.insert ``Quot.sound

/-- `collectAxioms` only needs to read the environment. -/
abbrev AuditM := ReaderT Environment Id

instance : MonadEnv AuditM where
  getEnv := read
  modifyEnv _ := pure ()

/-- Every `.lean` file under `root`, skipping Lake's build directory. -/
def proofFiles (root : System.FilePath) : IO (Array System.FilePath) := do
  let files ← root.walkDir (enter := fun dir => pure (dir.fileName != some ".lake"))
  return files.filter (·.extension == some "lean")

/-- `Landing/Design.lean` → `Landing.Design`. -/
def moduleOf (root file : System.FilePath) : Name :=
  let parts := (file.withExtension "").components.drop root.components.length
  parts.foldl (fun name part => Name.str name part) Name.anonymous

/-- Declarations defined in `module`, with the axioms each one depends on. -/
def auditedDeclarations (env : Environment) (module : Name) : Array (Name × Array Name) :=
  env.constants.map₁.fold (init := #[]) fun found name _ =>
    match env.getModuleIdxFor? name with
    | some idx =>
      if env.header.moduleNames[idx.toNat]! == module then
        found.push (name, (collectAxioms name : AuditM _) env)
      else found
    | none => found

/-- Prints each declaration of `module` that uses a banned axiom; returns how
many declarations it audited and how many failed. -/
def reportBanned (module : Name) (env : Environment) : IO (Nat × Nat) := do
  let declarations := auditedDeclarations env module
  let mut failures := 0
  for (name, axioms) in declarations do
    let banned := axioms.filter (!allowedAxioms.contains ·)
    unless banned.isEmpty do
      IO.eprintln s!"error: {module}: '{name}' depends on disallowed axioms: {banned}"
      failures := failures + 1
  return (declarations.size, failures)

/-- Blames a missing `lean_lib` only when `module` really has no .olean, so an
import that fails for another reason reports that reason instead. -/
def importFailure (module : Name) (error : IO.Error) : IO IO.Error := do
  let built ← try (← findOLean module).pathExists catch _ => pure false
  if built then
    return IO.userError s!"cannot import {module}: {error}"
  return IO.userError s!"{module} has no build output; add it to a lean_lib in \
    proofs/lakefile.toml so `lake build` checks it ({error})"

/-- Child mode: audits one module and prints `<declarations> <failures>` on
stdout for the parent. A file outside every `lean_lib` has no .olean, so its
import fails loudly rather than leaving that file unchecked. -/
def auditOne (module : Name) : IO UInt32 := do
  let env ← try importModules #[{ module }] {} catch error =>
    throw (← importFailure module error)
  let (count, failed) ← reportBanned module env
  IO.println s!"{count} {failed}"
  return if failed == 0 then 0 else 1

/-- The path `lean --run` was given for this script, so the parent can rerun
it on one module per process. -/
elab "this_script%" : term => return mkStrLit (← getFileName)

/-- Each Export module defines its own `main`, so modules cannot share one
environment. Importing them one after another in a single process fails on
Windows to read toolchain .olean files, while a fresh process imports each
one cleanly (#419), so every module gets its own `lean` process. -/
def spawnAudit (module : Name) : IO (Task (Except IO.Error IO.Process.Output)) := do
  let lean ← IO.appPath
  IO.asTask (prio := .dedicated) <| IO.Process.output
    { cmd := lean.toString, args := #["--run", this_script%, module.toString (escape := false)] }

structure Tally where
  audited : Nat := 0
  failures : Nat := 0

/-- Adds one child's counts; a child that printed none failed to audit, which
counts as a failure so the audit never passes a module it did not check. -/
def collect (tally : Tally) (module : Name) (result : Except IO.Error IO.Process.Output) :
    IO Tally := do
  let output ← IO.ofExcept result
  IO.eprint output.stderr
  match (output.stdout.trimAscii.toString.splitOn " ").map String.toNat? with
  | [some count, some failed] =>
    return { audited := tally.audited + count, failures := tally.failures + failed }
  | _ =>
    IO.eprintln s!"error: {module}: audit did not finish (exit code {output.exitCode})"
    return { tally with failures := tally.failures + 1 }

/-- How many module audits run at once; each starts its own `lean`, which
spends seconds loading `Lean` before the module's own work. -/
def parallelAudits : Nat := 4

def auditAll (modules : Array Name) : IO Tally := do
  let mut tally : Tally := {}
  for start in [0:modules.size:parallelAudits] do
    let batch := modules.extract start (start + parallelAudits)
    let tasks ← batch.mapM spawnAudit
    for (module, task) in batch.zip tasks do
      tally ← collect tally module (← IO.wait task)
  return tally

def main (args : List String) : IO UInt32 := do
  initSearchPath (← findSysroot)
  if let [module] := args then
    return ← auditOne module.toName
  let root : System.FilePath := "."
  let modules := (← proofFiles root).map (moduleOf root)
  let tally ← auditAll modules
  IO.println s!"Audited {tally.audited} declarations in {modules.size} modules; \
    {tally.failures} use disallowed axioms."
  return if tally.failures == 0 then 0 else 1
