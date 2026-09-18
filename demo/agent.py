from __future__ import annotations

import json
from collections.abc import Iterator
from datetime import datetime
from pathlib import Path
from typing import Any

import sys

from catalog import Expert
from config import MAX_AGENT_STEPS, OUT_ROOT, REPO_ROOT
from llm import LLMError, LLMStopped, StopFn, chat, stream_chat
from rag import list_kb, read_kb, search_kb

if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))


def _safe_write(path: Path, text: str) -> None:
    try:
        from packing_assistant.sandbox import guarded_write_text

        guarded_write_text(path, text)
    except PermissionError:
        raise
    except Exception:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "search_kb",
            "description": "检索本专家库、本大类共享库、公司硬规则。先检索再写正文。",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_kb",
            "description": "按相对路径阅读一条知识库全文，路径来自 search_kb 或 list_kb。",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "list_kb",
            "description": "列出本专家可见的全部知识文件（专家私库 + 大类共享 + 公司）。",
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_deliverable",
            "description": "把独立完成的成稿落到本会话交付目录。高风险稿须用户已确认。",
            "parameters": {
                "type": "object",
                "properties": {
                    "filename": {"type": "string", "description": "如 专项方案-AI草稿.md"},
                    "markdown": {"type": "string"},
                },
                "required": ["filename", "markdown"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_attachment",
            "description": "分段读用户本会话上传的附件全文（id 来自用户消息里的【用户上传】或附件列表）。",
            "parameters": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                },
                "required": ["id"],
            },
        },
    },
]

EXTRACT_TENDER = {
    "type": "function",
    "function": {
        "name": "extract_tender",
        "description": "招标解析独有：用主线 C 同一套 tender.parse 抽出评分点/★/专项/工期。无正文则拒绝。",
        "parameters": {
            "type": "object",
            "properties": {
                "tender_text": {"type": "string"},
                "project_name": {"type": "string"},
            },
            "required": ["tender_text"],
        },
    },
}
COMPLIANCE_GAPS = {
    "type": "function",
    "function": {
        "name": "compliance_gaps",
        "description": "废标检查独有：对照招标正文列出 P0 / ★ / 资格缺口。不判定可投标。",
        "parameters": {
            "type": "object",
            "properties": {"tender_text": {"type": "string"}},
            "required": ["tender_text"],
        },
    },
}
TECH_EXPAND = {
    "type": "function",
    "function": {
        "name": "tech_expand",
        "description": "技术标独有：按抽出的评分点出目录骨架，不套上个项目模板。",
        "parameters": {
            "type": "object",
            "properties": {
                "tender_text": {"type": "string"},
                "project_name": {"type": "string"},
            },
            "required": ["tender_text"],
        },
    },
}


def tools_for_expert(expert: Expert | None) -> list:
    extra = []
    eid = getattr(expert, "id", "")
    if eid == "bid-parse":
        extra.append(EXTRACT_TENDER)
    elif eid == "bid-compliance":
        extra.append(COMPLIANCE_GAPS)
    elif eid == "bid-tech":
        extra.append(TECH_EXPAND)
    return [*TOOLS, *extra]


def _run_tender(text: str, project_name: str = "工作台招标解析") -> dict[str, Any]:
    from packing_assistant.tools.tender_parse import run_tender_pipeline

    return run_tender_pipeline(text, project_name=project_name, source="civil-workbench-demo")


def _workbench_extract(text: str, project_name: str = "工作台招标解析") -> dict[str, Any]:
    from packing_assistant.tools.tender_parse import workbench_bid_extract

    return workbench_bid_extract(text, project_name=project_name)


