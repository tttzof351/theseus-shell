"""Deterministic stdio peer: acknowledge a phase on disk, then await cancellation."""

import json
import os
from pathlib import Path
import sys
import time

phase, marker = sys.argv[1:]
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == phase:
        Path(marker).write_text(str(os.getpid()))
        time.sleep(30)  # A broken test still has a bounded peer lifetime.
        break
    if method == "initialize":
        result = {
            "protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "stalled-fixture", "version": "1"},
        }
    elif method == "tools/list":
        result = {"tools": [{"name": "stall", "inputSchema": {"type": "object"}}]}
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
