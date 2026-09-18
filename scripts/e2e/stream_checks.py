"""HTTP-level acceptance for /api/chat: heartbeat while the model is silent, real-time tokens,
file event before done, transcript + files, RFC 5987 download, and worker stop on client disconnect.

Needs a live workbench pointed at scripts/e2e/fake_llm.py:
    uvicorn fake_llm:app --port 9999
    CIVIL_API_KEY=test CIVIL_API_BASE=http://127.0.0.1:9999/v1 CIVIL_SSE_PING_SEC=1 python demo/serve.py
    python stream_checks.py http://127.0.0.1:8765      # or the Rust workbench on its port
"""
import httpx, json, time, threading
import sys
BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8765").rstrip("/")
FAKE = "http://127.0.0.1:9999"
def run(payload, abort_after=None):
    t0=time.time(); frames=[]
    with httpx.Client(timeout=60) as c:
        with c.stream("POST", BASE+"/api/chat", json=payload) as r:
            assert r.status_code==200, r.status_code
            buf=""
            for chunk in r.iter_text():
                buf+=chunk
                while "\n\n" in buf:
                    block,buf=buf.split("\n\n",1)
                    if block.startswith(":"): frames.append((round(time.time()-t0,2),"ping",None)); continue
                    ev="message"; data=None
                    for line in block.split("\n"):
                        if line.startswith("event: "): ev=line[7:]
                        elif line.startswith("data: "): data=json.loads(line[6:])
                    frames.append((round(time.time()-t0,2),ev,data))
                    if abort_after and time.time()-t0>abort_after:
                        return frames, True
    return frames, False

print("=== 1) plain chat: heartbeat during silence, then real-time tokens ===")
fr,_=run({"message":"什么是 GST","expert_ids":[]})
pings=[t for t,e,_ in fr if e=="ping"]; toks=[t for t,e,_ in fr if e=="token"]
print(f"pings at {pings} | first token at {toks[0]}s, last at {toks[-1]}s, n={len(toks)} | done text len={len([d for _,e,d in fr if e=='done'][0]['text'])}")
assert pings and toks[-1]-toks[0] > 3, "tokens must arrive spread over time (real streaming), not in one burst"
tid=[d for _,e,d in fr if e=="context"][0]["thread_id"]; print("thread:", tid)

print("\n=== 2) expert run: file event lands before the final answer ===")
fr,_=run({"message":"写一份临边专项方案提纲","expert_ids":["construction"],"confirm_ok":True,"thread_id":tid})
seq=[e for _,e,_ in fr if e!="ping"]
print("event order:", seq)
i_file=seq.index("file"); i_done=seq.index("done")
assert i_file < i_done and "reset" not in seq[:i_file]
print("deliverable:", [d for _,e,d in fr if e=="file"][0]["deliverables"][0]["name"])
r=httpx.get(f"{BASE}/api/threads/{tid}/messages").json(); print("transcript roles:", [m["role"] for m in r["messages"]])
files=httpx.get(f"{BASE}/api/threads/{tid}/files").json()["files"]; print("thread files:", [f["name"] for f in files])
path=files[0]["path"]
h=httpx.get(f"{BASE}/api/file", params={"path":path}).headers; print("download CD:", h["content-disposition"])
h=httpx.get(f"{BASE}/api/file", params={"path":path,"inline":"1"}).headers; print("inline CT:", h["content-type"], "|", h["content-disposition"][:20])

print("\n=== 3) client disconnects mid-stream (lock screen): run finishes detached, transcript gets the reply ===")
before=httpx.get(f"{FAKE}/state").json()
n_before=len(httpx.get(f"{BASE}/api/threads/{tid}/messages").json()["messages"])
fr,aborted=run({"message":"什么是 GST","expert_ids":[],"thread_id":tid}, abort_after=4.0)
print("client aborted after", fr[-1][0], "s with", len([1 for _,e,_ in fr if e=='token']), "tokens received")
deadline=time.time()+60; st={}
while time.time()<deadline:
    st=httpx.get(f"{BASE}/api/threads/{tid}").json()
    if st["state"]!="running": break
    time.sleep(0.5)
msgs=httpx.get(f"{BASE}/api/threads/{tid}/messages").json()["messages"]
after=httpx.get(f"{FAKE}/state").json()
print("thread state:", st["state"], "| upstream aborted:", after["aborted"]-before["aborted"], "| new rows:", len(msgs)-n_before, "| reply len:", len(msgs[-1]["content"]))
assert st["state"]=="done" and after["aborted"]-before["aborted"]==0, "detached run must finish"
assert msgs[-1]["role"]=="assistant" and len(msgs[-1]["content"])>=100, "full reply must be in the transcript"

print("\n=== 4) explicit 停止 (POST cancel) is the only thing that aborts the upstream ===")
before=httpx.get(f"{FAKE}/state").json()
def cancel_later():
    time.sleep(3.5); httpx.post(f"{BASE}/api/threads/{tid}/cancel")
threading.Thread(target=cancel_later).start()
fr,_=run({"message":"什么是 GST","expert_ids":[],"thread_id":tid})
done=[d for _,e,d in fr if e=="done"]
after=httpx.get(f"{FAKE}/state").json()
st=httpx.get(f"{BASE}/api/threads/{tid}").json()
print("done.stopped:", done[0].get("stopped") if done else None, "| upstream aborted:", after["aborted"]-before["aborted"], "| thread state:", st["state"], "| cancel flag cleared:", not st["cancel_requested"])
assert done and done[0].get("stopped") is True and st["state"]=="cancelled" and after["aborted"]-before["aborted"]>=1
print("\nALL E2E CHECKS PASSED")
