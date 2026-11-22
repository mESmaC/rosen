"""Check that MCP reports an unavailable workspace as a structured error."""

import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory


def check(cli, missing):
    request = {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "status", "arguments": {}},
    }
    process = subprocess.run(
        [str(cli), "mcp", "--workspace", str(missing)],
        input=json.dumps(request) + "\n", capture_output=True,
        text=True, check=True, timeout=8,
    )
    result = json.loads(process.stdout)["result"]
    print(f"MCP status with a missing workspace: {result}")
    assert result["isError"], result
    assert result.get("structuredContent", {}).get("schema") == "rosen.error/v1", result


if __name__ == "__main__":
    suffix = ".exe" if os.name == "nt" else ""
    default_cli = Path(__file__).resolve().parents[1] / "target" / "debug" / f"rosen{suffix}"
    cli = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else default_cli
    with TemporaryDirectory(prefix="r429-mcp-") as scratch:
        check(cli, Path(scratch) / "missing")
