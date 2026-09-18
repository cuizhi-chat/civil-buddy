from __future__ import annotations

import asyncio
import json
import mimetypes
import os
import threading
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any
from urllib.parse import quote

from fastapi import FastAPI, File, Form, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, PlainTextResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

from agent import run_expert, run_plain
from catalog import catalog_payload, get_expert, resolve_mentions
from config import DEMO_ROOT, OUT_ROOT, llm_model
from kbio import MAX_FILE_BYTES, create_file, delete_file, format_bytes, read_text, write_text
from llm import LLMError, has_key
from rag import list_kb
from store import (
    disable_or_delete_expert,
    set_soft_limit,
    tree_payload,
    upsert_category,
    upsert_expert,
)

app = FastAPI(title="Civil Buddy Workbench")
STATIC = DEMO_ROOT / "static"
app.mount("/static", StaticFiles(directory=STATIC), name="static")

WORKBENCH_VERSION = "0.7.0"
# Dedicated pool for chat runs so long expert runs never starve the default sync-endpoint pool.
_CHAT_POOL = ThreadPoolExecutor(
    max_workers=int(os.environ.get("CIVIL_MAX_CHATS", "8") or 8), thread_name_prefix="civil-chat"
)
CAPABILITIES = {
    "backend": "python",
    "upload": True,
    "attachments": True,
    "local": True,
    "firm": False,  # 一人公司成套 only ships in the Rust workbench (harness steps)
    "threads": True,
    "thread_messages": True,
    "thread_files": True,
    "cancel": True,
    "config": True,
    "skills": True,
    "mcp": True,
    "heartbeat": True,
    "file_events": True,
}


class ChatIn(BaseModel):
    message: str
    history: list[dict] = Field(default_factory=list)
    expert_ids: list[str] = Field(default_factory=list)
    confirm_ok: bool = False
    session_id: str = ""
    thread_id: str = ""
    attachments: list[str] = Field(default_factory=list)


class ExpertIn(BaseModel):
    id: str
    name: str
    category: str
    title: str = ""
    delivers: str = ""
    risk: str = "low"
    aliases: str = ""
    pipeline: str = ""


class CategoryIn(BaseModel):
    id: str
    name: str
    blurb: str = ""


class FileIn(BaseModel):
    path: str
    content: str = ""


class LimitIn(BaseModel):
    kb_soft_limit_kb: int


@app.get("/")
def index() -> FileResponse:
    return FileResponse(STATIC / "index.html")


@app.get("/api/health")
def health() -> dict:
    from context import policy
    from packing_assistant.office_job import job_root, job_root_granted, list_job_files

    return {
        "ok": True,
        "product": "civil-codex",
        "product_name": "Civil Buddy",
        "tagline": "土木版 Codex",
        "version": WORKBENCH_VERSION,
        "capabilities": CAPABILITIES,
        "has_key": has_key(),
        "deepseek": has_key(),
        "model": llm_model(),
        "context": policy(),
        "job": {
            "granted": job_root_granted(),
            "root": str(job_root()) if job_root_granted() else "",
            "n": len(list_job_files()),
        },
    }


@app.get("/api/job")
def job_listing() -> dict:
    from packing_assistant.office_job import job_root, job_root_granted, list_job_files

    return {
        "ok": True,
        "granted": job_root_granted(),
        "root": str(job_root()) if job_root_granted() else "",
        "files": list_job_files(),
        "hint": (
            "说「写一份」会自动抄作业根里的 xlsx/docx/csv/txt，不必再上传。"
            if job_root_granted()
            else "设 CIVIL_JOB_ROOT 为工程文件夹后，本岗直接读该目录，不必上传。禁止 D:\\layout。"
        ),
    }


@app.get("/api/mcp/capabilities")
def mcp_capabilities() -> dict:
    from mcp_surface import initialize_capabilities

    return {"ok": True, "capabilities": initialize_capabilities()}


@app.get("/api/mcp/resources")
def mcp_resources(expert_id: str) -> dict:
    from mcp_surface import list_resources

    exp = get_expert(expert_id)
    if not exp:
        raise HTTPException(404, "unknown expert")
    return {"ok": True, "resources": list_resources(exp.id, exp.category)}


class McpResourceIn(BaseModel):
    uri: str
    expert_id: str = "bid-parse"


@app.post("/api/mcp/resources/read")
def mcp_resource_read(body: McpResourceIn) -> dict:
    from mcp_surface import read_resource

    exp = get_expert(body.expert_id)
    if not exp:
        raise HTTPException(404, "unknown expert")
    return {"ok": True, **read_resource(exp.id, exp.category, body.uri)}


