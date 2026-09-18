use crate::catalog::Expert;
use crate::config::{max_agent_steps, Paths};
use crate::context;
use crate::harness::{self, Run, Ticket};
use crate::llm::{self, LlmError};
use crate::packs::{self, ToolCtx};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const READ_ONLY_TOOLS: &[&str] = &[
    "search_kb",
    "read_kb",
    "list_kb",
    "web_search",
    "web_open",
    "list_attachments",
    "read_attachment",
];

fn skill_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 64 || b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    if id.contains("--") {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn strip_skill_frontmatter(raw: &str) -> &str {
    let t = raw.trim_start();
    if !t.starts_with("---") {
        return raw;
    }
    let rest = &t[3..];
    if let Some(end) = rest.find("\n---") {
        rest[end + 4..].trim_start()
    } else {
        raw
    }
}

fn load_expert_skill_body(expert_id: &str) -> String {
    if !skill_id_ok(expert_id) {
        return String::new();
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(m) = std::env::var("CARGO_MANIFEST_DIR") {
        cands.push(
            PathBuf::from(m)
                .join("..")
                .join(".agents")
                .join("skills")
                .join(expert_id)
                .join("SKILL.md"),
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(
            cwd.join(".agents")
                .join("skills")
                .join(expert_id)
                .join("SKILL.md"),
        );
        cands.push(
            cwd.join("..")
                .join(".agents")
                .join("skills")
                .join(expert_id)
                .join("SKILL.md"),
        );
    }
    for p in cands {
        let p = Path::new(&p);
        if let Ok(raw) = fs::read_to_string(p) {
            let body = strip_skill_frontmatter(&raw).trim();
            if !body.is_empty() {
                return body.to_string();
            }
        }
    }
    String::new()
}

pub fn build_expert_prompt(expert: &Expert, confirm_ok: bool) -> String {
    let pack = packs::pack_help(&expert.id, &expert.category);
    let skill = load_expert_skill_body(&expert.id);
    let skill_block = if skill.is_empty() {
        String::new()
    } else {
        format!("\n本岗 Skill（程序记忆，选用本岗才加载全文）：\n{skill}\n")
    };
    format!(
        r#"你是土木企业工作台里的【{name}】专家（大类：{cat}）。

提问权：全企业任何人都可以向你提问，不限于本部门。施工员可以问财务，商务可以问试验室，工人可以问造价。用户召唤了你，就用你的知识库答。

两类任务同等重要：
A. 问 / 不懂 / 解释 / 科普：用白话讲清楚本专业概念、流程、边界。可以只聊天，不必 write_deliverable。先 search_kb 再答。答完注明依据来自本岗知识、大类共享还是网上检索。
B. 成稿 / 出文件 / 写方案表：按工序独立成稿，写完调用本大类专用工具或 write_deliverable。高风险且未确认则不要写盘。

问其他人：不要读取其他专家的私库。问题明显属于别的专业时，先答你能答的边界，并请用户在左侧改召唤那位专家。

职责：{title}
默认交付：{delivers}
风险：{risk}
标准工序：{pipeline}
{pack}
知识分层（必须用工具，不许假装读过）：
1. 用户上传：有附件先 list_attachments / read_attachment。招标/图纸/表格以用户文件为准，不要用库里的虚构例子顶替。
2. 你的本岗知识 kb/{category}/{id}/ —— search_kb / read_kb，先读「联网核对要点」（文件 web-knowledge.md，2026-08-14 过，只认官方标题）。
3. 大类共享库 kb/{category}/_shared/
4. 公司规则 kb/company/；先认「官方门户与现行口径」（文件 web-portals.md，含 APPBCA-2026-12：2026-10-01 CORENET X 仅 GFA≥5,000 m² 强制）。
5. 现场联网：search_kb 不够、用户要现行网页、或日期可能过期时，必须 web_search；命中后再 web_open 官方页。搜索摘要不是条文。没打开原文的条款号标 unverified。禁止编造条款。PDF 官网请用户上传，不要 web_open PDF。

硬规则（摘要，细节用 read_kb company/hard-rules.md）：
- 不编条款号、强度、岩土参数、综合单价。
- 引用写 全名+年份+条款；没抽到原文就 unverified / UNSPECIFIED。
- 无来源数字写 [A001] 待填。
- 禁止断言：可交差、可提交专家论证、请监理审核后开工、可以开工、报审通过。
- 产出是内部讨论 AI 草稿，不是法定签认件。
- 辖区 CN/SG/EU/DUAL 禁止静默混用。
- 高风险写盘前确认门。当前 confirm_ok={confirm_ok}。
  若用户要成稿且 risk=high 且 confirm_ok 不为 true：只问用户打出「我明白，将由持证人员签认」，不要 write_deliverable。纯提问（A）不受确认门阻挡。

先理解用户再说。可以只聊天，也可以出稿。用户没要求成稿时不要落盘。
成稿走本岗 harness（steps 独占工具），不要用聊天当计算器，不要编 xyz / 单价 / 条款号。
现行口径（税率、门户、通告年份）必须 web_search，命中后再 web_open 官方页；搜索摘要不是条文。

先看上传，再 search_kb；要现行网页就 web_search。中文回答。
{skill_block}"#,
        name = expert.name,
        cat = expert.category_name,
        title = expert.title,
        delivers = expert.delivers,
        risk = expert.risk,
        pipeline = expert.pipeline,
        category = expert.category,
        id = expert.id,
        confirm_ok = confirm_ok,
        pack = pack,
        skill_block = skill_block,
    )
}

fn user_blob(history: &[Value]) -> String {
    history
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_packish(blob: &str) -> bool {
    ["成套", "易标", "一人公司", "完整方案", "整套标"]
        .iter()
        .any(|k| blob.contains(k))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Chat,
    Run,
    Both,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Chat => "chat",
            Intent::Run => "run",
            Intent::Both => "both",
        }
    }
}

fn has_any(s: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| s.contains(k))
}

/// Understand the user first. Default is chat. Write only when they ask for a draft.
pub fn understand(blob: &str) -> Intent {
    let t = blob.trim();
    if t.is_empty() {
        return Intent::Chat;
    }
    if is_packish(t) {
        return Intent::Run;
    }
    let phrase_write = has_any(
        t,
        &[
            "写一份",
            "出一份",
            "做一份",
            "出稿",
            "成稿",
            "编制",
            "起草",
            "抽出",
            "扩写",
            "落盘",
            "出判定",
            "出清单",
            "出作业单",
            "帮我写",
            "请写",
            "生成一份",
            "写个",
        ],
    );
    let write = phrase_write
        || (t.contains('写')
            && has_any(
                t,
                &["方案", "提纲", "草稿", "清单", "纪要", "台账", "日历", "交底", "作业单"],
            ));
    let ask = has_any(
        t,
        &[
            "什么是",
            "是什么",
            "怎么理解",
            "如何理解",
            "解释",
            "科普",
            "区别",
            "为什么",
            "怎么看",
            "先聊聊",
            "先别写",
            "只是问问",
            "算不算",
            "要不要",
            "能不能",
            "可不可以",
            "行不行",
            "对不对",
        ],
    );
    let qmark = t.contains('？') || t.contains('?') || t.ends_with('吗');
    let tender = has_any(
        t,
        &[
            "招标",
            "ITT",
            "评标",
            "Two Envelope",
            "双信封",
            "workhead",
            "必须编制",
        ],
    );
    if write && (ask || qmark) {
        return Intent::Both;
    }
    if (ask || qmark) && !write {
        return Intent::Chat;
    }
    if write || tender {
        return Intent::Run;
    }
    Intent::Chat
}

pub fn is_explain_only(blob: &str) -> bool {
    understand(blob) == Intent::Chat
}

/// Deterministic chat from company KB. GST must copy the IRAS 9% sentence; no live model.
pub fn offline_explain(paths: &Paths, blob: &str) -> Option<String> {
    let gst = blob.contains("GST") || blob.contains("消费税") || blob.contains("gst");
    if !gst {
        return None;
    }
    let page = paths.kb_root.join("company").join("web-portals.md");
    let kb = fs::read_to_string(page).ok()?;
    if !kb.contains("9%") {
        return None;
    }
    let copied = kb
        .lines()
        .find(|l| l.contains("9%") && (l.contains("GST") || l.contains("税率") || l.contains("Current GST")))
        .map(str::trim)
        .unwrap_or("IRAS Current GST rates 页述现行标准税率 **9%**。");
    Some(format!(
        "新加坡现行 GST 税率按 IRAS Current GST rates 页述为 9%。\n{copied}\n内部讨论 AI 草稿，不是税务意见书。submit_blocked。禁止把 7%/8% 写成现行税率。"
    ))
}

pub fn detect_jurisdiction(blob: &str) -> &'static str {
    let u = blob.to_ascii_uppercase();
    if blob.contains("DUAL") || blob.contains("双辖") {
        "DUAL"
    } else if blob.contains("Eurocode") || u.contains(" EN ") || blob.contains("欧盟") {
        "EU"
    } else if blob.contains("BCA")
        || blob.contains("GeBIZ")
        || blob.contains("SCDF")
        || blob.contains("LTA")
        || u.contains("SS EN")
    {
        "SG"
    } else if blob.contains("JGJ") || blob.contains("住建") || blob.contains("国标") {
        "CN"
    } else {
        "SG"
    }
}

