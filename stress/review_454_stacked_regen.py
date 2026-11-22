"""Compare the read-only and live R8 verdicts for a lower stack layer."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from uuid import uuid4


ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "stress"))
from review_428_land_check import seed


def check_stack(root, binaries):
    repo = seed(root)
    suffix = ".exe" if os.name == "nt" else ""
    cli = binaries / f"rosen{suffix}"
    daemon = binaries / f"rosen-server{suffix}"
    endpoint = f"review-454-{uuid4().hex}" if os.name == "nt" else str(root / "server.sock")
    env = os.environ.copy()
    for key in ("ROSEN_CHANGE", "ROSEN_MANAGED_WORKSPACE", "ROSEN_LISTEN"):
        env.pop(key, None)
    env["ROSEN_SERVER"] = str(daemon)

    def rosen(*command, cwd=root, check=True):
        return subprocess.run(
            [str(cli), "--endpoint", endpoint, "--json", *command],
            cwd=cwd, env=env, capture_output=True, text=True, check=check, timeout=20,
        )

    def answer(*command, cwd=root, check=True):
        return json.loads(rosen(*command, cwd=cwd, check=check).stdout.splitlines()[-1])

    def change(name, path, *parent):
        workspace = root / name
        result = answer("change", "new", "--no-pool", "--workspace", str(workspace), *parent)
        (workspace / path).write_bytes(name.encode())
        answer("commit", "-m", name, cwd=workspace)
        return result["change"]["id"]

    with (root / "daemon.log").open("w+") as log:
        server = subprocess.Popen(
            [str(daemon), "--endpoint", endpoint, "--data-dir", str(root / "data")],
            cwd=root, env=env, stdout=log, stderr=log,
        )
        try:
            for _ in range(30):
                if rosen("server", "status", check=False).returncode == 0:
                    break
                time.sleep(0.1)
            else:
                log.seek(0)
                raise RuntimeError(log.read())
            answer("import", str(repo))
            lower = change("lower", "artifact.lock")
            child = change("child", "child.txt", "--on", lower)
            concurrent = change("concurrent", "artifact.lock")
            base = answer("land", concurrent)["land"]["base"]
            check = answer("land", "--check", child, check=False)["check"]
            live = answer("land", child, check=False)["land"]
            outcome = live["outcome"]
            assert check["base"] == live["base"] == base
            assert not check["landable"], check
            assert check["merge"]["change"] == lower, check
            assert outcome["reason"] == "regeneration_failed", outcome
            assert "regen-boom" in outcome["log"], outcome
            refusal = (check.get("refusal") or {}).get("reason")
            print(f"base={base} check={refusal} land={outcome['reason']}")
            print(outcome["log"])
            assert refusal is None and check["merge"].get("pending_at_land"), check
        finally:
            if server.poll() is None:
                server.terminate()
                server.wait(timeout=5)


if __name__ == "__main__":
    git_dir = Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git" / "cmd"
    if os.name == "nt" and (git_dir / "git.exe").is_file():
        os.environ["PATH"] = str(git_dir) + os.pathsep + os.environ["PATH"]
    binaries = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target" / "debug"
    with tempfile.TemporaryDirectory(prefix="r454-stacked-") as scratch:
        check_stack(Path(scratch), binaries)
