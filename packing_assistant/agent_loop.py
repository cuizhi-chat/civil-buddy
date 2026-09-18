"""LLM 自主多轮 tool-call 循环（大 Team 控制面）。

设计:
  - LLM 只输出「下一步调用哪个工具 + 参数」，不写 xyz / 柜数拍脑袋
  - 工具白名单 → 确定性 handler（IntentSpec / TeamA / TeamB / critic / finalize…）
  - 无 API Key 或 LLM 失败时回退固定专业节点调度（big_team 默认路径）
  - 有界：max_rounds 默认 12；HITL 可中断

环境:
  PACKING_LLM_AGENT=1  启用（也可 API agent_mode=llm_toolcall）
"""

from __future__ import annotations

import json
import os
import re
import time
from typing import Any, Callable, Dict, Generator, List, Optional, Tuple


MAX_ROUNDS_DEFAULT = 12

# 允许 LLM 选择的工具（id → 说明）
LLM_TOOL_SPECS: List[Dict[str, str]] = [
    {
        "id": "intent.interpret",
        "team": "big",
        "desc": "解析 NL → IntentSpec，写入 packing_options / 柜数",
    },
    {
        "id": "container.select",
        "team": "big",
        "desc": "按材料推荐柜型",
    },
    {
        "id": "team_a.run",
        "team": "A",
        "desc": "跑小TeamA成箱全段（材料→结构→成箱→present）；auto_confirm 控制 HITL",
    },
    {
        "id": "team_a.rebox",
        "team": "A",
        "desc": "仅重做成箱（structure+box_scheme），用于 critic 打回",
    },
    {
        "id": "hitl.check",
        "team": "big",
        "desc": "检查是否需停在 HITL；若需等待则 stop",
    },
    {
        "id": "team_b.plan_load_eval",
        "team": "B",
        "desc": "小TeamB 内环一轮：planner→loader→evaluator",
    },
    {
        "id": "team_b.risk",
        "team": "B",
        "desc": "风险合规",
    },
    {
        "id": "team_b.visualize",
        "team": "B",
        "desc": "可视化出图",
    },
    {
        "id": "replan.critic",
        "team": "big",
        "desc": "有界批评：只改 options/路由，不写坐标",
    },
    {
        "id": "knowledge.search",
        "team": "big",
        "desc": "按 agent 窄接检索 knowledge_base（规则/协议/轨迹）；不返回坐标",
    },
    {
        "id": "finalize.run",
        "team": "big",
        "desc": "大Team收口；调用后应 finish",
    },
    {
        "id": "finish",
        "team": "big",
        "desc": "结束循环（HITL 等待或已 finalize）",
    },
]


def llm_agent_enabled(explicit: Optional[bool] = None) -> bool:
    if explicit is not None:
        return bool(explicit)
    v = (os.getenv("PACKING_LLM_AGENT") or "0").strip().lower()
    return v in ("1", "true", "yes", "on")


def _parse_action(text: str) -> Optional[Dict[str, Any]]:
    if not text or text.startswith("[LLM_ERROR]"):
        return None
    raw = text.strip()
    fence = re.search(r"```(?:json)?\s*([\s\S]*?)```", raw)
    if fence:
        raw = fence.group(1).strip()
    # 找第一个 { ... }
    start, end = raw.find("{"), raw.rfind("}")
    if start < 0 or end <= start:
        return None
    try:
        data = json.loads(raw[start : end + 1])
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    tool = str(data.get("tool") or data.get("name") or data.get("action") or "").strip()
    if not tool:
        return None
    args = data.get("args") or data.get("arguments") or {}
    if not isinstance(args, dict):
        args = {}
    return {
        "tool": tool,
        "args": args,
        "reason": str(data.get("reason") or data.get("thought") or "")[:300],
    }