def execute_tool(
    name: str,
    args: dict[str, Any],
    *,
    expert: Expert,
    confirm_ok: bool,
    out_dir: Path,
    citations: list[dict[str, Any]],
    deliverables: list[dict[str, str]],
    session_id: str = "",
) -> str:
    """Shipped expert-tool dispatch. Tests call this; the LLM only chooses the name."""
    if name == "read_attachment":
        from attach import read_upload

        try:
            return read_upload(
                session_id,
                str(args.get("id") or ""),
                int(args.get("offset") or 0),
                int(args.get("limit") or 8000),
            )
        except (ValueError, TypeError) as exc:
            return f"读附件失败：{exc}"
    if name == "extract_tender":
        if expert.id != "bid-parse":
            return "拒绝：extract_tender 是 bid-parse 独有。"
        text = str(args.get("tender_text") or args.get("text") or "")
        if not text.strip():
            return "拒绝：没有招标正文。请粘贴公告/须知/评标办法后再抽。"
        out = _workbench_extract(text, str(args.get("project_name") or "工作台招标解析"))
        md = str(out.get("extract_table_markdown") or "")
        path = out_dir / "招标解析表.md"
        _safe_write(path, md)
        deliverables.append({"expert": expert.id, "name": path.name, "path": str(path)})
        ho = out.get("handoff") or {}
        return json.dumps(
            {
                "ok": out.get("ok"),
                "duration_days": out.get("duration_days"),
                "star_items": out.get("star_items"),
                "scoring_points": out.get("scoring_points"),
                "next_experts": ho.get("next_experts"),
                "submit_blocked": True if out.get("submit_blocked") is None else out.get("submit_blocked"),
                "n_star": len(out.get("star_items") or []),
                "n_scores": len(out.get("scoring_points") or []),
                "wrote": str(path),
            },
            ensure_ascii=False,
        )
    if name == "compliance_gaps":
        if expert.id != "bid-compliance":
            return "拒绝：compliance_gaps 是 bid-compliance 独有。"
        text = str(args.get("tender_text") or "")
        if not text.strip():
            return "拒绝：没有招标正文。"
        out = _run_tender(text)
        p0 = (out.get("handoff") or {}).get("p0_reject_scan") or {}
        md = ["# 响应缺口清单", "", p0.get("note") or "", ""]
        for it in p0.get("items") or []:
            md.append(f"- [{it.get('risk')}] {it.get('title')} · {it.get('requirement_ref')} · {it.get('exact_text')}")
        path = out_dir / "响应缺口清单.md"
        _safe_write(path, "\n".join(md))
        deliverables.append({"expert": expert.id, "name": path.name, "path": str(path)})
        return json.dumps(
            {"ok": True, "p0_n": p0.get("n"), "human_confirm_required": True, "wrote": str(path)},
            ensure_ascii=False,
        )
    if name == "tech_expand":
        if expert.id != "bid-tech":
            return "拒绝：tech_expand 是 bid-tech 独有。"
        text = str(args.get("tender_text") or "")
        if not text.strip():
            return "拒绝：没有招标正文或评分点。"
        out = _run_tender(text, str(args.get("project_name") or "工作台技术标"))
        outline = out.get("tech_outline") or {}
        md = str(outline.get("markdown") or "")
        path = out_dir / "技术标目录草稿.md"
        _safe_write(path, md)
        deliverables.append({"expert": expert.id, "name": path.name, "path": str(path)})
        return json.dumps(
            {
                "ok": True,
                "from_extracted_scores": outline.get("from_extracted_scores"),
                "n_chapters": outline.get("n_chapters"),
                "wrote": str(path),
            },
            ensure_ascii=False,
        )
    if name == "search_kb":
        hits = search_kb(expert.id, expert.category, str(args.get("query") or ""))
        citations.extend(
            {"path": h.path, "layer": h.layer, "title": h.title, "snippet": h.snippet}
            for h in hits
        )
        return json.dumps(
            [{"path": h.path, "layer": h.layer, "snippet": h.snippet} for h in hits],
            ensure_ascii=False,
        )
    if name == "list_kb":
        return json.dumps(list_kb(expert.id, expert.category), ensure_ascii=False)
    if name == "read_kb":
        got = read_kb(str(args.get("path") or ""))
        if not got:
            return "文件不存在或越权"
        rel, text = got
        return f"# {rel}\n\n{text[:8000]}"
    if name == "write_deliverable":
        from packing_assistant.runtime.civil_config import CONFIRM, high_risk_unconfirmed

        if high_risk_unconfirmed(risk=expert.risk, confirmed=confirm_ok):
            return f"拒绝写盘：高风险稿需要用户确认句「{CONFIRM}」。"
        body = str(args.get("markdown") or "")
        if not body.strip():
            return "拒绝写盘：markdown 为空（多半是参数 JSON 没生成完整）。请重新调用 write_deliverable 并带上完整正文。"
        raw_name = Path(str(args.get("filename") or "draft.md")).name
        if not raw_name.endswith((".md", ".txt")):
            raw_name += ".md"
        path = out_dir / raw_name
        _safe_write(path, body)
        item = {"expert": expert.id, "name": raw_name, "path": str(path)}
        deliverables.append(item)
        return f"已写入 {path}"
    return f"未知工具 {name}"


