"""Inspect the landed R12 fixture tables for platform-specific coverage."""
from pathlib import Path
import argparse
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--root", type=Path, default=Path.cwd())
root = parser.parse_args().root
expected = {
    "batch-green.tsv": {"platform"},
    "bisection.tsv": {"platform"},
    "auto-revert.tsv": {"mode", "platform"},
}
missing = []
for name, tags in expected.items():
    table = root / "proofs" / "fixtures" / name
    observed = {line.split("\t", 1)[0] for line in table.read_text(encoding="utf-8").splitlines()[1:]}
    absent = tags - observed
    if absent:
        missing.append(f"{name}: missing {', '.join(sorted(absent))} rows")
if missing:
    print("\n".join(missing), file=sys.stderr)
    raise SystemExit(1)
