from __future__ import annotations

import json
import os
from collections.abc import Callable, Iterator
from typing import Any

import httpx

from config import llm_api_key, llm_base_url, llm_model

StopFn = Callable[[], bool]


class LLMError(RuntimeError):
    pass


class LLMStopped(LLMError):
    """Raised when the caller asked to stop mid-stream (client gone / user pressed stop)."""


def has_key() -> bool:
    return bool(llm_api_key())


def _timeout() -> httpx.Timeout:
    # read = max silence between two chunks (streaming) or total body wait (non-streaming)
    read = float(os.environ.get("CIVIL_LLM_READ_TIMEOUT", "180") or 180)
    return httpx.Timeout(connect=15.0, read=read, write=30.0, pool=15.0)


def _headers() -> dict[str, str]:
    key = llm_api_key()
    if not key:
        raise LLMError(
            "未配置 API Key。在 demo/.env 写入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY 后重启。"
        )
    return {
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/json",
    }


def _payload(messages: list[dict[str, Any]], tools: list[dict[str, Any]] | None, temperature: float, stream: bool) -> dict:
    payload: dict[str, Any] = {
        "model": llm_model(),
        "messages": messages,
        "temperature": temperature,
    }
    if tools:
        payload["tools"] = tools
        payload["tool_choice"] = "auto"
    if stream:
        payload["stream"] = True
    return payload


def _http_error(r: httpx.Response, body: str) -> LLMError:
    hint = ""
    if r.status_code == 401:
        hint = "（Key 无效或网关地址不对）"
    elif r.status_code == 429:
        hint = "（触发限流，稍后再试）"
    return LLMError(f"LLM {r.status_code}{hint}: {body[:400]}")


def chat(
    messages: list[dict[str, Any]],
    *,
    tools: list[dict[str, Any]] | None = None,
    temperature: float = 0.3,
) -> dict[str, Any]:
    """Blocking, non-streaming completion. Kept for tests and short tool-routing calls."""
    try:
        with httpx.Client(timeout=_timeout()) as client:
            r = client.post(
                f"{llm_base_url()}/chat/completions",
                headers=_headers(),
                json=_payload(messages, tools, temperature, stream=False),
            )
    except httpx.TimeoutException as exc:
        raise LLMError(f"LLM 超时：{exc}") from exc
    except httpx.HTTPError as exc:
        raise LLMError(f"LLM 网络错误：{exc}") from exc
    if r.status_code >= 400:
        raise _http_error(r, r.text)
    return r.json()["choices"][0]["message"]


def _iter_sse_json(r: httpx.Response, should_stop: StopFn | None) -> Iterator[dict]:
    for line in r.iter_lines():
        if should_stop and should_stop():
            raise LLMStopped("stopped")
        if not line or not line.startswith("data: "):
            continue
        data = line[6:]
        if data.strip() == "[DONE]":
            break
        try:
            yield json.loads(data)
        except json.JSONDecodeError:
            continue


def stream_chat(
    messages: list[dict[str, Any]],
    *,
    tools: list[dict[str, Any]] | None = None,
    temperature: float = 0.3,
    should_stop: StopFn | None = None,
) -> Iterator[dict[str, Any]]:
    """Streaming completion that also assembles tool calls.

    Yields {"type": "text", "text": piece} as content arrives, then exactly one
    {"type": "message", "message": {...}} shaped like a non-streaming choice.message
    (content + tool_calls) so the caller can append it to history unchanged.
    """
    content: list[str] = []
    calls: dict[int, dict[str, Any]] = {}
    finish = ""
    try:
        with httpx.Client(timeout=_timeout()) as client:
            with client.stream(
                "POST",
                f"{llm_base_url()}/chat/completions",
                headers=_headers(),
                json=_payload(messages, tools, temperature, stream=True),
            ) as r:
                if r.status_code >= 400:
                    raise _http_error(r, r.read().decode("utf-8", errors="ignore"))
                for chunk in _iter_sse_json(r, should_stop):
                    choices = chunk.get("choices") or []
                    if not choices:
                        continue
                    ch = choices[0]
                    finish = ch.get("finish_reason") or finish
                    delta = ch.get("delta") or {}
                    piece = delta.get("content") or ""
                    if piece:
                        content.append(piece)
                        yield {"type": "text", "text": piece}
                    for tc in delta.get("tool_calls") or []:
                        idx = int(tc.get("index") or 0)
                        slot = calls.setdefault(idx, {"id": "", "type": "function", "function": {"name": "", "arguments": ""}})
                        if tc.get("id"):
                            slot["id"] = tc["id"]
                        fn = tc.get("function") or {}
                        if fn.get("name"):
                            slot["function"]["name"] += fn["name"]
                        if fn.get("arguments"):
                            slot["function"]["arguments"] += fn["arguments"]
    except LLMStopped:
        raise
    except httpx.TimeoutException as exc:
        raise LLMError(f"LLM 超时：{exc}") from exc
    except httpx.HTTPError as exc:
        raise LLMError(f"LLM 网络错误：{exc}") from exc
    msg: dict[str, Any] = {"role": "assistant", "content": "".join(content) or None}
    if calls:
        msg["tool_calls"] = [calls[i] for i in sorted(calls)]
    msg["finish_reason"] = finish
    yield {"type": "message", "message": msg}


def stream_plain(
    messages: list[dict[str, Any]],
    temperature: float = 0.6,
    should_stop: StopFn | None = None,
) -> Iterator[str]:
    for ev in stream_chat(messages, temperature=temperature, should_stop=should_stop):
        if ev["type"] == "text":
            yield ev["text"]
