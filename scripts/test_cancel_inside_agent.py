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
    # 4) export and render loops also have checkpoints: a cancelled session cannot export in the background
    import tempfile

    from packing_assistant.demo_presets import materials_high_util, packing_options_high_util
    from packing_assistant.export_pack import export_shipment_xlsx
    from packing_assistant.harness import run_agent_pipeline
    from packing_assistant.tools.visualize import draw_layout_multi

    st = run_agent_pipeline(
        "cancel inside export probe",
        materials=materials_high_util(),
        packing_options=packing_options_high_util(),
        enable_auto_confirm=True,
        session_id="cancel-export-probe",
        save_artifacts=False,
    )
    assert (st.get("container_plan") or {}).get("can_fit") is True, st.get("phase")
    export_key = "export-cancel"
    cancel.request(export_key)
    with tempfile.TemporaryDirectory() as td:
        for label, fn in (
            ("export_shipment_xlsx", lambda: export_shipment_xlsx(st, output_dir=td)),
            ("draw_layout_multi", lambda: draw_layout_multi(st.get("container_plan") or {}, container_type=st.get("container_type") or "40HQ", output_dir=td)),
        ):
            try:
                with cancel.scope(export_key):
                    fn()
            except cancel.RunCancelled:
                print(f"  {label}: stopped at first checkpoint")
            else:
                raise AssertionError(f"{label} ran to completion despite cancel")
    cancel.clear(export_key)

    print(f"PASS cancel_inside_agent aborted_after={dt:.2f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
