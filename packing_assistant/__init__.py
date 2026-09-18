"""智能装箱与拼柜 — 大 Team ⊃ 小 Team A 成箱 + 小 Team B 拼柜；NL 通用 Agent。

The harness (langgraph & friends) is imported lazily: light consumers such as the
Civil Buddy workbench (demo/) only need packing_assistant.llm / office_job / runtime.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from packing_assistant.config import HARNESS_VERSION

if TYPE_CHECKING:  # pragma: no cover - typing only
    from packing_assistant.harness import (  # noqa: F401
        apply_user_confirmation,
        public_response,
        run_agent_pipeline,
        run_pipeline,
        run_team_a,
        run_team_b,
    )
    from packing_assistant.state import PackingState  # noqa: F401

_HARNESS = {
    "apply_user_confirmation",
    "public_response",
    "run_agent_pipeline",
    "run_pipeline",
    "run_team_a",
    "run_team_b",
}

__all__ = [
    "HARNESS_VERSION",
    "PackingState",
    "run_team_a",
    "run_team_b",
    "run_pipeline",
    "run_agent_pipeline",
    "apply_user_confirmation",
    "public_response",
]


def __getattr__(name: str) -> Any:  # PEP 562
    if name in _HARNESS:
        from packing_assistant import harness

        return getattr(harness, name)
    if name == "PackingState":
        from packing_assistant.state import PackingState

        return PackingState
    raise AttributeError(f"module 'packing_assistant' has no attribute {name!r}")