def _state_digest(state: Dict[str, Any]) -> Dict[str, Any]:
    plan = state.get("container_plan") or {}
    rr = state.get("risk_report") or {}
    ev = state.get("evaluation") or {}
    ispec = state.get("intent_spec") or {}
    return {
        "phase": state.get("phase"),
        "goal": state.get("goal"),
        "n_materials": len(state.get("materials") or []),
        "n_boxes": len(state.get("boxes") or []),
        "container_type": state.get("container_type"),
        "max_containers": state.get("max_containers"),
        "can_fit": plan.get("can_fit"),
        "containers_used": plan.get("containers_used"),
        "eval_decision": ev.get("decision"),
        "need_replan": ev.get("need_replan"),
        "risk_decision": rr.get("decision"),
        "ship_ok": state.get("ship_ok"),
        "replan_round": state.get("replan_round"),
        "ship_replan_round": state.get("ship_replan_round"),
        "scheme_id": ispec.get("scheme_id"),
        "cargo_mode": ispec.get("cargo_mode"),
        "user_action": state.get("user_action"),
        "enable_auto_confirm": state.get("enable_auto_confirm"),
        "has_views": bool((state.get("views") or {}).get("side") or (state.get("views") or {}).get("top")),
        "final_response_set": bool(state.get("final_response")),
    }


def _system_prompt() -> str:
    tools = "\n".join(
        f"- {t['id']} [{t['team']}]: {t['desc']}" for t in LLM_TOOL_SPECS
    )
    return f"""你是装柜大 Team 的调度 Agent。根据当前状态选择**下一步唯一工具**。

规则:
1. 只输出一个 JSON 对象，不要其它文字。格式:
   {{"tool":"<id>","args":{{}},"reason":"简短原因"}}
2. 禁止编造尺寸、坐标、柜数；数值一律由工具计算。
3. 典型顺序: intent.interpret → container.select(可选) → team_a.run
   → hitl.check → team_b.plan_load_eval → (need_replan 则 replan.critic 再 plan_load_eval)
   → team_b.risk → team_b.visualize → finalize.run → finish
4. enable_auto_confirm=false 且 phase=await_user_confirm 时调用 finish。
5. 已 finalize 或 ship 结束则 finish。
6. replan 内环≤3、出运外环≤2；到上限仍失败则 finalize 后 finish。
7. 需要规则依据/解释时用 knowledge.search；args 建议含 agent_id
   （orchestrator|replan_critic|risk_compliance|planner|box_scheme|finalize 等）；
   loader 不靠检索写坐标。

可用工具:
{tools}
"""


def _ask_llm(state: Dict[str, Any], history: List[str]) -> Optional[Dict[str, Any]]:
    from packing_assistant.llm import chat, llm_available

    if not llm_available():
        return None
    digest = _state_digest(state)
    user = (
        "当前状态:\n"
        + json.dumps(digest, ensure_ascii=False, indent=2)
        + "\n\n最近工具:\n"
        + ("\n".join(history[-6:]) if history else "(无)")
        + "\n\n请选择下一步 tool JSON。"
    )
    text = chat(_system_prompt(), user, temperature=0.1, max_tokens=400)
    return _parse_action(text or "")


def _fallback_policy(
    state: Dict[str, Any],
    called: set,
    *,
    last_tool: str = "",
    critic_count: int = 0,
) -> Dict[str, Any]:
    """无 LLM 时的确定性策略（保证可跑通，避免 critic 死循环）。"""
    dig = _state_digest(state)
    if "intent.interpret" not in called and not (state.get("intent_spec") or {}).get(
        "scheme_id"
    ):
        return {"tool": "intent.interpret", "args": {}, "reason": "fallback:intent"}
    if dig["n_boxes"] == 0 and "team_a.run" not in called:
        return {"tool": "team_a.run", "args": {}, "reason": "fallback:team_a"}
    if dig["phase"] == "await_user_confirm" and not dig["enable_auto_confirm"]:
        return {"tool": "finish", "args": {}, "reason": "fallback:hitl_wait"}
    # 刚 critic 过 → 必须再跑一轮 plan/load/eval
    if last_tool == "replan.critic":
        return {
            "tool": "team_b.plan_load_eval",
            "args": {},
            "reason": "fallback:after_critic",
        }
    if last_tool == "team_a.rebox":
        return {
            "tool": "team_b.plan_load_eval",
            "args": {},
            "reason": "fallback:after_rebox",
        }
    if dig["n_boxes"] and dig["can_fit"] is None and "team_b.plan_load_eval" not in called:
        return {"tool": "team_b.plan_load_eval", "args": {}, "reason": "fallback:b_loop"}
    if (
        dig["need_replan"]
        and int(dig.get("replan_round") or 0) <= 3
        and critic_count < 3
        and last_tool != "replan.critic"
    ):
        if dig.get("eval_decision") == "REJECT_STRUCTURE":
            return {"tool": "team_a.rebox", "args": {}, "reason": "fallback:rebox"}
        return {"tool": "replan.critic", "args": {}, "reason": "fallback:critic"}
    if dig["can_fit"] is not None and dig["risk_decision"] is None:
        return {"tool": "team_b.risk", "args": {}, "reason": "fallback:risk"}
    if dig["risk_decision"] and not dig["has_views"]:
        return {"tool": "team_b.visualize", "args": {}, "reason": "fallback:viz"}
    if not dig["final_response_set"]:
        return {"tool": "finalize.run", "args": {}, "reason": "fallback:finalize"}
    return {"tool": "finish", "args": {}, "reason": "fallback:done"}