def build_expert_prompt(expert: Expert, confirm_ok: bool) -> str:
    """Shipped system prompt for a summoned expert. Tests must call this."""
    from packing_assistant.runtime.expert_skills import prompt_suffix

    return f"""你是土木企业工作台里的【{expert.name}】专家（大类：{expert.category_name}）。

提问权：全企业任何人都可以向你提问，不限于本部门。施工员可以问财务，商务可以问试验室，工人可以问造价。用户召唤了你，就用你的知识库答。

两类任务同等重要：
A. 问 / 不懂 / 解释 / 科普：用白话讲清楚本专业概念、流程、边界。可以只聊天，不必 write_deliverable。先 search_kb 再答。答完注明依据来自私库还是大类库。
B. 成稿 / 出文件 / 写方案表：按工序独立成稿，写完调用 write_deliverable。高风险且未确认则不要写盘。

问其他人：不要读取其他专家的私库。问题明显属于别的专业时，先答你能答的边界，并请用户在左侧改召唤那位专家。

职责：{expert.title}
默认交付：{expert.delivers}
风险：{expert.risk}
标准工序：{expert.pipeline}

知识分层（必须用工具，不许假装读过）：
1. 你的私库 kb/{expert.category}/{expert.id}/
2. 大类共享库 kb/{expert.category}/_shared/（同类专家共用）
3. 公司库 kb/company/ 与硬规则

硬规则（摘要，细节用 read_kb company/hard-rules.md）：
- 不编条款号、强度、岩土参数、综合单价。
- 引用写 全名+年份+条款；没抽到原文就 unverified / UNSPECIFIED。
- 无来源数字写 [A001] 待填。
- 禁止断言：可交差、可提交专家论证、请监理审核后开工、可以开工、报审通过。
- 产出是内部讨论 AI 草稿，不是法定签认件。
- 辖区 CN/SG/EU/DUAL 禁止静默混用。
- 高风险写盘前确认门。当前 confirm_ok={confirm_ok}。
  若用户要成稿且 risk=high 且 confirm_ok 不为 true：只问用户打出「我明白，将由持证人员签认」，不要 write_deliverable。纯提问（A）不受确认门阻挡。

先 search_kb。中文回答。
经营投标：bid-parse 必须先 extract_tender（与装箱主线同一套 parse）；bid-compliance 用 compliance_gaps；bid-tech 用 tech_expand。不要编造天数、分值、证号，不要判定可投标。
""" + prompt_suffix(expert.id)


def _system(expert: Expert, confirm_ok: bool) -> str:
    return build_expert_prompt(expert, confirm_ok)


def _plain_system() -> str:
    return (
        "你是 Civil Buddy（土木版 Codex）的路由器：还没有选用具体 skill。"
        "可以科普、讨论、列提纲，但必须声明这不是专家稿，条款和数字需要用户核对。"
        "不要假装引用了企业规范库。任务对得上某岗时建议用户用 $construction 或左侧技能库点名。"
        "数字走工具。不判定可以投标、可以开工。"
    )


def _last_user(history: list[dict[str, str]]) -> str:
    for m in reversed(history):
        if m.get("role") == "user":
            return str(m.get("content") or "")
    return ""


def _stopped() -> dict[str, Any]:
    return {"event": "status", "data": {"phase": "stopped", "text": "已按要求停止"}}


