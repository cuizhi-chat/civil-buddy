"""
FastAPI 网关：对接主控 Harness + 静态 Vue2 前端。

启动:
  pip install fastapi uvicorn
  set SKJOLBER_URL=http://127.0.0.1:8080
  uvicorn gateway.app:app --reload --port 8000

无管理员：用户目录 JDK17 + Maven 起 skjolber 即可，见 scripts/start_skjolber_user.ps1
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

import asyncio
import queue as queue_mod

from fastapi import FastAPI, File, Form, HTTPException, UploadFile, WebSocket, WebSocketDisconnect
from urllib.parse import quote
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

# 保证可 import packing_assistant
ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

# 加载仓库根 .env（含 SKJOLBER_URL，无需系统环境变量 / 管理员）
try:
    from dotenv import load_dotenv

    load_dotenv(ROOT / ".env")
except Exception:
    pass
# 默认：不强制挂死 skjolber。未起 Java 服务时只走 Python 3D，避免探活拖死单线程网关。
# 需要 skjolber 时在 .env 设 SKJOLBER_URL，并确保 8080 已起；或设 PACKING_SKIP_SKJOLBER=0 且 URL 可达。
if (os.getenv("PACKING_SKIP_SKJOLBER") or "").strip() == "":
    # 未显式配置时：若 URL 指向本机 8080 且我们偏好稳 UI，默认 skip（可用 PACKING_SKIP_SKJOLBER=0 打开）
    os.environ.setdefault("PACKING_SKIP_SKJOLBER", "1")

from packing_assistant.config import HARNESS_VERSION, PRODUCT_NAME  # noqa: E402
from packing_assistant.harness import (  # noqa: E402
    apply_user_confirmation,
    iter_agent_pipeline,
    public_response,
    revise_plan_nl,
    run_agent_pipeline,
    run_pipeline,
    run_team_a,
    run_team_b,
)
from packing_assistant.session_store import (  # noqa: E402
    delete_checkpoint,
    list_checkpoints,
    load_checkpoint_meta,
    load_session,
    mark_checkpoint,
    save_session,
)
from packing_assistant.skjolber_client import health_check  # noqa: E402
from packing_assistant.trace_events import list_runs, read_trace_jsonl  # noqa: E402

# RAM cache + file-backed checkpoint（output/runs/<run_id>/session_state.json）
_SESSIONS: Dict[str, Dict[str, Any]] = {}


def _store_session(session_id: str, state: Dict[str, Any]) -> None:
    """Write RAM + disk so /api/confirm survives process restart."""
    sid = str(session_id or state.get("session_id") or "default")
    _SESSIONS[sid] = state
    rid = str(state.get("run_id") or "")
    if rid and rid != sid:
        _SESSIONS[rid] = state
    try:
        save_session(sid, state)
    except Exception:
        pass


def _get_session(session_id: str) -> Optional[Dict[str, Any]]:
    sid = str(session_id or "")
    state = _SESSIONS.get(sid)
    if state is not None:
        return state
    try:
        state = load_session(sid)
    except Exception:
        state = None
    if state is not None:
        _SESSIONS[sid] = state
        rid = str(state.get("run_id") or "")
        if rid:
            _SESSIONS[rid] = state
    return state


app = FastAPI(title="Civil Buddy Gateway", version=HARNESS_VERSION)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

FRONTEND_DIR = ROOT / "frontend"
if FRONTEND_DIR.exists():
    app.mount("/static", StaticFiles(directory=str(FRONTEND_DIR)), name="static")


class TeamARequest(BaseModel):
    user_input: str = ""
    session_id: str = "default"
    materials: Optional[List[Dict[str, Any]]] = None
    adjust_note: str = ""
    design_facts: Optional[Dict[str, Any]] = None
    preset: str = ""
    packing_options: Optional[Dict[str, Any]] = None


class ConfirmRequest(BaseModel):
    session_id: str = "default"
    packing_plan_id: str = ""
    action: str = Field(..., description="confirm | revise | cancel")
    container_type: str = "40HQ"
    # 0 = 自主定柜（N0 起 3D 递增），禁止把业务目标写死成 2
    max_containers: int = 0
    adjust_note: str = ""
    confirmed_box_ids: List[str] = Field(default_factory=list)
    # 装前/非标勾选写回
    checklist_checked: Dict[str, bool] = Field(default_factory=dict)
    # 前端比赛路径默认 true；自动化测试勿传或 false
    enforce_ns_checklist: bool = False


class ReviseNlRequest(BaseModel):
    """自然语言改方案（改材料/柜型/详设截面后重跑团队A）。"""

    session_id: str = "default"
    instruction: str = Field(
        ...,
        description="例如：去掉钢梁；柜型改成40GP；4米铁架框架用槽钢16#，底板槽钢12#3根，γ=2.0",
    )
    rerun_team_a: bool = True


class DemoRequest(BaseModel):
    user_input: str = "演示材料清单"
    container_type: str = "40HQ"
    session_id: str = "demo"
    # high_util | steel_light | default | "" 自动
    preset: str = "high_util"
    materials: Optional[List[Dict[str, Any]]] = None
    # 比赛演示默认 False：露出 HITL（await_user_confirm）；True=自动拼柜
    enable_auto_confirm: bool = False


@app.get("/")
def index():
    index_html = FRONTEND_DIR / "index.html"
    if index_html.exists():
        # 禁止浏览器缓存旧 index（否则修卡死/解锁按钮不生效）
        return FileResponse(
            index_html,
            headers={
                "Cache-Control": "no-store, no-cache, must-revalidate, max-age=0",
                "Pragma": "no-cache",
            },
        )
    return {"message": "frontend/index.html missing", "docs": "/docs"}


@app.get("/workbench")
def workbench():
    """工程装柜工作台（非默认；产品主线为投标应答+交付）。"""
    wb = FRONTEND_DIR / "workbench.html"
    if wb.exists():
        return FileResponse(
            wb,
            headers={
                "Cache-Control": "no-store, no-cache, must-revalidate, max-age=0",
                "Pragma": "no-cache",
            },
        )
    return {"message": "frontend/workbench.html missing", "fallback": "/"}


@app.get("/api/health")
def api_health():
    # 短超时 + 缓存，避免每次刷新 health 卡住整个单线程网关
    sk = health_check(timeout=0.25, use_cache=True)
    agent_count = 13
    try:
        from packing_assistant.teams.roster import AGENT_ROSTER

        agent_count = len(AGENT_ROSTER)
    except Exception:
        pass
    llm_key = bool(
        (os.getenv("DEEPSEEK_API_KEY") or os.getenv("OPENAI_API_KEY") or "").strip()
    )
    sk_ok = bool(isinstance(sk, dict) and sk.get("ok"))
    preflight = {
        "gateway": True,
        "skjolber": sk_ok,
        "skjolber_optional": True,  # 可本地回退
        "llm_key": llm_key,
        "llm_optional": True,  # steps 主路径不强制
        "harness_version": HARNESS_VERSION,
        "agent_count": agent_count,
        "demo_auto_confirm_default": False,
        "ok": True,  # gateway UP 即预检通过；skjolber/llm 仅提示
        "hints": [],
    }
    if not sk_ok:
        preflight["hints"].append("skjolber 未连接 → 3D 用本地 bin3d 回退（可演示）")
    if not llm_key:
        preflight["hints"].append("无 LLM Key → llm_toolcall 走 policy_fallback（主路径 steps 不受影响）")
    preflight["hints"].append(
        f"演示默认 enable_auto_confirm=false，露出 HITL · harness {HARNESS_VERSION} · {agent_count} agents"
    )
    preflight["hints"].append(
        "通用表：POST /api/table/parse（上传）或 /api/table/parse/json · 前端「表上传」"
    )
    return {
        "gateway": "UP",
        "product": PRODUCT_NAME,
        "harness_version": HARNESS_VERSION,
        "agent_count": agent_count,
        "architecture": "big_team_wraps_a_b",
        "agent_style": "nl_general_agent_with_tools",
        "llm_key_present": llm_key,
        "preflight": preflight,
        "features": {
            "sse_stream": True,
            "websocket": True,
            "ws_path": "/ws/session/{session_id}",
            "trace_jsonl": True,
            "trace_replay": True,
            "hitl_summary": True,
            "hitl_durable_checkpoint": True,
            "langgraph_sqlite_checkpoint": True,
            "stream_replan": True,
            "tool_events": True,
            "otel_export": True,
            "otel_optional": True,
            "stream_schema": "packing.stream.v1",
            "intent_spec": True,
            "big_team_a_b": True,
            "llm_toolcall": True,
            "graph_ab_resume": True,
            "demo_hitl_default": True,
            "table_parse": True,
            "table_parse_path": "/api/table/parse",
            "table_parse_json_path": "/api/table/parse/json",
            "nonstandard_inspect": True,
            "generic_table_profile": True,
            "path_honesty": True,
            "vgm_status": True,
            "vgm_human_signoff": True,
            "vgm_signoff_path": "/api/vgm/signoff",
            "demo_simple_mode": True,
            "demo_simple_default": True,
            "bounded_debate": True,
            "bounded_debate_default": True,
            "primary_agent_mode": "steps",
            "llm_toolcall_reference_only": True,
            "tender_handoff": True,
            "tender_p0_scan": True,
            "tender_tech_outline": True,
            "tender_parse_file": True,
            "tender_parse_file_path": "/api/tender/parse/file",
            "tender_parse_files": True,
            "tender_parse_files_path": "/api/tender/parse/files",
            "tender_ingest_tables": True,
            "tender_review": True,
            "pack_ship_mcp": True,
            "sandbox": True,
            "otel_dashboard": True,
            "otel_dashboard_path": "/api/otel/dashboard",
            "understand_default": True,
            "understand_path": "/api/turn",
            "expert_turn": True,
            "agent_loop": True,
            "agent_path": "/api/agent",
            "eval_live": True,
            "eval_live_path": "/api/eval/live",
        },
        "entries": {
            "tender_delivery": "/",
            "packing_workbench": "/workbench",
            "civil_workbench": "http://127.0.0.1:8765",
        },
        "otel": _otel_status_safe(),
        "langgraph_checkpoint": _lg_status_safe(),
        "skjolber_url": os.getenv("SKJOLBER_URL") or "",
        "skjolber": sk,
    }


@app.get("/api/architecture")
def api_architecture():
    """大 Team ⊃ A/B 架构元数据 + 名册 + Agent 知识绑定摘要。"""
    from packing_assistant.teams.roster import AGENT_ROSTER, TEAM_ARCHITECTURE

    kb_bindings = {}
    try:
        from packing_assistant.kb_bindings import bindings_summary

        kb_bindings = bindings_summary()
    except Exception as e:
        kb_bindings = {"ok": False, "error": str(e)}

    return {
        "ok": True,
        "harness_version": HARNESS_VERSION,
        "architecture": TEAM_ARCHITECTURE,
        "roster": AGENT_ROSTER,
        "kb_bindings": kb_bindings,
        "tender_mainline": {
            "id": "C_tender_delivery",
            "handoff": True,
            "p0_human_confirm": True,
            "submit_blocked_default": True,
            "entries": {
                "ui": "/",
                "parse": "/api/tender/parse",
                "parse_file": "/api/tender/parse/file",
                "parse_files": "/api/tender/parse/files",
                "delivery": "/api/tender/delivery",
                "review": "/api/tender/review",
                "turn": "/api/turn",
                "understand": "/api/understand",
                "agent": "/api/agent",
                "eval_live": "/api/eval/live",
            },
        },
    }


@app.get("/api/kb/bindings")
def api_kb_bindings():
    """Agent → knowledge_base 窄接表。"""
    from packing_assistant.kb_bindings import bindings_summary, get_binding, list_agent_ids

    agents = {aid: get_binding(aid) for aid in list_agent_ids()}
    return {"ok": True, "summary": bindings_summary(), "agents": agents}


@app.post("/api/kb/search")
def api_kb_search(body: dict = None):
    """按 agent_id 窄接检索 knowledge_base。"""
    from packing_assistant.kb_bindings import search_for_agent
    from packing_assistant.tools.search_knowledge import search_knowledge

    body = body or {}
    q = str(body.get("q") or body.get("query") or "")
    agent_id = str(body.get("agent_id") or "").strip()
    limit = int(body.get("limit") or 5)
    if agent_id:
        return search_for_agent(agent_id, q, limit=limit)
    return search_knowledge(q, limit=limit)


@app.get("/api/tools")
def api_tools(team: str = ""):
    """通用 Agent 工具注册表（按 big/A/B 过滤）。"""
    from packing_assistant.tool_registry import list_tools, tools_for_agent_prompt

    t = (team or "").strip() or None
    if t in ("big_team", "main"):
        t = "big"
    return {
        "ok": True,
        "tools": list_tools(team=t),
        "prompt_summary": tools_for_agent_prompt() if not t else None,
    }


def _tender_ingest_from_uploads(uploads: list) -> dict:
    from packing_assistant.tools.tender_ingest import ingest_files

    files = []
    for f in uploads:
        files.append({"filename": f.get("filename") or "upload.txt", "bytes": f.get("bytes") or b""})
    try:
        return ingest_files(files)
    except ValueError as e:
        raise HTTPException(400, str(e)) from e


@app.get("/api/skills")
def api_skills():
    from packing_assistant.runtime.expert_skills import catalog

    rows = catalog()
    return {"ok": True, "n": len(rows), "skills": rows, "host": "civil-buddy", "product": "civil-codex"}


@app.get("/api/config")
def api_config():
    from packing_assistant.runtime.civil_config import load_config

    return {"ok": True, **load_config().to_dict()}


@app.get("/api/threads")
def api_threads():
    from packing_assistant.runtime.threads import list_threads

    rows = [t.to_dict() for t in list_threads()]
    return {"ok": True, "n": len(rows), "threads": rows}


@app.get("/api/experts")
def api_experts():
    from packing_assistant.expert_roster import list_experts

    rows = [e.to_dict() for e in list_experts()]
    return {"ok": True, "n": len(rows), "experts": rows}


@app.post("/api/understand")
def api_understand(body: dict = None):
    """Classify user text. No writes."""
    from packing_assistant.understand import understand

    body = body or {}
    text = str(body.get("text") or body.get("message") or "")
    intent = understand(text)
    return {"ok": True, "intent": intent, "wrote": False, "schema": "civil.understand.v1"}


@app.post("/api/turn")
def api_turn(body: dict = None):
    """Default surface: chat does not write; run uses existing tender pipeline."""
    from packing_assistant.product_turn import run_turn

    body = body or {}
    return run_turn(
        str(body.get("text") or body.get("message") or body.get("tender_text") or ""),
        p0_confirmed=bool(body.get("p0_confirmed") or body.get("confirm_ok")),
        project_name=str(body.get("project_name") or "幕墙项目投标应答（草稿）"),
        force_intent=str(body.get("intent") or "") or None,
        expert_id=str(body.get("expert_id") or ""),
        session_id=str(body.get("session_id") or ""),
        packing_summary=body.get("packing_summary") if isinstance(body.get("packing_summary"), dict) else None,
    )


@app.post("/api/agent")
def api_agent(body: dict = None):
    """Complete agent loop: Scheduler + ToolEngine + sandbox. Chat never writes."""
    from packing_assistant.runtime.agent_loop import run_agent

    body = body or {}
    return run_agent(
        str(body.get("text") or body.get("message") or body.get("tender_text") or ""),
        session_id=str(body.get("session_id") or ""),
        expert_id=str(body.get("expert_id") or ""),
        p0_confirmed=bool(body.get("p0_confirmed") or body.get("confirm_ok")),
        force_intent=str(body.get("intent") or "") or None,
        packing_summary=body.get("packing_summary") if isinstance(body.get("packing_summary"), dict) else None,
        project_name=str(body.get("project_name") or "幕墙项目投标应答（草稿）"),
        max_steps=max(1, min(int(body.get("max_steps") or 8), 32)),
    )


@app.get("/api/context/{session_id}")
def api_session_context(session_id: str):
    """Session slots Civil Buddy owns. Not a DeepSeek transcript."""
    from packing_assistant.runtime.memory import assemble_context, prompt_prefix

    ctx = assemble_context(session_id)
    return {
        "ok": True,
        "schema": "civil.session.context.v1",
        "session_id": session_id,
        "context": ctx,
        "prompt_prefix": prompt_prefix(ctx),
    }


@app.get("/api/eval/live")
def api_eval_live():
    """Offline official-title needles + agent/sandbox smoke. No IRAS scrape."""
    from packing_assistant.runtime.eval_live import live_eval

    return live_eval()


@app.get("/api/runs/{run_id}/events")
def api_run_events(run_id: str):
    from packing_assistant.runtime.bus import get_bus
    from packing_assistant.runtime.scheduler import get_scheduler

    run = get_scheduler().get(run_id)
    events = [e.to_dict() for e in get_bus().for_run(run_id)]
    if not run and not events:
        raise HTTPException(404, "unknown run")
    return {"ok": True, "run_id": run_id, "events": events, "state": run.state if run else ""}


def _packing_disk_sidecar(run_id: str, include_trace: bool) -> Optional[Dict[str, Any]]:
    """Packing output/runs payload. Fallback only — never a Scheduler identity."""
    from packing_assistant.trace_events import RUNS_DIR

    d = RUNS_DIR / run_id
    if not d.is_dir():
        return None
    idx: Dict[str, Any] = {}
    p = d / "index.json"
    if p.exists():
        try:
            idx = json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            idx = {}
    out: Dict[str, Any] = {"run_dir": str(d), "index": idx}
    if include_trace:
        out["trace"] = read_trace_jsonl(run_id, limit=2000)
    return out


@app.get("/api/runs/{run_id}")
def api_run_get(run_id: str, include_trace: bool = True):
    from packing_assistant.runtime.scheduler import get_scheduler

    run = get_scheduler().get(run_id)
    disk = _packing_disk_sidecar(run_id, include_trace)
    if run:
        body: Dict[str, Any] = {"ok": True, "source": "scheduler", **run.to_dict()}
        body["tools"] = list(run.tools_used)
        body["step_log"] = list(run.history)
        if disk:
            body["disk"] = disk
        return body
    if disk:
        return {
            "ok": True,
            "source": "disk",
            "run_id": run_id,
            "messages": [],
            "tools_used": [],
            "artifacts": [],
            "history": [],
            "duration_ms": None,
            **disk,
        }
    raise HTTPException(404, "unknown run")


@app.post("/api/runs/{run_id}/cancel")
def api_run_cancel(run_id: str):
    """Cancel a Scheduler run, or cooperatively stop a packing pipeline by run_id / session_id.

    The packing loop (teams/big_team.run_one) checks the registry at every agent boundary and
    skips the rest with status=cancelled, so the blocking /api/pipeline returns quickly.
    """
    from packing_assistant.runtime import cancel as _cancel
    from packing_assistant.runtime.scheduler import get_scheduler

    sched_ok = get_scheduler().cancel(run_id)
    coop_ok = _cancel.request(run_id)
    if not (sched_ok or coop_ok):
        raise HTTPException(400, "cannot cancel")
    return {"ok": True, "run_id": run_id, "state": "cancelled", "scheduler": sched_ok, "cooperative": coop_ok}


def _tender_parse_via_engine(
    *,
    text: str,
    source: str,
    project_name: str,
    p0_confirmed: bool,
    packing_summary=None,
    ingest=None,
    intent: str = "run",
) -> dict:
    from packing_assistant.runtime.tool_engine import get_engine

    result = get_engine().execute(
        "tender.parse",
        {
            "text": text,
            "source": source,
            "project_name": project_name,
            "p0_confirmed": p0_confirmed,
            "packing_summary": packing_summary,
            "ingest": ingest,
        },
        intent=intent,
    )
    if not result.get("ok"):
        return {
            "ok": False,
            "product_mainline": "C_tender_delivery",
            "error_code": result.get("error_code"),
            "submit_blocked": True,
            "wrote": False,
            "matrix": None,
        }
    data = result.get("data") if isinstance(result.get("data"), dict) else result
    return {"ok": True, "product_mainline": "C_tender_delivery", **(data or {})}


@app.post("/api/tender/parse")
def api_tender_parse(body: dict = None):
    """招标文本 / 多节选 → requirements / checklist / response_matrix。走 ToolEngine。"""
    from packing_assistant.tools.tender_ingest import ingest_from_json

    body = body or {}
    ingest = None
    text = ingest_from_json(body)
    if text:
        ingest = {"schema": "tender.ingest.v1", "source": "json-sections"}
    else:
        text = str(body.get("text") or body.get("tender_text") or "")
    packing_summary = body.get("packing_summary")
    if packing_summary is not None and not isinstance(packing_summary, dict):
        packing_summary = None
    return _tender_parse_via_engine(
        text=text,
        source="api",
        project_name=str(body.get("project_name") or "幕墙项目投标应答（草稿）"),
        p0_confirmed=bool(body.get("p0_confirmed")),
        packing_summary=packing_summary,
        ingest=ingest,
        intent=str(body.get("intent") or "run"),
    )


@app.post("/api/tender/parse/file")
async def api_tender_parse_file(
    file: UploadFile = File(...),
    p0_confirmed: str = Form("false"),
    project_name: str = Form("幕墙项目投标应答（草稿）"),
):
    """Upload one ITT excerpt: txt/md/csv/docx/xlsx. No scanned-PDF vision."""
    raw = await file.read()
    ingested = _tender_ingest_from_uploads([{"filename": file.filename, "bytes": raw}])
    out = _tender_parse_via_engine(
        text=ingested["text"],
        source="api-upload",
        project_name=project_name,
        p0_confirmed=str(p0_confirmed).lower() in {"1", "true", "yes"},
        ingest=ingested,
        intent="run",
    )
    return {
        "filename": file.filename,
        "ingested_text": ingested["text"],
        **out,
    }


@app.post("/api/tender/parse/files")
async def api_tender_parse_files(
    files: list[UploadFile] = File(...),
    p0_confirmed: str = Form("false"),
    project_name: str = Form("幕墙项目投标应答（草稿）"),
):
    """Several excerpts (须知 + 评分表 + …) → one matrix. Still not a bid book."""
    if not files:
        raise HTTPException(400, "没有收到文件")
    if len(files) > 8:
        raise HTTPException(400, "一次最多 8 个节选")
    uploads = []
    for f in files:
        uploads.append({"filename": f.filename, "bytes": await f.read()})
    ingested = _tender_ingest_from_uploads(uploads)
    out = _tender_parse_via_engine(
        text=ingested["text"],
        source="api-uploads",
        project_name=project_name,
        p0_confirmed=str(p0_confirmed).lower() in {"1", "true", "yes"},
        ingest=ingested,
        intent="run",
    )
    return {
        "ingested_text": ingested["text"],
        **out,
    }


@app.get("/api/mcp/tools")
def api_mcp_tools(expert_id: str = ""):
    from packing_assistant.tools.pack_ship_mcp import list_pack_ship_tools

    if expert_id and expert_id not in {"pack-ship", ""}:
        return {"ok": True, "tools": []}
    return {"ok": True, "tools": list_pack_ship_tools()}


@app.post("/api/mcp/tools/call")
def api_mcp_tool_call(body: dict = None):
    from packing_assistant.tools.pack_ship_mcp import call_tool

    body = body or {}
    name = str(body.get("name") or "")
    expert_id = str(body.get("expert_id") or "pack-ship")
    if expert_id and expert_id not in {"pack-ship"}:
        raise HTTPException(403, "拒绝：当前专家看不见该工具")
    out = call_tool(name, body.get("arguments") or {})
    return {"ok": bool(out.get("ok", True)), **out}


@app.post("/api/tender/review")
def api_tender_review(body: dict = None):
    """成稿后再审一岗：禁语 + 矩阵缺项。不填业绩、不改 can_fit。"""
    from packing_assistant.tools.tender_review import review_draft, review_from_pipeline

    body = body or {}
    if body.get("pipeline"):
        out = review_from_pipeline(body["pipeline"])
    else:
        out = review_draft(
            draft=str(body.get("draft") or body.get("text") or body.get("bidbook_markdown") or ""),
            matrix=body.get("matrix") if isinstance(body.get("matrix"), dict) else None,
            packing_summary=body.get("packing_summary")
            if isinstance(body.get("packing_summary"), dict)
            else None,
            tech_outline=body.get("tech_outline") if isinstance(body.get("tech_outline"), dict) else None,
            bidbook_markdown=str(body.get("bidbook_markdown") or ""),
        )
    return {"ok": True, "product_mainline": "C_tender_delivery", **out}


@app.get("/api/otel/dashboard")
@app.get("/api/otel/spans")
def api_otel_dashboard(limit: int = 400):
    from packing_assistant.otel_hooks import dashboard_payload

    return dashboard_payload(limit=max(1, min(int(limit or 400), 2000)))


@app.post("/api/sandbox/check")
def api_sandbox_check(body: dict = None):
    from packing_assistant.sandbox import check_open, check_write, request_spawn

    body = body or {}
    action = str(body.get("action") or "write")
    if action == "spawn":
        return request_spawn(body.get("command"), kind=body.get("kind")).to_dict()
    if action == "open":
        return check_open(str(body.get("path") or "")).to_dict()
    return check_write(str(body.get("path") or "")).to_dict()


@app.post("/api/tender/delivery")
def api_tender_delivery(body: dict = None):
    """主线 C：投标解析 + 可选交付装柜 → 响应矩阵 + 一页导出包。"""
    from packing_assistant.tender_delivery import run_tender_delivery_pipeline

    body = body or {}
    materials = body.get("materials")
    if materials is not None and not isinstance(materials, list):
        materials = None
    return run_tender_delivery_pipeline(
        str(body.get("text") or body.get("tender_text") or ""),
        run_delivery=bool(body.get("run_delivery", True)),
        materials=materials,
        container_type=str(body.get("container_type") or "40HQ"),
        max_containers=int(body.get("max_containers") or 2),
        user_input=str(body.get("user_input") or "投标交付：按招标运输包装要求装柜"),
        session_id=str(body.get("session_id") or "tender-delivery"),
        project_name=str(body.get("project_name") or "幕墙项目投标应答（草稿）"),
        enable_auto_confirm=True,
        save_artifacts=False,
        p0_confirmed=bool(body.get("p0_confirmed")),
    )


@app.post("/api/tender/bidbook")
def api_tender_bidbook(body: dict = None):
    """Singapore façade English bid-book draft (no packing unless run_delivery)."""
    from packing_assistant.tender_delivery import run_tender_delivery_pipeline

    body = body or {}
    out = run_tender_delivery_pipeline(
        str(body.get("text") or body.get("tender_text") or ""),
        run_delivery=bool(body.get("run_delivery", False)),
        container_type=str(body.get("container_type") or "40HQ"),
        max_containers=int(body.get("max_containers") or 2),
        project_name=str(body.get("project_name") or "幕墙项目投标应答（草稿）"),
    )
    return {
        "ok": bool(out.get("ok")),
        "product": "sg_facade_bidbook",
        "bidbook": out.get("bidbook"),
        "bidbook_markdown": out.get("bidbook_markdown"),
        "open_actions": out.get("open_actions"),
        "matrix": out.get("matrix"),
        "packing_summary": out.get("packing_summary"),
    }


@app.post("/api/eval/workteams")
def api_eval_workteams(body: dict = None):
    """steps vs llm_toolcall 影子评测 + 路由/选工具 KPI。"""
    from packing_assistant.eval_harness import case_tiny
    from packing_assistant.eval_workteams import run_workteam_shadow_eval

    body = body or {}
    tiny_only = bool(body.get("tiny_only", True))
    cases = [case_tiny] if tiny_only else None
    report = run_workteam_shadow_eval(
        cases=cases,
        out_path=None,
        session_prefix=str(body.get("session_prefix") or "api-wt"),
    )
    return {"ok": bool(report.get("ok")), "report": report}


@app.get("/api/kpi/{session_id}")
def api_kpi_session(session_id: str):
    """从 session 抽取 workteam 路由/选工具 KPI。"""
    from packing_assistant.workteam_kpi import compute_kpis

    st = _get_session(session_id)
    if not st:
        from packing_assistant.session_store import load_session

        st = load_session(session_id)
    if not st:
        raise HTTPException(404, f"session {session_id} not found")
    return {"ok": True, "session_id": session_id, "kpi": compute_kpis(st)}


@app.post("/api/tms/booking/preview")
def api_tms_booking_preview(body: dict):
    """从 session 构建订舱请求（不提交）。"""
    from packing_assistant.tms_booking import submit_booking

    sid = str(body.get("session_id") or "")
    st = _get_session(sid) if sid else None
    if not st and sid:
        from packing_assistant.session_store import load_session

        st = load_session(sid)
    if not st and body.get("state"):
        st = body["state"]
    if not st:
        raise HTTPException(400, "需要 session_id 或 state")
    return submit_booking(st, dry_run=True)


@app.post("/api/tms/booking/submit")
def api_tms_booking_submit(body: dict):
    """提交订舱到 TMS（默认 stub；PACKING_TMS_MODE=http 走外部）。"""
    from packing_assistant.tms_booking import attach_booking_to_state, submit_booking

    sid = str(body.get("session_id") or "")
    st = _get_session(sid) if sid else None
    if not st and sid:
        from packing_assistant.session_store import load_session

        st = load_session(sid)
    if not st and body.get("state"):
        st = body["state"]
    if not st:
        raise HTTPException(400, "需要 session_id 或 state")
    result = submit_booking(
        st,
        mode=body.get("mode"),
        dry_run=bool(body.get("dry_run")),
    )
    if result.get("ok") and not body.get("dry_run") and sid:
        st2 = attach_booking_to_state(st, result)
        _store_session(sid, st2)
        try:
            from packing_assistant.session_store import save_session

            save_session(sid, st2)
        except Exception:
            pass
        result["session_updated"] = True
    return result


@app.get("/api/tms/bookings")
def api_tms_bookings(limit: int = 20):
    """列出本地 stub 订舱记录。"""
    from packing_assistant.tms_booking import list_stub_bookings, tms_mode

    return {
        "ok": True,
        "mode": tms_mode(),
        "bookings": list_stub_bookings(limit=limit),
    }


@app.post("/api/vgm/signoff")
def api_vgm_signoff(body: dict):
    """记录 VGM 托运人本地人签（不向船司申报）；回写 session 与 public vgm_status。"""
    from packing_assistant.harness import public_response
    from packing_assistant.tools.vgm_draft import record_human_signoff

    sid = str(body.get("session_id") or "")
    st = _get_session(sid) if sid else None
    if not st and sid:
        from packing_assistant.session_store import load_session

        st = load_session(sid)
    if not st and body.get("state"):
        st = body["state"]
    if not st:
        raise HTTPException(400, "需要 session_id 或 state")
    acknowledged = body.get("acknowledged")
    if acknowledged is None:
        acknowledged = True
    st2 = record_human_signoff(
        st,
        signer=str(body.get("signer") or body.get("signed_by") or "shipper"),
        acknowledged=bool(acknowledged),
        note=str(body.get("note") or ""),
    )
    if sid:
        _store_session(sid, st2)
        try:
            from packing_assistant.session_store import save_session

            save_session(sid, st2)
        except Exception:
            pass
    pub = public_response(st2)
    return {
        "ok": True,
        "session_id": sid or None,
        "vgm_status": pub.get("vgm_status"),
        "vgm_signoff": st2.get("vgm_signoff"),
        "checklist_checked": st2.get("checklist_checked"),
        "pre_ship_checked": st2.get("pre_ship_checked"),
    }


@app.post("/api/vgm/submit-preview")
def api_vgm_submit_preview(body: dict):
    """VGM 提交预览：未人签返回 blocked_unsigned。"""
    from packing_assistant.p2_stubs import draft_vgm_submit

    sid = str(body.get("session_id") or "")
    st = _get_session(sid) if sid else None
    if not st and sid:
        from packing_assistant.session_store import load_session

        st = load_session(sid)
    if not st and body.get("state"):
        st = body["state"]
    if not st:
        raise HTTPException(400, "需要 session_id 或 state")
    return draft_vgm_submit(st, dry_run=True)


@app.post("/api/intent")
def api_intent(body: dict):
    """仅解析 NL → IntentSpec（不跑装载）。"""
    from packing_assistant.intent_spec import intent_from_api

    spec = intent_from_api(
        user_input=str(body.get("user_input") or body.get("nl_query") or ""),
        materials=body.get("materials"),
        packing_options=body.get("packing_options"),
        max_containers=int(body.get("max_containers") or 0),
        goal=str(body.get("goal") or "deliver_valid_pack_plan"),
        container_type=str(body.get("container_type") or ""),
        source=str(body.get("source") or "api"),
    )
    return {"ok": True, "intent_spec": spec.to_dict()}


def _otel_status_safe() -> Dict[str, Any]:
    try:
        from packing_assistant.otel_hooks import otel_status

        return otel_status()
    except Exception as e:
        return {"error": str(e)}


def _lg_status_safe() -> Dict[str, Any]:
    try:
        from packing_assistant.lg_checkpoint import (
            checkpoint_db_path,
            checkpoint_enabled,
            get_checkpointer,
        )

        cp = get_checkpointer()
        return {
            "enabled": checkpoint_enabled(),
            "backend": type(cp).__name__ if cp else None,
            "path": str(checkpoint_db_path()),
        }
    except Exception as e:
        return {"error": str(e)}


def _apply_preset(
    *,
    preset: str = "",
    user_input: str = "",
    materials: Optional[List[Dict[str, Any]]] = None,
    packing_options: Optional[Dict[str, Any]] = None,
) -> tuple:
    """合并演示预设物料 / packing_options。"""
    from packing_assistant.demo_presets import resolve_preset

    pm, po, key = resolve_preset(preset, user_input=user_input)
    mats = materials if materials else pm
    opts = packing_options if packing_options else po
    text = user_input
    if key and (not text or text in ("演示材料清单", "Agent pipeline", "demo")):
        from packing_assistant.demo_presets import PRESETS

        text = PRESETS.get(key, {}).get("user_input") or text
    return mats, opts, key, text


@app.post("/api/team-a")
def api_team_a(body: TeamARequest):
    mats, opts, key, text = _apply_preset(
        preset=body.preset or "",
        user_input=body.user_input,
        materials=body.materials,
        packing_options=body.packing_options,
    )
    state = run_team_a(
        text,
        materials=mats,
        session_id=body.session_id,
        adjust_note=body.adjust_note,
        design_facts=body.design_facts,
        packing_options=opts,
    )
    _store_session(body.session_id, state)
    resp = public_response(state)
    resp["run_id"] = state.get("run_id")
    resp["session_id"] = body.session_id
    resp["preset"] = key
    return resp


@app.post("/api/revise-nl")
def api_revise_nl(body: ReviseNlRequest):
    """
    自然语言改方案。

    契约：
    - 可改：应用 ops → 可选重跑 Team A → status=applied，revise_ok=true
    - 不可改：方案不动 → status=unsupported，message 以「无此功能」开头，revise_ok=false
    """
    state = _get_session(body.session_id)
    if not state:
        from packing_assistant.harness import make_initial_state

        state = make_initial_state(session_id=body.session_id, enable_auto_confirm=False)
    state = revise_plan_nl(
        state, body.instruction, rerun_team_a=body.rerun_team_a
    )
    nr = dict(state.get("nl_revision") or {})
    # applied 时 state 是新方案；unsupported 时 state 与改前一致（仅多了 nl_revision）
    _store_session(body.session_id, state)
    resp = public_response(state)
    resp["nl_revision"] = nr
    resp["revise_ok"] = bool(nr.get("applied") and nr.get("status") == "applied")
    resp["feature_available"] = bool(nr.get("feature_available"))
    resp["revise_status"] = nr.get("status") or (
        "applied" if nr.get("applied") else "unsupported"
    )
    # 客户端可直接观察 prefer_single_row 等（public_response 默认不带 packing_options）
    resp["packing_options"] = dict(state.get("packing_options") or {})
    return resp


@app.post("/api/confirm")
def api_confirm(body: ConfirmRequest):
    # RAM miss → disk checkpoint（进程重启 / 多 worker 轻量恢复）
    state = _get_session(body.session_id)
    if not state and body.packing_plan_id:
        state = _get_session(body.packing_plan_id)
    if not state:
        raise HTTPException(
            400,
            "session 不存在（内存与磁盘均无）。请先 /api/team-a 或 /api/pipeline/stream HITL",
        )

    if body.action == "cancel":
        from packing_assistant.runtime import cancel as _cancel

        _cancel.request(body.session_id)  # also stops a pipeline still running for this session
        state = {**state, "phase": "cancelled", "user_action": "cancel",
                 "final_response": "已取消", "status": "success"}
        _store_session(body.session_id, state)
        try:
            mark_checkpoint(body.session_id, status="cancelled")
        except Exception:
            pass
        return public_response(state)

    if body.action == "revise":
        state = run_team_a(
            state.get("user_input") or "",
            materials=state.get("materials"),
            session_id=body.session_id,
            adjust_note=body.adjust_note or "用户调整",
        )
        _store_session(body.session_id, state)
        return public_response(state)

    if body.action != "confirm":
        raise HTTPException(400, "action 必须是 confirm | revise | cancel")

    # 写回勾选表
    checked = dict(body.checklist_checked or {})
    if checked:
        prev = dict(state.get("pre_ship_checked") or {})
        prev.update(checked)
        state = {**state, "pre_ship_checked": prev}

    # 非标严格门禁：FAIL + strict_nonstandard_gate 禁止进入 Team B
    opts = dict(state.get("packing_options") or {})
    ns = state.get("nonstandard_summary") or state.get("nonstandard_report") or {}
    if opts.get("strict_nonstandard_gate") and str(ns.get("overall") or "") == "FAIL":
        raise HTTPException(
            400,
            (ns.get("ship_gate") or {}).get("note")
            or "非标检验 FAIL 且 strict_nonstandard_gate：请整改后重跑 Team A",
        )

    # 非标必填勾选门禁（前端 enforce_ns_checklist 或 packing_options.require_ns_checklist）
    try:
        from packing_assistant.pre_ship_checklist import (
            build_pre_ship_checklist,
            evaluate_ns_checklist_gate,
        )

        gate = evaluate_ns_checklist_gate(
            state,
            checked=state.get("pre_ship_checked") or {},
            enforce=bool(body.enforce_ns_checklist),
        )
        cl = build_pre_ship_checklist(state, checked=state.get("pre_ship_checked") or {})
        state = {**state, "pre_ship_checklist": cl, "ns_checklist_gate": gate}
        if gate.get("blocks"):
            raise HTTPException(
                400,
                gate.get("note")
                or f"非标预检未齐: {', '.join(gate.get('missing') or [])}",
            )
    except HTTPException:
        raise
    except Exception:
        pass

    # resume 标记（interrupt → 小 Team B 子图）
    try:
        mark_checkpoint(body.session_id, status="resumed")
    except Exception:
        pass
    from packing_assistant.graph_resume import resume_team_b_segment

    state = resume_team_b_segment(
        state,
        session_id=body.session_id,
        container_type=body.container_type,
        max_containers=body.max_containers,
        adjust_note=body.adjust_note or "",
        confirmed_box_ids=body.confirmed_box_ids,
    )
    if "hitl_summary" in state:
        state = {**state, "hitl_summary": {}}
    state = {**state, "phase": state.get("phase") or "done"}
    _store_session(body.session_id, state)
    try:
        mark_checkpoint(body.session_id, status="done")
    except Exception:
        pass
    resp = public_response(state)
    resp["checkpoint"] = load_checkpoint_meta(body.session_id) or {}
    resp["resumed_from_disk"] = True
    resp["graph_segment"] = state.get("graph_segment")
    resp["resume_from"] = state.get("resume_from")
    return resp


@app.get("/api/resume/{session_id}")
def api_resume_status(session_id: str):
    """查询 A/B 分段 resume 是否可用（磁盘 / LangGraph）。"""
    from packing_assistant.graph_resume import describe_resume

    return describe_resume(session_id)


@app.post("/api/resume/{session_id}/team-b")
def api_resume_team_b(session_id: str, body: ConfirmRequest):
    """显式从 HITL resume 小 Team B（等同 confirm，可仅带 session）。"""
    from packing_assistant.graph_resume import load_resume_state, resume_team_b_segment

    st = _get_session(session_id) or load_resume_state(session_id)
    if not st:
        raise HTTPException(404, f"session {session_id} 不可 resume")
    state = resume_team_b_segment(
        st,
        session_id=session_id,
        container_type=body.container_type or st.get("container_type") or "40HQ",
        max_containers=body.max_containers,
        adjust_note=body.adjust_note or "",
        confirmed_box_ids=body.confirmed_box_ids,
    )
    _store_session(session_id, state)
    resp = public_response(state)
    resp["resume_from"] = state.get("resume_from")
    resp["graph_segment"] = state.get("graph_segment")
    return resp


class PipelineRequest(BaseModel):
    """大 Team 入口：NL→IntentSpec→小TeamA成箱→HITL→小TeamB拼柜→收口。"""

    user_input: str = "Agent pipeline"
    materials: Optional[List[Dict[str, Any]]] = None
    container_type: str = "40HQ"
    session_id: str = "pipeline"
    max_containers: int = 0
    # True=跳过确认闸门自动跑到 finalize；False=停在 HITL
    enable_auto_confirm: bool = True
    goal: str = Field(
        default="deliver_valid_pack_plan",
        description="deliver_valid_pack_plan | minimize_containers | safe_to_ship",
    )
    save_artifacts: bool = True
    # steps=固定节点；llm_toolcall=LLM多轮工具；auto=有Key则LLM；graph=LangGraph全图
    mode: str = "steps"
    agent_mode: str = Field(
        default="",
        description="覆盖 mode：steps | llm_toolcall | auto（空则用 mode）",
    )
    max_llm_rounds: int = 12
    preset: str = "high_util"
    packing_options: Optional[Dict[str, Any]] = None


class WhatIfRequest(BaseModel):
    """OptiGuide 式 what-if：NL/约束 → 重跑大 Team 闭环。"""

    session_id: str = "pipeline"
    scenario: str = Field(
        default="",
        description="空则靠 nl_query 解析；或 lock_containers|plus_one|minus_one|strict_mid50|iron_only|no_long|...",
    )
    nl_query: str = Field(
        default="",
        description="自然语言：如「锁 2 柜」「去掉超长」「只要铁件」",
    )
    max_containers: Optional[int] = Field(
        default=None,
        description="预算柜数；lock/minus/plus 场景使用",
    )
    profile: str = Field(
        default="",
        description="可选偏好档：balanced|strict_mid50|min_cabin|export_careful|crate_passthrough",
    )
    user_input: str = ""
    materials: Optional[List[Dict[str, Any]]] = None
    packing_options: Optional[Dict[str, Any]] = None
    container_type: str = "40HQ"
    store_result: bool = True


class WhatIfApplyRequest(BaseModel):
    """把 what-if 结果会话提升为主 session（result 替换 baseline）。"""

    session_id: str = "pipeline"
    whatif_session_id: str = Field(..., description="形如 {session}-whatif-{scenario}")


class ProfilePipelineRequest(BaseModel):
    """带偏好档的 pipeline 快捷入口。"""

    user_input: str = "Profile pipeline"
    materials: Optional[List[Dict[str, Any]]] = None
    container_type: str = "40HQ"
    session_id: str = "pipeline"
    max_containers: int = 0
    profile: str = "balanced"
    packing_options: Optional[Dict[str, Any]] = None
    preset: str = ""
    enable_auto_confirm: bool = True


@app.get("/api/profiles")
def api_profiles():
    from packing_assistant.packing_profiles import list_profiles

    return {"ok": True, "profiles": list_profiles()}


class TableParseJsonBody(BaseModel):
    """JSON 解析入口：path / rows / session 写入可选。"""

    path: str = ""
    rows: Optional[List[Dict[str, Any]]] = None
    session_id: str = ""
    apply_profile: str = "generic_table"
    store_session: bool = False


@app.post("/api/table/parse")
async def api_table_parse(
    file: Optional[UploadFile] = File(None),
    session_id: str = Form(""),
    store_session: str = Form("0"),
    path: str = Form(""),
):
    """通用材料表 → materials[]（multipart file= 或 form path=）。

    JSON 请用 POST /api/table/parse/json。
    不写 xyz / 柜数；仅列映射与单位归一。
    """
    from packing_assistant.tools.table_mapper import parse_table_bytes, parse_table_file

    result: Dict[str, Any]
    try:
        if file is not None and getattr(file, "filename", None):
            raw = await file.read()
            result = parse_table_bytes(raw, filename=file.filename or "upload.csv")
        elif (path or "").strip():
            p = Path(path.strip())
            if not p.is_absolute():
                p = ROOT / p
            if not p.exists():
                raise HTTPException(404, f"table not found: {path}")
            result = parse_table_file(p)
        else:
            result = {
                "ok": False,
                "materials": [],
                "column_map": {},
                "stats": {},
                "errors": [
                    "provide multipart file= or form path=, or POST /api/table/parse/json"
                ],
            }
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(400, f"parse failed: {type(e).__name__}: {e}") from e

    sid = (session_id or "").strip()
    store = str(store_session or "0").strip() in ("1", "true", "True", "yes")
    if store and sid and result.get("ok") and result.get("materials"):
        st = _SESSIONS.get(sid) or _load_session(sid) or {
            "session_id": sid,
            "phase": "materials_ready",
            "packing_options": {},
        }
        st = {
            **st,
            "materials": list(result["materials"]),
            "table_parse": {
                "column_map": result.get("column_map"),
                "stats": result.get("stats"),
                "path": result.get("path"),
            },
        }
        try:
            from packing_assistant.packing_profiles import apply_profile

            st["packing_options"] = apply_profile(
                st.get("packing_options") or {}, "generic_table"
            )
        except Exception:
            pass
        _store_session(sid, st)
        result["session_id"] = sid
        result["stored"] = True
    else:
        result["stored"] = False

    if len(result.get("ir") or []) > 50:
        result = {k: v for k, v in result.items() if k != "ir"}
    return {
        "ok": bool(result.get("ok")),
        "materials": result.get("materials") or [],
        "column_map": result.get("column_map") or {},
        "stats": result.get("stats") or {},
        "path": result.get("path"),
        "errors": result.get("errors") or [],
        "session_id": result.get("session_id"),
        "stored": result.get("stored"),
        "note": "tools map columns/units only; no xyz or container count",
    }


@app.post("/api/table/parse/json")
def api_table_parse_json(body: TableParseJsonBody):
    """JSON 入口：path 或 rows → materials（与 multipart 同一 mapper）。"""
    from packing_assistant.tools.table_mapper import parse_table_file, parse_table_rows

    if body.rows:
        result = parse_table_rows(body.rows, source="api_json_rows")
    elif (body.path or "").strip():
        p = Path(body.path.strip())
        if not p.is_absolute():
            p = ROOT / p
        if not p.exists():
            raise HTTPException(404, f"table not found: {body.path}")
        result = parse_table_file(p)
    else:
        raise HTTPException(400, "need path or rows")

    if body.store_session and body.session_id and result.get("ok"):
        st = _SESSIONS.get(body.session_id) or _load_session(body.session_id) or {
            "session_id": body.session_id,
            "phase": "materials_ready",
            "packing_options": {},
        }
        st = {**st, "materials": list(result.get("materials") or [])}
        try:
            from packing_assistant.packing_profiles import apply_profile

            pid = body.apply_profile or "generic_table"
            st["packing_options"] = apply_profile(st.get("packing_options") or {}, pid)
        except Exception:
            pass
        _store_session(body.session_id, st)
        result["session_id"] = body.session_id
        result["stored"] = True
    if len(result.get("ir") or []) > 50:
        result = {k: v for k, v in result.items() if k != "ir"}
    return {
        "ok": bool(result.get("ok")),
        "materials": result.get("materials") or [],
        "column_map": result.get("column_map") or {},
        "stats": result.get("stats") or {},
        "path": result.get("path"),
        "errors": result.get("errors") or [],
        "session_id": result.get("session_id"),
        "stored": result.get("stored", False),
        "note": "tools map columns/units only; no xyz or container count",
    }


@app.get("/api/whatif/scenarios")
def api_whatif_scenarios():
    from packing_assistant.whatif import list_whatif_scenarios

    return {"ok": True, "scenarios": list_whatif_scenarios()}


@app.post("/api/whatif")
def api_whatif(body: WhatIfRequest):
    """
    What-if：在 baseline state 上改 max_containers / 过滤材料 / packing_options，
    重跑 run_agent_pipeline，返回 before/after + plan_diff。
    """
    from packing_assistant.session_store import load_session, save_session
    from packing_assistant.whatif import run_whatif

    base = _SESSIONS.get(body.session_id) or _load_session(body.session_id)
    if not base or not (base.get("materials") or body.materials):
        # 无 baseline：用 materials 先跑一版再 what-if
        if not body.materials:
            raise HTTPException(
                400,
                "需要已有 session（先 /api/pipeline）或提供 materials",
            )
        base = run_agent_pipeline(
            body.user_input or "whatif baseline",
            materials=body.materials,
            container_type=body.container_type,
            enable_auto_confirm=True,
            session_id=f"{body.session_id}-base",
            packing_options=body.packing_options,
        )
        _store_session(f"{body.session_id}-base", base)

    # 合并偏好档
    if body.profile:
        from packing_assistant.packing_profiles import apply_profile

        base = dict(base)
        base["packing_options"] = apply_profile(
            base.get("packing_options") or body.packing_options,
            body.profile,
        )

    result = run_whatif(
        base,
        scenario=body.scenario or "",
        max_containers=body.max_containers,
        user_input=body.user_input or body.nl_query,
        nl_query=body.nl_query or body.user_input,
        profile=body.profile or "",
        session_id=body.session_id,
    )
    if not result.get("ok"):
        raise HTTPException(400, result.get("error") or "whatif failed")

    after = result.get("state") or {}
    if body.store_result and after:
        wid = f"{body.session_id}-whatif-{body.scenario}"
        _store_session(wid, after)
        try:
            save_session(wid, after)
        except Exception:
            pass
        result["result_session_id"] = wid

    out = {k: v for k, v in result.items() if k != "state"}
    out["ok"] = True
    return out


@app.post("/api/whatif/apply")
def api_whatif_apply(body: WhatIfApplyRequest):
    """把 what-if 结果写回主 session，便于前端直接展示为当前方案。"""
    from packing_assistant.session_store import save_session

    src = _SESSIONS.get(body.whatif_session_id) or _load_session(body.whatif_session_id)
    if not src:
        raise HTTPException(404, f"whatif session 不存在: {body.whatif_session_id}")
    _store_session(body.session_id, src)
    try:
        save_session(body.session_id, src)
    except Exception:
        pass
    pub = public_response(src)
    return {
        "ok": True,
        "session_id": body.session_id,
        "applied_from": body.whatif_session_id,
        "public": pub,
        "summary": {
            "containers_used": (src.get("container_plan") or {}).get("containers_used"),
            "can_fit": (src.get("container_plan") or {}).get("can_fit"),
            "ship_ok": src.get("ship_ok"),
            "worst_mid50": (src.get("container_plan") or {}).get("worst_mid50"),
        },
    }


@app.post("/api/pipeline/profile")
def api_pipeline_profile(body: ProfilePipelineRequest):
    """按偏好档跑单 Team 闭环。"""
    from packing_assistant.packing_profiles import apply_profile

    mats, opts, key, text = _apply_preset(
        preset=body.preset or "",
        user_input=body.user_input,
        materials=body.materials,
        packing_options=body.packing_options,
    )
    opts = apply_profile(opts, body.profile or "balanced")
    state = run_agent_pipeline(
        text,
        materials=mats,
        container_type=body.container_type,
        max_containers=int(body.max_containers or 0),
        enable_auto_confirm=body.enable_auto_confirm,
        session_id=body.session_id,
        packing_options=opts,
    )
    _store_session(body.session_id, state)
    pub = public_response(state)
    return {
        "ok": True,
        "profile": body.profile,
        "session_id": body.session_id,
        "public": pub,
        "summary": {
            "n0": (state.get("container_plan") or {}).get("n0"),
            "containers_used": (state.get("container_plan") or {}).get("containers_used"),
            "can_fit": (state.get("container_plan") or {}).get("can_fit"),
            "ship_ok": state.get("ship_ok"),
            "team_mode": state.get("team_mode"),
            "profile_id": opts.get("profile_id"),
        },
    }


@app.get("/api/business-presets")
def api_business_presets():
    from packing_assistant.business_presets import list_business_presets

    return {"ok": True, "presets": list_business_presets()}


def _export_root() -> Path:
    from packing_assistant.config import OUTPUT_DIR

    return (Path(OUTPUT_DIR) / "exports").resolve()


@app.post("/api/export/shipment")
def api_export_shipment(body: dict):
    """导出 POR+绑扎 xlsx。body: {session_id} → {xlsx_path, download_url, ...}"""
    from packing_assistant.export_pack import export_shipment_xlsx

    sid = str((body or {}).get("session_id") or "pipeline")
    st = _SESSIONS.get(sid) or _load_session(sid)
    if not st:
        raise HTTPException(404, "session 不存在")
    meta = export_shipment_xlsx(st, output_dir=_export_root())
    name = Path(str(meta.get("xlsx_path") or "")).name
    return {"ok": True, **meta, "download_url": f"/api/export/file?name={quote(name)}"}


@app.get("/api/export/file")
def api_export_file(name: str):
    """Download one exported workbook (name only; confined to <PACKING_OUTPUT_DIR>/exports)."""
    root = _export_root()
    target = (root / Path(name).name).resolve()
    if target.parent != root or not target.is_file():
        raise HTTPException(404, "export 不存在")
    return FileResponse(
        target,
        media_type="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        filename=target.name,  # starlette emits filename*=utf-8'' for non-ASCII names
    )


@app.post("/api/nonstandard/inspect")
def api_nonstandard_inspect(body: dict):
    """非标件检验 v2。body: materials? | session_id? | container_type? | with_boxes? | ns_llm_enrich?"""
    from packing_assistant.tools.nonstandard_inspect import (
        inspect_nonstandard,
        public_summary,
        report_markdown,
    )
    from packing_assistant.tools.nl_nonstandard_enrich import enrich_materials

    body = body or {}
    sid = str(body.get("session_id") or "")
    st = _get_session(sid) if sid else None
    mats = body.get("materials")
    if mats is None and st:
        mats = st.get("materials") or []
    mats = list(mats or [])
    boxes = body.get("boxes")
    if boxes is None and st:
        boxes = st.get("boxes") or []
    boxes = list(boxes or [])
    ctype = str(body.get("container_type") or (st or {}).get("container_type") or "40HQ")
    opts = dict((st or {}).get("packing_options") or {})
    if body.get("ns_llm_enrich"):
        opts["ns_llm_enrich"] = True
        mats = enrich_materials(mats, force_llm=True)
    elif opts.get("ns_llm_enrich") or __import__("os").environ.get("PACKING_NS_LLM", "").strip() in (
        "1",
        "true",
        "TRUE",
    ):
        mats = enrich_materials(mats)
    else:
        mats = enrich_materials(mats, force_llm=False)

    if body.get("with_boxes") and mats and not boxes:
        try:
            from packing_assistant.agents.box_scheme import agent_box_scheme

            bout = agent_box_scheme(
                {"materials": mats, "packing_options": opts, "messages": []}
            )
            boxes = list(bout.get("boxes") or [])
        except Exception:
            boxes = []

    full = inspect_nonstandard(
        materials=mats,
        boxes=boxes,
        container_type=ctype,
        case_id=sid or "api",
        packing_options=opts,
    )
    summary = public_summary(full)
    if st is not None and sid:
        st = {**st, "nonstandard_report": full, "nonstandard_summary": summary, "materials": mats}
        if boxes:
            st["boxes"] = boxes
        _store_session(sid, st)
    return {
        "ok": True,
        "overall": full.get("overall"),
        "summary": summary,
        "markdown": report_markdown(full),
        "full_available": True,
        "n_materials": (full.get("summary") or {}).get("n_materials"),
        "n_boxes": (full.get("summary") or {}).get("n_boxes"),
    }


@app.post("/api/checklist")
def api_checklist(body: dict):
    """更新装前检查表勾选。body: {session_id, checked: {id: bool}}"""
    from packing_assistant.pre_ship_checklist import build_pre_ship_checklist
    from packing_assistant.session_store import save_session

    sid = str((body or {}).get("session_id") or "pipeline")
    st = _SESSIONS.get(sid) or _load_session(sid)
    if not st:
        raise HTTPException(404, "session 不存在")
    checked = (body or {}).get("checked") or {}
    st["pre_ship_checked"] = checked
    cl = build_pre_ship_checklist(st, checked=checked)
    st["pre_ship_checklist"] = cl
    _store_session(sid, st)
    try:
        save_session(sid, st)
    except Exception:
        pass
    return {"ok": True, "checklist": cl}


@app.post("/api/eval/run")
def api_eval_run():
    """跑合成 tiny/20t 黄金评测（不依赖 t80 大文件）。"""
    from packing_assistant.eval_harness import run_eval_suite
    from pathlib import Path

    summary = run_eval_suite(out_path=Path("output/eval_harness_last.json"))
    return {"ok": summary.get("ok"), **summary}


@app.post("/api/p2/vgm-submit")
def api_p2_vgm(body: dict):
    from packing_assistant.p2_stubs import draft_vgm_submit

    sid = str((body or {}).get("session_id") or "")
    st = (_SESSIONS.get(sid) or _load_session(sid) or {}) if sid else {}
    return {"ok": True, **draft_vgm_submit(st, dry_run=True)}


@app.post("/api/p2/evidence")
def api_p2_evidence(body: dict):
    from packing_assistant.p2_stubs import build_evidence_pack

    sid = str((body or {}).get("session_id") or "pipeline")
    st = _SESSIONS.get(sid) or _load_session(sid)
    if not st:
        raise HTTPException(404, "session 不存在")
    return {"ok": True, **build_evidence_pack(st)}


@app.post("/api/demo")
def api_demo(body: DemoRequest):
    """演示入口：默认 high_util；**默认不停 auto 确认**以露出 HITL。

    enable_auto_confirm=true 时才自动进 B 并 finalize（非比赛主戏）。
    """
    mats, opts, key, text = _apply_preset(
        preset=body.preset or "high_util",
        user_input=body.user_input,
        materials=body.materials,
    )
    auto = bool(body.enable_auto_confirm)
    state = run_agent_pipeline(
        text,
        materials=mats,
        container_type=body.container_type,
        enable_auto_confirm=auto,
        max_containers=0,
        session_id=body.session_id,
        save_artifacts=True,
        packing_options=opts,
    )
    _store_session(body.session_id, state)
    resp = public_response(state)
    resp["agent_loop"] = "感知→规划→工具→行动→finalize"
    resp["preset"] = key or body.preset
    resp["enable_auto_confirm"] = auto
    resp["demo_note"] = (
        "auto_confirm=on 已跑完拼柜"
        if auto
        else "已停在 HITL：请确认柜型后 resume Team B"
    )
    return resp


@app.get("/api/demo-presets")
def api_demo_presets():
    from packing_assistant.demo_presets import list_presets

    return {"ok": True, "presets": list_presets()}


def _pipeline_agent_mode(body: PipelineRequest) -> str:
    am = (body.agent_mode or "").strip()
    if am:
        return am
    m = (body.mode or "steps").strip().lower()
    if m in ("llm", "llm_toolcall", "toolcall", "agent", "auto"):
        return m
    return "steps"


@app.post("/api/pipeline")
def api_pipeline(body: PipelineRequest):
    """
    **Agent 单一入口**：自动跑全程（可开关 confirm）→ finalize。

    mode/agent_mode: steps | llm_toolcall | auto | graph
    """
    mats, opts, key, text = _apply_preset(
        preset=body.preset or "",
        user_input=body.user_input,
        materials=body.materials,
        packing_options=body.packing_options,
    )
    if (body.mode or "steps").lower() == "graph":
        state = run_pipeline(
            text,
            materials=mats,
            container_type=body.container_type,
            enable_auto_confirm=body.enable_auto_confirm,
            max_containers=int(body.max_containers or 0),
            goal=body.goal,
            save_artifacts=body.save_artifacts,
            packing_options=opts,
        )
        steps = state.get("agent_steps") or []
        used_mode = "graph"
    else:
        used_mode = _pipeline_agent_mode(body)
        state = run_agent_pipeline(
            text,
            materials=mats,
            container_type=body.container_type,
            max_containers=int(body.max_containers or 0),
            enable_auto_confirm=body.enable_auto_confirm,
            goal=body.goal,
            session_id=body.session_id,
            save_artifacts=body.save_artifacts,
            packing_options=opts,
            agent_mode=used_mode,
            max_llm_rounds=int(body.max_llm_rounds or 12),
        )
        steps = state.get("agent_steps") or []

    _store_session(body.session_id, state)
    plan = state.get("container_plan") or {}
    book = state.get("booking") or plan.get("booking") or {}
    paths = state.get("artifact_paths") or {}
    pub = public_response(state)
    return {
        "ok": True,
        "agent_definition": {
            "style": state.get("agent_style")
            or (
                "llm_toolcall"
                if used_mode in ("llm_toolcall", "llm", "auto")
                else "nl_general_agent_with_tools"
            ),
            "architecture": "big_team_wraps_a_b",
            "goal": state.get("goal") or body.goal,
            "capabilities": ["感知环境", "推理与规划", "使用工具", "采取行动", "追求目标"],
            "loop": "大Team：Intent/LLM调度 → 小TeamA成箱 → HITL → 小TeamB拼柜 → critic → 收口",
            "team_mode": "big_team_a_b",
            "agent_mode": used_mode,
            "note": "数值由 tools 计算；LLM 只选工具不写坐标；What-if 见 POST /api/whatif",
        },
        "run_id": state.get("run_id"),
        "artifact_paths": paths,
        "goal_status": state.get("goal_status") or {},
        "volume_summary": pub.get("volume_summary") or {},
        "perception": state.get("perception") or state.get("materials_summary") or {},
        "planning_reasons": (state.get("plan") or {}).get("planning_reasons") or [],
        "steps": steps or pub.get("agent_steps") or [],
        "summary": {
            "boxes": len(state.get("boxes") or []),
            "n0": book.get("n0") or plan.get("n0"),
            "containers_used": plan.get("containers_used"),
            "can_fit": plan.get("can_fit"),
            "booking_volume_utilization": plan.get("booking_volume_utilization"),
            "outer_space_utilization": plan.get("outer_space_utilization")
            or plan.get("space_utilization"),
            "weight_utilization": plan.get("weight_utilization"),
            "risk_decision": (state.get("risk_report") or {}).get("decision"),
            "suggested_actions": (state.get("risk_report") or {}).get("suggested_actions")
            or [],
            "ship_ok": state.get("ship_ok"),
            "phase": state.get("phase"),
            "agent_mode": used_mode,
        },
        "public": pub,
    }


@app.post("/api/pipeline/stream")
def api_pipeline_stream(body: PipelineRequest):
    """
    SSE 流式 pipeline：逐 Agent 推送 agent_start / agent_end / hitl / done。
    前端可用 fetch + ReadableStream 解析 `data: {...}\\n\\n`。
    """
    mats, opts, _key, text = _apply_preset(
        preset=body.preset or "",
        user_input=body.user_input,
        materials=body.materials,
        packing_options=body.packing_options,
    )

    def gen():
        final_state = None
        for ev in iter_agent_pipeline(
            text,
            materials=mats,
            container_type=body.container_type,
            max_containers=int(body.max_containers or 0),
            enable_auto_confirm=body.enable_auto_confirm,
            goal=body.goal,
            session_id=body.session_id,
            save_artifacts=body.save_artifacts,
            packing_options=opts,
            agent_mode=_pipeline_agent_mode(body),
            max_llm_rounds=int(body.max_llm_rounds or 12),
        ):
            # SSE 不传完整 state（体积大）；hitl/done 落盘以便 resume
            out = {k: v for k, v in ev.items() if k != "state"}
            if ev.get("type") == "done":
                final_state = ev.get("state")
                if final_state is not None:
                    _store_session(body.session_id, final_state)
            elif ev.get("type") == "hitl":
                # harness 已 save_session；再刷 RAM（state 在后续 done 才完整 yield）
                # 若事件带 run_id，保证 session 索引可查
                rid = str(ev.get("run_id") or "")
                if rid:
                    disk = _get_session(rid) or _get_session(body.session_id)
                    if disk is not None:
                        _store_session(body.session_id, disk)
            yield f"data: {json.dumps(out, ensure_ascii=False, default=str)}\n\n"
        if final_state is None:
            yield f"data: {json.dumps({'type': 'error', 'message': 'empty pipeline'}, ensure_ascii=False)}\n\n"

    return StreamingResponse(
        gen(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


@app.websocket("/ws/session/{session_id}")
async def ws_session(websocket: WebSocket, session_id: str):
    """多 tab 订阅同一 session 的 agent 事件流（与 SSE 同源 HUB）。"""
    from packing_assistant.ws_hub import HUB

    await websocket.accept()
    q = HUB.subscribe(session_id)
    try:
        await websocket.send_json(
            {
                "type": "ws_subscribed",
                "session_id": session_id,
                "subscribers": HUB.subscriber_count(session_id),
                "schema": "packing.stream.v1",
            }
        )
        while True:
            # 非阻塞取事件 + 心跳
            try:
                ev = await asyncio.get_event_loop().run_in_executor(
                    None, lambda: q.get(timeout=15.0)
                )
                await websocket.send_json(ev)
            except queue_mod.Empty:
                try:
                    await websocket.send_json({"type": "ws_ping", "session_id": session_id})
                except Exception:
                    break
    except WebSocketDisconnect:
        pass
    except Exception:
        pass
    finally:
        HUB.unsubscribe(session_id, q)


@app.websocket("/ws/runs/{run_id}")
async def ws_run(websocket: WebSocket, run_id: str):
    """按 run_id 订阅（别名频道）。"""
    await ws_session(websocket, run_id)


@app.get("/api/lg/threads")
def api_lg_threads(limit: int = 30):
    from packing_assistant.lg_checkpoint import list_thread_ids

    return {"ok": True, "threads": list_thread_ids(limit=limit)}


@app.get("/api/lg/threads/{thread_id}")
def api_lg_thread_state(thread_id: str):
    """从 LangGraph Sqlite checkpointer 读取线程最新 state。"""
    from packing_assistant.graph import create_team_a_app_durable
    from packing_assistant.lg_checkpoint import get_thread_state

    app = create_team_a_app_durable()
    values = get_thread_state(thread_id, app)
    if not values:
        # 尝试文件 session 兜底
        values = load_session(thread_id)
    if not values:
        raise HTTPException(404, f"no langgraph/file state for thread={thread_id}")
    return {
        "ok": True,
        "thread_id": thread_id,
        "phase": values.get("phase"),
        "public": public_response(values),
        "source": "langgraph_or_file",
    }


@app.get("/api/checkpoints")
def api_list_checkpoints(pending_hitl: bool = False, limit: int = 40):
    """HITL / 会话 checkpoint 列表（文件持久化）。pending_hitl=true 仅未确认。"""
    items = list_checkpoints(limit=limit, pending_hitl_only=pending_hitl)
    return {
        "ok": True,
        "schema": "packing.checkpoint.v1",
        "pending_hitl_only": pending_hitl,
        "count": len(items),
        "checkpoints": items,
    }


@app.get("/api/checkpoints/{thread_id}")
def api_get_checkpoint(thread_id: str, include_state: bool = False):
    """读取某 thread 的 interrupt 元数据；可选完整 state。"""
    meta = load_checkpoint_meta(thread_id)
    state = load_session(thread_id) if include_state else None
    if meta is None and state is None:
        raise HTTPException(404, f"checkpoint not found: {thread_id}")
    out: Dict[str, Any] = {
        "ok": True,
        "thread_id": thread_id,
        "checkpoint": meta or {},
    }
    if include_state and state is not None:
        # 不回传超大 traces 时可裁剪
        out["public"] = public_response(state)
        out["phase"] = state.get("phase")
        out["run_id"] = state.get("run_id")
    return out


@app.post("/api/checkpoints/{thread_id}/resume")
def api_resume_checkpoint(thread_id: str, body: ConfirmRequest):
    """
    LangGraph 风格 resume：从磁盘 checkpoint 恢复后 confirm/revise/cancel。
    body.session_id 可省略，默认用 path 中的 thread_id。
    """
    body.session_id = body.session_id or thread_id
    if not body.session_id or body.session_id == "default":
        body.session_id = thread_id
    # 强制从磁盘再拉一次，证明 durable
    disk = load_session(thread_id) or load_session(body.session_id)
    if not disk:
        raise HTTPException(404, f"no durable checkpoint for thread={thread_id}")
    _SESSIONS[thread_id] = disk
    _SESSIONS[body.session_id] = disk
    return api_confirm(body)


@app.delete("/api/checkpoints/{thread_id}")
def api_delete_checkpoint(thread_id: str):
    ok = delete_checkpoint(thread_id)
    if not ok:
        raise HTTPException(404, f"checkpoint index not found: {thread_id}")
    _SESSIONS.pop(thread_id, None)
    return {"ok": True, "thread_id": thread_id, "deleted_index": True}


@app.get("/api/session/{session_id}")
def api_get_session(session_id: str):
    """恢复会话：RAM → 磁盘 checkpoint → public 形状。"""
    state = _get_session(session_id)
    if not state:
        raise HTTPException(404, f"session not found: {session_id}")
    resp = public_response(state)
    resp["checkpoint"] = state.get("_checkpoint") or load_checkpoint_meta(session_id) or {}
    resp["from_disk"] = session_id not in _SESSIONS or True
    return resp


@app.get("/api/runs")
def api_list_runs(limit: int = 30):
    """会话/运行历史：扫描 output/runs。"""
    return {"ok": True, "harness_version": HARNESS_VERSION, "runs": list_runs(limit=limit)}


@app.get("/api/runs/compare")
def api_compare_runs(a: str, b: str):
    """对比两次 run 的摘要指标（须在 {run_id} 路由之前注册）。"""
    from packing_assistant.trace_events import RUNS_DIR

    def load_idx(rid: str) -> Dict[str, Any]:
        p = RUNS_DIR / rid / "index.json"
        if not p.exists():
            return {"run_id": rid, "error": "missing"}
        try:
            return json.loads(p.read_text(encoding="utf-8"))
        except Exception as e:
            return {"run_id": rid, "error": str(e)}

    ia, ib = load_idx(a), load_idx(b)
    keys = ("n0", "containers_used", "can_fit", "risk_decision")
    diff = {}
    for k in keys:
        diff[k] = {"a": ia.get(k), "b": ib.get(k), "same": ia.get(k) == ib.get(k)}
    return {"ok": True, "a": ia, "b": ib, "diff": diff}


@app.get("/api/runs/{run_id}/replay")
def api_replay_run(run_id: str, delay_ms: int = 0):
    """
    将 trace.jsonl 以 SSE 回放（agents-observe 式 time-travel）。
    delay_ms>0 时逐步放慢，便于前端演示。
    """
    import time as _time

    events = read_trace_jsonl(run_id, limit=5000)
    if not events:
        raise HTTPException(404, f"no trace.jsonl for run {run_id}")

    def gen():
        yield f"data: {json.dumps({'type': 'replay_start', 'run_id': run_id, 'n': len(events), 'schema': 'packing.stream.v1'}, ensure_ascii=False)}\n\n"
        for ev in events:
            payload = dict(ev)
            payload["replay"] = True
            yield f"data: {json.dumps(payload, ensure_ascii=False, default=str)}\n\n"
            if delay_ms > 0:
                _time.sleep(min(delay_ms, 2000) / 1000.0)
        yield f"data: {json.dumps({'type': 'replay_done', 'run_id': run_id}, ensure_ascii=False)}\n\n"

    return StreamingResponse(
        gen(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


@app.get("/api/engine-ab")
def api_engine_ab(cases: str = "case_a_small_cartons_20gp,case_b_long_frames_40hq"):
    """轻量引擎 A/B（python-laff vs skjolber）。"""
    import subprocess
    import sys
    from pathlib import Path

    out_path = ROOT / "output" / "engine_ab_report.json"
    cmd = [
        sys.executable,
        str(ROOT / "scripts" / "compare_pack_engines.py"),
        "--cases",
        cases,
        "--out",
        str(out_path),
    ]
    try:
        subprocess.run(cmd, cwd=str(ROOT), timeout=120, check=False)
    except Exception as e:
        return {"ok": False, "error": str(e)}
    if out_path.exists():
        try:
            return {"ok": True, "report": json.loads(out_path.read_text(encoding="utf-8"))}
        except Exception as e:
            return {"ok": False, "error": str(e)}
    return {"ok": False, "error": "report not written"}


class TraceRequest(BaseModel):
    """逐步返回各 Agent 摘要，用于证明 Agent 链路在跑。"""

    user_input: str = "Agent API trace"
    materials: Optional[List[Dict[str, Any]]] = None
    container_type: str = "40HQ"
    session_id: str = "trace"
    max_containers: int = 0
    goal: str = "deliver_valid_pack_plan"
    enable_auto_confirm: bool = True


@app.post("/api/pipeline/trace")
def api_pipeline_trace(body: TraceRequest):
    """
    兼容入口：等同 POST /api/pipeline mode=steps。
    本地逐步调用 9 Agent，返回每步 message + tools_used + 落盘路径。
    """
    state = run_agent_pipeline(
        body.user_input,
        materials=body.materials,
        container_type=body.container_type,
        max_containers=int(body.max_containers or 0),
        enable_auto_confirm=body.enable_auto_confirm,
        goal=body.goal,
        session_id=body.session_id,
        save_artifacts=True,
    )
    _store_session(body.session_id, state)
    plan = state.get("container_plan") or {}
    book = state.get("booking") or plan.get("booking") or {}
    steps = state.get("agent_steps") or []
    return {
        "ok": True,
        "agent_definition": {
            "style": "multi_agent_workflow",
            "goal": state.get("goal") or "deliver_valid_pack_plan",
            "capabilities": ["感知", "规划", "使用工具", "采取行动", "追求目标(任务域)"],
            "note": "分角色流水线，非单体全能聊天 Agent；数值由 tools 计算",
        },
        "note": "数字由 tools 计算；Agent 负责任务分工、闸门、结构/风险裁决与过程可解释",
        "run_id": state.get("run_id"),
        "artifact_paths": state.get("artifact_paths") or {},
        "goal_status": state.get("goal_status") or {},
        "steps": steps,
        "summary": {
            "boxes": len(state.get("boxes") or []),
            "n0": book.get("n0") or plan.get("n0"),
            "containers_used": plan.get("containers_used"),
            "can_fit": plan.get("can_fit"),
            "booking_volume_utilization": plan.get("booking_volume_utilization"),
            "outer_space_utilization": plan.get("outer_space_utilization")
            or plan.get("space_utilization"),
            "weight_utilization": plan.get("weight_utilization"),
            "risk_decision": (state.get("risk_report") or {}).get("decision"),
            "phase": state.get("phase"),
        },
        "public": public_response(state),
    }


@app.get("/api/test-shipments")
def api_test_shipments():
    """读取 test 跑批汇总。"""
    p = ROOT / "output" / "test_shipments" / "summary.json"
    if not p.exists():
        return {"ok": False, "message": "尚未跑批，请先 python scripts/run_test_shipments.py"}
    return json.loads(p.read_text(encoding="utf-8"))


@app.get("/api/test-shipments/report")
def api_test_report():
    p = ROOT / "output" / "test_shipments" / "report.html"
    if not p.exists():
        raise HTTPException(404, "report.html 不存在，请先跑批")
    return FileResponse(p)


class PdfRunRequest(BaseModel):
    filename: str = ""
    container_type: str = "40HQ"
    session_id: str = "pdf-run"
    # project=同一 PDF 全部材料拼柜；per_container=仅第一柜（旧行为）
    mode: str = "project"


@app.post("/api/run-pdf")
def api_run_pdf(body: PdfRunRequest):
    """解析 test 下指定 PDF：默认同一项目拼柜（全部材料合并优化柜数）。"""
    from packing_assistant.tools.packing_list_parser import parse_packing_list_pdf
    from packing_assistant.tools.dims_override import apply_dims_override

    test_dir = ROOT / "test"
    path = None
    if body.filename:
        cand = test_dir / body.filename
        if cand.exists():
            path = cand
    if path is None:
        pdfs = sorted(test_dir.glob("*.pdf"))
        if not pdfs:
            raise HTTPException(404, "test/ 下无 PDF")
        path = pdfs[0]

    pl = parse_packing_list_pdf(path)
    mats = apply_dims_override(pl.get("materials") or [])
    if not mats:
        raise HTTPException(400, f"未能从 {path.name} 解析材料")

    mode = (body.mode or "project").strip().lower()
    pdf_ctns = pl.get("containers") or []
    if mode == "per_container":
        ctn = pdf_ctns[0] if pdf_ctns else None
        group = [m for m in mats if m.get("container_no") == ctn] if ctn else mats
        if not group:
            group = mats
        label = f"PDF:{path.name}:{ctn or 'ALL'}"
    else:
        # 同一项目：合并全部材料
        ctn = ",".join(pdf_ctns) if pdf_ctns else "PROJECT"
        group = mats
        label = f"PDF:{path.name}:PROJECT"

    # 按净重粗估柜数，避免整票重货 can_fit=False
    net = sum(float(m.get("total_weight_kg") or 0) for m in group)
    max_ctn = min(max(int(net / 18000) + 1, 1), 12)

    state = run_pipeline(
        raw_input=label,
        materials=group,
        container_type=body.container_type,
        enable_auto_confirm=True,
        max_containers=max_ctn,
    )
    # 若估少了，逐步加柜
    plan = state.get("container_plan") or {}
    if not plan.get("can_fit"):
        for mc in range(max_ctn + 1, 13):
            state = run_pipeline(
                raw_input=label,
                materials=group,
                container_type=body.container_type,
                enable_auto_confirm=True,
                max_containers=mc,
            )
            plan = state.get("container_plan") or {}
            if plan.get("can_fit"):
                break

    _store_session(body.session_id, state)
    resp = public_response(state)
    resp["pdf_file"] = path.name
    resp["container_no"] = ctn
    resp["mode"] = mode
    resp["materials_used"] = len(group)
    resp["pdf_containers"] = pdf_ctns
    return resp