def _run_agent_fn(fn: Callable, state: Dict[str, Any]) -> Dict[str, Any]:
    upd = fn(state) or {}
    for k, v in upd.items():
        if k in (
            "messages",
            "traces",
            "errors",
            "validation_warnings",
            "agent_steps",
        ) and isinstance(v, list):
            state[k] = list(state.get(k) or []) + v
        else:
            state[k] = v
    return state


def _dispatch(
    tool: str,
    state: Dict[str, Any],
    *,
    enable_auto_confirm: bool,
) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    """执行白名单工具。返回 (state, meta)."""
    meta: Dict[str, Any] = {"tool": tool, "ok": True}
    t0 = time.perf_counter()

    if tool in ("finish", "done", "stop"):
        meta["finish"] = True
        meta["duration_ms"] = 0
        return state, meta

    if tool == "intent.interpret":
        from packing_assistant.intent_spec import apply_intent_to_state, intent_from_api

        spec = intent_from_api(
            user_input=state.get("user_input") or "",
            materials=state.get("materials"),
            packing_options=state.get("packing_options"),
            max_containers=int(state.get("max_containers") or 0),
            goal=str(state.get("goal") or "deliver_valid_pack_plan"),
            container_type=str(state.get("container_type") or ""),
            source="llm_agent",
        )
        state = apply_intent_to_state(
            state, spec, materials=state.get("materials"), filter_materials=True
        )
        meta["artifacts"] = {"scheme_id": spec.scheme_id, "cargo_mode": spec.cargo_mode}

    elif tool == "container.select":
        from packing_assistant.tools.container_select import recommend_container

        rec = recommend_container(
            materials=state.get("materials") or [],
            user_hint=state.get("container_type"),
            phase="start",
        )
        if rec.get("recommended"):
            state["container_type"] = rec["recommended"]
        state.setdefault("orchestrator", {})["container_select_start"] = rec
        meta["artifacts"] = rec

    elif tool == "team_a.run":
        from packing_assistant.agents import (
            agent_box_scheme,
            agent_material_parser,
            agent_orchestrator,
            agent_present_team_a,
            agent_structure,
        )
        from packing_assistant.harness import apply_user_confirmation

        state["phase"] = "team_a_running"
        for fn in (
            agent_orchestrator,
            agent_material_parser,
            agent_structure,
            agent_box_scheme,
            agent_present_team_a,
        ):
            state = _run_agent_fn(fn, state)
        if enable_auto_confirm:
            state = apply_user_confirmation(
                state,
                action="confirm",
                container_type=state.get("container_type") or "40HQ",
                max_containers=int(state.get("max_containers") or 0),
            )
            state["phase"] = "team_b_running"
        else:
            state["phase"] = "await_user_confirm"
            state["user_action"] = None
        meta["artifacts"] = {"n_boxes": len(state.get("boxes") or []), "phase": state.get("phase")}

    elif tool == "team_a.rebox":
        from packing_assistant.agents import agent_box_scheme, agent_structure

        state = _run_agent_fn(agent_structure, state)
        state = _run_agent_fn(agent_box_scheme, state)
        state["replan_round"] = 0
        meta["artifacts"] = {"n_boxes": len(state.get("boxes") or [])}

    elif tool == "hitl.check":
        if (
            not enable_auto_confirm
            and (state.get("phase") or "") == "await_user_confirm"
        ):
            meta["finish"] = True
            meta["hitl"] = True
            try:
                from packing_assistant.hitl_summary import build_hitl_summary

                state["hitl_summary"] = build_hitl_summary(state)
            except Exception:
                pass
        meta["artifacts"] = {"phase": state.get("phase")}

    elif tool == "team_b.plan_load_eval":
        from packing_assistant.agents import (
            agent_evaluator,
            agent_loader,
            agent_planner,
        )

        state["phase"] = "team_b_running"
        for fn in (agent_planner, agent_loader, agent_evaluator):
            state = _run_agent_fn(fn, state)
        meta["artifacts"] = {
            "can_fit": (state.get("container_plan") or {}).get("can_fit"),
            "need_replan": (state.get("evaluation") or {}).get("need_replan"),
        }

    elif tool == "team_b.risk":
        from packing_assistant.agents import agent_risk_compliance

        state = _run_agent_fn(agent_risk_compliance, state)
        meta["artifacts"] = {
            "decision": (state.get("risk_report") or {}).get("decision")
        }

    elif tool == "team_b.visualize":
        from packing_assistant.agents import agent_visualizer

        state = _run_agent_fn(agent_visualizer, state)
        meta["artifacts"] = {"has_views": bool(state.get("views"))}

    elif tool == "replan.critic":
        from packing_assistant.agents.replan_critic import agent_replan_critic

        state = _run_agent_fn(agent_replan_critic, state)
        prop = state.get("replan_proposal") or {}
        route = str(prop.get("route") or "planner")
        if route == "box_scheme" and not prop.get("stop"):
            from packing_assistant.agents import agent_box_scheme, agent_structure

            state = _run_agent_fn(agent_structure, state)
            state = _run_agent_fn(agent_box_scheme, state)
            state["replan_round"] = 0
        meta["artifacts"] = {"proposal": prop}

    elif tool == "knowledge.search":
        from packing_assistant.kb_bindings import search_for_agent
        from packing_assistant.tools.search_knowledge import search_knowledge

        # args 由调用方写入 state["_llm_tool_args"] 或 meta 前置；此处从 state 取
        args = dict(state.get("_llm_tool_args") or {})
        agent_id = str(args.get("agent_id") or "orchestrator")
        q = str(args.get("q") or args.get("query") or state.get("goal") or "装柜规则")
        lim = int(args.get("limit") or 4)
        if agent_id:
            res = search_for_agent(agent_id, q, limit=lim)
        else:
            res = search_knowledge(q, limit=lim)
        state.setdefault("kb_hits", []).append(res)
        state["kb_last"] = res
        meta["artifacts"] = {
            "agent_id": agent_id,
            "n_hits": res.get("n_hits"),
            "paths": [h.get("path") for h in (res.get("hits") or [])[:5]],
        }

    elif tool == "finalize.run":
        from packing_assistant.agents import agent_finalize

        # 确保 risk/viz 至少尝试过
        if not (state.get("risk_report") or {}).get("decision"):
            from packing_assistant.agents import agent_risk_compliance

            state = _run_agent_fn(agent_risk_compliance, state)
        if not (state.get("views") or {}):
            from packing_assistant.agents import agent_visualizer

            state = _run_agent_fn(agent_visualizer, state)
        state = _run_agent_fn(agent_finalize, state)
        meta["finish_after"] = True
        meta["artifacts"] = {"ship_ok": state.get("ship_ok")}

    else:
        meta["ok"] = False
        meta["error"] = f"unknown or forbidden tool: {tool}"
        state.setdefault("errors", []).append(meta["error"])

    meta["duration_ms"] = int((time.perf_counter() - t0) * 1000)
    return state, meta


