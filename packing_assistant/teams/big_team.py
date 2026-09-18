"""大 Team：编排 + HITL 闸门 + 有界 critic + 收口。

内含小 Team A（成箱）与小 Team B（拼柜）。
通用 Agent：NL → IntentSpec → 调度工具/子团队。
"""

from __future__ import annotations

import time
from typing import Any, Dict, Generator, List, Optional

from packing_assistant.config import DEFAULT_CONTAINER_TYPE
from packing_assistant.teams.roster import AGENT_ROSTER, TEAM_ARCHITECTURE
from packing_assistant.teams.team_a import (
    team_a_agents,
    team_a_nodes,
    team_a_rebox_nodes,
)
from packing_assistant.teams.team_b import (
    team_b_agents,
    team_b_loop_nodes,
    team_b_tail_nodes,
)


def run_big_team(
    raw_input: str = "",
    *,
    materials: Optional[List[Dict[str, Any]]] = None,
    container_type: str = DEFAULT_CONTAINER_TYPE,
    max_containers: int = 0,
    enable_auto_confirm: bool = True,
    goal: str = "deliver_valid_pack_plan",
    packing_options: Optional[Dict[str, Any]] = None,
    session_id: str = "",
    save_artifacts: bool = True,
    on_event: Optional[Any] = None,
) -> Dict[str, Any]:
    final_state: Optional[Dict[str, Any]] = None
    for ev in iter_big_team_run(
        raw_input,
        materials=materials,
        container_type=container_type,
        max_containers=max_containers,
        enable_auto_confirm=enable_auto_confirm,
        goal=goal,
        packing_options=packing_options,
        session_id=session_id,
        save_artifacts=save_artifacts,
        on_event=on_event,
    ):
        if ev.get("type") == "done" and ev.get("state") is not None:
            final_state = ev.get("state")
    return final_state or {}