pub fn ticket_from_chat(session: &str, expert: &Expert, history: &[Value], confirm_ok: bool) -> Ticket {
    let brief = user_blob(history);
    let project = brief
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(40).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| expert.name.clone());
    Ticket {
        session: session.to_string(),
        project,
        jurisdiction: detect_jurisdiction(&brief).into(),
        brief,
        path: String::new(),
        confirm_ok,
    }
}

pub fn format_run_text(label: &str, run: &Run) -> String {
    let mut out = format!(
        "【{label}】Harness mode=`{}` · run_id={}\n内部讨论 AI 草稿，不是法定签认件。缺的数字 [A001] / UNSPECIFIED。\n",
        run.mode, run.run_id
    );
    if let Some(e) = &run.error {
        out.push_str(e);
        out.push('\n');
        return out;
    }
    if run.hitl.pending {
        out.push_str("HITL：高风险写盘未确认。请勾选「我明白，将由持证人员签认」后再发一次。本岗未出稿。\n");
    }
    out.push_str("\n## Runtime 步骤\n");
    for s in &run.steps {
        out.push_str(&format!(
            "- {} · {} / {} · legal={} ok={}\n  {}\n",
            s.name, s.expert, s.tool, s.legal, s.ok, s.note
        ));
    }
    if !run.files.is_empty() {
        out.push_str("\n## 已出稿\n");
        for f in &run.files {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("- {name}\n  `{path}`\n"));
        }
    }
    if !run.notes.is_empty() {
        out.push_str("\n## 记录\n");
        for n in &run.notes {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out.push_str(&format!(
        "\nillegal_tool_calls={}\n",
        run.illegal_count()
    ));
    out
}

pub fn events_from_run(label: &str, expert_id: &str, run: &Run) -> Vec<EventOut> {
    let text = format_run_text(label, run);
    let ctx = context::inspect(
        &[json!({"role": "assistant", "content": text})],
        &[&text],
    );
    let mut events = vec![(
        "status".into(),
        json!({
            "phase": "harness",
            "text": format!("{label} · harness steps · run {}", run.run_id),
            "expert": expert_id,
            "run_id": run.run_id,
        }),
    )];
    for s in &run.steps {
        events.push((
            "status".into(),
            json!({
                "phase": s.name,
                "text": format!("{} · {} legal={} ok={}", s.tool, s.note, s.legal, s.ok),
                "expert": expert_id,
            }),
        ));
    }
    events.push(("context".into(), ctx.to_value()));
    events
}

fn done_from_run(label: &str, expert_id: &str, run: &Run) -> EventOut {
    let text = format_run_text(label, run);
    let ctx = context::inspect(
        &[json!({"role": "assistant", "content": text})],
        &[&text],
    );
    (
        "done".into(),
        json!({
            "mode": "expert",
            "harness": true,
            "runtime": run.mode,
            "intent": "run",
            "expert": expert_id,
            "text": text,
            "citations": [],
            "deliverables": run.files.clone(),
            "run_id": run.run_id,
            "hitl": {
                "required": run.hitl.required,
                "confirmed": run.hitl.confirmed,
                "pending": run.hitl.pending,
                "gate": run.hitl.gate,
            },
            "illegal_tool_calls": run.illegal_count(),
            "intent": "run",
            "wrote": !run.files.is_empty(),
            "submit_blocked": true,
            "stamp": chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string(),
            "context": ctx.to_value(),
        }),
    )
}

#[allow(dead_code)]
fn fill_empty_source_args(expert: &Expert, name: &str, args: &mut Value, history: &[Value]) {
    let blob = user_blob(history);
    if blob.trim().is_empty() {
        return;
    }
    let empty_key = |args: &Value, k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    };
    if name == "bid-parse__extract" && empty_key(args, "tender_text") && empty_key(args, "text") {
        args["tender_text"] = json!(blob);
    }
    if name == "construction__scheme_draft"
        && empty_key(args, "work_scope")
        && empty_key(args, "known_facts")
    {
        args["work_scope"] = json!(blob);
    }
    if (name.ends_with("__extract") || name.ends_with("__scheme_draft"))
        && empty_key(args, "project_name")
    {
        args["project_name"] = json!(expert.name.clone());
    }
}

