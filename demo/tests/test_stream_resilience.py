"""断流 / 任务切换 / 上传 / 下载 回归。全部离线，不需要 API Key。"""

from __future__ import annotations

import io
import json
import re
import sys
import time
import zipfile
from pathlib import Path

import pytest

DEMO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(DEMO))

from llm import LLMError  # noqa: E402


@pytest.fixture()
def client(monkeypatch):
    from fastapi.testclient import TestClient

    import app as m

    monkeypatch.setattr(m, "has_key", lambda: True)
    return TestClient(m.app)


@pytest.fixture()
def sess():
    """Unique session id per test; uploads/deliverables it creates are removed afterwards."""
    import shutil
    import uuid

    import app as m
    from attach import UPLOAD_ROOT

    sid = f"t{uuid.uuid4().hex[:10]}"
    yield sid
    for d in (UPLOAD_ROOT / sid, m.OUT_ROOT / sid):
        shutil.rmtree(d, ignore_errors=True)


def _events(body: str) -> list[tuple[str, dict | None]]:
    out = []
    for block in body.split("\n\n"):
        block = block.strip()
        if not block:
            continue
        if block.startswith(":"):
            out.append(("ping", None))
            continue
        name, data = "message", None
        for line in block.split("\n"):
            if line.startswith("event: "):
                name = line[7:]
            elif line.startswith("data: "):
                data = json.loads(line[6:])
        out.append((name, data))
    return out


def _docx(text: str) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr(
            "word/document.xml",
            '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
            f"<w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>",
        )
    return buf.getvalue()


# ---------- health / capabilities ----------


def test_health_declares_capabilities(client):
    j = client.get("/api/health").json()
    caps = j["capabilities"]
    assert caps["backend"] == "python"
    assert caps["upload"] and caps["threads"] and caps["heartbeat"]
    assert caps["firm"] is False


# ---------- upload / attachments ----------


def test_upload_roundtrip_and_limits(client, monkeypatch, sess):
    import attach

    r = client.post("/api/upload", data={"session_id": sess}, files={"file": ("说明.txt", "临边高度 [A001]".encode())})
    assert r.status_code == 200, r.text
    meta = r.json()["files"][0]
    assert meta["name"] == "说明.txt" and meta["chars"] > 0

    r = client.post("/api/upload", data={"session_id": sess}, files={"file": ("招标.docx", _docx("交货期 90 个日历天"))})
    assert r.status_code == 200, r.text

    r = client.get("/api/attachments", params={"session_id": sess})
    names = {f["name"] for f in r.json()["files"]}
    assert {"说明.txt", "招标.docx"} <= names

    fid = [f for f in r.json()["files"] if f["name"] == "招标.docx"][0]["id"]
    assert "90 个日历天" in attach.read_upload(sess, fid)
    bundled = attach.bundle_for_prompt(sess, [fid], "抽评分点")
    assert "用户上传：招标.docx" in bundled and bundled.endswith("抽评分点")

    r = client.post("/api/upload", data={"session_id": sess}, files={"file": ("x.exe", b"MZ")})
    assert r.status_code == 400 and "只接受" in r.text
    r = client.post("/api/upload", data={"session_id": "x"}, files={"file": ("a.txt", b"hi")})
    assert r.status_code == 400
    monkeypatch.setattr(attach, "MAX_BYTES", 8)
    with pytest.raises(ValueError):
        attach.extract_text("big.txt", b"0123456789")


def test_local_import_rejects_layout_and_missing(client, tmp_path, sess):
    r = client.post("/api/local", json={"session_id": sess, "path": "D:\\layout"})
    assert r.status_code == 400
    r = client.post("/api/local", json={"session_id": sess, "path": str(tmp_path / "nope")})
    assert r.status_code == 400 and "工作台所在电脑" in r.text
    (tmp_path / "台账.csv").write_text("a,b\n1,2\n", encoding="utf-8")
    r = client.post("/api/local", json={"session_id": sess, "path": str(tmp_path)})
    assert r.status_code == 200 and r.json()["files"][0]["name"] == "台账.csv"