@app.get("/api/mcp/prompts")
def mcp_prompts(expert_id: str = "") -> dict:
    from mcp_surface import list_prompts

    if expert_id and not get_expert(expert_id):
        raise HTTPException(404, "unknown expert")
    return {"ok": True, "prompts": list_prompts(expert_id=expert_id or None)}


class McpPromptIn(BaseModel):
    name: str
    expert_id: str = "bid-parse"
    arguments: dict = Field(default_factory=dict)


@app.post("/api/mcp/prompts/get")
def mcp_prompt_get(body: McpPromptIn) -> dict:
    from mcp_surface import get_prompt

    if not get_expert(body.expert_id):
        raise HTTPException(404, "unknown expert")
    return {"ok": True, **get_prompt(body.name, body.arguments, expert_id=body.expert_id)}


def _mcp_expert_ok(expert_id: str) -> bool:
    return (not expert_id) or bool(get_expert(expert_id)) or expert_id in {"pack-ship"}


@app.get("/api/mcp/tools")
def mcp_tools(expert_id: str = "") -> dict:
    from mcp_surface import list_tools

    if expert_id and not _mcp_expert_ok(expert_id):
        raise HTTPException(404, "unknown expert")
    return {"ok": True, "tools": list_tools(expert_id=expert_id or None)}


class McpToolIn(BaseModel):
    name: str
    expert_id: str = "pack-ship"
    arguments: dict = Field(default_factory=dict)


@app.post("/api/mcp/tools/call")
def mcp_tool_call(body: McpToolIn) -> dict:
    from mcp_surface import call_tool

    if body.expert_id and not _mcp_expert_ok(body.expert_id):
        raise HTTPException(404, "unknown expert")
    return {"ok": True, **call_tool(body.name, body.arguments, expert_id=body.expert_id or None)}


@app.get("/api/skills")
def skills() -> dict:
    from packing_assistant.runtime.expert_skills import catalog

    rows = catalog()
    return {"ok": True, "n": len(rows), "skills": rows, "host": "civil-buddy"}


@app.get("/api/config")
def get_config() -> dict:
    from packing_assistant.runtime.civil_config import load_config

    return {"ok": True, **load_config().to_dict()}


class PolicyIn(BaseModel):
    sandbox: str = ""
    approval: str = ""


@app.post("/api/config")
def set_config(body: PolicyIn) -> dict:
    import os

    from packing_assistant.runtime.civil_config import SANDBOX_MODES, APPROVAL_MODES, load_config

    if body.sandbox:
        os.environ["CIVIL_SANDBOX"] = body.sandbox
    if body.approval:
        os.environ["CIVIL_APPROVAL"] = body.approval
    cfg = load_config()
    if body.sandbox and cfg.sandbox not in SANDBOX_MODES:
        raise HTTPException(400, "bad sandbox")
    if body.approval and cfg.approval not in APPROVAL_MODES:
        raise HTTPException(400, "bad approval")
    return {"ok": True, **cfg.to_dict()}


@app.get("/api/threads")
def threads_list() -> dict:
    from packing_assistant.runtime.threads import list_threads

    rows = [t.to_dict() for t in list_threads()]
    return {"ok": True, "n": len(rows), "threads": rows}


class ThreadIn(BaseModel):
    text: str = ""
    title: str = ""
    skill: str = ""
    confirm_ok: bool = False
    background: bool = False
    thread_id: str = ""


@app.post("/api/threads")
def threads_run(body: ThreadIn) -> dict:
    from packing_assistant.runtime.threads import new_thread, run_on_thread, spawn

    if body.background and body.text.strip():
        return spawn(body.text, skill=body.skill, confirm=body.confirm_ok, title=body.title or body.text[:40])
    tid = (body.thread_id or "").strip()
    if not tid:
        th = new_thread(body.title or body.text[:40] or "新对话", confirm=body.confirm_ok)
        tid = th.thread_id
        if not body.text.strip():
            return {"ok": True, **th.to_dict()}
    return run_on_thread(tid, body.text, skill=body.skill, confirm=body.confirm_ok, background=body.background)


@app.get("/api/threads/{thread_id}")
def thread_one(thread_id: str) -> dict:
    from packing_assistant.runtime.threads import thread_status

    got = thread_status(thread_id)
    if not got.get("ok"):
        raise HTTPException(404, "unknown thread")
    return got


