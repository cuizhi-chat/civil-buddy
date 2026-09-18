use crate::agent::{self, LlmMode};
use crate::attach;
use crate::config::{llm_model, Paths};
use crate::kbio::{self, MAX_FILE_BYTES};
use crate::llm;
use crate::rag::list_kb;
use crate::store;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::services::ServeDir;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub paths: Paths,
    pub llm: LlmMode,
    pub force_has_key: Option<bool>,
}

impl AppState {
    pub fn live(paths: Paths) -> Self {
        Self {
            paths,
            llm: LlmMode::Live,
            force_has_key: None,
        }
    }

    fn has_key(&self) -> bool {
        self.force_has_key.unwrap_or_else(llm::has_key)
    }
}

pub fn app(state: AppState) -> Router {
    let static_dir = state.paths.static_dir.clone();
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/catalog", get(catalog))
        .route("/api/kb/{expert_id}", get(kb))
        .route("/api/studio/tree", get(studio_tree))
        .route("/api/studio/file", get(studio_read).put(studio_write).post(studio_create).delete(studio_delete))
        .route("/api/studio/experts", post(studio_expert))
        .route("/api/studio/experts/{expert_id}", delete(studio_expert_del))
        .route("/api/studio/categories", post(studio_category))
        .route("/api/studio/limit", post(studio_limit))
        .route("/api/chat", post(chat))
        .route(
            "/api/upload",
            post(upload).layer(DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route("/api/attachments", get(attachments))
        .route("/api/local", post(import_local))
        .route("/api/job", get(job_listing))
        .route("/api/firm/bid", post(firm_bid))
        .route("/api/architecture", get(architecture))
        .route("/api/eval/shadow", post(eval_shadow))
        .route("/api/eval/shadow-expert", post(eval_shadow_expert))
        .route("/api/eval/live", get(eval_live))
        .route("/api/harness/expert", post(harness_expert))
        .route("/api/harness/trace/{session}/{run_id}", get(harness_trace))
        .route("/api/file", get(file_get))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(Arc::new(state))
}

type ApiError = (StatusCode, Json<Value>);

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(json!({"detail": msg.into()})))
}

async fn index(State(st): State<Arc<AppState>>) -> Response {
    let path = st.paths.static_dir.join("index.html");
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            bytes,
        )
            .into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "missing index").into_response(),
    }
}

async fn health(State(st): State<Arc<AppState>>) -> Json<Value> {
    let keyed = st.has_key();
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        // The shared frontend hides any button whose capability is missing instead of hitting a 404.
        "capabilities": {
            "backend": "rust",
            "upload": true,
            "attachments": true,
            "local": true,
            "firm": true,
            "threads": false,
            "thread_messages": false,
            "thread_files": false,
            "cancel": false,
            "config": false,
            "skills": false,
            "mcp": false,
            "heartbeat": true,
            "file_events": false,
        },
        "has_key": keyed,
        "deepseek": keyed,
        "model": llm_model(),
        "context": crate::context::Policy::from_env().to_value(),
        "harness": crate::harness::architecture(),
        "parse": crate::parse::probe(),
        "packing_agent": crate::packing_bridge::probe(),
    }))
}

async fn architecture() -> Json<Value> {
    Json(crate::harness::architecture())
}

async fn eval_shadow(State(st): State<Arc<AppState>>, Json(body): Json<FirmBidIn>) -> Result<Json<Value>, ApiError> {
    let session = if body.session_id.is_empty() {
        Uuid::new_v4().simple().to_string().chars().take(12).collect()
    } else {
        body.session_id.clone()
    };
    let args = json!({
        "project_name": body.project_name,
        "jurisdiction": body.jurisdiction,
        "path": body.path,
        "brief": body.brief,
        "tender_text": body.brief,
        "confirm_ok": body.confirm_ok,
    });
    let ticket = crate::harness::Ticket::from_args(&session, &args);
    Ok(Json(crate::harness::shadow_eval(&st.paths, ticket)))
}

async fn harness_trace(
    State(st): State<Arc<AppState>>,
    AxPath((session, run_id)): AxPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    crate::harness::load_trace(&st.paths, &session, &run_id)
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "trace not found"))
}

#[derive(Deserialize)]
struct ExpertRunIn {
    #[serde(default)]
    session_id: String,
    expert_id: String,
    #[serde(default)]
    project_name: String,
    #[serde(default)]
    jurisdiction: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    brief: String,
    #[serde(default)]
    confirm_ok: bool,
}

