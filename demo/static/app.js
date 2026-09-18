// Civil Buddy workbench — shared by the Rust (workbench/) and Python (demo/) backends.
// Everything optional is gated on /api/health.capabilities so a backend that lacks a
// route hides the button instead of leaking a 404 into the conversation.

function uid() {
  if (window.crypto && typeof crypto.randomUUID === "function") {
    // secure contexts only (https / localhost); http://192.168.x.x on a phone lands in the fallback
    return crypto.randomUUID().replace(/-/g, "").slice(0, 12);
  }
  let s = "";
  while (s.length < 12) s += Math.floor(Math.random() * 16).toString(16);
  return s;
}

const state = {
  experts: [],
  catalog: null,
  summoned: new Set(),
  history: [],
  session: uid(),
  modelName: "",
  attachments: [],
  threadId: "",
  threads: [],
  caps: {},
  policy: { sandbox: "workspace-write", approval: "on-request" },
  context: {
    limit: 1000000,
    reserve: 16384,
    compress_pct: 70,
    warn_pct: 50,
    keep_recent: 4,
    compress_at: 86732,
  },
  stream: null, // { ctrl, bodyEl, acc } while a reply is streaming
  foregroundThread: "", // thread whose latest run was streamed live here (its poller must not re-render it)
  stickToBottom: true,
  filesSeen: new Set(),
  pollers: {},
};

const $ = (id) => document.getElementById(id);
const LS_THREAD = "civil-buddy.thread";
const TEXT_PREVIEW = /\.(md|txt|csv|json|log)$/i;

// ---------- boot ----------

async function boot() {
  let health = {};
  try {
    health = await getJson("/api/health");
  } catch (e) {
    addStatus(`工作台不可达：${e.message || e}`);
  }
  state.caps = health.capabilities || legacyCaps(health);
  if (state.caps.auth && !document.cookie.includes(`${TOKEN_COOKIE}=`)) {
    await askToken("这个工作台开了访问口令");
  }
  const badge = $("keyBadge");
  if (health.has_key || health.deepseek) {
    badge.textContent = "已配置 API Key";
    badge.className = "pill ok";
  } else {
    badge.textContent = "缺少 API Key";
    badge.className = "pill warn";
  }
  if (health.model) state.modelName = health.model;
  if (health.model && $("modelBadge")) {
    const lim = health.context && health.context.limit;
    $("modelBadge").textContent = lim ? `模型 ${health.model} · 上下文 ${lim}` : `模型 ${health.model}`;
  }
  if (health.harness && $("harnessBadge")) {
    $("harnessBadge").textContent =
      health.harness.summoned_default === "chat" ? "Harness 能聊能跑" : `Harness ${health.harness.default_mode || "steps"}`;
  }
  if (health.context) state.context = { ...state.context, ...health.context };
  applyCaps();
  paintContext(estimateLocalContext());
  await reloadCatalog();
  await loadJobRoot();
  if (state.caps.config) await loadPolicy();
  if (state.caps.threads) {
    await loadThreads();
    const saved = localStorage.getItem(LS_THREAD);
    if (saved) {
      const t = state.threads.find((x) => x.thread_id === saved);
      if (t) await selectThread(t, { quiet: true });
    }
  }
}

function legacyCaps(health) {
  // Backends without a capabilities block: assume the Rust surface (upload/local/firm) and probe nothing else.
  return { backend: health.product === "civil-codex" ? "python" : "rust", upload: true, local: true, firm: true, threads: false };
}

function applyCaps() {
  const c = state.caps;
  show($("btnUpload"), !!c.upload);
  show($("btnLocal"), !!c.local);
  show($("btnFirm"), !!c.firm);
  show(document.querySelector(".local-row"), !!(c.local || c.firm));
  show(document.querySelector(".thread-box"), !!c.threads);
  show($("btnBg"), !!c.threads);
  show($("navThreads"), !!c.threads);
  if (!c.threads) state.threadId = "";
}

function show(el, on) {
  if (el) el.classList.toggle("hidden", !on);
}

// ---------- http ----------

// Optional shared secret (CIVIL_TOKEN on the server). Kept in a cookie so plain download
// links and XHR uploads carry it too; asked for once, on the first 401.
const TOKEN_COOKIE = "cb_token";
let tokenPromptOpen = false;

function setToken(tok) {
  const v = encodeURIComponent((tok || "").trim());
  document.cookie = `${TOKEN_COOKIE}=${v}; path=/; max-age=${60 * 60 * 24 * 30}; SameSite=Lax`;
}

async function askToken(reason) {
  if (tokenPromptOpen) return false;
  tokenPromptOpen = true;
  try {
    const tok = window.prompt(`${reason || "这个工作台需要访问口令"}（CIVIL_TOKEN）`);
    if (!tok) return false;
    setToken(tok);
    return true;
  } finally {
    tokenPromptOpen = false;
  }
}

