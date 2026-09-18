// Front-end flow acceptance in a phone-sized jsdom window against a *live* workbench.
//
//   node mobile_flow.mjs http://127.0.0.1:8765     # Python backend
//   node mobile_flow.mjs http://127.0.0.1:8766     # Rust backend
//
// It loads /static/app.js unmodified, drives the DOM the way a thumb would (tap the drawer
// buttons, type, submit, tap 停止), and checks what a phone user would see: partial text kept
// after a stop, file links after a run, transcript reload on thread switch, upload chips.
// Playwright/Chromium cannot be downloaded in every environment; jsdom + Node fetch is enough
// for logic-level acceptance. Real-device visual checks are listed in docs/civil-buddy/optimize-2026-09-18.md.

import { JSDOM } from "jsdom";

const BASE = (process.argv[2] || "http://127.0.0.1:8765").replace(/\/$/, "");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const results = [];

function check(name, ok, detail = "") {
  results.push([ok ? "PASS" : "FAIL", name, detail]);
  if (!ok) failures += 1;
}

async function waitFor(fn, { timeout = 30000, every = 60, label = "condition" } = {}) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    let v;
    try {
      v = await fn();
    } catch (e) {
      v = false;
    }
    if (v) return v;
    await sleep(every);
  }
  throw new Error(`timeout waiting for ${label}`);
}