fn expert_ticket(session: &str, body: &ExpertRunIn) -> crate::harness::Ticket {
    let args = json!({
        "project_name": body.project_name,
        "jurisdiction": body.jurisdiction,
        "path": body.path,
        "brief": body.brief,
        "tender_text": body.brief,
        "confirm_ok": body.confirm_ok,
    });
    crate::harness::Ticket::from_args(session, &args)
}

async fn harness_expert(State(st): State<Arc<AppState>>, Json(body): Json<ExpertRunIn>) -> Result<Json<Value>, ApiError> {
    let exp = store::get_expert(&st.paths, &body.expert_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown expert"))?;
    let session = if body.session_id.is_empty() {
        Uuid::new_v4().simple().to_string().chars().take(12).collect()
    } else {
        body.session_id.clone()
    };
    let ticket = expert_ticket(&session, &body);
    Ok(Json(crate::harness::run_turn(&st.paths, &exp, ticket).to_value()))
}

async fn eval_live(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(tokio::task::spawn_blocking({
        let paths = st.paths.clone();
        move || crate::eval_live::report(&paths)
    })
    .await
    .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()})))
}

async fn eval_shadow_expert(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ExpertRunIn>,
) -> Result<Json<Value>, ApiError> {
    let exp = store::get_expert(&st.paths, &body.expert_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown expert"))?;
    let session = if body.session_id.is_empty() {
        Uuid::new_v4().simple().to_string().chars().take(12).collect()
    } else {
        body.session_id.clone()
    };
    let ticket = expert_ticket(&session, &body);
    Ok(Json(crate::harness::shadow_eval_expert(&st.paths, &exp, ticket)))
}

async fn catalog(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(store::catalog_payload(&st.paths))
}

async fn kb(State(st): State<Arc<AppState>>, AxPath(expert_id): AxPath<String>) -> Result<Json<Value>, ApiError> {
    let exp = store::get_expert(&st.paths, &expert_id).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown expert"))?;
    let files = list_kb(&st.paths, &exp.id, &exp.category);
    let total: u64 = files
        .iter()
        .filter_map(|f| f.get("bytes").and_then(|v| v.as_u64()))
        .sum();
    Ok(Json(json!({
        "expert": expert_id,
        "files": files,
        "bytes": total,
        "label": kbio::format_bytes(total),
    })))
}

async fn studio_tree(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(store::tree_payload(&st.paths))
}

#[derive(Deserialize)]
struct PathQ {
    path: String,
}

async fn studio_read(State(st): State<Arc<AppState>>, Query(q): Query<PathQ>) -> Result<Json<Value>, ApiError> {
    let (text, stat) = kbio::read_text(&st.paths, &q.path).ok_or_else(|| err(StatusCode::NOT_FOUND, "文件不存在"))?;
    Ok(Json(json!({
        "path": q.path,
        "content": text,
        "title": stat.title,
        "display": stat.display,
        "layer": stat.layer,
        "layer_label": stat.layer_label,
        "bytes": stat.bytes,
        "chars": stat.chars,
        "lines": stat.lines,
    })))
}

#[derive(Deserialize)]
struct FileIn {
    path: String,
    #[serde(default)]
    content: String,
}

async fn studio_write(State(st): State<Arc<AppState>>, Json(body): Json<FileIn>) -> Result<Json<Value>, ApiError> {
    let stat = kbio::write_text(&st.paths, &body.path, &body.content).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({
        "ok": true,
        "path": stat.path,
        "title": stat.title,
        "display": stat.display,
        "layer": stat.layer,
        "layer_label": stat.layer_label,
        "bytes": stat.bytes,
        "chars": stat.chars,
        "lines": stat.lines,
        "label": kbio::format_bytes(stat.bytes),
    })))
}

async fn studio_create(State(st): State<Arc<AppState>>, Json(body): Json<FileIn>) -> Result<Json<Value>, ApiError> {
    let stat = kbio::create_file(&st.paths, &body.path).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({
        "ok": true,
        "path": stat.path,
        "title": stat.title,
        "bytes": stat.bytes,
        "chars": stat.chars,
        "lines": stat.lines,
    })))
}