def run_llm_agent_loop(
    raw_input: str = "",
    *,
    materials: Optional[List[Dict[str, Any]]] = None,
    container_type: str = "40HQ",
    max_containers: int = 0,
    enable_auto_confirm: bool = True,
    goal: str = "deliver_valid_pack_plan",
    packing_options: Optional[Dict[str, Any]] = None,
    session_id: str = "",
    save_artifacts: bool = True,
    max_rounds: int = MAX_ROUNDS_DEFAULT,
    on_event: Optional[Any] = None,
    force_llm: bool = True,
) -> Dict[str, Any]:
    final = None
    for ev in iter_llm_agent_loop(
        raw_input,
        materials=materials,
        container_type=container_type,
        max_containers=max_containers,
        enable_auto_confirm=enable_auto_confirm,
        goal=goal,
        packing_options=packing_options,
        session_id=session_id,
        save_artifacts=save_artifacts,
        max_rounds=max_rounds,
        on_event=on_event,
        force_llm=force_llm,
    ):
        if ev.get("type") == "done" and ev.get("state") is not None:
            final = ev["state"]
    return final or {}


def iter_llm_agent_loop(
    raw_input: str = "",
    *,
    materials: Optional[List[Dict[str, Any]]] = None,
    container_type: str = "40HQ",
    max_containers: int = 0,
    enable_auto_confirm: bool = True,
    goal: str = "deliver_valid_pack_plan",
    packing_options: Optional[Dict[str, Any]] = None,
    session_id: str = "",
    save_artifacts: bool = True,
    max_rounds: int = MAX_ROUNDS_DEFAULT,
    on_event: Optional[Any] = None,
    force_llm: bool = True,
) -> Generator[Dict[str, Any], None, None]:
    """多轮 tool-call；yield stream 事件。"""
    from packing_assistant.config import DEFAULT_CONTAINER_TYPE
    from packing_assistant.harness import _attach_artifacts, make_initial_state, public_response
    from packing_assistant.llm import llm_available
    from packing_assistant.teams.roster import AGENT_ROSTER, TEAM_ARCHITECTURE
    from packing_assistant.tool_registry import list_tools
    from packing_assistant.trace_events import append_trace_event

    state = make_initial_state(
        user_input=raw_input or "llm_agent",
        materials=list(materials or []),
        container_type=container_type or DEFAULT_CONTAINER_TYPE,
        enable_auto_confirm=enable_auto_confirm,
        max_containers=int(max_containers or 0),
        goal=goal,
        session_id=session_id,
    )
    if packing_options:
        state["packing_options"] = dict(packing_options)
    state["team_mode"] = "big_team_a_b"
    state["agent_style"] = "llm_toolcall" if (force_llm and llm_available()) else "policy_fallback"
    state["team_architecture"] = dict(TEAM_ARCHITECTURE)
    state["agent_roster"] = list(AGENT_ROSTER)
    state["available_tools"] = list_tools()
    state["llm_tool_specs"] = list(LLM_TOOL_SPECS)

    rid = str(state.get("run_id") or "run")
    sid = str(state.get("session_id") or rid)
    seq = 0
    steps: List[Dict[str, Any]] = []
    history: List[str] = []
    called: set = set()
    use_llm = bool(force_llm and llm_available())

    def emit(ev: Dict[str, Any]) -> Dict[str, Any]:
        nonlocal seq
        seq += 1
        payload = dict(ev)
        payload["seq"] = seq
        payload.setdefault("run_id", rid)
        payload.setdefault("session_id", sid)
        payload.setdefault("team_mode", "big_team_a_b")
        payload.setdefault("agent_style", state.get("agent_style"))
        try:
            payload = append_trace_event(rid, payload)
        except Exception:
            pass
        try:
            from packing_assistant.ws_hub import HUB

            HUB.publish(sid, payload)
        except Exception:
            pass
        if on_event:
            try:
                on_event(payload)
            except Exception:
                pass
        return payload

    yield emit(
        {
            "type": "run_start",
            "status": "running",
            "agent_style": state.get("agent_style"),
            "llm_available": use_llm,
            "max_rounds": max_rounds,
        }
    )

    finished = False
    last_tool = ""
    critic_count = 0
    for rnd in range(max_rounds):
        from packing_assistant.runtime import cancel as _cancel

        _cancel.check()
        action = _ask_llm(state, history) if use_llm else None
        if not action:
            action = _fallback_policy(
                state, called, last_tool=last_tool, critic_count=critic_count
            )
            if use_llm:
                history.append(f"r{rnd}:llm_parse_fail→fallback")

        tool = str(action.get("tool") or "finish")
        reason = str(action.get("reason") or "")
        args = action.get("args") if isinstance(action.get("args"), dict) else {}
        state["_llm_tool_args"] = dict(args or {})
        yield emit(
            {
                "type": "agent_start",
                "node": "llm_scheduler",
                "team": "big",
                "title": f"LLM调度 r{rnd+1}",
                "status": "running",
                "tool": tool,
                "reason": reason,
            }
        )
        yield emit(
            {
                "type": "tool_start",
                "node": "llm_scheduler",
                "tool": tool,
                "team": "big",
                "status": "running",
                "round": rnd + 1,
            }
        )

        state, meta = _dispatch(
            tool, state, enable_auto_confirm=enable_auto_confirm
        )
        state.pop("_llm_tool_args", None)
        called.add(tool)
        last_tool = tool
        if tool == "replan.critic":
            critic_count += 1
        history.append(f"r{rnd+1}:{tool} ok={meta.get('ok')} {reason[:80]}")

        step = {
            "node": "llm_scheduler" if tool not in (
                "team_a.run",
                "team_b.plan_load_eval",
                "finalize.run",
            ) else tool.replace(".", "_"),
            "title": f"tool:{tool}",
            "team": "big",
            "message": f"【LLM tool-call】{tool} | {reason} | {meta.get('artifacts') or {}}",
            "tools_used": [tool],
            "capability": ["规划", "使用工具"],
            "artifacts": meta.get("artifacts") or {},
            "duration_ms": meta.get("duration_ms"),
            "status": "ok" if meta.get("ok") else "error",
            "round": rnd + 1,
        }
        # 把子 agent 已写入的 agent_steps 保留；再追加调度步
        steps.append(step)
        state["agent_steps"] = list(state.get("agent_steps") or []) + [step]
        state.setdefault("messages", []).append(
            {
                "role": "assistant",
                "content": step["message"],
                "agent": "llm_scheduler",
            }
        )

        yield emit(
            {
                "type": "tool_end",
                "node": "llm_scheduler",
                "tool": tool,
                "team": "big",
                "status": step["status"],
                "duration_ms": meta.get("duration_ms"),
                "round": rnd + 1,
            }
        )
        yield emit(
            {
                "type": "agent_end",
                "node": "llm_scheduler",
                "team": "big",
                "status": step["status"],
                "step": step,
                "phase": state.get("phase"),
            }
        )

        if meta.get("hitl"):
            yield emit(
                {
                    "type": "hitl",
                    "node": "hitl_wait",
                    "status": "wait",
                    "team": "big",
                    "phase": "await_user_confirm",
                    "hitl_summary": state.get("hitl_summary"),
                }
            )
            finished = True
            break

        if meta.get("finish") or meta.get("finish_after") or tool == "finish":
            if meta.get("finish_after") or tool == "finalize.run":
                # ensure finish after finalize
                pass
            if tool == "finish" or meta.get("finish"):
                finished = True
                break
            if meta.get("finish_after"):
                # one more loop likely finish
                continue

        # after replan.critic, auto one more plan_load if need_replan cleared path
        if tool == "replan.critic":
            prop = state.get("replan_proposal") or {}
            if not prop.get("stop") and prop.get("route") != "box_scheme":
                # next round LLM/policy will pick plan_load_eval
                pass

    if not finished and not state.get("final_response"):
        # 强制收口
        state, meta = _dispatch(
            "finalize.run", state, enable_auto_confirm=enable_auto_confirm
        )
        steps.append(
            {
                "node": "finalize_run",
                "title": "tool:finalize.run",
                "team": "big",
                "message": "【强制收口】max_rounds 到顶",
                "tools_used": ["finalize.run"],
                "status": "ok",
            }
        )

    state["agent_steps"] = list(state.get("agent_steps") or []) + [
        s for s in steps if s not in (state.get("agent_steps") or [])
    ]
    # dedupe by keeping full list from state
    if save_artifacts:
        state = _attach_artifacts(state, steps=state.get("agent_steps"))

    try:
        from packing_assistant.session_store import save_session

        save_session(sid, state)
    except Exception:
        pass

    pub = public_response(state)
    summary = {
        "boxes": len(state.get("boxes") or []),
        "can_fit": (state.get("container_plan") or {}).get("can_fit"),
        "risk_decision": (state.get("risk_report") or {}).get("decision"),
        "n_steps": len(state.get("agent_steps") or []),
        "agent_style": state.get("agent_style"),
        "team_mode": "big_team_a_b",
        "llm_toolcall": True,
    }
    yield emit(
        {
            "type": "done",
            "phase": state.get("phase"),
            "status": state.get("status"),
            "summary": summary,
            "artifact_paths": state.get("artifact_paths") or {},
        }
    )
    yield {
        "type": "done",
        "run_id": rid,
        "session_id": sid,
        "phase": state.get("phase"),
        "status": state.get("status"),
        "summary": summary,
        "artifact_paths": state.get("artifact_paths") or {},
        "public": pub,
        "state": state,
        "seq": seq + 1,
        "team_mode": "big_team_a_b",
        "agent_style": state.get("agent_style"),
    }
