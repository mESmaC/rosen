#!/usr/bin/env bash
# Axiom audit for the Lean proofs (AGENTS.md "Proofs"): every declaration in
# proofs/ may depend only on propext, Classical.choice and Quot.sound.
# `sorry` shows up as sorryAx, `native_decide` as an auxiliary
# `<decl>._native.native_decide.ax_*` axiom, and a hand-written `axiom` as
# itself, so this one check
# also enforces the "never use sorry, admit, axiom or native_decide" rule.
#
# Run after `lake build` in proofs/, from any directory:
#   bash .github/scripts/audit-axioms.sh
# It audits every .lean file under proofs/ (not only the default targets), so
# a new proof library is covered without editing this script.
set -euo pipefail

scripts_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$scripts_dir/../../proofs"

# Module names of every source file, e.g. Landing/Design.lean -> Landing.Design.
mapfile -t modules < <(find . -name '*.lean' -not -path './.lake/*' \
  | sed -e 's|^\./||' -e 's|\.lean$||' -e 's|/|.|g' | LC_ALL=C sort)
if [ "${#modules[@]}" -eq 0 ]; then
  echo "audit-axioms: no .lean files under proofs/" >&2
  exit 1
fi

lake env lean --run "$scripts_dir/AxiomAudit.lean" "${modules[@]}"