def iter_big_team_run(
    raw_input: str = "",
    *,
    materials: Optional[List[Dict[str, Any]]] = None,
    container_type: str = DEFAULT_CONTAINER_TYPE,
    max_containers: int = 0,
    enable_auto_confirm: bool = True,
    goal: str = "deliver_valid_pack_plan",
    packing_options: Optional[Dict[str, Any]] = None,
    session_id: str = "",
    save_artifacts: bool = True,
    on_event: Optional[Any] = None,
) -> Generator[Dict[str, Any], None, None]:
    """大 Team 主循环：yield stream 事件；最后 done 含 state。"""
    from packing_assistant.agents import agent_finalize, agent_orchestrator
    from packing_assistant.harness import (
        make_initial_state,
        public_response,
        _attach_artifacts,
    )
    from packing_assistant.intent_spec import apply_intent_to_state, intent_from_api
    from packing_assistant.otel_hooks import span as otel_span
    from packing_assistant.trace_events import append_trace_event
    from packing_assistant.tool_registry import list_tools

    # —— 0) NL 通用入口 → IntentSpec ——
    spec = intent_from_api(
        user_input=raw_input or "NL 装柜请求",
        materials=materials,
        packing_options=packing_options,
        max_containers=int(max_containers or 0),
        goal=goal,
        container_type=container_type,
        source="nl" if (raw_input or "").strip() else "api",
    )

    state = make_initial_state(
        user_input=spec.raw_nl or raw_input or "big_team_run",
        materials=list(materials or []),
        container_type=container_type or DEFAULT_CONTAINER_TYPE,
        enable_auto_confirm=enable_auto_confirm,
        max_containers=int(max_containers or 0),
        goal=spec.goal or goal,
        session_id=session_id,
    )
    state = apply_intent_to_state(state, spec, materials=materials, filter_materials=True)
    # API 显式 max_containers 优先
    if max_containers and int(max_containers) > 0:
        state["max_containers"] = int(max_containers)
    state["team_mode"] = "big_team_a_b"
    state["team_architecture"] = dict(TEAM_ARCHITECTURE)
    state["agent_roster"] = list(AGENT_ROSTER)
    state["available_tools"] = list_tools()
    state["team_loop_round"] = 0
    opts0 = dict(state.get("packing_options") or {})
    opts0.setdefault("architecture", "big_team_a_b")
    opts0.setdefault("intent_driven", True)
    state["packing_options"] = opts0

    agents: Dict[str, Any] = {
        "orchestrator": agent_orchestrator,
        **team_a_agents(),
        **team_b_agents(),
        "finalize": agent_finalize,
    }

    rid = str(state.get("run_id") or state.get("session_id") or "run")
    sid = str(state.get("session_id") or rid)
    seq = 0
    from packing_assistant.runtime import cancel as _cancel

    _cancel.clear(rid, sid)  # a new run of this session must not inherit an old cancel request

    def emit(ev: Dict[str, Any]) -> Dict[str, Any]:
        nonlocal seq
        seq += 1
        payload = dict(ev)
        payload["seq"] = seq
        payload.setdefault("run_id", rid)
        payload.setdefault("session_id", sid)
        payload.setdefault("team_mode", "big_team_a_b")
        try:
            payload = append_trace_event(rid, payload)
        except Exception:
            pass
        try:
            from packing_assistant.ws_hub import HUB

            HUB.publish(sid, payload)
            if rid != sid:
                HUB.publish(rid, payload)
        except Exception:
            pass
        if on_event:
            try:
                on_event(payload)
            except Exception:
                pass
        return payload

    def _merge_update(upd: Dict[str, Any]) -> None:
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

    def _build_step(
        node: str,
        title: str,
        upd: Dict[str, Any],
        ms: int,
        err: Optional[str],
        *,
        team: str = "",
    ) -> Dict[str, Any]:
        last = ""
        for m in reversed(state.get("messages") or []):
            if isinstance(m, dict) and m.get("content"):
                last = str(m["content"])
                break
        meta = upd.get("agent_meta") if isinstance(upd.get("agent_meta"), dict) else {}
        # Plan→Act→Observe→Reflect（比赛可解释轨迹）
        plan_s = str(meta.get("plan") or "")[:120]
        act_s = str(meta.get("act") or "")[:120]
        obs_s = str(meta.get("observe") or "")[:120]
        ref_s = str(meta.get("reflect") or "")[:120]
        if not plan_s:
            plan_s = f"执行节点 {node}"
        if not act_s:
            tools = meta.get("tools_used") or []
            act_s = f"调用 {','.join(tools[:4])}" if tools else f"运行 {node}"
        if not obs_s:
            obs_s = (last[:120] if last else f"status={'error' if err else 'ok'}")
        if not ref_s:
            ref_s = "继续下一节点" if not err else "记录错误并继续/收口"
        step: Dict[str, Any] = {
            "node": node,
            "title": title,
            "team": team,
            "message": last[:900],
            "role": "agent",
            "tools_used": meta.get("tools_used") or [],
            "capability": meta.get("capability") or [],
            "artifacts": meta.get("artifacts") or {},
            "duration_ms": ms,
            "status": "error" if err else "ok",
            "plan": plan_s,
            "act": act_s,
            "observe": obs_s,
            "reflect": ref_s,
        }
        if err:
            step["error"] = err
        if node == "material_parser":
            step["perception"] = state.get("perception") or state.get("materials_summary")
        if node == "planner":
            pl = state.get("plan") or {}
            step["planning_reasons"] = pl.get("planning_reasons") or []
            step["n0"] = pl.get("n0")
            step["plan"] = plan_s or f"N0规划 n0={pl.get('n0')}"
            step["observe"] = obs_s or f"binding={(pl.get('booking') or {}).get('binding_constraint')}"
        if node == "loader":
            p = state.get("container_plan") or {}
            step["containers_used"] = p.get("containers_used")
            step["can_fit"] = p.get("can_fit")
            step["observe"] = (
                f"can_fit={p.get('can_fit')} used={p.get('containers_used')}"
            )
            step["reflect"] = (
                "可进入评估" if p.get("can_fit") else "需 replan/拆箱"
            )
        if node == "risk_compliance":
            rr = state.get("risk_report") or {}
            step["decision"] = rr.get("decision")
            step["observe"] = f"risk={rr.get('decision')}"
            step["reflect"] = (
                "可出运讨论" if rr.get("decision") != "REJECT" else "打回/阻断"
            )
        if node == "finalize":
            step["goal_status"] = state.get("goal_status") or {}
            step["ship_ok"] = state.get("ship_ok")
            step["reflect"] = f"ship_ok={state.get('ship_ok')}"
        if node == "evaluator":
            evl = state.get("evaluation") or {}
            step["need_replan"] = evl.get("need_replan")
            step["replan_round"] = state.get("replan_round")
            step["observe"] = (
                f"decision={evl.get('decision')} need_replan={evl.get('need_replan')}"
            )
            step["reflect"] = (
                "触发 critic" if evl.get("need_replan") else "进入风险合规"
            )
        if node == "box_scheme":
            step["observe"] = obs_s or str(
                (meta.get("artifacts") or {}).get("standard_hit_rate")
            )
        return step

    prev_node: Optional[str] = None
    steps: List[Dict[str, Any]] = []

    def run_one(node: str, title: str, fn: Any, *, team: str = "big"):
        nonlocal prev_node, state
        parent = None if node in ("intent", "orchestrator") else prev_node
        if _cancel.is_cancelled(rid) or _cancel.is_cancelled(sid):
            # cooperative cancel: the user asked to stop; skip this and every later agent
            state["phase"] = "cancelled"
            state["cancelled"] = True
            state.setdefault("errors", []).append(f"{node}: cancelled by user")
            step = _build_step(node, title, {}, 0, "cancelled by user", team=team)
            step["status"] = "cancelled"
            steps.append(step)
            state["agent_steps"] = list(state.get("agent_steps") or []) + [step]
            yield emit(
                {
                    "type": "agent_end",
                    "node": node,
                    "title": title,
                    "team": team,
                    "status": "cancelled",
                    "duration_ms": 0,
                    "parent_node": parent,
                    "step": step,
                    "phase": "cancelled",
                    "run_id": rid,
                }
            )
            prev_node = node
            return
        yield emit(
            {
                "type": "agent_start",
                "node": node,
                "title": title,
                "team": team,
                "status": "running",
                "parent_node": parent,
                "run_id": rid,
            }
        )
        t0 = time.perf_counter()
        err: Optional[str] = None
        upd: Dict[str, Any] = {}
        try:
            with otel_span(
                f"agent.{node}",
                {
                    "node": node,
                    "team": team,
                    "run_id": rid,
                    "session_id": sid,
                    "replan_round": int(state.get("replan_round") or 0),
                },
            ):
                upd = fn(state) or {}
            _merge_update(upd)
            if node == "present_team_a" and enable_auto_confirm:
                from packing_assistant.harness import apply_user_confirmation as _auc

                state = _auc(
                    state,
                    action="confirm",
                    container_type=state.get("container_type") or container_type,
                    max_containers=int(state.get("max_containers") or max_containers or 0),
                )
        except Exception as e:
            err = str(e)
            state.setdefault("errors", []).append(f"{node}: {e}")
        ms = int((time.perf_counter() - t0) * 1000)
        step = _build_step(node, title, upd, ms, err, team=team)
        steps.append(step)
        state["agent_steps"] = list(state.get("agent_steps") or []) + [step]
        # tool 事件（供 smoke / 观测）
        for tool in step.get("tools_used") or []:
            yield emit(
                {
                    "type": "tool_start",
                    "node": node,
                    "tool": tool,
                    "team": team,
                    "status": "running",
                    "run_id": rid,
                }
            )
            yield emit(
                {
                    "type": "tool_end",
                    "node": node,
                    "tool": tool,
                    "team": team,
                    "status": "ok" if not err else "error",
                    "run_id": rid,
                }
            )
        yield emit(
            {
                "type": "agent_end",
                "node": node,
                "title": title,
                "team": team,
                "status": step.get("status"),
                "duration_ms": ms,
                "parent_node": parent,
                "step": step,
                "phase": state.get("phase"),
                "run_id": rid,
            }
        )
        prev_node = node

    def _apply_replan_critic() -> Dict[str, Any]:
        """有界辩论（默认）或纯 critic；失败回退 stop。"""
        try:
            opts = state.get("packing_options") or {}
            # 默认开启 bounded_debate；显式 false 则仅 critic
            if opts.get("bounded_debate") is False:
                from packing_assistant.agents.replan_critic import agent_replan_critic

                return agent_replan_critic(state) or {}
            from packing_assistant.bounded_debate import run_bounded_debate

            return run_bounded_debate(state) or {}
        except Exception as e:
            try:
                from packing_assistant.agents.replan_critic import agent_replan_critic

                fb = agent_replan_critic(state) or {}
                if fb:
                    return fb
            except Exception:
                pass
            return {
                "replan_proposal": {
                    "stop": True,
                    "reasons": [str(e)],
                    "route": "stop",
                },
                "messages": [
                    {
                        "role": "assistant",
                        "content": f"【replan_critic/debate】失败: {e}",
                        "agent": "replan_critic",
                    }
                ],
            }

    def _should_replan() -> bool:
        ev = state.get("evaluation") or {}
        if ev.get("decision") == "REJECT_STRUCTURE" or ev.get("structure_fail_box_ids"):
            return False
        return bool(ev.get("need_replan")) and int(state.get("replan_round") or 0) <= 3

    def _should_ship_replan() -> bool:
        rr = state.get("risk_report") or {}
        plan = state.get("container_plan") or {}
        if int(state.get("ship_replan_round") or 0) >= 2:
            return False
        if rr.get("auto_replanable"):
            return True
        if rr.get("decision") == "REJECT" and rr.get("reject_to") in (
            "box_scheme",
            "planner",
            "loader",
        ):
            return True
        if not plan.get("can_fit"):
            return True
        return False

    # —— run_start ——
    yield emit(
        {
            "type": "run_start",
            "run_id": rid,
            "session_id": sid,
            "goal": state.get("goal"),
            "enable_auto_confirm": enable_auto_confirm,
            "container_type": state.get("container_type"),
            "status": "running",
            "team_mode": "big_team_a_b",
            "architecture": TEAM_ARCHITECTURE["mode"],
            "intent_spec": {
                "scheme_id": (state.get("intent_spec") or {}).get("scheme_id"),
                "cargo_mode": (state.get("intent_spec") or {}).get("cargo_mode"),
                "container_budget": (state.get("intent_spec") or {}).get(
                    "container_budget"
                ),
                "confidence": (state.get("intent_spec") or {}).get("confidence"),
            },
        }
    )

    # —— 大 Team · 意图节点（显式 step）——
    intent_step = {
        "node": "intent",
        "title": "大Team·NL意图解析",
        "team": "big",
        "message": (
            f"【IntentSpec】scheme={spec.scheme_id} cargo={spec.cargo_mode} "
            f"budget={spec.container_budget} conf={spec.confidence:.2f} | "
            f"tools={len(list_tools())} | notes={';'.join(spec.notes[:2])}"
        ),
        "tools_used": ["intent.interpret"],
        "capability": ["感知", "规划"],
        "artifacts": {
            "scheme_id": spec.scheme_id,
            "cargo_mode": spec.cargo_mode,
            "max_containers": spec.max_containers(),
        },
        "status": "ok",
        "duration_ms": 0,
    }
    steps.append(intent_step)
    state["agent_steps"] = [intent_step]
    state.setdefault("messages", []).append(
        {
            "role": "assistant",
            "content": intent_step["message"],
            "agent": "intent",
        }
    )
    yield emit(
        {
            "type": "agent_end",
            "node": "intent",
            "title": intent_step["title"],
            "team": "big",
            "status": "ok",
            "step": intent_step,
            "run_id": rid,
        }
    )
    yield emit(
        {
            "type": "tool_start",
            "node": "intent",
            "tool": "intent.interpret",
            "team": "big",
            "status": "running",
        }
    )
    yield emit(
        {
            "type": "tool_end",
            "node": "intent",
            "tool": "intent.interpret",
            "team": "big",
            "status": "ok",
        }
    )

    # —— 大 Team · 主控编排 ——
    yield from run_one(
        "orchestrator", "大Team·主控编排", agents["orchestrator"], team="big"
    )

    # —— 小 Team A · 成箱 ——
    state["phase"] = "team_a_running"
    hitl_break = False
    for node, title in team_a_nodes():
        yield from run_one(node, title, agents[node], team="A")
        if node == "present_team_a" and not enable_auto_confirm:
            wait = {
                "node": "hitl_wait",
                "title": "大Team·HITL闸（等待确认后进小TeamB）",
                "team": "big",
                "message": (
                    "phase=await_user_confirm；请 POST /api/confirm 后进入小 Team B 拼柜；"
                    "HITL 为大 Team 环境反馈｜tools=hitl.confirm"
                ),
                "tools_used": ["hitl.confirm"],
                "capability": ["感知环境"],
                "status": "ok",
            }
            steps.append(wait)
            state["agent_steps"] = list(state.get("agent_steps") or []) + [wait]
            try:
                from packing_assistant.hitl_summary import build_hitl_summary

                state["hitl_summary"] = build_hitl_summary(state)
            except Exception:
                pass
            try:
                from packing_assistant.session_store import save_session

                save_session(sid, state)
            except Exception:
                pass
            yield emit(
                {
                    "type": "hitl",
                    "node": "hitl_wait",
                    "status": "wait",
                    "team": "big",
                    "run_id": rid,
                    "session_id": sid,
                    "hitl_summary": state.get("hitl_summary"),
                    "phase": "await_user_confirm",
                    "team_mode": "big_team_a_b",
                }
            )
            hitl_break = True
            break

    # —— 小 Team B + 大 Team critic 有界闭环 ——
    if not hitl_break:
        state["phase"] = "team_b_running"
        state["team_mode"] = "big_team_a_b"

        while True:
            state["team_loop_round"] = int(state.get("team_loop_round") or 0) + 1
            # 内环：B 规划/装载/评估
            while True:
                for node, title in team_b_loop_nodes():
                    yield from run_one(node, title, agents[node], team="B")
                if _should_replan():
                    crit = _apply_replan_critic()
                    if crit:
                        _merge_update(crit)
                    prop = state.get("replan_proposal") or {}
                    debate = state.get("bounded_debate") or {}
                    if prop.get("stop"):
                        break
                    try:
                        from packing_assistant.replan_log import append_replan_event

                        append_replan_event(
                            state, ring="inner", proposal=prop, run_id=rid
                        )
                    except Exception:
                        pass
                    if debate.get("enabled"):
                        yield emit(
                            {
                                "type": "debate",
                                "node": "bounded_debate",
                                "team": "B",
                                "parent_node": "evaluator",
                                "status": "debate",
                                "message": debate.get("note")
                                or f"有界辩论 {debate.get('outcome')}",
                                "bounded_debate": {
                                    "rounds": debate.get("rounds"),
                                    "outcome": debate.get("outcome"),
                                    "transcript": (debate.get("transcript") or [])[:6],
                                    "tools_adjudicate": True,
                                },
                                "run_id": rid,
                            }
                        )
                        steps.append(
                            {
                                "node": "bounded_debate",
                                "title": "小TeamB·有界辩论 critic↔planner",
                                "team": "B",
                                "message": (
                                    f"outcome={debate.get('outcome')} "
                                    f"turns={debate.get('rounds')} · tools 裁决装载"
                                ),
                                "tools_used": ["bounded_debate.critic_planner"],
                                "status": "ok",
                                "bounded_debate": True,
                            }
                        )
                    if prop.get("route") == "box_scheme":
                        for node, title in team_a_rebox_nodes():
                            yield from run_one(
                                node, title, agents[node], team="A"
                            )
                        state["replan_round"] = 0
                        yield emit(
                            {
                                "type": "replan",
                                "node": "replan_critic",
                                "team": "big",
                                "parent_node": "evaluator",
                                "status": "replan",
                                "route": "box_scheme",
                                "message": "大Team critic → 小TeamA 成箱重做",
                                "replan_proposal": prop,
                                "run_id": rid,
                            }
                        )
                        steps.append(
                            {
                                "node": "replan_critic",
                                "title": "大Team·批评→小TeamA成箱",
                                "team": "big",
                                "message": "；".join(prop.get("reasons") or [])
                                or "box_scheme replan",
                                "tools_used": ["replan.critic"],
                                "status": "ok",
                                "route": "box_scheme",
                            }
                        )
                        continue
                    rr = int(state.get("replan_round") or 0)
                    yield emit(
                        {
                            "type": "replan",
                            "node": "replan_critic",
                            "team": "big",
                            "parent_node": "evaluator",
                            "status": "replan",
                            "replan_round": rr,
                            "message": f"大Team critic → 小TeamB planner r={rr}",
                            "replan_proposal": prop,
                            "run_id": rid,
                        }
                    )
                    steps.append(
                        {
                            "node": "replan_critic",
                            "title": "大Team·内环批评→小TeamB",
                            "team": "big",
                            "message": "；".join(prop.get("reasons") or []) or "replan",
                            "tools_used": ["replan.critic"],
                            "status": "ok",
                            "replan_round": rr,
                        }
                    )
                    continue
                break

            # 风险（B）
            yield from run_one(
                "risk_compliance",
                "小TeamB·风险合规",
                agents["risk_compliance"],
                team="B",
            )

            if not _should_ship_replan():
                break

            crit = _apply_replan_critic()
            if crit:
                _merge_update(crit)
            prop = state.get("replan_proposal") or {}
            if prop.get("stop"):
                break

            route = str(prop.get("route") or "planner")
            sr = int(state.get("ship_replan_round") or 0)
            try:
                from packing_assistant.replan_log import append_replan_event

                append_replan_event(state, ring="ship", proposal=prop, run_id=rid)
            except Exception:
                pass
            yield emit(
                {
                    "type": "replan",
                    "node": "replan_critic",
                    "team": "big",
                    "parent_node": "risk_compliance",
                    "status": "replan",
                    "ship_replan_round": sr,
                    "route": route,
                    "message": f"大Team出运打回 route={route} ship_r={sr}",
                    "replan_proposal": prop,
                    "run_id": rid,
                }
            )
            steps.append(
                {
                    "node": "replan_critic",
                    "title": f"大Team·出运→{route}",
                    "team": "big",
                    "message": "；".join(prop.get("reasons") or []) or "ship replan",
                    "tools_used": ["replan.critic"],
                    "status": "ok",
                    "ship_replan_round": sr,
                    "route": route,
                }
            )
            if route == "box_scheme":
                for node, title in team_a_rebox_nodes():
                    yield from run_one(node, title, agents[node], team="A")
                state["replan_round"] = 0
            else:
                state["replan_round"] = int(state.get("replan_round") or 0)
            continue

        # 可视化（B）+ 收口（大 Team）
        yield from run_one(
            "visualizer", "小TeamB·可视化", agents["visualizer"], team="B"
        )
        yield from run_one(
            "finalize", "大Team·收口", agents["finalize"], team="big"
        )

    state["agent_steps"] = steps
    if save_artifacts:
        state = _attach_artifacts(state, steps=steps)
        try:
            from packing_assistant.trace_events import run_trace_path

            paths = dict(state.get("artifact_paths") or {})
            paths["trace_jsonl"] = str(run_trace_path(rid))
            state["artifact_paths"] = paths
        except Exception:
            pass

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
        "n_steps": len(steps),
        "replan_round": state.get("replan_round"),
        "team_mode": "big_team_a_b",
        "architecture": "big_team_wraps_a_b",
        "scheme_id": (state.get("intent_spec") or {}).get("scheme_id"),
    }
    yield emit(
        {
            "type": "done",
            "run_id": rid,
            "session_id": sid,
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
    }
