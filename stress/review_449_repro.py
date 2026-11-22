"""Exercise #449 on a private Rosen daemon with local fixture data."""
import json
import os
import socket
import struct
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
MARKER = "synthetic-positional-value-449"

def command(args, cwd, env, check=True):
    p = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True,
                       timeout=20)
    if check and p.returncode:
        raise RuntimeError(f"{args!r}: {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p

def rpc(endpoint, method, params=None):
    def exchange(stream, request):
        body = json.dumps(request).encode()
        stream.write(struct.pack(">I", len(body)) + body)
        stream.flush()
        (length,) = struct.unpack(">I", stream.read(4))
        return json.loads(stream.read(length))

    request = {"method": method}
    if params is not None:
        request["params"] = params
    if os.name == "nt":
        conn = open("\\\\.\\pipe\\" + endpoint, "r+b", buffering=0)
    else:
        sock = socket.socket(socket.AF_UNIX)
        sock.connect(endpoint)
        conn = sock.makefile("rwb")
        sock.close()
    with conn:
        hello = {"type": "hello", "versions": [1], "client": "review-fixture"}
        welcome = exchange(conn, hello)
        assert welcome["type"] == "welcome", welcome
        frame = {"type": "request", "id": 1, "request": request}
        answer = exchange(conn, frame)
        assert "ok" in answer, answer
        return answer["ok"]

def run_mode(root, env, mode):
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

    if mode == "label":
        if os.name == "nt":
            cli("run", "--full", "--", "cmd", "/C", f"echo {MARKER}")
        else:
            cli("run", "--full", "--", "sh", "-c", f"echo {MARKER}")
        labels = [item["label"] for item in report()["window"]["queue_wait_by_label"]]
        print(f"Persisted queue labels: {labels!r}")
        assert labels, "the normal run produced no timing record"
        assert all(MARKER not in label for label in labels), (
            "the default label persisted a synthetic positional argument"
        )
        return

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
    if mode == "external":
        endpoint = env["ROSEN_ENDPOINT"]
        offer = rpc(endpoint, "post_land_next")
        job = offer["body"]["job"]
        for check in job["check_set"]["commands"]:
            shell = ["cmd", "/C"] if os.name == "nt" else ["sh", "-c"]
            cli("run", "--class", "verify", "--full", "--", *shell, check)
        rpc(endpoint, "post_land_passed", job)
        checkpoints = report()["checkpoints"]
        print(f"Recorded checkpoints: {checkpoints!r}")
        assert len(checkpoints) == 1 and checkpoints[0]["outcome"] == "passed"
        assert checkpoints[0]["test_ms"] is not None, (
            "a successful, nonempty external check has no checkpoint duration"
        )
        return
    items = {c["change"]: c for c in report()["changes"]}
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
                run_mode(root, env, mode)
            finally:
                if server.poll() is None:
                    command([str(CLI), "server", "stop"], root, env, check=False)
                server.wait(timeout=10)

if __name__ == "__main__":
    main(sys.argv[1])