# ---------- download ----------


def test_download_keeps_chinese_filename_and_supports_inline(client, sess):
    import app as m

    d = m.OUT_ROOT / sess / "construction"
    d.mkdir(parents=True, exist_ok=True)
    f = d / "专项方案-AI草稿.md"
    f.write_text("# 草稿\n", encoding="utf-8")

    r = client.get("/api/file", params={"path": str(f)})
    assert r.status_code == 200
    cd = r.headers["content-disposition"]
    assert cd.startswith("attachment") and "filename*=utf-8''" in cd.lower()
    assert "%E4%B8%93" in cd  # 专 percent-encoded, not raw UTF-8 bytes

    r = client.get("/api/file", params={"path": f"{sess}/construction/专项方案-AI草稿.md", "inline": "1"})
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/plain")
    assert r.headers["content-disposition"].startswith("inline")

    assert client.get("/api/file", params={"path": "../../README.md"}).status_code == 403


# ---------- stream resilience ----------


def test_files_are_surfaced_before_a_mid_run_failure(client, monkeypatch):
    import agent as ag

    calls = {"n": 0}

    def fake_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        calls["n"] += 1
        if calls["n"] == 1:
            args = json.dumps({"filename": "专项方案-AI草稿.md", "markdown": "# 草稿\n正文"}, ensure_ascii=False)
            yield {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "write_deliverable", "arguments": args}}],
                },
            }
            return
        raise LLMError("LLM 超时：read timeout")

    monkeypatch.setattr(ag, "stream_chat", fake_stream)
    r = client.post(
        "/api/chat",
        json={"message": "写一份临边专项方案提纲", "expert_ids": ["construction"], "confirm_ok": True},
    )
    assert r.status_code == 200
    evs = _events(r.text)
    names = [n for n, _ in evs]
    assert "file" in names and "error" in names
    assert names.index("file") < names.index("error")
    file_ev = [d for n, d in evs if n == "file"][0]
    assert file_ev["deliverables"][0]["name"] == "专项方案-AI草稿.md"
    err = [d for n, d in evs if n == "error"][0]
    assert err["recoverable"] is True and err["deliverables"]

    ctx = [d for n, d in evs if n == "context"][0]
    tid = ctx["thread_id"]
    msgs = client.get(f"/api/threads/{tid}/messages").json()["messages"]
    assert msgs[0]["role"] == "user" and msgs[-1]["role"] == "assistant" and msgs[-1].get("error")
    files = client.get(f"/api/threads/{tid}/files").json()["files"]
    assert files and files[0]["name"] == "专项方案-AI草稿.md"
    assert client.get(f"/api/threads/{tid}").json()["state"] == "failed"


def test_tokens_stream_before_done_and_transcript_persists(client, monkeypatch):
    import agent as ag

    def fake_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        for piece in ("临边", "防护", "三米"):
            yield {"type": "text", "text": piece}
        yield {"type": "message", "message": {"role": "assistant", "content": "临边防护三米", "finish_reason": "stop"}}

    monkeypatch.setattr(ag, "stream_chat", fake_stream)
    r = client.post("/api/chat", json={"message": "写一份临边提纲", "expert_ids": ["construction"], "confirm_ok": True})
    evs = _events(r.text)
    names = [n for n, _ in evs]
    assert names.count("token") == 3 and names.index("token") < names.index("done")
    done = [d for n, d in evs if n == "done"][0]
    assert done["text"] == "临边防护三米" and done["skill"] == "construction"
    tid = [d for n, d in evs if n == "context"][0]["thread_id"]
    msgs = client.get(f"/api/threads/{tid}/messages").json()["messages"]
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["content"] == "临边防护三米"
    # second turn on the same thread reuses the session folder and keeps the transcript growing
    r2 = client.post("/api/chat", json={"message": "再写一份", "expert_ids": ["construction"], "confirm_ok": True, "thread_id": tid,
                                        "history": [{"role": "user", "content": "写一份临边提纲"}, {"role": "assistant", "content": "临边防护三米"}]})
    ctx2 = [d for n, d in _events(r2.text) if n == "context"][0]
    assert ctx2["thread_id"] == tid
    assert len(client.get(f"/api/threads/{tid}/messages").json()["messages"]) == 4


