"""On Windows, check diff's incomplete-workspace warning after a refused flush."""

from contextlib import contextmanager
import ctypes
from ctypes import wintypes
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from review_429_cli import Project


@contextmanager
def hold_without_sharing(path):
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    open_file = kernel.CreateFileW
    open_file.argtypes = (
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE,
    )
    open_file.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
    read_access, no_sharing, open_existing, normal_file = 0x80000000, 0, 3, 128
    handle = open_file(str(path), read_access, no_sharing, None, open_existing, normal_file, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        yield
    finally:
        kernel.CloseHandle(handle)


def check(project):
    change, work = project.change("held")
    (work / "initial.txt").write_text("land this\n")
    project.answer("commit", "-m", "ready", cwd=work)
    project.answer("land", change)
    (work / "pending.txt").write_text("new and unrecorded\n")
    with hold_without_sharing(work / "src" / "tracked.txt"):
        held = project.answer("diff", cwd=work)
    unlocked = project.answer("diff", cwd=work)
    print(f"unsnapshotted with tracked.txt held: {held['unsnapshotted']}; "
          f"after release: {unlocked['unsnapshotted']}")
    assert "pending.txt" in unlocked["unsnapshotted"], unlocked
    assert "pending.txt" in held["unsnapshotted"], held


if __name__ == "__main__":
    if os.name != "nt":
        sys.exit("This check requires Windows sharing modes.")
    native_git = Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git" / "cmd"
    if (native_git / "git.exe").is_file():
        os.environ["PATH"] = str(native_git) + os.pathsep + os.environ["PATH"]
    default = Path(__file__).resolve().parents[1] / "target" / "debug"
    binaries = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else default
    with TemporaryDirectory(prefix="r429-held-") as scratch:
        Project(Path(scratch), binaries).run(check)