pub fn finish_if_no_deliverable(
    expert: &Expert,
    history: &[Value],
    ctx: &mut ToolCtx,
) -> Option<String> {
    let blob = user_blob(history);
    if blob.chars().count() < 8 {
        return None;
    }
    let packish = blob.contains("成套")
        || blob.contains("易标")
        || blob.contains("一人公司")
        || blob.contains("完整方案")
        || blob.contains("整套标");
    if packish {
        let v = crate::firm::run_bid_job(
            &ctx.paths,
            &ctx.session_id,
            &json!({
                "brief": blob,
                "project_name": expert.name,
                "confirm_ok": ctx.confirm_ok,
            }),
        );
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            if let Some(arr) = v.get("files").and_then(|x| x.as_array()) {
                for f in arr {
                    ctx.deliverables.push(f.clone());
                }
            }
            return Some(v.to_string());
        }
    }
    let (tool, args) = match expert.id.as_str() {
        "bid-parse" => (
            "bid-parse__extract",
            json!({"tender_text": blob, "project_name": expert.name}),
        ),
        "construction" => (
            "construction__scheme_draft",
            json!({
                "work_scope": blob,
                "project_name": expert.name,
                "known_facts": blob
            }),
        ),
        _ => return None,
    };
    let out = packs::execute(ctx, tool, &args);
    if out.contains("已写入") {
        Some(out)
    } else {
        None
    }
}

