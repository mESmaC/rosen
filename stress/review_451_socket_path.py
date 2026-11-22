"""Compare the previous and current rosen-stress Unix endpoint lengths."""
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import tempfile


def bind(path):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(path))
        listener.listen()
    path.unlink()


def main():
    if os.name == "nt":
        return subprocess.run(
            ["wsl.exe", "-d", "Ubuntu", "--", "python3", "-"],
            input=Path(__file__).read_bytes(),
            check=False,
        ).returncode

    source = Path("crates/rosen-stress/src/cluster.rs").read_text(encoding="utf-8")
    endpoint = source.split("pub fn endpoint(", 1)[1].split("pub fn report(", 1)[0]
    folders = re.findall(r'\.join\("([^"]+)"\)', endpoint)
    name = f"rosen-stress-{os.getpid()}-m0.sock"

    os.umask(0o002)
    with tempfile.TemporaryDirectory(dir="/tmp", prefix="rosen-review-451-") as temp:
        # The old endpoint fits below Linux's 108-byte sockaddr_un capacity.
        padding = 103 - len(os.fsencode(temp)) - len(os.fsencode(name)) - 2
        root = Path(temp) / ("r" * padding)
        root.mkdir(mode=0o700)
        old_path = root / name
        new_path = root.joinpath(*folders, name)
        new_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        for path in (old_path, new_path):
            mode = stat.S_IMODE(path.parent.stat().st_mode)
            assert mode == 0o700, (path, mode)
        bind(old_path)
        print(f"Previous endpoint ({len(os.fsencode(old_path))} bytes): bind succeeded")
        try:
            bind(new_path)
        except OSError as error:
            print(f"Current endpoint ({len(os.fsencode(new_path))} bytes): {error}")
            return 1
        print(f"Current endpoint ({len(os.fsencode(new_path))} bytes): bind succeeded")
        return 0


if __name__ == "__main__":
    sys.exit(main())
