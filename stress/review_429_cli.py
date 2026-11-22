"""Exercise the CLI and MCP contracts introduced in #429 on an isolated server."""

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
from uuid import uuid4


def seed(root):
    repo = root / "seed"
    repo.mkdir()
    (repo / "rosen.toml").write_text('review = "after-land"\n[warm_pool]\nsize = 0\n')
    (repo / "src").mkdir()
    (repo / "src" / "tracked.txt").write_text("base\n")
    for command in (
        ("init", "-q", "-b", "main"),
        ("add", "."),
        ("-c", "user.name=Rosen Scout", "-c", "user.email=scout@example.test",
         "commit", "-qm", "fixture"),
    ):
        subprocess.run(["git", *command], cwd=repo, check=True, timeout=10)
    return repo


class Project:
    def __init__(self, root, binaries):
        suffix = ".exe" if os.name == "nt" else ""
        self.cli = binaries / f"rosen{suffix}"
        self.daemon = binaries / f"rosen-server{suffix}"
        self.root = root
        self.endpoint = f"review-429-{uuid4().hex}" if suffix else str(root / "s.sock")
        self.env = os.environ.copy()
        for key in ("ROSEN_CHANGE", "ROSEN_MANAGED_WORKSPACE", "ROSEN_LISTEN"):
            self.env.pop(key, None)
        self.env.update(ROSEN_AGENT="review-429", ROSEN_SERVER=str(self.daemon))

    def cli_call(self, *words, cwd=None, managed=None, stdin=None):
        env = self.env.copy()
        if managed:
            env.update(ROSEN_CHANGE=managed, ROSEN_MANAGED_WORKSPACE="1")
        return subprocess.run(
            [str(self.cli), "--endpoint", self.endpoint, *words],
            cwd=cwd or self.root, env=env, input=stdin, capture_output=True,
            text=True, timeout=12,
        )

    def answer(self, *words, **kwargs):
        result = self.cli_call("--json", *words, **kwargs)
        assert result.returncode == 0, (words, result.stdout, result.stderr)
        return json.loads(result.stdout.splitlines()[-1])

    def change(self, name):
        workspace = self.root / name
        created = self.answer("change", "new", "--no-pool", "--workspace", str(workspace))
        return created["change"]["id"], workspace

    def tool(self, name, arguments, **kwargs):
        request = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                              "params": {"name": name, "arguments": arguments}})
        result = self.cli_call("mcp", stdin=request + "\n", **kwargs)
        assert result.returncode == 0, (result.stdout, result.stderr)
        return json.loads(result.stdout)["result"]

    def run(self, case):
        repo = seed(self.root)
        with (self.root / "server.log").open("w+") as log:
            server = subprocess.Popen(
                [str(self.daemon), "--endpoint", self.endpoint,
                 "--data-dir", str(self.root / "data")],
                cwd=self.root, env=self.env, stdout=log, stderr=log,
                creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
            )
            try:
                for _ in range(30):
                    if self.cli_call("server", "status").returncode == 0:
                        break
                    time.sleep(0.1)
                else:
                    log.seek(0)
                    raise RuntimeError(log.read())
                self.answer("import", str(repo))
                case(self)
            finally:
                if server.poll() is None:
                    server.terminate()
                    server.wait(timeout=8)


def conflict_command(project):
    authored, work = project.change("authored")
    competing, other = project.change("competing")
    path = Path("src") / "tracked.txt"
    for workspace, message in ((work, "ours"), (other, "theirs")):
        (workspace / path).write_text(message + "\n")
        project.answer("commit", "-m", message, cwd=workspace)
    project.answer("land", competing)
    current_dir = work / "src"
    answer = project.answer("rebase", cwd=current_dir, managed=authored)["rebase"]
    assert answer["conflict_paths"] == [path.as_posix()], answer
    status = project.answer("status", cwd=current_dir, managed=authored)
    assert status["conflict"] == {
        "paths": [path.as_posix()], "base_applied": True, "needs_naming_again": True,
    }, status
    (work / path).write_text("ours and theirs\n")
    command = next(cmd for cmd in answer["next_commands"] if cmd.startswith("rosen resolve "))
    returned = project.cli_call(*shlex.split(command)[1:], cwd=current_dir, managed=authored)
    print(f"from src, {command!r} exited {returned.returncode}: {returned.stdout}{returned.stderr}")
    assert returned.returncode == 0 and "no open conflicts" in returned.stdout