def _stream_text(messages: list[dict[str, Any]], *, temperature: float, should_stop: StopFn | None) -> Iterator[str]:
    """Single choke point for every model call in this module (tests patch agent.stream_chat)."""
    for ev in stream_chat(messages, temperature=temperature, should_stop=should_stop):
        if ev["type"] == "text":
            yield ev["text"]


def run_plain(history: list[dict[str, str]], should_stop: StopFn | None = None) -> Iterator[dict[str, Any]]:
    messages = [{"role": "system", "content": _plain_system()}, *history]
    yield {"event": "status", "data": {"phase": "plain", "text": "土木版 Codex · 未选用 skill · 路由器"}}
    buf: list[str] = []
    stopped = False
    try:
        for piece in _stream_text(messages, temperature=0.6, should_stop=should_stop):
            buf.append(piece)
            yield {"event": "token", "data": {"text": piece}}
    except LLMStopped:
        stopped = True
    yield {
        "event": "done",
        "data": {"mode": "plain", "text": "".join(buf), "citations": [], "deliverables": [], "stopped": stopped},
    }


def _expert_chat(
    expert: Expert,
    history: list[dict[str, str]],
    *,
    should_stop: StopFn | None,
) -> Iterator[dict[str, Any]]:
    """Question to a summoned expert: real model answer grounded on KB hits (no tools, no writes)."""
    question = _last_user(history)
    hits = search_kb(expert.id, expert.category, question)
    citations = [
        {"path": h.path, "layer": h.layer, "title": h.title, "snippet": h.snippet} for h in hits[:6]
    ]
    context = "\n\n".join(f"[{c['layer']}] {c['path']}\n{c['snippet']}" for c in citations) or "（本岗库没有命中片段）"
    system = (
        build_expert_prompt(expert, confirm_ok=False)
        + "\n\n本轮是提问，不成稿、不写盘。只根据下面的知识片段和硬规则作答；片段没有的就说没有，"
        "不要编条款号、数字。答完注明依据来自私库/大类库/公司库。\n\n知识片段：\n"
        + context
    )
    messages = [{"role": "system", "content": system}, *history]
    buf: list[str] = []
    stopped = False
    try:
        for piece in _stream_text(messages, temperature=0.3, should_stop=should_stop):
            buf.append(piece)
            yield {"event": "token", "data": {"text": piece}}
    except LLMStopped:
        stopped = True
    except LLMError:
        # model down: fall back to the offline template so the user still gets something
        try:
            from packing_assistant.expert_roster import get_expert as _ge
            from packing_assistant.expert_turn import explain_expert

            rec = _ge(expert.id)
            text = explain_expert(rec, question) if rec else ""
        except Exception:  # noqa: BLE001
            text = ""
        if not text:
            raise
        buf = [text]
        yield {"event": "token", "data": {"text": text}}
    yield {
        "event": "done",
        "data": {
            "mode": "expert",
            "expert": expert.id,
            "intent": "chat",
            "text": "".join(buf),
            "citations": citations,
            "deliverables": [],
            "stopped": stopped,
        },
    }