async function fetchApi(url, init, retry = true) {
  const r = await fetch(url, init);
  if (r.status === 401 && retry && (await askToken("口令缺失或不对"))) return fetchApi(url, init, false);
  return r;
}

async function getJson(url) {
  const r = await fetchApi(url);
  if (!r.ok) throw new Error(await apiError(r));
  return r.json();
}

async function postJson(url, body, method = "POST") {
  const r = await fetchApi(url, {
    method,
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) throw new Error(await apiError(r));
  return r.json();
}

async function apiError(res) {
  const t = await res.text();
  try {
    const j = JSON.parse(t);
    if (typeof j.detail === "string") return j.detail;
    if (Array.isArray(j.detail)) return j.detail.map((d) => d.msg || JSON.stringify(d)).join("; ");
  } catch (_) {}
  return t || `${res.status} ${res.statusText}`;
}

// ---------- policy / job root ----------

async function loadPolicy() {
  try {
    const cfg = await getJson("/api/config");
    state.policy = cfg;
    if ($("sandboxBadge")) $("sandboxBadge").textContent = `sandbox ${cfg.sandbox || ""}`;
    if ($("approvalBadge")) $("approvalBadge").textContent = `approval ${cfg.approval || ""}`;
  } catch (e) {
    /* optional */
  }
}

async function loadJobRoot() {
  try {
    const job = await getJson("/api/job");
    if (!job.granted) {
      addStatus(job.hint || "未授权作业根。设 CIVIL_JOB_ROOT 后可直接读本机文件。");
      return;
    }
    addStatus(`作业根 ${job.root} · 已看到 ${(job.files || []).length} 个文件，说「写一份」会自动抄，不必再上传。`);
    for (const f of job.files || []) {
      if (state.attachments.some((a) => a.id === `job:${f.name}`)) continue;
      state.attachments.push({ id: `job:${f.name}`, name: f.name, layer: "job" });
    }
    renderAttaches();
  } catch (e) {
    /* optional */
  }
}

// ---------- threads (对话) ----------

async function loadThreads() {
  const box = $("threadList");
  if (!box || !state.caps.threads) return;
  try {
    const data = await getJson("/api/threads");
    state.threads = data.threads || [];
  } catch (e) {
    state.threads = [];
  }
  box.innerHTML = "";
  for (const t of state.threads) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "thread-row" + (t.thread_id === state.threadId ? " on" : "");
    const st = document.createElement("span");
    st.className = `tstate ${t.state || ""}`;
    st.textContent = threadStateLabel(t.state);
    const title = document.createElement("span");
    title.className = "ttitle";
    title.textContent = t.title || t.thread_id;
    b.append(st, title);
    b.title = `${t.thread_id} · ${t.n_messages || 0} 条`;
    b.addEventListener("click", () => selectThread(t));
    box.appendChild(b);
    if (t.state === "running") pollThread(t.thread_id);
  }
}

function threadStateLabel(s) {
  return { running: "跑", done: "完", failed: "败", stale: "停", cancelled: "取消", waiting_hitl: "待确认", idle: "新" }[s] || s || "";
}

async function selectThread(t, opts = {}) {
  if (state.stream) stopStreaming("切换对话");
  state.threadId = t.thread_id;
  state.session = t.session_id || t.thread_id;
  state.foregroundThread = "";
  localStorage.setItem(LS_THREAD, state.threadId);
  closeDrawers();
  clearConversation();
  state.attachments = state.attachments.filter((a) => String(a.id).startsWith("job:"));
  renderAttaches();
  let rows = [];
  if (state.caps.thread_messages) {
    try {
      const data = await getJson(`/api/threads/${encodeURIComponent(t.thread_id)}/messages`);
      rows = data.messages || [];
    } catch (e) {
      addStatus(`读取对话失败：${e.message || e}`);
    }
  }
  for (const m of rows) {
    if (m.role === "user") {
      addMsg("user", "你", m.content);
    } else {
      const body = addMsg("assistant", skillWho(m.expert || "", m.expert ? "given" : ""), m.content);
      if (m.error) {
        body.parentElement.classList.add("interrupted");
        addStatus(`⚠ 这条回复中断：${m.error}`);
      }
      renderCites(m.citations || []);
      renderFiles(m.deliverables || []);
    }
    state.history.push({ role: m.role, content: m.content });
  }
  if (state.caps.thread_files) {
    try {
      const data = await getJson(`/api/threads/${encodeURIComponent(t.thread_id)}/files`);
      renderFiles(data.files || []);
    } catch (e) {
      /* optional */
    }
  }
  if (state.caps.attachments) {
    try {
      const data = await getJson(`/api/attachments?session_id=${encodeURIComponent(state.session)}`);
      for (const f of data.files || []) if (!state.attachments.some((a) => a.id === f.id)) state.attachments.push(f);
      renderAttaches();
    } catch (e) {
      /* optional */
    }
  }
  paintContext(estimateLocalContext());
  if (!opts.quiet) addStatus(`已切到对话 ${t.title || t.thread_id}（${rows.length} 条）`);
  if (t.state === "running") pollThread(t.thread_id);
  await loadThreads();
}

