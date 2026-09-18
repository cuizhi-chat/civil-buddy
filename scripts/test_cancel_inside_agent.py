#!/usr/bin/env python3
"""Cooperative cancel reaches *inside* a long agent (bin3d placement loop), not only agent boundaries."""

from __future__ import annotations

import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from packing_assistant.runtime import cancel  # noqa: E402
from packing_assistant.tools.bin3d import Item3D, pack_items  # noqa: E402


def big_items(n: int = 600) -> list:
    out = []
    for i in range(n):
        out.append(Item3D(box_id=f"b{i}", dx=900 + (i % 7) * 40, dy=700 + (i % 5) * 30, dz=500 + (i % 3) * 20, weight_kg=120.0))
    return out


def main() -> int:
    # 1) outside a scope, check() is a no-op even with a pending cancel for some other key
    cancel.request("someone-else")
    cancel.check()
    cancel.clear("someone-else")

    # 2) inside a scope, a cancel requested mid-run aborts the placement loop promptly
    key = "run-cancel-inside"
    t0 = time.time()
    threading.Timer(0.15, lambda: cancel.request(key)).start()
    raised = False
    try:
        with cancel.scope(key, "sess-x"):
            pack_items(big_items(), container_type="40HQ", max_containers=8)
    except cancel.RunCancelled:
        raised = True
    dt = time.time() - t0
    cancel.clear(key)
    assert raised, "pack_items ran to completion despite cancel"
    assert dt < 5.0, f"cancel took {dt:.1f}s to take effect"

    # 3) the same session runs again cleanly after clear()
    with cancel.scope(key, "sess-x"):
        res = pack_items(big_items(60), container_type="40HQ", max_containers=2)
    assert res.get("bins") is not None or res, "second run produced nothing"
    print(f"PASS cancel_inside_agent aborted_after={dt:.2f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