pub fn plain_system() -> &'static str {
    "你是 DeepSeek，在土木工作台里以「未召唤专家」模式回答。没有岗位知识库，没有出稿工具。可以科普、讨论、列提纲，但必须声明：这不是专家稿，条款和数字需要用户自行核对。不要假装引用了企业规范库。用户若要可核验成稿，请他们在左侧召唤专家。"
}

#[derive(Clone, Debug)]
pub enum LlmMode {
    Live,
    FakePlain { text: String },
}

pub type EventOut = (String, Value);

/// Live delivery of events while a run is in progress.
///
/// `sink` forwards every event the moment it is produced (the SSE handler drains it);
/// `stop` is raised when the client went away or pressed 停止. Functions still return
/// the full `Vec<EventOut>` so tests and offline callers keep working unchanged.
#[derive(Clone, Default)]
pub struct Live {
    pub sink: Option<tokio::sync::mpsc::UnboundedSender<EventOut>>,
    pub stop: Option<Arc<AtomicBool>>,
}

impl Live {
    pub fn none() -> Self {
        Self::default()
    }
    pub fn stopped(&self) -> bool {
        self.stop.as_ref().map(|s| s.load(Ordering::Relaxed)).unwrap_or(false)
    }
}

fn emit(events: &mut Vec<EventOut>, live: &Live, ev: EventOut) {
    if let Some(tx) = &live.sink {
        let _ = tx.send(ev.clone());
    }
    events.push(ev);
}

