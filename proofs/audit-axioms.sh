#!/usr/bin/env bash
# Axiom audit required by AGENTS.md "Proofs": every declaration in every
# module under proofs/ may depend only on propext, Classical.choice and
# Quot.sound. That one check also rejects sorry and admit (sorryAx),
# native_decide (Lean.ofReduceBool, Lean.trustCompiler) and any declared
# axiom, so it does not depend on a file remembering `#print axioms`.
#
# Run after `lake build`, from anywhere: `proofs/audit-axioms.sh`.
# Exits non-zero and names each offending declaration when the audit fails.
set -euo pipefail
cd "$(dirname "$0")"

# Every Lean source is audited, so a file that no lean_lib builds fails the
# import below instead of escaping both the build and the audit.
modules=$(find . -path ./.lake -prune -o -name '*.lean' -print |
  sed -e 's|^\./||' -e 's|\.lean$||' -e 's|/|.|g' | sort)
if [ -z "$modules" ]; then
  echo "audit-axioms: no Lean modules found" >&2
  exit 1
fi

auditor=$(mktemp -d)
trap 'rm -rf "$auditor"' EXIT

# Each module is imported into a fresh environment, one at a time, because
# the fixture exporters all define `main` and cannot be imported together.
cat >"$auditor/Audit.lean" <<'LEAN'
import Lean
open Lean

def allowedAxioms : Array Name := #[``propext, ``Classical.choice, ``Quot.sound]

instance : MonadEnv (StateM Environment) where
  getEnv := get
  modifyEnv := modify

/-- One line per declaration of `mod` that depends on a disallowed axiom. -/
def auditModule (mod : Name) : IO (Array String) := do
  let env ← importModules #[{ module := mod }] {}
  let some idx := env.getModuleIdx? mod
    | throw <| IO.userError s!"{mod} did not load"
  let decls := env.header.moduleData[idx.toNat]!.constNames
  let check (decl : Name) : StateM Environment (Option String) := do
    let extra := (← collectAxioms decl).filter (!allowedAxioms.contains ·)
    return if extra.isEmpty then none else some s!"{decl} depends on {extra.toList}"
  return (decls.filterMapM check).run' env

def main (mods : List String) : IO UInt32 := do
  initSearchPath (← findSysroot)
  let mut failed := false
  for m in mods do
    try
      let bad ← auditModule m.toName
      if bad.isEmpty then
        IO.println s!"ok   {m}"
      else
        failed := true
        for line in bad do IO.eprintln s!"FAIL {m}: {line}"
    catch e =>
      failed := true
      IO.eprintln s!"FAIL {m}: {e} (is it in a lean_lib, and did `lake build` run?)"
  return if failed then 1 else 0
LEAN

# shellcheck disable=SC2086 # one argument per module name
lake env lean --run "$auditor/Audit.lean" $modules