function clearConversation() {
  state.history = [];
  state.filesSeen = new Set();
  $("log").innerHTML = "";
  $("cites").innerHTML = "";
  $("files").innerHTML = "";
  if ($("navFiles")) $("navFiles").textContent = "文件";
}

async function ensureThread(title) {
  if (state.threadId || !state.caps.threads) return state.threadId;
  const data = await postJson("/api/threads", { title: (title || "新对话").slice(0, 40) });
  state.threadId = data.thread_id;
  state.session = data.session_id || data.thread_id;
  localStorage.setItem(LS_THREAD, state.threadId);
  loadThreads();
  return state.threadId;
}

async function newThread() {
  if (state.stream) stopStreaming("新建对话");
  const data = await postJson("/api/threads", { title: "新对话" });
  await selectThread({ thread_id: data.thread_id, session_id: data.session_id, title: data.title }, { quiet: true });
  addStatus(`/new ${data.thread_id}`);
  $("input").focus();
}

function pollThread(threadId) {
  if (state.pollers[threadId]) return;
  state.pollers[threadId] = setInterval(async () => {
    let t;
    try {
      t = await getJson(`/api/threads/${encodeURIComponent(threadId)}`);
    } catch (e) {
      clearInterval(state.pollers[threadId]);
      delete state.pollers[threadId];
      return;
    }
    if (t.state === "running" || t.running) return;
    clearInterval(state.pollers[threadId]);
    delete state.pollers[threadId];
    await loadThreads();
    if (threadId === state.threadId && !state.stream && state.foregroundThread !== threadId) {
      // a background run on the thread we are looking at finished: pull its transcript + files
      const cur = state.threads.find((x) => x.thread_id === threadId) || t;
      await selectThread(cur, { quiet: true });
      addStatus(`后台任务 ${threadStateLabel(t.state)}：${(t.last_reply || "").slice(0, 120)}`);
    } else if (threadId !== state.threadId) {
      addStatus(`后台对话「${t.title || threadId}」${threadStateLabel(t.state)}，点左侧查看。`);
    }
  }, 3000);
}

async function runBackground(text) {
  const data = await postJson("/api/threads", { text, background: true, confirm_ok: $("confirmOk").checked });
  addStatus(`并行 thread ${data.thread_id} ${data.state || "running"} · 完成后会提示`);
  await loadThreads();
  pollThread(data.thread_id);
}

if ($("btnNewThread")) $("btnNewThread").addEventListener("click", () => newThread().catch((e) => addStatus(String(e.message || e))));
if ($("btnBg")) {
  $("btnBg").addEventListener("click", () => {
    const text = $("input").value.trim();
    if (!text) {
      addStatus("/bg 先在输入框写任务");
      return;
    }
    $("input").value = "";
    runBackground(text).catch((e) => addStatus(`并行失败：${e.message || e}`));
  });
}

// ---------- slash commands ----------

async function handleSlash(message) {
  const parts = message.slice(1).split(/\s+/);
  const cmd = (parts[0] || "").toLowerCase();
  const arg = parts.slice(1).join(" ");
  if (cmd === "skills") {
    const data = await getJson("/api/skills");
    const q = arg.toLowerCase();
    const rows = (data.skills || []).filter((s) => !q || `${s.name} ${s.description}`.toLowerCase().includes(q));
    addStatus(`${rows.length} skills`);
    addMsg("assistant", "skills", rows.slice(0, 20).map((s) => `$${s.name}  ${s.description}`).join("\n"));
    return true;
  }
  if (cmd === "new") {
    await newThread();
    return true;
  }
  if (cmd === "bg") {
    await runBackground(arg);
    return true;
  }
  if (cmd === "stop") {
    stopStreaming("用户 /stop");
    return true;
  }
  if (cmd === "sandbox" || cmd === "approvals" || cmd === "approval") {
    const body = cmd === "sandbox" ? { sandbox: arg } : { approval: arg };
    if (arg) await postJson("/api/config", body);
    await loadPolicy();
    addStatus(`${cmd} ${arg || (cmd === "sandbox" ? state.policy.sandbox : state.policy.approval)}`);
    return true;
  }
  if (cmd === "threads") {
    await loadThreads();
    addStatus("threads 已刷新");
    if (isNarrow()) openDrawer("rail");
    return true;
  }
  if (cmd === "files") {
    openDrawer("side");
    return true;
  }
  if (cmd === "help") {
    addMsg(
      "assistant",
      "help",
      "/skills /new /bg /stop /threads /files /sandbox /approvals\n全企业可问任意专家。确认句：我明白，将由持证人员签认"
    );
    return true;
  }
  return false;
}

// ---------- catalog / summon ----------

async function reloadCatalog() {
  const cat = await getJson("/api/catalog");
  state.catalog = cat;
  state.experts = cat.experts;
  renderWall(cat);
  renderSummon();
  if (window.studioOnCatalog) window.studioOnCatalog(cat);
}