@app.get("/api/threads/{thread_id}/messages")
def thread_messages(thread_id: str, limit: int = 400) -> dict:
    from packing_assistant.runtime.threads import load_messages, load_thread

    th = load_thread(thread_id)
    if not th:
        raise HTTPException(404, "unknown thread")
    return {"ok": True, **th.to_dict(), "messages": load_messages(thread_id, limit=max(1, min(limit, 2000)))}


def _session_files(session_id: str) -> list[dict[str, Any]]:
    """Every deliverable written under out/<session>/<expert>/ — survives a broken stream."""
    from attach import sanitize_session

    try:
        sid = sanitize_session(session_id)
    except ValueError:
        return []
    root = OUT_ROOT / sid
    if not root.is_dir():
        return []
    rows = []
    for p in sorted(root.glob("*/*"), key=lambda x: x.stat().st_mtime, reverse=True):
        if p.is_file():
            rows.append({"expert": p.parent.name, "name": p.name, "path": str(p), "bytes": p.stat().st_size, "mtime": p.stat().st_mtime})
    return rows


@app.get("/api/threads/{thread_id}/files")
def thread_files(thread_id: str) -> dict:
    from packing_assistant.runtime.threads import load_thread

    th = load_thread(thread_id)
    if not th:
        raise HTTPException(404, "unknown thread")
    return {"ok": True, "thread_id": thread_id, "session_id": th.session_id, "files": _session_files(th.session_id)}


@app.post("/api/threads/{thread_id}/cancel")
def thread_cancel(thread_id: str) -> dict:
    from packing_assistant.runtime.threads import request_cancel

    got = request_cancel(thread_id)
    if not got.get("ok"):
        raise HTTPException(404, "unknown thread")
    return got


@app.delete("/api/threads/{thread_id}")
def thread_delete(thread_id: str) -> dict:
    from packing_assistant.runtime.threads import delete_thread

    if not delete_thread(thread_id):
        raise HTTPException(404, "unknown thread")
    return {"ok": True, "thread_id": thread_id}


@app.get("/api/catalog")
def catalog() -> dict:
    return catalog_payload()


@app.get("/api/kb/{expert_id}")
def kb(expert_id: str) -> dict:
    exp = get_expert(expert_id)
    if not exp:
        raise HTTPException(404, "unknown expert")
    files = list_kb(exp.id, exp.category)
    total = sum(int(f.get("bytes") or 0) for f in files)
    return {
        "expert": expert_id,
        "files": files,
        "bytes": total,
        "label": format_bytes(total),
    }


@app.get("/api/studio/tree")
def studio_tree() -> dict:
    return tree_payload()


@app.get("/api/studio/file")
def studio_read(path: str) -> dict:
    got = read_text(path)
    if not got:
        raise HTTPException(404, "文件不存在")
    text, st = got
    return {"path": path, "content": text, **st}


@app.put("/api/studio/file")
def studio_write(body: FileIn) -> dict:
    try:
        st = write_text(body.path, body.content)
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True, **st, "label": format_bytes(st["bytes"])}


@app.post("/api/studio/file")
def studio_create(body: FileIn) -> dict:
    try:
        st = create_file(body.path)
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True, **st}


@app.delete("/api/studio/file")
def studio_delete(path: str) -> dict:
    try:
        delete_file(path)
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@app.post("/api/studio/experts")
def studio_expert(body: ExpertIn) -> dict:
    try:
        exp = upsert_expert(body.model_dump())
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    if not exp:
        raise HTTPException(500, "保存失败")
    return exp.to_dict()


@app.delete("/api/studio/experts/{expert_id}")
def studio_expert_del(expert_id: str, delete_kb: bool = True) -> dict:
    if not get_expert(expert_id):
        raise HTTPException(404, "unknown expert")
    disable_or_delete_expert(expert_id, delete_kb=delete_kb)
    return {"ok": True}


@app.post("/api/studio/categories")
def studio_category(body: CategoryIn) -> dict:
    try:
        return upsert_category(body.id, body.name, body.blurb)
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc


@app.post("/api/studio/limit")
def studio_limit(body: LimitIn) -> dict:
    return {"kb_soft_limit_kb": set_soft_limit(body.kb_soft_limit_kb), "max_file_bytes": MAX_FILE_BYTES}


@app.post("/api/upload")
async def upload(session_id: str = Form(...), file: list[UploadFile] = File(...)) -> dict:
    from attach import MAX_BYTES, save_upload

    if not session_id.strip():
        raise HTTPException(400, "缺少 session_id")
    if not file:
        raise HTTPException(400, "没有收到文件")
    saved = []
    errors = []
    for up in file:
        data = await up.read(MAX_BYTES + 1)
        try:
            saved.append(save_upload(session_id, up.filename or "upload.bin", data))
        except ValueError as exc:
            errors.append(f"{up.filename}：{exc}")
        finally:
            await up.close()
    if not saved:
        raise HTTPException(400, "；".join(errors) or "上传失败")
    return {"ok": True, "files": saved, "errors": errors}


