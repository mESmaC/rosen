"""Check stack timing stats and optional log attribution on a private Rosen daemon."""
import json
import os
from contextlib import closing
import sqlite3
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "debug"
SUFFIX = ".exe" if os.name == "nt" else ""
CLI = BIN / f"rosen{SUFFIX}"
SERVER = BIN / f"rosen-server{SUFFIX}"

def command(args, cwd, env, check=True):
    p = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True,
                       timeout=20)
    if check and p.returncode:
        raise RuntimeError(f"{args!r}: {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p

def check_stats(root, env):
    def cli(*args, cwd=root, check=True):
        return command([str(CLI), *args], cwd, env, check=check)

    def report():
        return json.loads(cli("stats", "--json", "--since", "1h").stdout)

    def change(name, parent=None):
        ws = root / name
        args = ["change", "new", "--workspace", str(ws), "--no-pool", "--owner", name]
        if parent:
            args += ["--on", parent]
        change_id = cli(*args).stdout.split()[1]
        (ws / f"{name}.txt").write_text(f"{name}\n", encoding="utf-8")
        cli("commit", "-m", f"Name {name}", cwd=ws)
        return change_id

    git = r"C:\Program Files\Git\cmd\git.exe" if os.name == "nt" else "git"
    seed = root / "seed"
    seed.mkdir()
    for args in [
        ["init", "-q", "--initial-branch=main"],
        ["config", "user.name", "Review fixture"],
        ["config", "user.email", "review@example.invalid"],
    ]:
        command([git, *args], seed, env)
    (seed / "rosen.toml").write_text(
        'review = "after-land"\n[warm_pool]\nsize = 0\n'
        '[checks.post_land]\ncommands = ["echo fixture-check"]\n', encoding="utf-8"
    )
    command([git, "add", "rosen.toml"], seed, env)
    command([git, "commit", "-qm", "fixture base"], seed, env)
    cli("import", str(seed))
    lower = change("lower")
    upper = change("upper", lower)
    cli("land", upper)
    stats = report()
    items = {c["change"]: c for c in stats["changes"]}
    for name, change_id in [("lower", lower), ("upper", upper)]:
        item = items[change_id]
        print(f"{name} {change_id}: state={item['state']}, "
              f"landed_at_ms={item['timeline']['landed_at_ms']}, "
              f"land_phases={item.get('land_phases')}")
        assert item["state"] == "landed"
        assert item["timeline"]["landed_at_ms"] is not None
    assert items[upper].get("land_phases") is not None
    assert items[lower].get("land_phases") is not None, (
        "the lower change landed atomically but has no land phase totals"
    )

    assert items[lower]["land_phases"] == items[upper]["land_phases"]
    assert stats["window"]["lands"] == 2, stats["window"]
    phases = items[upper]["land_phases"]
    assert stats["window"]["land_latency_p50_ms"] == phases["total_ms"]
    assert stats["window"]["land_latency_p95_ms"] == phases["total_ms"]
    only_lower = json.loads(cli("stats", "--json", "--change", lower).stdout)
    assert [c["land_phases"] for c in only_lower["changes"]] == [phases]
    with closing(sqlite3.connect(root / "data" / "metadata.sqlite")) as db:
        row, = db.execute("SELECT change_id FROM land_timings WHERE outcome = 'landed'")
        assert row == (upper,), row
    print("Both layers report identical phases and the stack contributed one latency sample.")
    return lower, upper

def main(mode):
    with tempfile.TemporaryDirectory(prefix="rosen-449-") as tmp:
        root = Path(tmp)
        endpoint = "rosen-449-" + uuid.uuid4().hex
        if os.name != "nt":
            endpoint = str(root / endpoint)
        env = os.environ.copy()
        for key in list(env):
            if key.startswith("ROSEN_"):
                del env[key]
        if os.name == "nt":
            fixture_git = Path(r"C:\Program Files\Git\cmd")
            if (fixture_git / "git.exe").is_file():
                env["PATH"] = str(fixture_git) + os.pathsep + env["PATH"]
        env["ROSEN_ENDPOINT"] = endpoint
        env["ROSEN_WARM_POOL_DIR"] = str(root / "pool")
        if mode == "log":
            env["ROSEN_LAND_TIMINGS"] = "1"
        flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        with (root / "daemon.log").open("w+", encoding="utf-8") as log:
            server = subprocess.Popen(
                [str(SERVER), "--data-dir", str(root / "data"), "--endpoint",
                 endpoint, "--not-mirror-owner"], cwd=root, env=env,
                stdout=log, stderr=log, creationflags=flags
            )
            try:
                for _ in range(50):
                    status = command([str(CLI), "server", "status", "--json"],
                                     root, env, check=False)
                    if status.returncode == 0:
                        print("Binary build:", json.loads(status.stdout).get("commit"))
                        break
                    if server.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    time.sleep(0.05)
                else:
                    raise RuntimeError("private server did not start")
                lower, upper = check_stats(root, env)
            finally:
                if server.poll() is None:
                    command([str(CLI), "server", "stop"], root, env, check=False)
                server.wait(timeout=10)
            if mode == "log":
                log.seek(0)
                lines = [line.strip() for line in log if "land_timing " in line]
                changes = sorted({line.split("change=", 1)[1].split()[0] for line in lines})
                print(f"Land timing log names {changes}; lower={lower}; upper={upper}")
                assert any(f"change={upper} " in line for line in lines), lines
                assert any(f"change={lower} " in line for line in lines), (
                    "the lower layer has stats timing but no attribution in the opt-in timing log"
                )

if __name__ == "__main__":
    main(sys.argv[1])