window.reloadCatalog = reloadCatalog;

function renderWall(cat) {
  const wall = $("wall");
  if (!wall || !cat) return;
  const q = (($("skillQ") && $("skillQ").value) || "").trim().toLowerCase();
  wall.innerHTML = "";
  for (const c of cat.categories) {
    const experts = cat.experts.filter((x) => {
      if (x.category !== c.id) return false;
      if (!q) return true;
      const blob = `${x.name} ${x.id} ${(x.aliases || []).join(" ")} ${x.delivers || ""}`.toLowerCase();
      return blob.includes(q);
    });
    if (!experts.length) continue;
    const h = document.createElement("div");
    h.className = "cat";
    h.textContent = c.name;
    wall.appendChild(h);
    for (const e of experts) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "exp";
      b.dataset.id = e.id;
      const size = e.kb_label ? ` · 本岗 ${e.kb_label}` : "";
      const mark = e.over_limit ? " over" : "";
      b.innerHTML = `<b>${escapeHtml(e.name)}</b><span class="${mark}">${escapeHtml(e.delivers || "")}${escapeHtml(size)}</span>`;
      b.addEventListener("click", () => toggle(e.id));
      wall.appendChild(b);
    }
  }
  renderSummon();
}

function toggle(id) {
  if (state.summoned.has(id)) state.summoned.delete(id);
  else state.summoned.add(id);
  renderSummon();
  refreshKb();
  if (isNarrow()) closeDrawers();
}

function renderSummon() {
  document.querySelectorAll(".exp").forEach((el) => {
    el.classList.toggle("on", state.summoned.has(el.dataset.id));
  });
  const names = [...state.summoned].map((id) => {
    const e = state.experts.find((x) => x.id === id);
    return e ? `${e.category_name}/${e.name}` : id;
  });
  $("summonBar").innerHTML = names.length
    ? `当前：<em>${escapeHtml(names.join(" · "))}</em>`
    : "当前：<em>未点名岗位</em> · 直接下任务即可";
  if ($("navExperts")) $("navExperts").textContent = names.length ? `岗位 ${names.length}` : "岗位";
}

if ($("skillQ")) $("skillQ").addEventListener("input", () => state.catalog && renderWall(state.catalog));

$("clearExperts").addEventListener("click", () => {
  state.summoned.clear();
  renderSummon();
  $("kblist").innerHTML = "";
});

document.querySelectorAll("[data-fill]").forEach((btn) => {
  btn.addEventListener("click", () => {
    $("input").value = btn.dataset.fill;
    $("input").focus();
  });
});

async function refreshKb() {
  const box = $("kblist");
  box.innerHTML = "";
  for (const id of state.summoned) {
    let data;
    try {
      data = await getJson(`/api/kb/${encodeURIComponent(id)}`);
    } catch (e) {
      continue;
    }
    for (const f of data.files || []) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "kb-link";
      const label = f.display || f.title || (f.path || "").split("/").pop();
      const layer = f.layer_label || layerName(f.layer);
      const sz = f.bytes != null ? ` · ${fmtBytes(f.bytes)}` : "";
      btn.innerHTML = `<span class="layer ${escapeHtml(f.layer || "")}">${escapeHtml(layer)}</span>${escapeHtml(label)}<span class="kb-size">${escapeHtml(sz)}</span>`;
      btn.title = f.path || "";
      btn.addEventListener("click", () => window.openStudio && window.openStudio(f.path, f.layer === "expert" ? id : null));
      li.appendChild(btn);
      box.appendChild(li);
    }
  }
}

function layerName(layer) {
  if (layer === "expert") return "本岗知识";
  if (layer === "category") return "大类共享";
  if (layer === "web") return "网上检索";
  if (layer === "upload") return "用户上传";
  if (layer === "job") return "作业根";
  return "公司规则";
}

// ---------- formatting helpers ----------

function fmtBytes(n) {
  const x = Number(n) || 0;
  if (x < 1024) return `${x} B`;
  if (x < 1024 * 1024) return `${(x / 1024).toFixed(1)} KB`;
  return `${(x / (1024 * 1024)).toFixed(2)} MB`;
}

function fmtNum(n) {
  return Number(n || 0).toLocaleString("zh-CN");
}

function isCjk(ch) {
  const c = ch.codePointAt(0);
  return (c >= 0x4e00 && c <= 0x9fff) || (c >= 0x3400 && c <= 0x4dbf) || (c >= 0xf900 && c <= 0xfaff);
}

function estimateTokens(text) {
  let cjk = 0;
  let other = 0;
  for (const ch of String(text || "")) {
    if (/\s/.test(ch)) continue;
    if (isCjk(ch)) cjk += 1;
    else other += 1;
  }
  return cjk + Math.ceil(other / 4);
}

