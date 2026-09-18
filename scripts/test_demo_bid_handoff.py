#!/usr/bin/env python3
"""Drive demo workbench bid tools against shipped tender.parse (no LLM)."""

from __future__ import annotations

import json
import sys
import os
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEMO = ROOT / "demo"
sys.path.insert(0, str(DEMO))
sys.path.insert(0, str(ROOT))

from agent import execute_tool, tools_for_expert  # noqa: E402
from catalog_seed import EXPERTS  # noqa: E402


def _exp(eid: str):
    return next(e for e in EXPERTS if e.id == eid)


def main() -> int:
    parse = _exp("bid-parse")
    tech = _exp("bid-tech")
    parse_names = [t["function"]["name"] for t in tools_for_expert(parse)]
    tech_names = [t["function"]["name"] for t in tools_for_expert(tech)]
    assert "extract_tender" in parse_names
    assert "extract_tender" not in tech_names

    text = "交货期 90 个日历天。★深基坑专项须编制，不满足即废标。施工组织设计 25 分。"
    with tempfile.TemporaryDirectory() as td:
        out = Path(td)
        os.environ["CIVIL_SANDBOX_ROOTS"] = td  # sandbox only allows repo out dirs by default
        refuse = execute_tool(
            "extract_tender",
            {"tender_text": text},
            expert=tech,
            confirm_ok=True,
            out_dir=out,
            citations=[],
            deliverables=[],
        )
        assert "拒绝" in refuse
        got = execute_tool(
            "extract_tender",
            {"tender_text": text, "project_name": "handoff-unit"},
            expert=parse,
            confirm_ok=True,
            out_dir=out,
            citations=[],
            deliverables=[],
        )
        payload = json.loads(got)
        assert payload.get("duration_days") == 90
        assert payload.get("submit_blocked") is True
        table = (out / "招标解析表.md").read_text(encoding="utf-8")
        assert "90 日历天" in table
        expand = execute_tool(
            "tech_expand",
            {"tender_text": text},
            expert=tech,
            confirm_ok=True,
            out_dir=out,
            citations=[],
            deliverables=[],
        )
        assert json.loads(expand).get("from_extracted_scores") is True
    print("PASS demo_bid_handoff duration=90 exclusive_ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
