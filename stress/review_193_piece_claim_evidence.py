"""Check archived §5 stress runs for participants in R13 piece claims."""

from contextlib import closing
from pathlib import Path
import json
import sqlite3
import sys
import tempfile


def check_run(run: Path) -> bool:
    reports = [
        json.loads(path.read_text(encoding="utf-8"))
        for path in run.glob("report-*.json")
    ]
    lands = sum(
        change["landed"] is not None for report in reports for change in report["changes"]
    )
    database = (run / "m0" / "metadata.sqlite").resolve(strict=True)
    with closing(sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True)) as connection:
        # Leaving the channel records left_at and keeps the member row.
        members = connection.execute("SELECT COUNT(*) FROM channel_members").fetchone()[0]
    print(f"{run.name}: {lands} lands across {len(reports)} reports; {members} AIRC member records")
    return members >= 2


archive = (
    Path(sys.argv[1]) if len(sys.argv) > 1 else Path(tempfile.gettempdir()) / "rosen-stress-193"
)
covered = [check_run(archive / name) for name in ("clean-1", "faults-1")]
if not all(covered):
    raise SystemExit("Piece-claim contention needs at least two AIRC members in each run")
