"""Check that a large line diff cannot erase the summary of a successful commit."""

import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from review_429_cli import Project


def check(project):
    change, work = project.change("large")
    lines = 2_700_000
    (work / "many-lines.txt").write_bytes(b"x\n" * lines)
    commit = project.answer("commit", "-m", "many lines", cwd=work)
    assert commit["recorded"], commit
    check = project.answer("land", "--check", change)["check"]
    assert check["head"] == commit["tree"], check
    print(f"Recorded {lines} lines. Commit returned files={commit['files']!r} and "
          f"land_check={commit['land_check']!r}; a fresh land check succeeded.")
    assert commit["land_check"] == check, commit
    assert len(commit["files"]) == 1, commit
    assert commit["files"][0]["lines_added"] == lines, commit


if __name__ == "__main__":
    native_git = Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git" / "cmd"
    if os.name == "nt" and (native_git / "git.exe").is_file():
        os.environ["PATH"] = str(native_git) + os.pathsep + os.environ["PATH"]
    default = Path(__file__).resolve().parents[1] / "target" / "debug"
    binaries = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else default
    with TemporaryDirectory(prefix="r429-summary-") as scratch:
        Project(Path(scratch), binaries).run(check)