async fn studio_delete(State(st): State<Arc<AppState>>, Query(q): Query<PathQ>) -> Result<Json<Value>, ApiError> {
    kbio::delete_file(&st.paths, &q.path).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ExpertIn {
    id: String,
    name: String,
    category: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    delivers: String,
    #[serde(default = "low")]
    risk: String,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    pipeline: String,
}

fn low() -> String {
    "low".into()
}

async fn studio_expert(State(st): State<Arc<AppState>>, Json(body): Json<ExpertIn>) -> Result<Json<Value>, ApiError> {
    let payload = json!({
        "id": body.id,
        "name": body.name,
        "category": body.category,
        "title": body.title,
        "delivers": body.delivers,
        "risk": body.risk,
        "aliases": body.aliases,
        "pipeline": body.pipeline,
    });
    let exp = store::upsert_expert(&st.paths, &payload).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    serde_json::to_value(exp)
        .map(Json)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct DelQ {
    #[serde(default = "default_true")]
    delete_kb: bool,
}

fn default_true() -> bool {
    true
}

async fn studio_expert_del(
    State(st): State<Arc<AppState>>,
    AxPath(expert_id): AxPath<String>,
    Query(q): Query<DelQ>,
) -> Result<Json<Value>, ApiError> {
    if store::get_expert(&st.paths, &expert_id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "unknown expert"));
    }
    store::disable_or_delete_expert(&st.paths, &expert_id, q.delete_kb)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct CategoryIn {
    id: String,
    name: String,
    #[serde(default)]
    blurb: String,
}

async fn studio_category(State(st): State<Arc<AppState>>, Json(body): Json<CategoryIn>) -> Result<Json<Value>, ApiError> {
    store::upsert_category(&st.paths, &body.id, &body.name, &body.blurb)
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct LimitIn {
    kb_soft_limit_kb: i64,
}

async fn studio_limit(State(st): State<Arc<AppState>>, Json(body): Json<LimitIn>) -> Json<Value> {
    Json(json!({
        "kb_soft_limit_kb": store::set_soft_limit(&st.paths, body.kb_soft_limit_kb),
        "max_file_bytes": MAX_FILE_BYTES,
    }))
}

#[derive(Deserialize)]
struct SessionQ {
    #[serde(default)]
    session_id: String,
}

async fn attachments(
    State(st): State<Arc<AppState>>,
    Query(q): Query<SessionQ>,
) -> Result<Json<Value>, ApiError> {
    if q.session_id.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "缺少 session_id"));
    }
    Ok(Json(json!({
        "files": attach::list_uploads(&st.paths, &q.session_id),
    })))
}

async fn upload(State(st): State<Arc<AppState>>, mut multipart: Multipart) -> Result<Json<Value>, ApiError> {
    let mut session = String::new();
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "session_id" {
            session = field.text().await.unwrap_or_default();
            continue;
        }
        if name != "file" && name != "files" {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload.bin").to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
        pending.push((filename, bytes.to_vec()));
    }
    if session.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "缺少 session_id"));
    }
    if pending.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "没有收到文件"));
    }
    let mut saved = Vec::new();
    for (filename, bytes) in pending {
        let meta = attach::save_upload(&st.paths, &session, &filename, &bytes)
            .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        saved.push(meta);
    }
    Ok(Json(json!({"ok": true, "files": saved})))
}

#[derive(Deserialize)]
struct LocalIn {
    session_id: String,
    path: String,
}

async fn job_listing() -> Json<Value> {
    let raw = std::env::var("CIVIL_JOB_ROOT").unwrap_or_default();
    let p = std::path::PathBuf::from(raw.trim());
    let lower = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    let denied = lower == "d:\\layout" || lower.starts_with("d:\\layout\\");
    if raw.trim().is_empty() || denied || !p.is_dir() {
        return Json(json!({
            "ok": true,
            "granted": false,
            "root": "",
            "files": [],
            "hint": "设 CIVIL_JOB_ROOT 为工程文件夹后直接读本机 xlsx/docx，不必上传。禁止 D:\\layout。",
        }));
    }
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&p) {
        let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        for f in names {
            if !f.is_file() {
                continue;
            }
            let ext = f
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "xlsx" | "csv" | "txt" | "md" | "json" | "docx" | "log") {
                continue;
            }
            files.push(json!({
                "name": f.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                "path": f.to_string_lossy(),
                "suffix": format!(".{ext}"),
            }));
            if files.len() >= 12 {
                break;
            }
        }
    }
    Json(json!({
        "ok": true,
        "granted": true,
        "root": p.to_string_lossy(),
        "files": files,
        "hint": "说「写一份」会自动抄作业根文件，不必再上传。",
    }))
}

