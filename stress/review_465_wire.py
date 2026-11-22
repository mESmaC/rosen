"""Check the Rosen stats sender against Collapse's checked-out intake schema."""
from pathlib import Path
import re
import sys

rosen = Path(__file__).resolve().parents[1]
collapse = (
    Path(sys.argv[1]) if len(sys.argv) > 1 else
    rosen.parents[1] / "collapse.sh" / "deploy-checkout-server"
)
unit = (rosen / "crates/rosen-server/src/state_push/unit.rs").read_text()
batch = (rosen / "crates/rosen-server/src/state_push/batch.rs").read_text()
intake = (collapse / "server/internal/rosen/intake.go").read_text()

sender = re.search(r'Section::Stats\s*=>\s*"([^"]+)"', unit)
assert sender is not None, "Locate the current timing-document section"
assert 'envelope.insert(Section::Stats.key().into()' in batch
decoder = re.search(r"type rawWireBatch struct \{(.*?)\n\}", intake, re.S)
assert decoder is not None, "Locate the current Collapse intake envelope"
accepted = set(re.findall(r'json:"([^",]+)', decoder.group(1)))
print(f"Rosen emits {sender[1]!r}; Collapse decodes {sorted(accepted)!r}")
assert sender[1] in accepted, (
    "Collapse ignores Rosen's timing section as an unknown JSON field"
)