fn emit_all(events: &mut Vec<EventOut>, live: &Live, evs: Vec<EventOut>) {
    for ev in evs {
        emit(events, live, ev);
    }
}

fn stopped_status() -> EventOut {
    ("status".into(), json!({"phase": "stopped", "text": "已按要求停止"}))
}

pub async fn run_plain(history: Vec<Value>, mode: &LlmMode, live: &Live) -> Result<Vec<EventOut>, LlmError> {
    let mut events = Vec::new();
    emit(&mut events, live, (
        "status".into(),
        json!({"phase": "plain", "text": "未召唤专家 · 普通 DeepSeek"}),
    ));
    match mode {
        LlmMode::FakePlain { text } => {
            let ctx = context::inspect(&history, &[plain_system(), text]);
            emit(&mut events, live, ("context".into(), ctx.to_value()));
            emit(&mut events, live, ("token".into(), json!({"text": text})));
            emit(&mut events, live, (
                "done".into(),
                json!({"mode": "plain", "text": text, "citations": [], "deliverables": [], "context": ctx.to_value()}),
            ));
        }
        LlmMode::Live => {
            let mut messages = vec![json!({"role": "system", "content": plain_system()})];
            messages.extend(history);
            let mut buf = String::new();
            let mut stopped = false;
            let res = llm::stream_chat(
                &messages,
                None,
                0.6,
                |piece| {
                    buf.push_str(piece);
                    emit(&mut events, live, ("token".into(), json!({"text": piece})));
                },
                || live.stopped(),
            )
            .await;
            match res {
                Ok(_) => {}
                Err(e) if e.is_stopped() => {
                    stopped = true;
                    emit(&mut events, live, stopped_status());
                }
                Err(e) => return Err(e),
            }
            let ctx = context::inspect(&messages, &[&buf]);
            emit(&mut events, live, ("context".into(), ctx.to_value()));
            emit(&mut events, live, (
                "done".into(),
                json!({"mode": "plain", "text": buf, "citations": [], "deliverables": [], "context": ctx.to_value(), "stopped": stopped}),
            ));
        }
    }
    Ok(events)
}

