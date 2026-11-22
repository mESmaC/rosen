"""Reproduce the R8 land-check disagreement in an isolated Rosen project."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from uuid import uuid4


def git(cwd, *args):
    subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, timeout=15
    )


def seed(root):
    repo = root / "seed"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    (repo / "rosen.toml").write_text(
        'review = "after-land"\n'
        "[warm_pool]\nsize = 0\n"
        '[generated.commands]\n"artifact.lock" = "echo regen-boom && exit 1"\n',
        encoding="utf-8",
    )
    (repo / "artifact.lock").write_bytes(b"initial")
    git(repo, "add", ".")
    git(
        repo, "-c", "user.name=Rosen Scout", "-c", "user.email=scout@example.test",
        "commit", "-qm", "fixture",
    )
    return repo


def run_case(root, cli, daemon):
    repo = seed(root)
    endpoint = f"rosen-review-428-{uuid4().hex}"
    if os.name != "nt":
        endpoint = str(root / "server.sock")
    env = os.environ.copy()
    for key in ("ROSEN_CHANGE", "ROSEN_MANAGED_WORKSPACE", "ROSEN_LISTEN"):
        env.pop(key, None)
    env["ROSEN_SERVER"] = str(daemon)
    flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    args = [str(cli), "--endpoint", endpoint, "--json"]

    def rosen(*command, cwd=root, allow_refusal=False):
        result = subprocess.run(
            [*args, *command], cwd=cwd, env=env, capture_output=True,
            text=True, check=not allow_refusal, timeout=20,
        )
        return json.loads(result.stdout.splitlines()[-1])

    with (root / "server.log").open("w+", encoding="utf-8") as log:
        server = subprocess.Popen(
            [str(daemon), "--endpoint", endpoint, "--data-dir", str(root / "data")],
            cwd=root, env=env, stdout=log, stderr=log, creationflags=flags,
        )
        try:
            for _ in range(30):
                status = subprocess.run(
                    [*args, "server", "status"], cwd=root, env=env,
                    capture_output=True, timeout=3,
                )
                if status.returncode == 0:
                    break
                if server.poll() is not None:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError("isolated server did not become ready")
            rosen("import", str(repo))
            changes = {}
            for name in ("owner", "base"):
                workspace = root / name
                record = rosen("change", "new", "--no-pool", "--workspace", str(workspace))
                changes[name] = record["change"]["id"]
                (workspace / "artifact.lock").write_bytes(name.encode("ascii"))
                rosen("commit", "-m", name, cwd=workspace)
            base = rosen("land", changes["base"])["land"]["base"]
            check = rosen("land", "--check", changes["owner"], allow_refusal=True)["check"]
            land = rosen("land", changes["owner"], allow_refusal=True)["land"]
            outcome = land["outcome"]
            assert check["base"] == land["base"] == base
            assert outcome["reason"] == "regeneration_failed", outcome
            assert "regen-boom" in outcome["log"], outcome
            print(f"base={base} landable={check['landable']} land={outcome['reason']}")
            print(outcome["log"])
            assert not check["landable"], "land check promised a land that refused on the same base"
        except (subprocess.CalledProcessError, RuntimeError):
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            if server.poll() is None:
                server.terminate()
                server.wait(timeout=5)


if __name__ == "__main__":
    target = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else (
        Path(__file__).resolve().parents[1] / "target" / "debug"
    )
    suffix = ".exe" if os.name == "nt" else ""
    with tempfile.TemporaryDirectory(prefix="r428-") as directory:
        run_case(Path(directory), target / f"rosen{suffix}", target / f"rosen-server{suffix}")