function estimateLocalContext() {
  const policy = state.context;
  const limit = policy.limit || 1000000;
  const reserve = policy.reserve || 4096;
  const usable = Math.max(1, limit - reserve);
  let used = 0;
  for (const m of state.history) used += estimateTokens(m.role) + estimateTokens(m.content) + 4;
  const draft = $("input") ? $("input").value : "";
  if (draft) used += estimateTokens(draft) + 8;
  const pct = Math.min(100, Math.round((Math.min(used, usable) * 100) / usable));
  const compressAt = policy.compress_at || Math.floor((usable * (policy.compress_pct || 70)) / 100);
  const keep = policy.keep_recent || 4;
  let zone = "room";
  if (pct >= 90) zone = "full";
  else if (pct >= (policy.compress_pct || 70)) zone = "compact";
  else if (pct >= (policy.warn_pct || 50)) zone = "warn";
  let note;
  if (pct >= 90) {
    note = `上下文快满（约 ${fmtNum(used)} / ${fmtNum(limit)}，${pct}%）。再发可能只留最近 ${keep} 条原文。`;
  } else if (pct >= (policy.warn_pct || 50)) {
    note = `已过半（约 ${fmtNum(used)} / ${fmtNum(limit)}，${pct}%）。用到 ${fmtNum(compressAt)} token（${policy.compress_pct || 70}%）会把更早对话压成摘要，近 ${keep} 条原文保留。`;
  } else {
    note = `还很宽裕（约 ${fmtNum(used)} / ${fmtNum(limit)}，${pct}%）。用到 ${fmtNum(compressAt)} token（${policy.compress_pct || 70}%）会压缩更早对话，近 ${keep} 条原文保留。`;
  }
  return { used, limit, usable, pct, zone, note, estimated: true, compress_at: compressAt, keep_recent: keep };
}

function paintContext(ctx) {
  if (!ctx) return;
  const bar = $("ctxBar");
  const fill = $("ctxFill");
  const text = $("ctxText");
  if (!bar || !fill || !text) return;
  const pct = Math.max(0, Math.min(100, Number(ctx.pct) || 0));
  fill.style.width = `${Math.max(pct, pct > 0 ? 2 : 0)}%`;
  bar.dataset.zone = ctx.zone || "room";
  text.textContent = ctx.note ? ctx.note : `上下文 ${fmtNum(ctx.used)} / ${fmtNum(ctx.limit)} · ${pct}%`;
}

// ---------- log ----------

function addMsg(role, who, text) {
  const div = document.createElement("div");
  div.className = `msg ${role}`;
  div.innerHTML = `<div class="who"></div><div class="body"></div>`;
  div.querySelector(".who").textContent = who;
  div.querySelector(".body").textContent = text;
  $("log").appendChild(div);
  scrollLog(true);
  return div.querySelector(".body");
}

function addStatus(text) {
  const p = document.createElement("p");
  p.className = "status-line";
  p.textContent = text;
  $("log").appendChild(p);
  scrollLog(true);
}

function scrollLog(force) {
  const log = $("log");
  if (force || state.stickToBottom) log.scrollTop = log.scrollHeight;
}

$("log").addEventListener("scroll", () => {
  const log = $("log");
  state.stickToBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 48;
});

function skillWho(id, source) {
  if (!id) return "未点名岗位";
  const name = (state.experts.find((e) => e.id === id) || {}).name || id;
  const how = source === "given" ? "显式" : source === "matched" ? "规则选用" : "未点名";
  return `$${id} · ${name} · ${how}`;
}

function namesOrPlain() {
  if (!state.summoned.size) return "未点名岗位";
  return [...state.summoned].map((id) => skillWho(id, "given")).join(" / ");
}

// ---------- send / stream ----------

$("form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const message = $("input").value.trim();
  if (!message) return;
  const welcome = document.querySelector(".welcome");
  if (welcome) welcome.remove();
  if (message.startsWith("/")) {
    $("input").value = "";
    addMsg("user", "你", message);
    try {
      const ok = await handleSlash(message);
      if (!ok) addStatus(`未知命令 ${message}。/help`);
    } catch (err) {
      addStatus(String(err.message || err));
    }
    return;
  }
  if (state.stream) {
    addStatus("上一条还在生成，先点停止或等它结束。");
    return;
  }
  $("input").value = "";
  addMsg("user", "你", message);
  state.history.push({ role: "user", content: message });
  paintContext(estimateLocalContext());
  const bodyEl = addMsg("assistant", namesOrPlain(), "");
  setStreaming(true, bodyEl);
  try {
    await ensureThread(message);
    await streamChat(message, bodyEl);
  } catch (err) {
    onStreamBroken(bodyEl, err);
  } finally {
    setStreaming(false);
    if (state.caps.threads) loadThreads();
  }
});

function setStreaming(on, bodyEl) {
  state.stream = on ? { ctrl: typeof AbortController !== "undefined" ? new AbortController() : null, bodyEl, acc: "" } : null;
  $("send").disabled = on;
  show($("stop"), on);
}