pub async fn run_expert(
    paths: &Paths,
    expert: &Expert,
    history: Vec<Value>,
    confirm_ok: bool,
    session_id: &str,
    mode: &LlmMode,
    live: &Live,
) -> Result<Vec<EventOut>, LlmError> {
    let mut events = Vec::new();
    emit(&mut events, live, (
        "status".into(),
        json!({
            "phase": "summon",
            "text": format!("已召唤 {} / {} · 独立收工", expert.category_name, expert.name),
            "expert": expert.id,
        }),
    ));

    if let LlmMode::FakePlain { text } = mode {
        let ctx = context::inspect(&history, &[text]);
        emit(&mut events, live, ("context".into(), ctx.to_value()));
        emit(&mut events, live, ("token".into(), json!({"text": text})));
        emit(&mut events, live, (
            "done".into(),
            json!({
                "mode": "expert",
                "expert": expert.id,
                "text": text,
                "citations": [],
                "deliverables": [],
                "context": ctx.to_value(),
            }),
        ));
        return Ok(events);
    }

    let blob = user_blob(&history);
    let intent = understand(&blob);
    emit(&mut events, live, (
        "status".into(),
        json!({
            "phase": "understand",
            "text": format!("{} · 听懂为 {}（能聊能跑）", expert.name, intent.as_str()),
            "expert": expert.id,
            "intent": intent.as_str(),
        }),
    ));
    match intent {
        Intent::Chat => {
            if let Some(text) = offline_explain(paths, &blob) {
                emit(&mut events, live, ("token".into(), json!({"text": text})));
                emit(&mut events, live, (
                    "done".into(),
                    json!({
                        "mode": "expert",
                        "intent": "chat",
                        "wrote": false,
                        "submit_blocked": true,
                        "expert": expert.id,
                        "text": text,
                        "deliverables": [],
                    }),
                ));
            } else {
                let evs = run_expert_explain(paths, expert, history, confirm_ok, session_id, live).await?;
                events.extend(evs); // already forwarded live inside
            }
        }
        Intent::Run | Intent::Both => {
            let ticket = ticket_from_chat(session_id, expert, &history, confirm_ok);
            let run = harness::run_expert_steps(paths, expert, ticket);
            emit_all(&mut events, live, events_from_run(&expert.name, &expert.id, &run));
            if !run.files.is_empty() {
                // surface the drafts now: if the narration below fails, the links are already out
                emit(&mut events, live, ("file".into(), json!({"deliverables": run.files.clone()})));
            }
            if live.stopped() {
                emit(&mut events, live, stopped_status());
                let mut done = done_from_run(&expert.name, &expert.id, &run);
                done.1["stopped"] = json!(true);
                emit(&mut events, live, done);
                return Ok(events);
            }
            match talk_after_run(paths, expert, &history, confirm_ok, session_id, &run, live).await {
                Ok(talk) => events.extend(talk), // already forwarded live inside
                Err(e) if e.is_stopped() => {
                    emit(&mut events, live, stopped_status());
                    let mut done = done_from_run(&expert.name, &expert.id, &run);
                    done.1["stopped"] = json!(true);
                    emit(&mut events, live, done);
                }
                Err(_) => {
                    let text = format_run_text(&expert.name, &run);
                    emit(&mut events, live, ("token".into(), json!({"text": text})));
                    emit(&mut events, live, done_from_run(&expert.name, &expert.id, &run));
                }
            }
        }
    }
    Ok(events)
}