@app.get("/api/attachments")
def attachments(session_id: str) -> dict:
    from attach import list_uploads

    if not session_id.strip():
        raise HTTPException(400, "缺少 session_id")
    return {"ok": True, "files": list_uploads(session_id)}


class LocalIn(BaseModel):
    session_id: str
    path: str = ""


@app.post("/api/local")
def local_import(body: LocalIn) -> dict:
    from attach import import_local

    try:
        files = import_local(body.session_id, body.path)
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    except PermissionError as exc:
        raise HTTPException(403, str(exc)) from exc
    return {"ok": True, "files": files}


@app.post("/api/firm/bid")
def firm_bid_unavailable() -> dict:
    raise HTTPException(501, "成套投标（一人公司 harness）只在 Rust 工作台里提供；Python 参考实现未移植。")


def _resolve_ids(body: ChatIn) -> tuple[list[str], str]:
    skill_source = ""
    ids = [i for i in body.expert_ids if get_expert(i)]
    if ids:
        skill_source = "given"
    if not ids:
        ids = resolve_mentions(body.message)
        if ids:
            skill_source = "given"
    if not ids:
        from packing_assistant.runtime.expert_skills import match_skill

        hit = match_skill(body.message)
        if hit and get_expert(hit):
            ids = [hit]
            skill_source = "matched"
    return ids, skill_source


def _clean_history(raw: list[dict]) -> list[dict]:
    history = []
    for item in raw[-80:]:
        role = item.get("role")
        content = item.get("content")
        if role in {"user", "assistant"} and isinstance(content, str) and content:
            # never send two consecutive user turns (a broken stream leaves one behind)
            if history and history[-1]["role"] == role == "user":
                history[-1] = {"role": "user", "content": history[-1]["content"] + "\n" + content}
                continue
            history.append({"role": role, "content": content})
    return history


def _run_events(
    *,
    ids: list[str],
    skill_source: str,
    history: list[dict],
    confirm_ok: bool,
    session: str,
    should_stop,
):
    """Synchronous event producer; runs on _CHAT_POOL."""
    if not ids:
        for ev in run_plain(history, should_stop=should_stop):
            if ev.get("event") == "done" and isinstance(ev.get("data"), dict):
                ev["data"]["skill"] = ""
                ev["data"]["skill_source"] = ""
            yield ev
        return
    n = len(ids)
    for i, eid in enumerate(ids):
        if should_stop():
            return
        exp = get_expert(eid)
        if not exp:
            continue
        if n > 1:
            yield {"event": "status", "data": {"phase": "queue", "text": f"独立专家 {i + 1}/{n}：{exp.name}"}}
        for ev in run_expert(exp, history, confirm_ok=confirm_ok, session_id=session, should_stop=should_stop):
            if ev.get("event") == "done" and isinstance(ev.get("data"), dict):
                ev["data"]["skill"] = eid
                ev["data"]["skill_source"] = skill_source or "given"
            yield ev