def test_expert_question_uses_model_grounded_on_kb(client, monkeypatch):
    import agent as ag

    seen = {}

    def fake_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        seen["system"] = messages[0]["content"]
        seen["tools"] = tools
        yield {"type": "text", "text": "要看高度，[A001] 待填。"}
        yield {"type": "message", "message": {"role": "assistant", "content": "要看高度，[A001] 待填。"}}

    monkeypatch.setattr(ag, "stream_chat", fake_stream)
    r = client.post("/api/chat", json={"message": "临边防护算不算危大？", "expert_ids": ["method-hazard"]})
    evs = _events(r.text)
    done = [d for n, d in evs if n == "done"][0]
    assert done["intent"] == "chat" and done["text"].startswith("要看高度")
    assert seen["tools"] is None and "本轮是提问" in seen["system"]
    assert done["deliverables"] == []


def test_sse_heartbeat_while_model_is_silent(client, monkeypatch):
    import agent as ag

    monkeypatch.setenv("CIVIL_SSE_PING_SEC", "0.15")

    def slow_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        time.sleep(0.5)
        yield {"type": "text", "text": "ok"}
        yield {"type": "message", "message": {"role": "assistant", "content": "ok"}}

    monkeypatch.setattr(ag, "stream_chat", slow_stream)
    r = client.post("/api/chat", json={"message": "什么是 GST", "expert_ids": []})
    assert ": ping" in r.text
    assert [n for n, _ in _events(r.text)].count("ping") >= 1


def test_cancel_mid_run_stops_before_next_step(client, monkeypatch):
    import agent as ag
    from packing_assistant.runtime import threads as T

    th = T.new_thread("stop-me")
    calls = {"n": 0}

    def fake_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        calls["n"] += 1
        yield {"type": "text", "text": "先检索"}
        # user presses 停止 while step 1 is streaming
        T.request_cancel(th.thread_id)
        args = json.dumps({"query": "临边"})
        yield {
            "type": "message",
            "message": {
                "role": "assistant",
                "content": "先检索",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "search_kb", "arguments": args}}],
            },
        }

    monkeypatch.setattr(ag, "stream_chat", fake_stream)
    r = client.post(
        "/api/chat",
        json={"message": "写一份提纲", "expert_ids": ["construction"], "confirm_ok": True, "thread_id": th.thread_id},
    )
    evs = _events(r.text)
    done = [d for n, d in evs if n == "done"][0]
    assert done["stopped"] is True
    assert calls["n"] == 1, "no second LLM step after cancel"
    assert any(n == "status" and d.get("phase") == "stopped" for n, d in evs if d)
    assert client.get(f"/api/threads/{th.thread_id}").json()["state"] == "cancelled"
    assert not T.cancel_requested(th.thread_id)


def test_consecutive_user_turns_are_merged(client, monkeypatch):
    import agent as ag

    seen = {}

    def fake_stream(messages, *, tools=None, temperature=0.3, should_stop=None):
        seen["messages"] = messages
        yield {"type": "message", "message": {"role": "assistant", "content": "ok"}}

    monkeypatch.setattr(ag, "stream_chat", fake_stream)
    hist = [{"role": "user", "content": "第一问"}, {"role": "user", "content": "第二问（上一条断流没回）"}]
    client.post("/api/chat", json={"message": "第三问", "history": hist, "expert_ids": []})
    roles = [m["role"] for m in seen["messages"]]
    assert roles == ["system", "user"]
    assert "第一问" in seen["messages"][1]["content"] and "第三问" in seen["messages"][1]["content"]


# ---------- threads ----------