function stopStreaming(reason) {
  const s = state.stream;
  if (!s) return;
  if (s.ctrl) s.ctrl.abort();
  if (state.caps.cancel && state.threadId) {
    postJson(`/api/threads/${encodeURIComponent(state.threadId)}/cancel`, {}).catch(() => {});
  }
  addStatus(`已停止（${reason || "用户"}）。已生成的内容保留。`);
}

if ($("stop")) $("stop").addEventListener("click", () => stopStreaming("用户点击停止"));

function onStreamBroken(bodyEl, err) {
  const s = state.stream;
  const acc = s ? s.acc : "";
  const aborted = err && (err.name === "AbortError" || /abort/i.test(String(err.message || "")));
  const wrap = bodyEl.parentElement;
  if (wrap) wrap.classList.add("interrupted");
  if (!acc) bodyEl.textContent = aborted ? "（已停止）" : `（未收到回复：${err.message || err}）`;
  else if (!aborted) addStatus(`⚠ 连接中断：${err.message || err}。上面的内容已保留；文件若已落盘，切回本对话即可看到。`);
  state.history.push({ role: "assistant", content: acc || "（这条回复没有收到）" });
  paintContext(estimateLocalContext());
}

async function streamChat(message, bodyEl) {
  const s = state.stream;
  const res = await fetchApi("/api/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
    signal: s && s.ctrl ? s.ctrl.signal : undefined,
    body: JSON.stringify({
      message,
      history: state.history.slice(0, -1),
      expert_ids: [...state.summoned],
      confirm_ok: $("confirmOk").checked,
      session_id: state.session,
      thread_id: state.threadId,
      attachments: state.attachments.filter((a) => !String(a.id || "").startsWith("job:") && !a.uploading).map((a) => a.id),
    }),
  });
  if (!res.ok) throw new Error(await apiError(res));
  if (!res.body) throw new Error("浏览器不支持流式读取");
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  let curBody = bodyEl;
  let finished = false;
  const seen = new Set();

  const handle = (name, data) => {
    if (name === "context") {
      paintContext(data);
      if (data.thread_id && data.thread_id !== state.threadId) {
        // server did not know our thread (e.g. demo/out wiped) and opened a new one: follow it
        state.threadId = data.thread_id;
        localStorage.setItem(LS_THREAD, state.threadId);
      }
      if (data.session_id) state.session = data.session_id;
      state.foregroundThread = state.threadId;
      return;
    }
    if (name === "status") {
      if (data.phase === "summon" && s.acc) {
        state.history.push({ role: "assistant", content: s.acc });
        s.acc = "";
        curBody = addMsg("assistant", skillWho(data.expert || "", "given"), "");
        s.bodyEl = curBody;
      }
      addStatus(data.text || "");
      return;
    }
    if (name === "token") {
      s.acc += data.text || "";
      curBody.textContent = s.acc;
      scrollLog(false);
      return;
    }
    if (name === "reset") {
      s.acc = "";
      curBody.textContent = "";
      return;
    }
    if (name === "file") {
      renderFiles(data.deliverables || []);
      addStatus(`已落盘：${(data.deliverables || []).map((f) => f.name).join("、")}（文件栏可下载）`);
      return;
    }
    if (name === "error") {
      // keep whatever was streamed; show the failure as a status line, not as the reply
      if (!s.acc && data.partial_text) {
        s.acc = data.partial_text;
        curBody.textContent = s.acc;
      }
      renderFiles(data.deliverables || []);
      renderCites(data.citations || []);
      const wrap = curBody.parentElement;
      if (wrap) wrap.classList.add("interrupted");
      addStatus(`⚠ 中断：${data.text || "error"}${data.recoverable ? "（已生成的内容和文件已保留，可直接重发）" : ""}`);
      if (!s.acc) curBody.textContent = "（这条回复没有正文）";
      state.history.push({ role: "assistant", content: s.acc || "（这条回复中断）" });
      s.acc = "";
      finished = true;
      return;
    }
    if (name === "done") {
      s.acc = data.text || s.acc;
      curBody.textContent = s.acc || (data.stopped ? "（已停止）" : "");
      const whoEl = curBody.parentElement && curBody.parentElement.querySelector(".who");
      if (whoEl) whoEl.textContent = skillWho(data.skill || data.expert || "", data.skill_source || "");
      if (s.acc) state.history.push({ role: "assistant", content: s.acc });
      s.acc = "";
      paintContext(data.context || estimateLocalContext());
      renderCites(data.citations || []);
      renderFiles(data.deliverables || []);
      finished = true;
    }
  };

  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    const parts = buf.split("\n\n");
    buf = parts.pop() || "";
    for (const block of parts) {
      let dataLine = "";
      let id = "";
      let eventName = "message";
      for (const line of block.split("\n")) {
        if (line.startsWith(":")) continue; // heartbeat comment
        if (line.startsWith("event: ")) eventName = line.slice(7).trim();
        else if (line.startsWith("data: ")) dataLine += line.slice(6);
        else if (line.startsWith("id: ")) id = line.slice(4).trim();
      }
      if (!dataLine) continue;
      if (id && seen.has(id)) continue;
      if (id) seen.add(id);
      let data;
      try {
        data = JSON.parse(dataLine);
      } catch (e) {
        addStatus("（跳过一帧无法解析的数据）");
        continue;
      }
      handle(eventName, data);
    }
  }
  if (!finished) throw new Error("流在结束前断开");
}

