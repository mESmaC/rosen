import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid
from contextlib import contextmanager

RELEASE = "https://github.com/mESmaC/rosen/releases/download/v0.1.0-pin.g33426bb"

def release_binaries(folder):
    platform = "pc-windows-msvc.exe" if os.name == "nt" else "unknown-linux-gnu"
    names = [f"rosen-x86_64-{platform}", f"rosen-server-x86_64-{platform}"]
    with urllib.request.urlopen(f"{RELEASE}/SHA256SUMS.txt", timeout=15) as response:
        checksums = {name.lstrip("*"): digest for digest, name in
                     (line.split() for line in response.read().decode().splitlines())}
    for name in names:
        path = folder / name
        with urllib.request.urlopen(f"{RELEASE}/{name}", timeout=15) as response:
            path.write_bytes(response.read())
        assert hashlib.sha256(path.read_bytes()).hexdigest() == checksums[name]
        path.chmod(0o700)
    return tuple(folder / name for name in names)

def endpoint(folder):
    token = uuid.uuid4().hex[:10]
    if os.name == "nt":
        return r"\\.\pipe\rosen-review-" + token
    return str(folder / (token + ".sock"))

@contextmanager
def daemon(server, folder, address, *flags):
    env = {k: v for k, v in os.environ.items() if not k.startswith("ROSEN_")}
    with (folder.parent / (folder.name + ".stderr")).open("wb") as diagnostics:
        proc = subprocess.Popen(
            [server, "--endpoint", address, "--data-dir", folder, *flags],
            cwd=folder.parent, env=env, stdout=subprocess.DEVNULL, stderr=diagnostics)
        try:
            yield proc
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=5)

def cli(client, folder, address, *args):
    return subprocess.run([client, "--json", "--endpoint", address, *args],
                          cwd=folder, capture_output=True, text=True, timeout=10)

def wait_for_status(client, folder, address, proc):
    expires = time.monotonic() + 8
    while time.monotonic() < expires and proc.poll() is None:
        status = cli(client, folder, address, "server", "status")
        if status.returncode == 0:
            return json.loads(status.stdout)
        time.sleep(0.05)
    raise RuntimeError("daemon did not become ready")


def main():
    with tempfile.TemporaryDirectory(prefix="rosen-schema-review-") as tmp:
        folder = Path(tmp)
        client, server = release_binaries(folder)
        address = endpoint(folder)
        with daemon(server, folder / "authority", address) as proc:
            status = wait_for_status(client, folder, address, proc)
            catalog = json.loads(subprocess.check_output([client, "--json-schema"], cwd=folder))
            definitions = catalog.get("$defs") or catalog["schemas"]
            schema = definitions[status["schema"]]["oneOf"]
            running = next(case for case in schema if case["properties"]["running"].get("const") is True)
            extra = sorted(set(status) - set(running["properties"]))
            print(f"{status['schema']} commit={status['commit']}: additionalProperties={running['additionalProperties']}, undeclared output fields={extra}")
            stopped = cli(client, folder, address, "server", "stop")
            stopped.check_returncode()
            return int(bool(extra) and running["additionalProperties"] is False)

if __name__ == "__main__":
    raise SystemExit(main())

