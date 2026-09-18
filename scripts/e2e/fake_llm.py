"""Fake OpenAI-compatible upstream: streams slowly; first call with tools returns a write_deliverable tool call."""
import asyncio, json, time
from fastapi import FastAPI, Request
from fastapi.responses import StreamingResponse
app = FastAPI()
STATE = {"calls": 0, "chunks_sent": 0, "aborted": 0}

def sse(obj): return f"data: {json.dumps(obj, ensure_ascii=False)}\n\n"

@app.post("/v1/chat/completions")
async def chat(req: Request):
    body = await req.json()
    STATE["calls"] += 1
    has_tools = bool(body.get("tools"))
    last = body["messages"][-1]
    async def gen():
        try:
            await asyncio.sleep(float(body.get("temperature", 0.3)) * 0 + 2.5)  # silent period → heartbeat must cover it
            if has_tools and last.get("role") != "tool":
                args = json.dumps({"filename": "临边专项方案-AI草稿.md", "markdown": "# 草稿\n1. 临边高度 [A001]\n"}, ensure_ascii=False)
                yield sse({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write_deliverable","arguments":""}}]},"finish_reason":None}]})
                for i in range(0, len(args), 12):
                    yield sse({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":args[i:i+12]}}]},"finish_reason":None}]})
                yield sse({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})
            else:
                for tok in ["临边", "防护", "高度", "以现场", "为准，", "[A001]", " 待填。"] * 6:
                    await asyncio.sleep(0.25)
                    STATE["chunks_sent"] += 1
                    yield sse({"choices":[{"delta":{"content":tok},"finish_reason":None}]})
                yield sse({"choices":[{"delta":{},"finish_reason":"stop"}]})
            yield "data: [DONE]\n\n"
        except asyncio.CancelledError:
            STATE["aborted"] += 1
            raise
    return StreamingResponse(gen(), media_type="text/event-stream")

@app.get("/state")
def state(): return STATE
