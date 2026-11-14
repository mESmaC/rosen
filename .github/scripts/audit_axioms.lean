/-
CI audit for AGENTS.md "Proofs": every declaration in `proofs/` may depend only
on `propext`, `Classical.choice` and `Quot.sound`.

The `#print axioms` lines in each Design.lean list only what their author chose
to print, so this audit walks every declaration of every module instead. `sorry`
and `admit` surface as `sorryAx`, `native_decide` as an auxiliary axiom of the
declaration that uses it (`<name>._native.native_decide.ax_1_1` on the pinned
toolchain), and a stray `axiom` as itself, so one allow-list check covers all
four bans.

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

/-- Prints each declaration of `module` that uses a banned axiom; returns how many. -/
def auditModule (module : Name) : IO (Nat × Nat) := do
  -- Each Export module defines its own `main`, so modules cannot share one
  -- environment. A file outside every `lean_lib` has no .olean, so this import
  -- fails loudly rather than leaving that file unchecked.
  let env ← try importModules #[{ module }] {} catch error =>
    throw <| IO.userError s!"{module} has no build output; add it to a lean_lib in \
      proofs/lakefile.toml so `lake build` checks it ({error})"
  let declarations := auditedDeclarations env module
  let mut failures := 0
  for (name, axioms) in declarations do
    let banned := axioms.filter (!allowedAxioms.contains ·)
    unless banned.isEmpty do
      IO.eprintln s!"error: {module}: '{name}' depends on disallowed axioms: {banned}"
      failures := failures + 1
  return (declarations.size, failures)

def main : IO UInt32 := do
  let root : System.FilePath := "."
  let modules := (← proofFiles root).map (moduleOf root)
  initSearchPath (← findSysroot)
  let mut audited := 0
  let mut failures := 0
  for module in modules do
    let (count, failed) ← auditModule module
    audited := audited + count
    failures := failures + failed
  IO.println s!"Audited {audited} declarations in {modules.size} modules; {failures} use disallowed axioms."
  return if failures == 0 then 0 else 1