async fn import_local(State(st): State<Arc<AppState>>, Json(body): Json<LocalIn>) -> Result<Json<Value>, ApiError> {
    let path = if body.path.trim().is_empty() {
        std::env::var("CIVIL_JOB_ROOT").unwrap_or_default()
    } else {
        body.path.clone()
    };
    let files = attach::import_local(&st.paths, &body.session_id, &path)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"ok": true, "files": files})))
}

#[derive(Deserialize)]
struct FirmBidIn {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    project_name: String,
    #[serde(default)]
    jurisdiction: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    brief: String,
    #[serde(default)]
    confirm_ok: bool,
}

async fn firm_bid(State(st): State<Arc<AppState>>, Json(body): Json<FirmBidIn>) -> Result<Json<Value>, ApiError> {
    let session = if body.session_id.is_empty() {
        Uuid::new_v4().simple().to_string().chars().take(12).collect()
    } else {
        body.session_id.clone()
    };
    let args = json!({
        "project_name": body.project_name,
        "jurisdiction": body.jurisdiction,
        "path": body.path,
        "brief": body.brief,
        "confirm_ok": body.confirm_ok,
    });
    let v = crate::firm::run_bid_job(&st.paths, &session, &args);
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let msg = v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("成套失败");
        return Err(err(StatusCode::BAD_REQUEST, msg));
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
struct ChatIn {
    message: String,
    #[serde(default)]
    history: Vec<Value>,
    #[serde(default)]
    expert_ids: Vec<String>,
    #[serde(default)]
    confirm_ok: bool,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    attachments: Vec<String>,
}

fn sse_offline_chat(text: String) -> Response {
    let done = json!({
        "mode": "chat",
        "intent": "chat",
        "wrote": false,
        "submit_blocked": true,
        "text": text,
        "deliverables": [],
    });
    let body = format!(
        "event: status\ndata: {{\"phase\":\"understand\",\"intent\":\"chat\"}}\n\nevent: token\ndata: {}\n\nevent: done\ndata: {}\n\n",
        serde_json::to_string(&json!({"text": text})).unwrap_or_else(|_| "{}".into()),
        serde_json::to_string(&done).unwrap_or_else(|_| "{}".into()),
    );
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/event-stream"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn chat(State(st): State<Arc<AppState>>, Json(body): Json<ChatIn>) -> Result<Response, ApiError> {
    let intent = crate::agent::understand(&body.message);
    if !st.has_key() {
        if intent == crate::agent::Intent::Chat {
            if let Some(text) = crate::agent::offline_explain(&st.paths, &body.message) {
                return Ok(sse_offline_chat(text));
            }
        }
        if intent == crate::agent::Intent::Chat {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "未配置 API Key。在 demo/.env 写入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY。",
            ));
        }
        // Run/Both: exclusive steps do not need a live model.
    }
    let session = if body.session_id.is_empty() {
        let raw = Uuid::new_v4().simple().to_string();
        raw.chars().take(12).collect()
    } else {
        body.session_id.clone()
    };
    let _ = std::fs::create_dir_all(&st.paths.out_root);
    let mut ids: Vec<String> = body
        .expert_ids
        .iter()
        .filter(|i| store::get_expert(&st.paths, i).is_some())
        .cloned()
        .collect();
    if ids.is_empty() {
        ids = store::resolve_mentions(&st.paths, &body.message);
    }
    let mut history = Vec::new();
    for item in body.history.iter().rev().take(80).collect::<Vec<_>>().into_iter().rev() {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = item.get("content").and_then(|v| v.as_str());
        if matches!(role, "user" | "assistant") {
            if let Some(c) = content {
                history.push(json!({"role": role, "content": c}));
            }
        }
    }
    let user_text = if body.attachments.is_empty() {
        body.message.clone()
    } else {
        attach::bundle_for_prompt(&st.paths, &session, &body.attachments, &body.message)
    };
    history.push(json!({"role": "user", "content": user_text}));
    let (history, ctx_report) = crate::context::prepare_history(history);

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);
    let st2 = st.clone();
    let llm = st.llm.clone();
    let confirm_ok = body.confirm_ok;
    tokio::spawn(async move {
        let send = |ev: agent::EventOut| {
            let (name, data) = ev;
            let payload = serde_json::to_string(&data).unwrap_or_else(|_| "{}".into());
            Event::default().event(name).data(payload)
        };
        let result = async {
            let ctx_ev = (
                "context".into(),
                ctx_report.to_value(),
            );
            if tx.send(Ok(send(ctx_ev))).await.is_err() {
                return Ok(());
            }
            if ctx_report.compressed {
                let ev = (
                    "status".into(),
                    json!({"phase": "compress", "text": ctx_report.note()}),
                );
                if tx.send(Ok(send(ev))).await.is_err() {
                    return Ok(());
                }
            }
            if ids.is_empty() {
                let evs = agent::run_plain(history, &llm).await?;
                for ev in evs {
                    if tx.send(Ok(send(ev))).await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            let last_user = history
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
                .and_then(|m| m.get("content").and_then(|v| v.as_str()))
                .unwrap_or("");
            if agent::is_packish(last_user) {
                let args = json!({
                    "brief": last_user,
                    "tender_text": last_user,
                    "project_name": last_user.chars().take(40).collect::<String>(),
                    "confirm_ok": confirm_ok,
                });
                let run_v = crate::firm::run_bid_job(&st2.paths, &session, &args);
                let evs = vec![
                    (
                        "status".into(),
                        json!({"phase": "harness", "text": "一人公司成套 · harness steps（一次，不按专家重复）"}),
                    ),
                    ("token".into(), json!({"text": run_v.to_string()})),
                    (
                        "done".into(),
                        json!({
                            "mode": "firm",
                            "harness": true,
                            "runtime": run_v.get("mode").cloned().unwrap_or(json!("steps")),
                            "text": run_v.to_string(),
                            "citations": [],
                            "deliverables": run_v.get("files").cloned().unwrap_or(json!([])),
                        }),
                    ),
                ];
                for ev in evs {
                    if tx.send(Ok(send(ev))).await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            let n = ids.len();
            for (i, eid) in ids.iter().enumerate() {
                let Some(exp) = store::get_expert(&st2.paths, eid) else {
                    continue;
                };
                if n > 1 {
                    let ev = (
                        "status".into(),
                        json!({"phase": "queue", "text": format!("独立专家 {}/{}：{}", i + 1, n, exp.name)}),
                    );
                    if tx.send(Ok(send(ev))).await.is_err() {
                        break;
                    }
                }
                match agent::run_expert(&st2.paths, &exp, history.clone(), confirm_ok, &session, &llm).await {
                    Ok(evs) => {
                        for ev in evs {
                            if tx.send(Ok(send(ev))).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        let ev = ("error".into(), json!({"text": e.to_string()}));
                        let _ = tx.send(Ok(send(ev))).await;
                    }
                }
            }
            Ok::<(), llm::LlmError>(())
        }
        .await;
        if let Err(e) = result {
            let ev = Event::default()
                .event("error")
                .data(json!({"text": e.to_string()}).to_string());
            let _ = tx.send(Ok(ev)).await;
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn percent_encode_utf8(name: &str) -> String {
    // RFC 5987 / 8187: attachment; filename*=UTF-8''%E4%B8%93... — raw UTF-8 in filename= garbles on Safari/iOS
    let mut out = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn guess_media_type(name: &str) -> &'static str {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" => "text/markdown; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "pdf" => "application/pdf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}

async fn file_get(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    let raw = q.get("path").cloned().unwrap_or_default();
    let inline = matches!(q.get("inline").map(|s| s.as_str()), Some("1") | Some("true"));
    let target = PathBuf::from(&raw);
    // relative paths are resolved under out_root so links need not leak absolute server paths
    let target = if target.is_absolute() { target } else { st.paths.out_root.join(target) };
    let target = target.canonicalize().map_err(|_| err(StatusCode::NOT_FOUND, "missing"))?;
    let root = st
        .paths
        .out_root
        .canonicalize()
        .map_err(|_| err(StatusCode::FORBIDDEN, "not a deliverable"))?;
    if !target.starts_with(&root) {
        return Err(err(StatusCode::FORBIDDEN, "not a deliverable"));
    }
    if !target.is_file() {
        return Err(err(StatusCode::NOT_FOUND, "missing"));
    }
    let bytes = tokio::fs::read(&target)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "missing"))?;
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let media = guess_media_type(name);
    let text_like = media.starts_with("text/") || media.starts_with("application/json");
    let disposition = if inline && text_like { "inline" } else { "attachment" };
    let mut headers = axum::http::HeaderMap::new();
    let content_type = if inline && text_like { "text/plain; charset=utf-8" } else { media };
    headers.insert(axum::http::header::CONTENT_TYPE, content_type.parse().unwrap());
    let ascii_fallback: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if let Ok(v) = format!(
        "{disposition}; filename=\"{ascii_fallback}\"; filename*=UTF-8''{}",
        percent_encode_utf8(name)
    )
    .parse()
    {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    Ok((headers, bytes).into_response())
}
