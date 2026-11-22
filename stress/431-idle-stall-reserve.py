"""Reproduce #431's misleading idle-stall advice using an isolated Windows queue."""

import msvcrt
import os
from pathlib import Path
import subprocess
import tempfile
import time


def run_rosen(env, *args):
    result = subprocess.run(
        ["rosen", "run", *args],
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )
    return result.stdout


def check_queue(directory):
    env = dict(os.environ, ROSEN_RUN_QUEUE=directory)
    env.pop("ROSEN_RUN_HELD", None)
    run_rosen(env, "--slots", "reserve-gib=1024")

    # Seed a locked head ticket just past STARVING without sleeping five minutes.
    gib = 1 << 30
    arrived = time.time_ns() - 301_000_000_000
    ticket = Path(directory) / f"1-{arrived:030}-{os.getpid()}-0~1x{gib}.wait"
    with ticket.open("w+b") as live:
        msvcrt.locking(live.fileno(), msvcrt.LK_NBLCK, 1)
        status = run_rosen(env, "--slots")
    ticket.unlink()

    print(status, end="")
    stall = next(line for line in status.splitlines() if line.startswith("stall: "))
    assert "0.00 GiB reserved by runs; 1024.00 GiB reserve" in status

    # A real head with this budget starts immediately under the same reserve.
    started = run_rosen(
        env, "--class", "build", "--jobs", "1", "--memory-gib", "1",
        "--full", "--", "cmd.exe", "/d", "/c", "echo", "started-under-1024-gib",
    )
    print(started, end="")
    assert "started-under-1024-gib" in started
    assert "unblock with" not in stall, (
        "Idle admission already uses the 4 GiB floor. Lowering reserve-gib "
        "cannot unblock this head and changes the budget for concurrent turns."
    )


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rosen-431-scout-") as queue:
        check_queue(queue)