// ---------- side panel: cites / files ----------

function renderCites(cites) {
  const box = $("cites");
  for (const c of cites || []) {
    const li = document.createElement("li");
    const title = c.display || c.title || (c.path || "").split("/").pop();
    const layer = c.layer_label || layerName(c.layer);
    li.title = c.path || "";
    li.innerHTML = `<span class="layer ${escapeHtml(c.layer || "")}">${escapeHtml(layer)}</span><b>${escapeHtml(title)}</b><br>${escapeHtml(c.snippet || c.path || "")}`;
    box.prepend(li);
  }
}

function renderFiles(files) {
  const box = $("files");
  let added = 0;
  for (const f of files || []) {
    const key = f.path || `${f.expert}/${f.name}`;
    if (!key || state.filesSeen.has(key)) continue;
    state.filesSeen.add(key);
    added += 1;
    const li = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = `${f.expert ? f.expert + " · " : ""}${f.name}${f.bytes != null ? " · " + fmtBytes(f.bytes) : ""}`;
    const dl = document.createElement("a");
    dl.href = `/api/file?path=${encodeURIComponent(f.path)}`;
    dl.setAttribute("download", f.name || "");
    dl.textContent = "下载";
    dl.className = "file-act";
    li.append(label, dl);
    if (TEXT_PREVIEW.test(f.name || "")) {
      const pv = document.createElement("a");
      pv.href = `/api/file?path=${encodeURIComponent(f.path)}&inline=1`;
      pv.target = "_blank";
      pv.rel = "noopener";
      pv.textContent = "预览";
      pv.className = "file-act";
      li.appendChild(pv);
    }
    box.prepend(li);
  }
  if (added && $("navFiles")) $("navFiles").textContent = `文件 ${state.filesSeen.size}`;
}

function escapeHtml(s) {
  return String(s == null ? "" : s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

if ($("input")) $("input").addEventListener("input", () => paintContext(estimateLocalContext()));

// ---------- attachments ----------

function renderAttaches() {
  const box = $("attaches");
  if (!box) return;
  box.innerHTML = "";
  for (const f of state.attachments) {
    const chip = document.createElement("span");
    chip.className = "chip" + (f.uploading ? " uploading" : "");
    const kb = f.bytes != null ? fmtBytes(f.bytes) : "";
    const prog = f.uploading ? ` · 上传 ${f.progress || 0}%` : "";
    chip.textContent = `${f.name || f.id} · ${kb}${prog}`;
    if (!f.uploading) {
      const x = document.createElement("button");
      x.type = "button";
      x.textContent = "×";
      x.setAttribute("aria-label", "移除附件");
      x.addEventListener("click", () => {
        state.attachments = state.attachments.filter((a) => a.id !== f.id);
        renderAttaches();
      });
      chip.appendChild(x);
    }
    box.appendChild(chip);
  }
}

function uploadOne(file) {
  // XHR instead of fetch: phones on slow radios need a progress number, fetch cannot report upload progress
  return new Promise((resolve, reject) => {
    const tmp = { id: `tmp:${uid()}`, name: file.name, bytes: file.size, uploading: true, progress: 0 };
    state.attachments.push(tmp);
    renderAttaches();
    const fd = new FormData();
    fd.append("session_id", state.session);
    fd.append("file", file, file.name);
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/api/upload");
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) {
        tmp.progress = Math.round((e.loaded * 100) / e.total);
        renderAttaches();
      }
    };
    const finish = () => {
      state.attachments = state.attachments.filter((a) => a.id !== tmp.id);
    };
    xhr.onload = () => {
      finish();
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText));
        } catch (e) {
          reject(new Error("服务器返回无法解析"));
        }
      } else {
        let msg = xhr.statusText || `HTTP ${xhr.status}`;
        try {
          const j = JSON.parse(xhr.responseText);
          if (typeof j.detail === "string") msg = j.detail;
        } catch (e) {}
        if (xhr.status === 401) askToken("上传需要口令，填好后再传一次");
        reject(new Error(msg));
      }
    };
    xhr.onerror = () => {
      finish();
      reject(new Error("网络错误"));
    };
    xhr.send(fd);
  });
}

