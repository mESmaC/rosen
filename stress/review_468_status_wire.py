"""Replay the R8 check response from a blocked stack through rosen status."""

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
from uuid import uuid4

PARENT = "m" * 32
CHILD = "n" * 32
PARENT_HEAD = "1" * 40
CHILD_HEAD = "2" * 40
BASE = "3" * 40
MERGED = "4" * 40


def change_record():
    return {
        "id": CHILD, "owner": "scout", "parent": {"change": PARENT},
        "snapshots": [], "head": CHILD_HEAD, "state": "ready", "conflicts": [],
        "created_at": 0, "updated_at": 0,
    }


def land_check(pending):
    merge = {
        "status": "regeneration_required", "change": CHILD,
        "paths": ["artifact.lock"], "merged_tree": MERGED,
        "commands": {"artifact.lock": "echo regen-boom && exit 1"},
    }
    if pending:
        merge["pending_at_land"] = True
    return {
        "change": CHILD, "base_ref": "main", "base": BASE, "head": CHILD_HEAD,
        "review_mode": "after-land", "recheck_mode": "reach", "tier": "normal",
        "reach": "all", "merge": merge, "approval": {"status": "not_required"},
        "receipt": {"status": "not_checked"}, "landable": False,
        "stack": [{
            "change": PARENT, "head": PARENT_HEAD, "state": "abandoned",
            "tier": "normal", "blocked": "abandoned",
        }],
        "refusal": {"reason": "parent_unlanded", "message": "stack parent is abandoned"},
    }


def read_exact(read, length):
    data = bytearray()
    while len(data) < length:
        chunk = read(length - len(data))
        if not chunk:
            raise EOFError("incomplete Rosen frame")
        data.extend(chunk)
    return data


def serve(read, write, pending):
    def receive():
        length, = struct.unpack(">I", read_exact(read, 4))
        return json.loads(read_exact(read, length))

    def send(reply):
        data = json.dumps(reply).encode("utf-8")
        write(struct.pack(">I", len(data)) + data)

    hello = receive()
    assert hello["type"] == "hello", hello
    send({"type": "welcome", "version": hello["versions"][0], "server": "status fixture"})
    responses = {
        "change_show": ("change", change_record()),
        "claim_list": ("path_claims", []),
        "land_check": ("land_check", land_check(pending)),
    }
    while True:
        request = receive()
        method = request["request"]["method"]
        reply = {"type": "response", "id": request["id"]}
        if method == "post_land_status":
            reply["error"] = {"code": "internal", "message": "unused by this fixture"}
            send(reply)
            return
        kind, body = responses[method]
        reply["ok"] = {"kind": kind, "body": body}
        send(reply)


def windows_listener(pending):
    import _winapi

    endpoint = f"rosen-status-regression-{uuid4().hex}"
    pipe = _winapi.CreateNamedPipe(
        "\\\\.\\pipe\\" + endpoint, 3, 0, 1, 8192, 8192, 0, 0,
    )

    def server():
        try:
            try:
                _winapi.ConnectNamedPipe(pipe)
            except OSError as error:
                if error.winerror != 535:  # The CLI connected first.
                    raise
            serve(
                lambda length: _winapi.ReadFile(pipe, length)[0],
                lambda data: _winapi.WriteFile(pipe, data),
                pending,
            )
        finally:
            _winapi.CloseHandle(pipe)

    return endpoint, server


def unix_listener(root, pending):
    import socket

    endpoint = str(root / "status.sock")
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(endpoint)
    listener.listen()

    def server():
        with listener, listener.accept()[0] as stream:
            serve(stream.recv, stream.sendall, pending)

    return endpoint, server


def replay(root, cli, pending):
    listener = windows_listener if os.name == "nt" else lambda p: unix_listener(root, p)
    endpoint, server = listener(pending)
    worker = threading.Thread(target=server, daemon=True)
    worker.start()
    env = os.environ.copy()
    env.pop("ROSEN_CHANGE", None)
    env.pop("ROSEN_MANAGED_WORKSPACE", None)
    result = subprocess.run(
        [str(cli), "--endpoint", endpoint, "--json", "status", "--change", CHILD],
        cwd=root, env=env, capture_output=True, text=True, timeout=20,
    )
    worker.join(timeout=2)
    if result.returncode:
        raise RuntimeError(f"rosen status failed: {result.stdout}\n{result.stderr}")
    return json.loads(result.stdout)


if __name__ == "__main__":
    cli = Path(sys.argv[1]).resolve()
    expected = ("unblock_parent", PARENT_HEAD)
    with tempfile.TemporaryDirectory(prefix="rosen-status-") as temp:
        root = Path(temp)
        prior_root, landed_root = root / "prior", root / "landed"
        prior_root.mkdir()
        landed_root.mkdir()
        for label, case_root, pending in (
            ("prior", prior_root, True), ("landed", landed_root, False),
        ):
            status = replay(case_root, cli, pending)
            check = status["land_check"]
            assert check["refusal"]["reason"] == "parent_unlanded", status
            assert check["merge"].get("pending_at_land", False) == pending, check
            actual = (status["next_action"], status["affected_head"])
            print(f"{label}: refusal=parent_unlanded pending={pending} action={actual}")
            assert actual == expected, f"{label}: expected {expected}, got {actual}"