def test_threads_stale_after_restart_and_cancel(client):
    from packing_assistant.runtime import threads as T

    th = T.new_thread("orphan")
    th.state = "running"
    T.save_thread(th)
    th.updated_at = T._BOOT - 10
    T._atomic_write(T._path(th.thread_id), json.dumps(th.to_dict(), ensure_ascii=False))
    rows = {t["thread_id"]: t for t in client.get("/api/threads").json()["threads"]}
    assert rows[th.thread_id]["state"] == "stale"

    th2 = T.new_thread("cancel-me")
    got = client.post(f"/api/threads/{th2.thread_id}/cancel").json()
    assert got["ok"] and got["state"] == "cancelled"
    assert client.delete(f"/api/threads/{th2.thread_id}").json()["ok"]
    assert client.get(f"/api/threads/{th2.thread_id}").status_code == 404
    assert client.post("/api/threads/nope/cancel").status_code == 404


# ---------- frontend ↔ backend contract ----------


def test_frontend_calls_only_routes_this_backend_serves_or_gates():
    import app as m

    js = (DEMO / "static" / "app.js").read_text(encoding="utf-8")
    called = set(re.findall(r"""(?:fetch\(|href\s*=\s*)[`"'](/api/[A-Za-z0-9_/{}$.-]+)""", js))
    called |= set(re.findall(r"""apiPath\(["'](/api/[A-Za-z0-9_/-]+)""", js))
    assert called, "no /api calls found in app.js — regex broke?"
    served = [r.path for r in m.app.routes if getattr(r, "path", "").startswith("/api/")]

    def matches(path: str) -> bool:
        parts = path.split("/")
        for route in served:
            rp = route.split("/")
            if len(rp) != len(parts):
                continue
            if all(a == b or a.startswith("{") or b.startswith("${") or b.startswith("{") for a, b in zip(rp, parts)):
                return True
        return False

    gated = {"/api/firm/bid"}  # hidden by capabilities.firm=false; the route still answers 501 politely
    missing = sorted(p for p in called if not matches(p) and p not in gated)
    assert not missing, f"frontend calls routes this backend does not serve: {missing}"


def test_frontend_guards_secure_context_only_apis():
    js = (DEMO / "static" / "app.js").read_text(encoding="utf-8")
    # crypto.randomUUID is undefined on http://LAN-IP (phones): must be feature-detected, never called bare
    assert "session: uid()" in js
    assert 'typeof crypto.randomUUID === "function"' in js
    assert js.count("crypto.randomUUID(") == 1
    # every optional surface is gated on capabilities, not on a 404
    for cap in ("!!c.upload", "!!c.threads", "!!c.firm", "!!c.local", "caps.cancel", "caps.thread_messages", "caps.thread_files"):
        assert cap in js, cap


# ---------- optional token gate (CIVIL_TOKEN) ----------


def test_token_gate_when_civil_token_set(client, monkeypatch):
    monkeypatch.setenv("CIVIL_TOKEN", "s3cret")
    assert client.get("/api/catalog").status_code == 401
    assert client.get("/").status_code == 200, "page must load so it can ask for the token"
    h = client.get("/api/health")
    assert h.status_code == 200 and h.json()["capabilities"]["auth"] is True
    assert client.get("/api/catalog", cookies={"cb_token": "s3cret"}).status_code == 200
    assert client.get("/api/catalog", headers={"Authorization": "Bearer s3cret"}).status_code == 200
    assert client.get("/api/catalog", params={"token": "wrong"}).status_code == 401
    monkeypatch.delenv("CIVIL_TOKEN")
    assert client.get("/api/catalog").status_code == 200


def test_chat_refuses_while_detached_run_is_still_producing(client):
    from packing_assistant.runtime import threads as T

    th = T.new_thread("busy")
    th.state = "running"
    T.save_thread(th)  # updated_at >= _BOOT → a live detached run, not a stale one
    r = client.post("/api/chat", json={"message": "再来一条", "expert_ids": [], "thread_id": th.thread_id})
    assert r.status_code == 409 and "自动同步" in r.text
    th.state = "done"
    T.save_thread(th)
    assert client.get("/api/health").json()["capabilities"]["detach"] is True