async function uploadFiles(fileList) {
  const files = [...fileList];
  if (!files.length) return;
  if (!state.caps.upload) {
    addStatus("这个后端不支持上传；请用作业根（CIVIL_JOB_ROOT）。");
    return;
  }
  for (const file of files) {
    try {
      const data = await uploadOne(file);
      for (const f of data.files || []) {
        if (!state.attachments.some((a) => a.id === f.id)) state.attachments.push(f);
      }
      for (const e of data.errors || []) addStatus(`上传提示：${e}`);
    } catch (e) {
      addStatus(`上传失败 ${file.name}：${e.message || e}`);
    }
    renderAttaches();
  }
}

async function importLocalPath() {
  const path = $("localPath") ? $("localPath").value.trim() : "";
  if (!path) {
    addStatus("请填写工作台电脑上的完整路径（不是手机上的）。不要填 D:\\layout。");
    return;
  }
  const data = await postJson("/api/local", { session_id: state.session, path });
  for (const f of data.files || []) if (!state.attachments.some((a) => a.id === f.id)) state.attachments.push(f);
  renderAttaches();
  addStatus(`已导入本机 ${(data.files || []).length} 个文件${state.caps.firm ? "，可点成套投标" : ""}。`);
}

async function runFirmBid() {
  const path = $("localPath") ? $("localPath").value.trim() : "";
  const brief = $("input") ? $("input").value.trim() : "";
  addStatus("Harness steps：parse → qa → outline → price …");
  const data = await postJson("/api/firm/bid", {
    session_id: state.session,
    project_name: brief.slice(0, 40) || "未命名投标项目",
    path,
    brief,
    confirm_ok: $("confirmOk") ? $("confirmOk").checked : false,
    jurisdiction: "SG",
  });
  const body = addMsg("assistant", "一人公司", "");
  const hitl = data.hitl && data.hitl.pending ? "HITL 待确认（专项未出施工草稿）" : "HITL 未挡";
  const lines = [
    `mode=${data.mode || "steps"} · run ${data.run_id || ""} · ${hitl}`,
    `${data.project || "成套"} · 作业目录 ${data.job_dir || ""}`,
    `illegal_tool_calls=${data.illegal_tool_calls ?? 0}`,
    ...(data.steps || []).map((s) => `${s.name}: ${s.tool} legal=${s.legal} ok=${s.ok}`),
    ...(data.notes || []),
    "文件栏可下载各份草稿。这不是可提交标书。",
  ];
  body.textContent = lines.join("\n");
  renderFiles(data.files || []);
  addStatus("成套已落盘。");
}

if ($("btnLocal")) $("btnLocal").addEventListener("click", () => importLocalPath().catch((e) => addStatus(`导入失败：${e.message || e}`)));
if ($("btnFirm")) $("btnFirm").addEventListener("click", () => runFirmBid().catch((e) => addStatus(`成套失败：${e.message || e}`)));

if ($("btnUpload") && $("filePick")) {
  $("btnUpload").addEventListener("click", () => $("filePick").click());
  $("filePick").addEventListener("change", async (ev) => {
    await uploadFiles(ev.target.files);
    ev.target.value = "";
  });
}

const composer = document.querySelector(".composer");
if (composer) {
  composer.addEventListener("dragover", (ev) => {
    ev.preventDefault();
    composer.classList.add("drop");
  });
  composer.addEventListener("dragleave", () => composer.classList.remove("drop"));
  composer.addEventListener("drop", async (ev) => {
    ev.preventDefault();
    composer.classList.remove("drop");
    if (ev.dataTransfer && ev.dataTransfer.files.length) await uploadFiles(ev.dataTransfer.files);
  });
  composer.addEventListener("paste", async (ev) => {
    const files = [...((ev.clipboardData && ev.clipboardData.files) || [])];
    if (files.length) {
      ev.preventDefault();
      await uploadFiles(files);
    }
  });
}

// ---------- mobile drawers ----------

function isNarrow() {
  return window.matchMedia("(max-width: 1100px)").matches;
}

function openDrawer(which) {
  closeDrawers();
  const el = document.querySelector(which === "rail" ? ".rail" : ".side");
  if (!el) return;
  el.classList.add("open");
  if ($("scrim")) $("scrim").classList.add("on");
}

function closeDrawers() {
  document.querySelectorAll(".rail.open, .side.open").forEach((el) => el.classList.remove("open"));
  if ($("scrim")) $("scrim").classList.remove("on");
}

function toggleDrawer(which) {
  const el = document.querySelector(which === "rail" ? ".rail" : ".side");
  if (el && el.classList.contains("open")) closeDrawers();
  else openDrawer(which);
}

if ($("navExperts")) $("navExperts").addEventListener("click", () => toggleDrawer("rail"));
if ($("navThreads")) $("navThreads").addEventListener("click", () => toggleDrawer("rail"));
if ($("navFiles")) $("navFiles").addEventListener("click", () => toggleDrawer("side"));
if ($("scrim")) $("scrim").addEventListener("click", closeDrawers);
window.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") closeDrawers();
});
window.addEventListener("beforeunload", (ev) => {
  if (state.stream) {
    ev.preventDefault();
    ev.returnValue = "";
  }
});

boot();