def run_expert(
    expert: Expert,
    history: list[dict[str, str]],
    *,
    confirm_ok: bool,
    session_id: str,
    should_stop: StopFn | None = None,
) -> Iterator[dict[str, Any]]:
    yield {
        "event": "status",
        "data": {
            "phase": "summon",
            "text": f"已召唤 {expert.category_name} / {expert.name} · 独立收工",
            "expert": expert.id,
        },
    }
    # Classify the *latest* user message (same rule as workbench/src/agent.rs), not the whole history.
    last = _last_user(history)
    try:
        from packing_assistant.understand import understand

        intent = understand(last)
    except Exception:
        intent = "run"
    yield {
        "event": "status",
        "data": {
            "phase": "understand",
            "text": f"{expert.name} · 听懂为 {intent}（能聊能跑）",
            "expert": expert.id,
            "intent": intent,
        },
    }
    if intent == "chat":
        yield from _expert_chat(expert, history, should_stop=should_stop)
        return

    messages: list[dict[str, Any]] = [
        {"role": "system", "content": _system(expert, confirm_ok)},
        *history,
    ]
    citations: list[dict[str, Any]] = []
    deliverables: list[dict[str, str]] = []
    out_dir = OUT_ROOT / session_id / expert.id
    out_dir.mkdir(parents=True, exist_ok=True)

    def _exec(name: str, args: dict[str, Any]) -> str:
        nonlocal citations, deliverables
        if intent == "chat" and name in {"write_deliverable", "extract_tender", "compliance_gaps", "tech_expand"}:
            return "拒绝写盘：本轮是提问，不成稿。"
        return execute_tool(
            name,
            args,
            expert=expert,
            confirm_ok=confirm_ok,
            out_dir=out_dir,
            citations=citations,
            deliverables=deliverables,
            session_id=session_id,
        )

    def _uniq_cites() -> list[dict[str, Any]]:
        uniq = []
        seen = set()
        for c in citations:
            if c["path"] in seen:
                continue
            seen.add(c["path"])
            uniq.append(c)
        return uniq

    final_text = ""
    streamed = ""
    stopped = False
    expert_tools = tools_for_expert(expert)
    try:
        for step in range(MAX_AGENT_STEPS):
            if should_stop and should_stop():
                stopped = True
                break
            yield {"event": "status", "data": {"phase": "think", "text": f"{expert.name} 步骤 {step + 1}/{MAX_AGENT_STEPS}"}}
            msg: dict[str, Any] | None = None
            streamed = ""
            for ev in stream_chat(messages, tools=expert_tools, should_stop=should_stop):
                if ev["type"] == "text":
                    streamed += ev["text"]
                    yield {"event": "token", "data": {"text": ev["text"]}}
                else:
                    msg = ev["message"]
            if msg is None:
                break
            tool_calls = msg.get("tool_calls") or []
            if not tool_calls:
                final_text = msg.get("content") or streamed
                break
            if streamed:
                # interim "thinking" text was shown; tools come next, so clear the bubble
                yield {"event": "reset", "data": {"reason": "tools"}}
            messages.append({k: v for k, v in msg.items() if k != "finish_reason"})
            for call in tool_calls:
                fn = call.get("function") or {}
                name = fn.get("name") or ""
                raw = fn.get("arguments") or "{}"
                bad_json = False
                try:
                    args = json.loads(raw) if isinstance(raw, str) else (raw or {})
                except json.JSONDecodeError:
                    args, bad_json = {}, True
                yield {
                    "event": "status",
                    "data": {
                        "phase": name,
                        "text": f"{expert.name} · {name} {args.get('query') or args.get('path') or args.get('filename') or args.get('id') or ''}",
                    },
                }
                if bad_json:
                    result = f"工具 {name} 的参数不是合法 JSON（可能被截断）。请缩短单次输出或分段重发。"
                else:
                    n_before = len(deliverables)
                    result = _exec(name, args)
                    if len(deliverables) > n_before:
                        # surface files the moment they land, so a later timeout cannot hide them
                        yield {"event": "file", "data": {"deliverables": deliverables[n_before:]}}
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": call.get("id") or name,
                        "content": result[:12000],
                    }
                )
        else:
            final_text = final_text or "（达到步数上限，请把任务拆小或再发一次）"
    except LLMStopped:
        stopped = True
    except LLMError as exc:
        yield {
            "event": "error",
            "data": {
                "text": str(exc),
                "partial_text": streamed,
                "expert": expert.id,
                "citations": _uniq_cites(),
                "deliverables": deliverables,
                "recoverable": True,
            },
        }
        return

    if stopped:
        yield _stopped()
        final_text = streamed
    elif not final_text:
        final_text = "已完成检索，但模型没有返回正文。请再试一次。"

    if final_text and final_text != streamed:
        # final text came back without streaming (rare: non-stream gateways) — show it in one go
        yield {"event": "reset", "data": {"reason": "final"}}
        yield {"event": "token", "data": {"text": final_text}}

    yield {
        "event": "done",
        "data": {
            "mode": "expert",
            "expert": expert.id,
            "text": final_text,
            "citations": _uniq_cites(),
            "deliverables": deliverables,
            "stopped": stopped,
            "stamp": datetime.now().strftime("%Y-%m-%dT%H-%M-%S"),
        },
    }
