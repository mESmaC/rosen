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

def test_identity(client, server, folder):
    address = endpoint(folder)
    with daemon(server, folder / "authority", address, "--listen", "127.0.0.1:0") as proc:
        status = wait_for_status(client, folder, address, proc)
        assert status["pid"] == proc.pid
        assert Path(status["executable"]).samefile(server)
        print(f"Build {status['commit']}, executable {Path(status['executable']).name}", flush=True)
        identity = json.loads((folder / "authority/network/identity.json").read_text())
        cert = base64.b64decode(identity["certificate"])
        cert_path = folder / "cert.pem"
        cert_path.write_text(ssl.DER_cert_to_PEM_cert(cert) +
                             "-----BEGIN PRIVATE KEY-----\n" + identity["private_key"] +
                             "\n-----END PRIVATE KEY-----\n", encoding="ascii")
        cert_path.chmod(0o600)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert_path)
        stopped = cli(client, folder, address, "server", "stop")
        stopped.check_returncode()
        assert json.loads(stopped.stdout)["pid"] == proc.pid
        assert proc.poll() == 0
        absent = cli(client, folder, address, "server", "stop")
        absent.check_returncode()
        assert not json.loads(absent.stdout)["was_running"]
    return context, hashlib.sha256(cert).hexdigest()

def read_frame(stream):
    length = int.from_bytes(stream.read(4), "big")
    assert 0 < length < 65536, length
    return json.loads(stream.read(length))

def write_frame(tls, record):
    encoded = json.dumps(record).encode()
    tls.sendall(len(encoded).to_bytes(4, "big") + encoded)

def delayed_upstream(listener, context, received):
    for _ in range(5):
        raw, _ = listener.accept()
        raw.settimeout(8)
        with raw, context.wrap_socket(raw, server_side=True) as tls, tls.makefile("rb") as stream:
            hello = read_frame(stream)
            assert hello["type"] == "hello"
            write_frame(tls, {"type": "welcome", "version": 1, "server": "review fixture"})
            request = read_frame(stream)
            if request.get("request", {}).get("method") != "change_list":
                continue
            received.set()
            time.sleep(3)
            write_frame(tls, {"type": "response", "id": request["id"],
                              "ok": {"kind": "changes", "body": []}})
            return
    raise RuntimeError("no change list arrived")

def run_case(client, server, folder, tls, fingerprint, stopping):
    received = threading.Event()
    credential = folder / "fixture-credential"
    credential.write_text("local-review-fixture\n", encoding="ascii")
    address = endpoint(folder)
    with socket.socket() as listener:
        listener.settimeout(8)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        upstream = f"127.0.0.1:{listener.getsockname()[1]}"
        with daemon(server, folder / ("stopping" if stopping else "control"), address,
                    "--upstream", upstream, "--fingerprint", fingerprint,
                    "--credential-file", credential) as proc:
            status = wait_for_status(client, folder, address, proc)
            assert status["upstream"] == upstream
            with ThreadPoolExecutor(max_workers=2) as tasks:
                fixture = tasks.submit(delayed_upstream, listener, tls, received)
                request = tasks.submit(cli, client, folder, address, "change", "list")
                if not received.wait(5):
                    fixture.result(timeout=9)
                    raise RuntimeError("cache never forwarded change list")
                print(f"{'Shutdown' if stopping else 'Control'}: upstream received change list and will reply after 3s", flush=True)
                if stopping:
                    started = time.monotonic()
                    stopped = cli(client, folder, address, "server", "stop")
                    stopped.check_returncode()
                    print(f"Stop returned after {time.monotonic() - started:.2f}s (cache exit={proc.poll()})", flush=True)
                    assert json.loads(stopped.stdout)["was_running"]
                result = request.result(timeout=10)
                try:
                    fixture.result(timeout=9)
                except (OSError, ssl.SSLError):
                    if not stopping:
                        raise
                print(f"Change list exit={result.returncode}, stdout={result.stdout.strip()!r}", flush=True)
                return result.returncode

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--client", type=Path)
    parser.add_argument("--server", type=Path)
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="rosen-review-") as tmp:
        folder = Path(tmp)
        client, server = (options.client.resolve(), options.server.resolve()) if options.client else release_binaries(folder)
        tls, fingerprint = test_identity(client, server, folder)
        assert run_case(client, server, folder, tls, fingerprint, False) == 0, "control failed"
        return int(run_case(client, server, folder, tls, fingerprint, True) != 0)

if __name__ == "__main__":
    raise SystemExit(main())