async fn run_expert_explain(
    paths: &Paths,
    expert: &Expert,
    history: Vec<Value>,
    confirm_ok: bool,
    session_id: &str,
    live: &Live,
) -> Result<Vec<EventOut>, LlmError> {
    let mut events = Vec::new();
    let mut ctx = ToolCtx::new(
        paths.clone(),
        &expert.id,
        &expert.category,
        &expert.risk,
        confirm_ok,
        session_id,
    );
    let tools: Vec<Value> = packs::tools_for_expert(&expert.id)
        .iter()
        .filter(|t| READ_ONLY_TOOLS.contains(&t.name))
        .map(|t| t.openai_tool())
        .collect();
    let mut messages = vec![json!({"role": "system", "content": build_expert_prompt(expert, confirm_ok)})];
    messages.extend(history);
    emit(&mut events, live, (
        "status".into(),
        json!({
            "phase": "chat",
            "text": format!("{} · 在跟你说话（可联网，不成稿）", expert.name),
            "expert": expert.id,
        }),
    ));
    emit(&mut events, live, (
        "context".into(),
        context::inspect(&messages, &[]).to_value(),
    ));

    let max_steps = max_agent_steps();
    let mut final_text = String::new();
    let mut streamed = false;
    for step in 0..max_steps {
        if live.stopped() {
            return Err(LlmError::stopped());
        }
        emit(&mut events, live, (
            "status".into(),
            json!({"phase": "think", "text": format!("{} 解释 {}/{}", expert.name, step + 1, max_steps)}),
        ));
        let mut got_text = false;
        let msg = llm::stream_chat(
            &messages,
            Some(&tools),
            0.3,
            |piece| {
                got_text = true;
                emit(&mut events, live, ("token".into(), json!({"text": piece})));
            },
            || live.stopped(),
        )
        .await?;
        let tool_calls = msg.get("tool_calls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if tool_calls.is_empty() {
            final_text = msg.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            streamed = got_text;
            break;
        }
        if got_text {
            // interim "thinking" text was shown; tools come next, so clear the bubble
            emit(&mut events, live, ("reset".into(), json!({"reason": "tools"})));
        }
        messages.push(msg);
        for call in tool_calls {
            let fnx = call.get("function").cloned().unwrap_or(json!({}));
            let name = fnx.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let raw = fnx.get("arguments").cloned().unwrap_or(json!("{}"));
            let args = match raw {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(json!({})),
                other => other,
            };
            if !READ_ONLY_TOOLS.contains(&name.as_str()) {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.get("id").and_then(|v| v.as_str()).unwrap_or(&name),
                    "content": "拒绝：解释模式只允许只读工具。成稿请直接下达作业，走 harness steps。",
                }));
                continue;
            }
            let hint = args
                .get("query")
                .or_else(|| args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            emit(&mut events, live, (
                "status".into(),
                json!({"phase": name, "text": format!("{} · {} {}", expert.name, name, hint)}),
            ));
            let result = packs::execute(&mut ctx, &name, &args);
            let cut: String = result.chars().take(12000).collect();
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.get("id").and_then(|v| v.as_str()).unwrap_or(&name),
                "content": cut,
            }));
        }
    }
    if final_text.is_empty() {
        final_text = "（达到步数上限，请把任务拆小或再发一次）".into();
    }
    if !streamed {
        // nothing came through the stream (fallback text): show it in one piece, never split UTF-8 bytes
        emit(&mut events, live, ("reset".into(), json!({"reason": "final"})));
        emit(&mut events, live, ("token".into(), json!({"text": final_text})));
    }

    let mut uniq = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in ctx.citations {
        if let Some(p) = c.get("path").and_then(|v| v.as_str()) {
            if seen.insert(p.to_string()) {
                uniq.push(c);
            }
        }
    }

    let ctx_end = context::inspect(&messages, &[&final_text]);
    emit(&mut events, live, ("context".into(), ctx_end.to_value()));
    emit(&mut events, live, (
        "done".into(),
        json!({
            "mode": "expert",
            "harness": true,
            "runtime": "chat",
            "intent": "chat",
            "expert": expert.id,
            "text": final_text,
            "citations": uniq,
            "deliverables": ctx.deliverables,
            "stamp": chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string(),
            "context": ctx_end.to_value(),
        }),
    ));
    Ok(events)
}

fn read_run_grounding(run: &Run) -> String {
    let mut g = format_run_text("本岗", run);
    for f in &run.files {
        let Some(path) = f.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Ok(body) = std::fs::read_to_string(path) {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or(path);
            let cut: String = body.chars().take(4000).collect();
            g.push_str(&format!("\n\n## 文件 {name}\n{cut}\n"));
        }
    }
    g
}