@app.post("/api/chat")
async def chat(body: ChatIn, request: Request) -> StreamingResponse:
    if not has_key():
        raise HTTPException(
            400,
            "未配置 API Key。在 demo/.env 写入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY。",
        )
    from attach import bundle_for_prompt
    from context import prepare_history
    from packing_assistant.runtime.threads import (
        append_message,
        cancel_requested,
        clear_cancel,
        load_thread,
        new_thread,
        save_thread,
    )

    OUT_ROOT.mkdir(parents=True, exist_ok=True)
    # one thread per conversation; the session (= deliverable folder) follows the thread
    th = load_thread(body.thread_id) if body.thread_id else None
    if th is None:
        th = new_thread(body.message[:40] or "新对话", confirm=body.confirm_ok)
    session = th.session_id or body.session_id or uuid.uuid4().hex[:12]
    thread_id = th.thread_id
    clear_cancel(thread_id)
    ids, skill_source = _resolve_ids(body)

    history = _clean_history(body.history)
    user_text = bundle_for_prompt(session, body.attachments, body.message) if body.attachments else body.message
    if history and history[-1]["role"] == "user":
        history[-1] = {"role": "user", "content": history[-1]["content"] + "\n" + user_text}
    else:
        history.append({"role": "user", "content": user_text})
    history, ctx_report = prepare_history(history)
    ctx_report = {**ctx_report, "thread_id": thread_id, "session_id": session, "experts": ids, "skill_source": skill_source}

    append_message(thread_id, "user", body.message, attachments=list(body.attachments), experts=ids)
    th.state = "running"
    th.last_text = body.message
    save_thread(th)

    loop = asyncio.get_running_loop()
    queue: asyncio.Queue = asyncio.Queue()
    stop = threading.Event()
    done_marker = object()

    def should_stop() -> bool:
        return stop.is_set() or cancel_requested(thread_id)

    def push(ev: Any) -> None:
        loop.call_soon_threadsafe(queue.put_nowait, ev)

    def worker() -> None:
        state = "done"
        try:
            for ev in _run_events(
                ids=ids,
                skill_source=skill_source,
                history=history,
                confirm_ok=body.confirm_ok,
                session=session,
                should_stop=should_stop,
            ):
                name = ev.get("event")
                data = ev.get("data") if isinstance(ev.get("data"), dict) else {}
                if name == "done":
                    text = str(data.get("text") or "")
                    if text:
                        append_message(
                            thread_id,
                            "assistant",
                            text,
                            expert=data.get("expert") or "",
                            citations=data.get("citations") or [],
                            deliverables=data.get("deliverables") or [],
                            stopped=bool(data.get("stopped")),
                        )
                    if data.get("stopped"):
                        state = "cancelled"
                elif name == "error":
                    state = "failed"
                    partial = str(data.get("partial_text") or "")
                    append_message(
                        thread_id,
                        "assistant",
                        partial or f"（中断：{data.get('text')}）",
                        expert=data.get("expert") or "",
                        deliverables=data.get("deliverables") or [],
                        error=str(data.get("text") or ""),
                    )
                push(ev)
                if stop.is_set():
                    state = "cancelled"
                    break
        except LLMError as exc:
            state = "failed"
            push({"event": "error", "data": {"text": str(exc), "recoverable": True}})
        except Exception as exc:  # noqa: BLE001
            state = "failed"
            push({"event": "error", "data": {"text": f"内部错误：{exc}", "recoverable": False}})
        finally:
            try:
                cur = load_thread(thread_id)
                if cur:
                    cur.state = state
                    cur.cancel_requested = False
                    save_thread(cur)
            except Exception:  # noqa: BLE001
                pass
            clear_cancel(thread_id)
            push(done_marker)

    _CHAT_POOL.submit(worker)
    ping_every = float(os.environ.get("CIVIL_SSE_PING_SEC", "15") or 15)

    async def events():
        try:
            yield _sse({"event": "context", "data": ctx_report})
            if ctx_report.get("compressed"):
                yield _sse({"event": "status", "data": {"phase": "compress", "text": ctx_report.get("note")}})
            while True:
                try:
                    ev = await asyncio.wait_for(queue.get(), timeout=ping_every)
                except asyncio.TimeoutError:
                    if await request.is_disconnected():
                        break
                    yield ": ping\n\n"  # SSE comment: keeps proxies/mobile radios from dropping the idle socket
                    continue
                if ev is done_marker:
                    break
                yield _sse(ev)
        finally:
            stop.set()  # client went away or we finished: the worker stops at its next LLM chunk / step

    return StreamingResponse(
        events(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no", "Connection": "keep-alive"},
    )


def _sse(ev: dict) -> str:
    return f"event: {ev['event']}\ndata: {json.dumps(ev['data'], ensure_ascii=False)}\n\n"


_TEXT_INLINE = {".md", ".txt", ".csv", ".json", ".log"}


def _resolve_deliverable(path: str) -> Path:
    raw = Path(path)
    target = (raw if raw.is_absolute() else OUT_ROOT / raw).resolve()
    try:
        target.relative_to(OUT_ROOT.resolve())
    except ValueError as exc:
        raise HTTPException(403, "not a deliverable") from exc
    if not target.is_file():
        raise HTTPException(404, "missing")
    return target


@app.get("/api/file")
def file(path: str, inline: bool = False):
    """Download a deliverable. `inline=1` previews text formats in the browser (phones)."""
    target = _resolve_deliverable(path)
    name = target.name
    if inline and target.suffix.lower() in _TEXT_INLINE:
        text = target.read_text(encoding="utf-8", errors="replace")
        return PlainTextResponse(text, headers={"Content-Disposition": f"inline; filename*=UTF-8''{quote(name)}"})
    media = mimetypes.guess_type(name)[0] or "application/octet-stream"
    # starlette emits RFC 5987 filename*= for non-ASCII names when filename= is given
    return FileResponse(target, media_type=media, filename=name)