async function makePage() {
  const html = await (await fetch(BASE + "/")).text();
  const dom = new JSDOM(html, {
    url: BASE + "/",
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const w = dom.window;
  // --- phone: 390px wide, non-secure origin (no crypto.randomUUID), matchMedia mocked ---
  Object.defineProperty(w, "innerWidth", { value: 390, configurable: true });
  w.matchMedia = (q) => {
    const m = /max-width:\s*(\d+)px/.exec(q);
    return { matches: m ? 390 <= Number(m[1]) : false, media: q, addListener() {}, removeListener() {} };
  };
  if (w.crypto && "randomUUID" in w.crypto) {
    try {
      delete w.crypto.randomUUID;
    } catch (e) {
      Object.defineProperty(w.crypto, "randomUUID", { value: undefined, configurable: true });
    }
  }
  // Node's fetch/streams stand in for the browser's; relative URLs resolve against BASE.
  w.fetch = (u, init) => fetch(new URL(String(u), BASE).href, init);
  w.TextDecoder = TextDecoder;
  w.AbortController = AbortController;
  w.scrollTo = () => {};
  w.prompt = () => "";
  const errors = [];
  w.addEventListener("error", (e) => errors.push(String(e.message || e.error || e)));
  w.onunhandledrejection = (e) => errors.push("unhandled: " + String(e.reason));
  const js = await (await fetch(BASE + "/static/app.js")).text();
  // top-level const/function do not become window properties: expose the two the test inspects
  w.eval(js + "\nwindow.state = state; window.uploadFiles = uploadFiles;");
  return { dom, w, d: w.document, errors };
}

const text = (el) => (el ? el.textContent.trim() : "");

async function main() {
  const health = await (await fetch(BASE + "/api/health")).json();
  console.log(`backend=${health.capabilities?.backend} version=${health.version || "?"} threads=${health.capabilities?.threads}`);
  const { w, d, errors } = await makePage();
  const $ = (id) => d.getElementById(id);

  // ---- boot: key badge, drawers present, no js errors, thread restored/created ----
  await waitFor(() => /已配置|缺少/.test(text($("keyBadge"))) && w.state.experts.length > 0, { label: "boot" });
  check("boot without crypto.randomUUID (LAN phone)", errors.length === 0, errors.join(" | "));
  check("key badge shows configured key", text($("keyBadge")) === "已配置 API Key", text($("keyBadge")));
  const caps = w.state.caps;
  check("capabilities loaded from /api/health", !!caps && caps.upload === true);
  check("nav buttons unhidden per capability", !$("navThreads").classList.contains("hidden") && !$("navFiles").classList.contains("hidden"));
  check("local-path row hidden on phone (CSS) / present in DOM", !!d.querySelector(".local-row"));
  check("no raw fetch errors leaked into the log", !/SyntaxError|Unexpected end of JSON/.test(text($("log"))), text($("log")).slice(0, 200));

  // ---- drawers ----
  $("navExperts").click();
  check("岗位 drawer opens", d.querySelector(".rail").classList.contains("open") && $("scrim").classList.contains("on"));
  const firstExpert = d.querySelector(".exp[data-id='construction']");
  check("expert wall rendered inside drawer", !!firstExpert);
  firstExpert.click();
  check("tapping an expert summons it and closes the drawer", w.state.summoned.has("construction") && !d.querySelector(".rail").classList.contains("open"));
  $("navFiles").click();
  check("文件 drawer opens", d.querySelector(".side").classList.contains("open"));
  $("scrim").click();
  check("scrim closes drawers", !d.querySelector(".side").classList.contains("open"));

  // ---- new thread, expert run with confirm, streaming + file event ----
  $("btnNewThread").click();
  await waitFor(() => w.state.threadId && text($("log")).includes("/new"), { label: "new thread" });
  const tid1 = w.state.threadId;
  check("new thread clears the log and is remembered", w.localStorage.getItem("civil-buddy.thread") === tid1 && d.querySelectorAll(".msg").length === 0);

  $("confirmOk").checked = true;
  $("input").value = "写一份临边专项方案提纲";
  $("form").dispatchEvent(new w.Event("submit", { cancelable: true }));
  await waitFor(() => $("send").disabled && !$("stop").classList.contains("hidden"), { label: "stream started" });
  check("停止 button appears while streaming", true);
  await waitFor(() => !$("send").disabled, { label: "stream finished", timeout: 90000 });
  const bubbles = [...d.querySelectorAll(".msg.assistant .body")];
  const last = bubbles[bubbles.length - 1];
  check("assistant reply rendered", !!last && text(last).length > 0, text(last).slice(0, 80) + " || LOG: " + text($("log")).slice(-400));
  check("run produced deliverables in the 文件 panel", d.querySelectorAll("#files li").length >= 1, String(d.querySelectorAll("#files li").length));
  const dl = d.querySelector("#files li a.file-act");
  check("download link carries download attr + /api/file", !!dl && dl.hasAttribute("download") && dl.href.includes("/api/file?path="));
  const pv = [...d.querySelectorAll("#files li a.file-act")].find((a) => a.textContent === "预览");
  check("text deliverable has 预览 (inline)", !pv || pv.href.includes("inline=1"));
  const statusText = text($("log"));
  check("file event announced before done", /已落盘/.test(statusText));
  check("history alternates user/assistant", w.state.history.length === 2 && w.state.history[0].role === "user" && w.state.history[1].role === "assistant");

  // server transcript + files
  const msgs = await (await fetch(`${BASE}/api/threads/${tid1}/messages`)).json();
  check("server transcript has user+assistant rows", (msgs.messages || []).length >= 2, JSON.stringify((msgs.messages || []).map((m) => m.role)));
  const files = await (await fetch(`${BASE}/api/threads/${tid1}/files`)).json();
  check("server lists session files", (files.files || []).length >= 1);
  if (dl) {
    const r = await fetch(dl.href);
    const cd = r.headers.get("content-disposition") || "";
    check("download has RFC 5987 filename*", r.status === 200 && /filename\*=utf-8''/i.test(cd), cd);
  }

  // ---- 断流: stop mid-stream keeps partial text ----
  $("input").value = "什么是 GST";
  w.state.summoned.clear();
  $("form").dispatchEvent(new w.Event("submit", { cancelable: true }));
  await waitFor(() => {
    const b = [...d.querySelectorAll(".msg.assistant .body")].pop();
    return b && text(b).length >= 2;
  }, { label: "first tokens", timeout: 60000 });
  const partialBefore = text([...d.querySelectorAll(".msg.assistant .body")].pop());
  $("stop").click();
  await waitFor(() => !$("send").disabled, { label: "stopped" });
  const partialAfter = text([...d.querySelectorAll(".msg.assistant .body")].pop());
  check("partial text kept after 停止", partialAfter.startsWith(partialBefore.slice(0, 2)) && !/内部错误|TypeError/.test(partialAfter), partialAfter.slice(0, 60));
  check("stop leaves history alternating", w.state.history.length === 4 && w.state.history[3].role === "assistant");
  check("stop is announced, not shown as an error bubble", /已停止/.test(text($("log"))));
  await sleep(1500);
  const st1 = await (await fetch(`${BASE}/api/threads/${tid1}`)).json();
  check("server thread state after stop is cancelled/done (not stuck running)", ["cancelled", "done"].includes(st1.state), st1.state);

  // ---- 锁屏: the OS kills the socket (not the user) → server finishes detached → page re-syncs ----
  const rowsBefore = (await (await fetch(`${BASE}/api/threads/${tid1}/messages`)).json()).messages.length;
  $("input").value = "什么是 GST";
  $("form").dispatchEvent(new w.Event("submit", { cancelable: true }));
  await waitFor(() => {
    const b = [...d.querySelectorAll(".msg.assistant .body")].pop();
    return b && text(b).length >= 2;
  }, { label: "tokens before lock", timeout: 60000 });
  const nStatusBefore = d.querySelectorAll(".status-line").length;
  w.state.stream.ctrl.abort(); // iOS Safari dropping the fetch in the background looks exactly like this
  await waitFor(() => !$("send").disabled, { label: "stream torn down" });
  const newStatus = [...d.querySelectorAll(".status-line")].slice(nStatusBefore).map((p) => p.textContent).join(" | ");
  check("dropped connection is not reported as a user stop", /连接中断/.test(newStatus) && !/已停止（用户/.test(newStatus), newStatus.slice(0, 120));
  await waitFor(async () => {
    const t = await (await fetch(`${BASE}/api/threads/${tid1}`)).json();
    return t.state === "done";
  }, { label: "detached run finished on the server", timeout: 60000, every: 500 });
  const rowsAfter = (await (await fetch(`${BASE}/api/threads/${tid1}/messages`)).json()).messages;
  check("server finished the run and wrote the reply", rowsAfter.length === rowsBefore + 2 && rowsAfter[rowsAfter.length - 1].content.length >= 100);
  // simulate the phone coming back: visibilitychange → resync (poller may already have re-rendered)
  Object.defineProperty(d, "visibilityState", { value: "visible", configurable: true });
  d.dispatchEvent(new w.Event("visibilitychange"));
  await waitFor(() => {
    const b = [...d.querySelectorAll(".msg.assistant .body")].pop();
    return b && text(b).length >= 100 && !b.parentElement.classList.contains("interrupted");
  }, { label: "re-synced full reply", timeout: 30000, every: 300 });
  check("full reply shown after coming back, no longer marked interrupted", true);
  check("history in sync with server transcript", w.state.history.length === rowsAfter.length, `${w.state.history.length} vs ${rowsAfter.length}`);

  // ---- thread switch reloads transcript from server ----
  $("btnNewThread").click();
  await waitFor(() => w.state.threadId && w.state.threadId !== tid1, { label: "second thread" });
  check("second thread starts empty", d.querySelectorAll(".msg").length === 0 && w.state.history.length === 0);
  $("navThreads").click();
  await waitFor(() => d.querySelectorAll(".thread-row").length >= 2, { label: "thread list" });
  const row1 = [...d.querySelectorAll(".thread-row")].find((b) => b.title.startsWith(tid1));
  check("first thread listed with state badge", !!row1 && !!row1.querySelector(".tstate"));
  row1.click();
  await waitFor(() => w.state.threadId === tid1 && d.querySelectorAll(".msg.user").length >= 2, { label: "switch back" });
  check("switching back re-renders the transcript", d.querySelectorAll(".msg.user").length === 3 && w.state.history.length >= 5);
  check("files panel repopulated from server on switch", d.querySelectorAll("#files li").length >= 1);
  check("drawer closed after choosing a thread", !d.querySelector(".rail").classList.contains("open"));

  // ---- upload (XHR + FormData) ----
  const file = new w.File(["临边高度 [A001] 待填"], "现场说明.txt", { type: "text/plain" });
  const beforeN = w.state.attachments.length;
  await w.uploadFiles([file]);
  check("upload adds an attachment chip", w.state.attachments.length === beforeN + 1 && !!d.querySelector("#attaches .chip"), text(d.querySelector("#attaches")));
  const att = await (await fetch(`${BASE}/api/attachments?session_id=${encodeURIComponent(w.state.session)}`)).json();
  check("attachment visible on the server for this session", (att.files || []).some((f) => f.name === "现场说明.txt"));

  // ---- cleanup ----
  for (const t of [tid1, w.state.threadId]) {
    await fetch(`${BASE}/api/threads/${t}`, { method: "DELETE" }).catch(() => {});
  }
  check("no uncaught JS errors during the flow", errors.length === 0, errors.join(" | "));

  for (const [s, n, dtl] of results) console.log(`${s}  ${n}${dtl ? "  — " + dtl : ""}`);
  console.log(`\n${results.length - failures}/${results.length} passed (${BASE})`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error("E2E crashed:", e);
  for (const [s, n, dtl] of results) console.log(`${s}  ${n}${dtl ? "  — " + dtl : ""}`);
  process.exit(2);
});