async fn talk_after_run(
    paths: &Paths,
    expert: &Expert,
    history: &[Value],
    confirm_ok: bool,
    session_id: &str,
    run: &Run,
    live: &Live,
) -> Result<Vec<EventOut>, LlmError> {
    let grounding = read_run_grounding(run);
    let extra = format!(
        "\n\n你已经按 harness steps 出稿（或 HITL 未出稿）。现在用白话跟用户说话：\n\
         1. 先复述你听懂的任务；\n\
         2. 说明出了什么、缺什么（[A001]/UNSPECIFIED）；\n\
         3. 现行门户只引用草稿或 web_search/web_open 看到的官方标题，条款没打开就 unverified；\n\
         4. 不要再调用写盘工具，不要编 xyz / 单价 / 条款号。\n\
         5. 用户若还想聊，直接答。\n\n--- 草稿与步骤 ---\n{grounding}\n"
    );
    let mut messages = vec![json!({
        "role": "system",
        "content": format!("{}{extra}", build_expert_prompt(expert, confirm_ok))
    })];
    messages.extend(history.iter().cloned());
    let tools: Vec<Value> = packs::tools_for_expert(&expert.id)
        .iter()
        .filter(|t| READ_ONLY_TOOLS.contains(&t.name))
        .map(|t| t.openai_tool())
        .collect();
    let mut events = Vec::new();
    emit(&mut events, live, (
        "status".into(),
        json!({
            "phase": "talk",
            "text": format!("{} · 出稿后用白话说明", expert.name),
            "expert": expert.id,
        }),
    ));
    let mut ctx = ToolCtx::new(
        paths.clone(),
        &expert.id,
        &expert.category,
        &expert.risk,
        confirm_ok,
        session_id,
    );
    let max_steps = max_agent_steps();
    let mut final_text = String::new();
    let mut streamed = false;
    for step in 0..max_steps {
        if live.stopped() {
            return Err(LlmError::stopped());
        }
        emit(&mut events, live, (
            "status".into(),
            json!({"phase": "think", "text": format!("{} 说明 {}/{}", expert.name, step + 1, max_steps)}),
        ));
        let mut got_text = false;
        let msg = llm::stream_chat(
            &messages,
            Some(&tools),
            0.3,
            |piece| {
                got_text = true;
                emit(&mut events, live, ("token".into(), json!({"text": piece})));
            },
            || live.stopped(),
        )
        .await?;
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if tool_calls.is_empty() {
            final_text = msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            streamed = got_text;
            break;
        }
        if got_text {
            emit(&mut events, live, ("reset".into(), json!({"reason": "tools"})));
        }
        messages.push(msg);
        for call in tool_calls {
            let fnx = call.get("function").cloned().unwrap_or(json!({}));
            let name = fnx
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw = fnx.get("arguments").cloned().unwrap_or(json!("{}"));
            let args = match raw {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(json!({})),
                other => other,
            };
            if !READ_ONLY_TOOLS.contains(&name.as_str()) {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.get("id").and_then(|v| v.as_str()).unwrap_or(&name),
                    "content": "拒绝：说明阶段只读。写盘已由 harness steps 完成。",
                }));
                continue;
            }
            let hint = args
                .get("query")
                .or_else(|| args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            emit(&mut events, live, (
                "status".into(),
                json!({"phase": name, "text": format!("{} · {} {}", expert.name, name, hint)}),
            ));
            let result = packs::execute(&mut ctx, &name, &args);
            let cut: String = result.chars().take(12000).collect();
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.get("id").and_then(|v| v.as_str()).unwrap_or(&name),
                "content": cut,
            }));
        }
    }
    if final_text.trim().is_empty() {
        final_text = grounding;
        streamed = false;
    }
    if !streamed {
        emit(&mut events, live, ("reset".into(), json!({"reason": "final"})));
        emit(&mut events, live, ("token".into(), json!({"text": final_text})));
    }
    let ctx_end = context::inspect(&messages, &[&final_text]);
    emit(&mut events, live, ("context".into(), ctx_end.to_value()));
    emit(&mut events, live, (
        "done".into(),
        json!({
            "mode": "expert",
            "harness": true,
            "runtime": "steps+talk",
            "intent": "run",
            "expert": expert.id,
            "text": final_text,
            "citations": ctx.citations,
            "deliverables": run.files.clone(),
            "run_id": run.run_id,
            "hitl": {
                "required": run.hitl.required,
                "confirmed": run.hitl.confirmed,
                "pending": run.hitl.pending,
                "gate": run.hitl.gate,
            },
            "illegal_tool_calls": run.illegal_count(),
            "stamp": chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string(),
            "context": ctx_end.to_value(),
        }),
    ));
    Ok(events)
}
