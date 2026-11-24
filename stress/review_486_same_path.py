"""Run a marked Rosen data directory and its copies against a loopback feed."""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BIN = Path(sys.argv[1]).resolve()
ENV = {key: value for key, value in os.environ.items() if not key.startswith("ROSEN_")}
REQUESTS = []


class Spy(BaseHTTPRequestHandler):
    def respond(self):
        size = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(size)
        REQUESTS.append((self.command, self.path))
        body = b'{"highest_sequence":0,"generation":1,"actions":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(body)

    do_GET = respond
    do_POST = respond

    def log_message(self, *_args):
        pass


def command(endpoint, *args):
    return subprocess.run(
        [str(BIN / "rosen.exe"), "--endpoint", endpoint, *args],
        env=ENV, capture_output=True, text=True, timeout=5,
    )


@contextlib.contextmanager
def running(data, marked=False):
    endpoint = "rosen-review-433-" + uuid.uuid4().hex
    app = [str(BIN / "rosen-server.exe"), "--data-dir", str(data), "--endpoint", endpoint]
    if marked:
        app.append("--mirror-owner")
    with subprocess.Popen(
        app, cwd=data.parent, env=ENV, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW,
    ) as process:
        try:
            for _ in range(50):
                result = command(endpoint, "server", "status")
                if result.returncode == 0:
                    break
                if process.poll() is not None:
                    raise RuntimeError(f"server exited early ({process.returncode})")
                time.sleep(0.1)
            else:
                raise RuntimeError(result.stderr)
            yield endpoint
        finally:
            if process.poll() is None:
                command(endpoint, "server", "stop")
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()


def store_object(data, kind, payload):
    raw = f"{kind} {len(payload)}\0".encode() + payload
    oid = hashlib.sha1(raw).hexdigest()
    path = data / "objects" / oid[:2] / oid[2:]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(zlib.compress(raw))
    return oid


def seed_base(data, url):
    setup = f'collapse = "{url}"\nreview = "after-land"\n[warm_pool]\nsize = 0\n'.encode()
    blob = store_object(data, "blob", setup)
    tree = store_object(data, "tree", b"100644 rosen.toml\0" + bytes.fromhex(blob))
    author = "Scout <scout@example.invalid> 1700000000 +0000"
    commit = f"tree {tree}\nauthor {author}\ncommitter {author}\n\nseed\n".encode()
    base = store_object(data, "commit", commit)
    with contextlib.closing(sqlite3.connect(data / "metadata.sqlite")) as db, db:
        db.execute("INSERT INTO base_refs(name,target) VALUES('main',?)", (base,))
        db.execute(
            "INSERT INTO base_history(ref_name,commit_id,tier) VALUES('main',?,'initial')",
            (base,),
        )


def observe(name, data, wait_for_request):
    start = len(REQUESTS)
    with running(data) as endpoint:
        status = command(endpoint, "server", "status")
        status.check_returncode()
        mirror = next(line for line in status.stdout.splitlines() if line.startswith("mirror:"))
        relay = next(line for line in status.stdout.splitlines() if line.startswith("collapse relay:"))
        expected = ("/ledgers", "/rosen/state", "/rosen/actions")
        for _ in range(30):
            paths = REQUESTS[start:]
            reached = all(any(suffix in path for _, path in paths) for suffix in expected)
            if wait_for_request and reached:
                break
            time.sleep(0.1)
    paths = REQUESTS[start:]
    print(json.dumps({"case": name, "mirror": mirror, "relay": relay, "requests": paths}), flush=True)
    return mirror, paths


def main():
    with tempfile.TemporaryDirectory(prefix="rosen-review-433-") as temp, ThreadingHTTPServer(
        ("127.0.0.1", 0), Spy,
    ) as spy:
        threading.Thread(target=spy.serve_forever, daemon=True).start()
        original_path = Path(temp) / "project"
        try:
            with running(original_path, marked=True) as endpoint:
                command(endpoint, "server", "relay", "--allow", "config").check_returncode()
            seed_base(original_path, f"http://127.0.0.1:{spy.server_port}/o/r")

            copy_other_path = Path(temp) / "different-path-copy"
            copy_same_path = Path(temp) / "copy-for-original-path"
            shutil.copytree(original_path, copy_other_path)
            shutil.copytree(original_path, copy_same_path)
            original_mirror, original_requests = observe("owner restart", original_path, True)
            other_mirror, other_requests = observe("copy at another path", copy_other_path, False)

            original_dir = Path(temp) / "saved-original"
            original_path.rename(original_dir)
            copy_same_path.rename(original_path)
            same_mirror, same_requests = observe("fresh copy at the original path", original_path, True)

            assert "pushes (mirror owner)" in original_mirror
            assert any("/rosen/state" in path for _, path in original_requests), "owner never reached spy"
            assert "not the mirror owner" in other_mirror and not other_requests
            assert "not the mirror owner" in same_mirror and not same_requests, (
                "a copied data directory was accepted as owner and contacted Collapse"
            )
        finally:
            spy.shutdown()


if __name__ == "__main__":
    main()