def complete_diff(project):
    change, work = project.change("diff")
    (work / "pending.txt").write_text("pending\n")
    diff = project.answer("diff", "--change", f" {change} ", cwd=work)
    visible = {item["path"] for item in diff["diff"]["files"]} | set(diff["unsnapshotted"])
    print(f"diff for the workspace change with surrounding spaces showed {sorted(visible)}")
    assert "pending.txt" in visible, diff


def mcp_empty_argument(project):
    change, work = project.change("empty")
    (work / "pending.txt").write_text("pending\n")
    cli = project.answer("commit", "--message=", cwd=work)
    assert cli["land_check"]["head"] == cli["tree"], cli
    assert [item["path"] for item in cli["files"]] == ["pending.txt"], cli
    result = project.tool("commit", {"message": ""}, cwd=work, managed=change)
    print(f"CLI accepted --message=; MCP returned {result}")
    assert not result["isError"], result
    assert result["structuredContent"]["schema"] == "rosen.snapshot/v1", result


def smoke(project):
    holder, _ = project.change("holder")
    waiter, work = project.change("waiter")
    (work / "immediate.txt").write_text("x\n")
    diff = project.answer("diff", cwd=work)
    assert [item["path"] for item in diff["diff"]["files"]] == ["immediate.txt"], diff
    summary = project.tool("commit", {"message": "ready"}, cwd=work, managed=waiter)
    result = summary["structuredContent"]
    assert not summary["isError"] and result["land_check"]["head"] == result["tree"], summary
    assert [item["path"] for item in result["files"]] == ["immediate.txt"], summary
    status = project.tool("status", {}, cwd=work, managed=waiter)["structuredContent"]
    assert (status["next_action"], status["actor"], status["affected_head"]) == (
        "land", "velocity", result["tree"],
    ), status
    for words, hidden in ((["--help"], ["land", "receipt"]),
                          (["workspace", "--help"], ["delete"])):
        help_text = project.cli_call(*words, cwd=work, managed=waiter).stdout
        assert all(f"\n  {name} " not in help_text for name in hidden), help_text
    assert "watcher stopped" not in project.cli_call(
        "rebase", "--help", cwd=work, managed=waiter,
    ).stdout
    project.answer("claim", "--change", holder, "--exclusive", "src/tracked.txt")
    parked = project.tool("wait", {"paths": ["src/tracked.txt"]}, cwd=work, managed=waiter)
    wait = parked["structuredContent"]["wait"]
    assert not parked["isError"] and wait["waiting_on"], parked
    project.answer("release", "--change", holder, "src/tracked.txt")
    batch = project.tool("events", {"after": str(wait["cursor"])}, cwd=work, managed=waiter)
    events = batch["structuredContent"]["events"]
    assert any(event["kind"] == "released" and event["change"] == waiter for event in events), batch
    command = ["cmd.exe", "/C", "echo ok"] if os.name == "nt" else ["sh", "-c", "echo ok"]
    run = project.tool("run", {"command": command}, cwd=work, managed=waiter)
    assert not run["isError"] and run["structuredContent"]["success"], run
    print(f"verified status, commit, diff, managed help and MCP wait/release/events/run ({holder})")


if __name__ == "__main__":
    cases = {test.__name__: test for test in (conflict_command, complete_diff, mcp_empty_argument, smoke)}
    test = cases[sys.argv[1]]
    binaries = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else (
        Path(__file__).resolve().parents[1] / "target" / "debug"
    )
    native_git = Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git" / "cmd"
    if os.name == "nt" and (native_git / "git.exe").is_file():
        os.environ["PATH"] = str(native_git) + os.pathsep + os.environ["PATH"]
    with TemporaryDirectory(prefix="r429-") as scratch:
        Project(Path(scratch), binaries).run(test)
