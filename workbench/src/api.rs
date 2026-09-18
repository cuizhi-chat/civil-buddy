use crate::agent::{self, LlmMode};
use crate::attach;
use crate::config::{llm_model, Paths};
use crate::kbio::{self, MAX_FILE_BYTES};
use crate::llm;
use crate::rag::list_kb;
use crate::store;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
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
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use crate::threads;
use tower_http::services::ServeDir;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub paths: Paths,
    pub llm: LlmMode,
    pub force_has_key: Option<bool>,
    /// Shared secret for /api/* when the workbench is exposed on a LAN (CIVIL_TOKEN). None = open.
    pub auth_token: Option<String>,
}

impl AppState {
    pub fn live(paths: Paths) -> Self {
        Self {
            paths,
            llm: LlmMode::Live,
            force_has_key: None,
            auth_token: auth_token(),
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
        .route("/api/config", get(config_get).post(config_set))
        .route("/api/skills", get(skills_list))
        .route("/api/mcp/capabilities", get(mcp_capabilities))
        .route("/api/mcp/resources", get(mcp_resources))
        .route("/api/mcp/resources/read", post(mcp_resource_read))
        .route("/api/mcp/prompts", get(mcp_prompts))
        .route("/api/mcp/prompts/get", post(mcp_prompt_get))
        .route("/api/mcp/tools", get(mcp_tools))
        .route("/api/mcp/tools/call", post(mcp_tool_call))
        .route("/api/threads", get(threads_list).post(threads_run))
        .route("/api/threads/{thread_id}", get(thread_one).delete(thread_delete))
        .route("/api/threads/{thread_id}/messages", get(thread_messages))
        .route("/api/threads/{thread_id}/files", get(thread_files))
        .route("/api/threads/{thread_id}/cancel", post(thread_cancel))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(middleware::from_fn_with_state(state.auth_token.clone(), require_token))
        .with_state(Arc::new(state))
}

/// Optional shared-secret gate: set CIVIL_TOKEN when the workbench is bound to a LAN address.
/// Accepts `Authorization: Bearer`, `?token=`, or the `cb_token` cookie (downloads are plain links).
/// `/` and `/static/*` stay open so the page can load and ask for the token.
pub fn auth_token() -> Option<String> {
    std::env::var("CIVIL_TOKEN").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn token_presented(req: &Request) -> Option<String> {
    let h = req.headers();
    if let Some(v) = h.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = v.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    }
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            if let Some(t) = pair.strip_prefix("token=") {
                return Some(t.to_string());
            }
        }
    }
    for c in h.get_all(axum::http::header::COOKIE) {
        let Ok(raw) = c.to_str() else { continue };
        for part in raw.split(';') {
            if let Some(t) = part.trim().strip_prefix("cb_token=") {
                return Some(t.trim().to_string());
            }
        }
    }
    None
}

async fn require_token(State(expected): State<Option<String>>, req: Request, next: Next) -> Response {
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if !path.starts_with("/api/") || path == "/api/health" {
        return next.run(req).await;
    }
    match token_presented(&req) {
        Some(t) if t == expected => next.run(req).await,
        _ => err(StatusCode::UNAUTHORIZED, "需要访问口令（CIVIL_TOKEN）").into_response(),
    }
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
            "threads": true,
            "thread_messages": true,
            "thread_files": true,
            "cancel": true,
            "config": true,
            "skills": true,
            "mcp": true,
            "heartbeat": true,
            "file_events": true,
            "detach": true,
            "auth": st.auth_token.is_some(),
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
    #[serde(default)]
    thread_id: String,
}

// ---------- skills catalog (.agents/skills/<id>/SKILL.md), same rows as packing_assistant/runtime/expert_skills.catalog ----------

fn skill_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--")
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn frontmatter_field(text: &str, key: &str) -> String {
    // minimal YAML: top-level `key: value` inside the leading --- block; quotes stripped
    let Some(rest) = text.strip_prefix("---") else { return String::new() };
    let Some(end) = rest.find("\n---") else { return String::new() };
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix(&format!("{key}:")) {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
            let v = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).unwrap_or(v);
            return v.replace("\\\"", "\"");
        }
    }
    String::new()
}

fn skills_catalog(paths: &Paths) -> Vec<Value> {
    let dir = paths.repo_root.join(".agents").join("skills");
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| !n.starts_with('.') && n != "civil-buddy" && skill_id_valid(n))
        .collect();
    names.sort();
    let mut rows = Vec::new();
    for id in names {
        let p = dir.join(&id).join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let name = frontmatter_field(&text, "name");
        let desc: String = frontmatter_field(&text, "description").chars().take(500).collect();
        rows.push(json!({
            "name": if name.is_empty() { id.clone() } else { name },
            "description": desc,
            "path": p.to_string_lossy(),
        }));
    }
    rows
}

async fn skills_list(State(st): State<Arc<AppState>>) -> Json<Value> {
    let rows = skills_catalog(&st.paths);
    Json(json!({"ok": true, "n": rows.len(), "skills": rows, "host": "civil-workbench"}))
}

// ---------- MCP surface over HTTP: thin wrappers around mcp::handle_rpc (same filter rules as civil-mcp) ----------

fn mcp_filter(st: &AppState, expert_id: &str) -> Result<crate::mcp::McpFilter, ApiError> {
    let eid = expert_id.trim();
    if eid.is_empty() {
        return Ok(crate::mcp::McpFilter { pack: None, expert: None });
    }
    if let Some(exp) = store::get_expert(&st.paths, eid) {
        return Ok(crate::mcp::McpFilter { pack: Some(exp.category), expert: Some(exp.id) });
    }
    if crate::packs::valid_pack(eid) {
        return Ok(crate::mcp::McpFilter { pack: Some(eid.to_string()), expert: None });
    }
    Err(err(StatusCode::NOT_FOUND, "unknown expert"))
}

fn mcp_rpc(st: &AppState, filter: &crate::mcp::McpFilter, method: &str, params: Value) -> Result<Value, ApiError> {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let reply = crate::mcp::handle_rpc(&st.paths, filter, msg).ok_or_else(|| err(StatusCode::BAD_REQUEST, "no reply"))?;
    if let Some(e) = reply.get("error") {
        let m = e.get("message").and_then(|v| v.as_str()).unwrap_or("mcp error");
        return Err(err(StatusCode::BAD_REQUEST, m));
    }
    Ok(reply.get("result").cloned().unwrap_or(json!({})))
}

fn q_expert(q: &HashMap<String, String>) -> String {
    q.get("expert_id").cloned().unwrap_or_default()
}

async fn mcp_capabilities(State(st): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let filter = crate::mcp::McpFilter { pack: None, expert: None };
    let r = mcp_rpc(&st, &filter, "initialize", json!({"protocolVersion": "2024-11-05"}))?;
    Ok(Json(json!({"ok": true, "capabilities": r.get("capabilities").cloned().unwrap_or(json!({})), "serverInfo": r.get("serverInfo").cloned().unwrap_or(json!({}))})))
}

async fn mcp_resources(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, ApiError> {
    let eid = q_expert(&q);
    if eid.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "unknown expert"));
    }
    let filter = mcp_filter(&st, &eid)?;
    let r = mcp_rpc(&st, &filter, "resources/list", json!({}))?;
    Ok(Json(json!({"ok": true, "resources": r.get("resources").cloned().unwrap_or(json!([]))})))
}

#[derive(Deserialize)]
struct McpResourceIn {
    uri: String,
    #[serde(default = "default_bid_parse")]
    expert_id: String,
}

fn default_bid_parse() -> String {
    "bid-parse".into()
}

async fn mcp_resource_read(State(st): State<Arc<AppState>>, Json(body): Json<McpResourceIn>) -> Result<Json<Value>, ApiError> {
    let filter = mcp_filter(&st, &body.expert_id)?;
    let mut r = mcp_rpc(&st, &filter, "resources/read", json!({"uri": body.uri}))?;
    r["ok"] = json!(true);
    Ok(Json(r))
}

async fn mcp_prompts(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, ApiError> {
    let filter = mcp_filter(&st, &q_expert(&q))?;
    let r = mcp_rpc(&st, &filter, "prompts/list", json!({}))?;
    Ok(Json(json!({"ok": true, "prompts": r.get("prompts").cloned().unwrap_or(json!([]))})))
}

#[derive(Deserialize)]
struct McpPromptIn {
    name: String,
    #[serde(default = "default_bid_parse")]
    expert_id: String,
    #[serde(default)]
    arguments: Value,
}

async fn mcp_prompt_get(State(st): State<Arc<AppState>>, Json(body): Json<McpPromptIn>) -> Result<Json<Value>, ApiError> {
    let filter = mcp_filter(&st, &body.expert_id)?;
    let args = if body.arguments.is_object() { body.arguments } else { json!({}) };
    let mut r = mcp_rpc(&st, &filter, "prompts/get", json!({"name": body.name, "arguments": args}))?;
    r["ok"] = json!(true);
    Ok(Json(r))
}

async fn mcp_tools(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, ApiError> {
    let filter = mcp_filter(&st, &q_expert(&q))?;
    let r = mcp_rpc(&st, &filter, "tools/list", json!({}))?;
    Ok(Json(json!({"ok": true, "tools": r.get("tools").cloned().unwrap_or(json!([]))})))
}

#[derive(Deserialize)]
struct McpToolIn {
    name: String,
    #[serde(default)]
    expert_id: String,
    #[serde(default)]
    arguments: Value,
}

async fn mcp_tool_call(State(st): State<Arc<AppState>>, Json(body): Json<McpToolIn>) -> Result<Json<Value>, ApiError> {
    let filter = mcp_filter(&st, &body.expert_id)?;
    let args = if body.arguments.is_object() { body.arguments } else { json!({}) };
    let mut r = mcp_rpc(&st, &filter, "tools/call", json!({"name": body.name, "arguments": args}))?;
    r["ok"] = json!(true);
    Ok(Json(r))
}

// ---------- config (sandbox / approval), same modes as packing_assistant/runtime/civil_config.py ----------

const SANDBOX_MODES: &[&str] = &["read-only", "workspace-write"];
const APPROVAL_MODES: &[&str] = &["untrusted", "on-request", "never"];
const CONFIRM_SENTENCE: &str = "我明白，将由持证人员签认";

fn config_value() -> Value {
    let pick = |var: &str, modes: &[&str], default: &str| -> String {
        let raw = std::env::var(var).unwrap_or_default();
        let v = raw.trim().to_ascii_lowercase();
        if modes.contains(&v.as_str()) { v } else { default.into() }
    };
    json!({
        "sandbox": pick("CIVIL_SANDBOX", SANDBOX_MODES, "workspace-write"),
        "approval": pick("CIVIL_APPROVAL", APPROVAL_MODES, "on-request"),
        "max_steps": crate::config::max_agent_steps(),
        "max_parallel": std::env::var("CIVIL_MAX_PARALLEL").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(4).clamp(1, 8),
        "model": llm_model(),
        "job_root": std::env::var("CIVIL_JOB_ROOT").unwrap_or_default(),
        "confirm_sentence": CONFIRM_SENTENCE,
        "sandbox_modes": SANDBOX_MODES,
        "approval_modes": APPROVAL_MODES,
    })
}

async fn config_get() -> Json<Value> {
    let mut v = config_value();
    v["ok"] = json!(true);
    Json(v)
}

#[derive(Deserialize)]
struct PolicyIn {
    #[serde(default)]
    sandbox: String,
    #[serde(default)]
    approval: String,
}

async fn config_set(Json(body): Json<PolicyIn>) -> Result<Json<Value>, ApiError> {
    let sb = body.sandbox.trim().to_ascii_lowercase();
    let ap = body.approval.trim().to_ascii_lowercase();
    if !sb.is_empty() && !SANDBOX_MODES.contains(&sb.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "bad sandbox"));
    }
    if !ap.is_empty() && !APPROVAL_MODES.contains(&ap.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "bad approval"));
    }
    // process-wide, like the Python reference; single-operator tool
    if !sb.is_empty() {
        std::env::set_var("CIVIL_SANDBOX", &sb);
    }
    if !ap.is_empty() {
        std::env::set_var("CIVIL_APPROVAL", &ap);
    }
    let mut v = config_value();
    v["ok"] = json!(true);
    Ok(Json(v))
}

// ---------- threads ----------

async fn threads_list(State(st): State<Arc<AppState>>) -> Json<Value> {
    let rows: Vec<Value> = threads::list(&st.paths)
        .into_iter()
        .filter_map(|t| serde_json::to_value(t).ok())
        .collect();
    Json(json!({"ok": true, "n": rows.len(), "threads": rows}))
}

#[derive(Deserialize)]
struct ThreadIn {
    #[serde(default)]
    text: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    skill: String,
    #[serde(default)]
    confirm_ok: bool,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    thread_id: String,
}

/// Run one turn on a thread without a live stream (the `并行` button / a scripted client).
/// Returns the reply, artifacts and final state; the transcript is appended like /api/chat.
async fn run_thread_turn(st: Arc<AppState>, thread_id: String, text: String, skill: String, confirm: bool) -> Value {
    let paths = st.paths.clone();
    let Some(mut th) = threads::load(&paths, &thread_id) else {
        return json!({"ok": false, "error": "unknown thread", "thread_id": thread_id});
    };
    threads::clear_cancel(&thread_id);
    threads::mark_running(&thread_id, true);
    th.state = "running".into();
    th.last_text = text.clone();
    let _ = threads::save(&paths, &mut th);
    let _ = threads::append_message(&paths, &thread_id, "user", &text, json!({}));
    let session = th.session_id.clone();
    let history = vec![json!({"role": "user", "content": text})];
    let mut ids: Vec<String> = if !skill.is_empty() && store::get_expert(&paths, &skill).is_some() {
        vec![skill.clone()]
    } else {
        store::resolve_mentions(&paths, &text)
    };
    if ids.is_empty() && !skill.is_empty() {
        ids = vec![];
    }
    let live = agent::Live::none();
    let result = if ids.is_empty() {
        agent::run_plain(history, &st.llm, &live).await
    } else {
        let exp = store::get_expert(&paths, &ids[0]).expect("checked above");
        agent::run_expert(&paths, &exp, history, confirm || th.confirm, &session, &st.llm, &live).await
    };
    let mut cur = threads::load(&paths, &thread_id).unwrap_or(th);
    let out = match result {
        Ok(evs) => {
            let done = evs.iter().rev().find(|(n, _)| n == "done").map(|(_, d)| d.clone()).unwrap_or(json!({}));
            let reply = done.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let files = done.get("deliverables").cloned().unwrap_or(json!([]));
            let _ = threads::append_message(&paths, &thread_id, "assistant", &reply, json!({
                "expert": done.get("expert").cloned().unwrap_or(json!("")),
                "citations": done.get("citations").cloned().unwrap_or(json!([])),
                "deliverables": files.clone(),
            }));
            cur.skill = ids.first().cloned().unwrap_or_default();
            cur.last_reply = reply.clone();
            cur.wrote = files.as_array().map(|a| !a.is_empty()).unwrap_or(false);
            cur.artifacts = files
                .as_array()
                .map(|a| a.iter().filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(String::from)).collect())
                .unwrap_or_default();
            cur.hitl_pending = done.pointer("/hitl/pending").and_then(|v| v.as_bool()).unwrap_or(false);
            cur.state = if cur.hitl_pending { "waiting_hitl".into() } else { "done".into() };
            cur.error = String::new();
            json!({"ok": true, "thread_id": thread_id, "reply": reply, "files": files, "skill": cur.skill, "state": cur.state})
        }
        Err(e) => {
            cur.state = if e.is_stopped() { "cancelled".into() } else { "failed".into() };
            cur.error = e.to_string();
            cur.last_reply = e.to_string();
            let _ = threads::append_message(&paths, &thread_id, "assistant", &format!("（中断：{e}）"), json!({"error": e.to_string()}));
            json!({"ok": false, "thread_id": thread_id, "error": e.to_string(), "state": cur.state})
        }
    };
    cur.cancel_requested = false;
    let _ = threads::save(&paths, &mut cur);
    threads::mark_running(&thread_id, false);
    threads::clear_cancel(&thread_id);
    out
}

async fn threads_run(State(st): State<Arc<AppState>>, Json(body): Json<ThreadIn>) -> Result<Json<Value>, ApiError> {
    let text = body.text.trim().to_string();
    let tid = body.thread_id.trim().to_string();
    let th = if tid.is_empty() {
        let title = if body.title.trim().is_empty() { text.chars().take(40).collect::<String>() } else { body.title.trim().to_string() };
        threads::new_thread(&st.paths, &title, body.confirm_ok).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
    } else {
        threads::load(&st.paths, &tid).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown thread"))?
    };
    if text.is_empty() {
        let mut v = serde_json::to_value(&th).unwrap_or(json!({}));
        v["ok"] = json!(true);
        return Ok(Json(v));
    }
    if !st.has_key() {
        return Err(err(StatusCode::BAD_REQUEST, "未配置 API Key。在 demo/.env 写入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY。"));
    }
    let thread_id = th.thread_id.clone();
    if body.background {
        threads::mark_running(&thread_id, true);
        let st2 = st.clone();
        let tid2 = thread_id.clone();
        tokio::spawn(async move {
            let _ = run_thread_turn(st2, tid2, text, body.skill, body.confirm_ok).await;
        });
        return Ok(Json(json!({"ok": true, "background": true, "thread_id": thread_id, "session_id": th.session_id, "state": "running"})));
    }
    Ok(Json(run_thread_turn(st, thread_id, text, body.skill, body.confirm_ok).await))
}

async fn thread_one(State(st): State<Arc<AppState>>, AxPath(thread_id): AxPath<String>) -> Result<Json<Value>, ApiError> {
    threads::status(&st.paths, &thread_id)
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown thread"))
}

async fn thread_messages(
    State(st): State<Arc<AppState>>,
    AxPath(thread_id): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let th = threads::load(&st.paths, &thread_id).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown thread"))?;
    let limit = q.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(threads::MAX_MESSAGES);
    let mut v = serde_json::to_value(&th).unwrap_or(json!({}));
    v["ok"] = json!(true);
    v["messages"] = json!(threads::load_messages(&st.paths, &thread_id, limit));
    Ok(Json(v))
}

async fn thread_files(State(st): State<Arc<AppState>>, AxPath(thread_id): AxPath<String>) -> Result<Json<Value>, ApiError> {
    let th = threads::load(&st.paths, &thread_id).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown thread"))?;
    Ok(Json(json!({
        "ok": true,
        "thread_id": thread_id,
        "session_id": th.session_id,
        "files": threads::session_files(&st.paths, &th.session_id),
    })))
}

async fn thread_cancel(State(st): State<Arc<AppState>>, AxPath(thread_id): AxPath<String>) -> Result<Json<Value>, ApiError> {
    threads::request_cancel(&st.paths, &thread_id)
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown thread"))
}

async fn thread_delete(State(st): State<Arc<AppState>>, AxPath(thread_id): AxPath<String>) -> Result<Json<Value>, ApiError> {
    if threads::delete(&st.paths, &thread_id) {
        Ok(Json(json!({"ok": true, "thread_id": thread_id})))
    } else {
        Err(err(StatusCode::NOT_FOUND, "unknown thread"))
    }
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
    // one thread per conversation; the session (= deliverable folder) follows the thread
    let mut th = match threads::load(&st.paths, body.thread_id.trim()) {
        Some(t) => t,
        None => threads::new_thread(&st.paths, &body.message.chars().take(40).collect::<String>(), body.confirm_ok)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?,
    };
    let thread_id = th.thread_id.clone();
    if threads::is_running(&thread_id) {
        // a detached run (client dropped) is still producing on this thread: let it finish first
        return Err(err(StatusCode::CONFLICT, "上一条还在生成中（连接断开后服务端继续跑）；稍等，完成后会自动同步。"));
    }
    let session = if !th.session_id.is_empty() {
        th.session_id.clone()
    } else if !body.session_id.is_empty() {
        body.session_id.clone()
    } else {
        Uuid::new_v4().simple().to_string().chars().take(12).collect()
    };
    threads::clear_cancel(&thread_id);
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
    let mut history: Vec<Value> = Vec::new();
    for item in body.history.iter().rev().take(80).collect::<Vec<_>>().into_iter().rev() {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(role, "user" | "assistant") || content.is_empty() {
            continue;
        }
        // never send two consecutive user turns (a broken stream leaves one behind)
        if role == "user" {
            if let Some(last) = history.last_mut() {
                if last.get("role").and_then(|v| v.as_str()) == Some("user") {
                    let merged = format!("{}\n{}", last.get("content").and_then(|v| v.as_str()).unwrap_or(""), content);
                    *last = json!({"role": "user", "content": merged});
                    continue;
                }
            }
        }
        history.push(json!({"role": role, "content": content}));
    }
    let user_text = if body.attachments.is_empty() {
        body.message.clone()
    } else {
        attach::bundle_for_prompt(&st.paths, &session, &body.attachments, &body.message)
    };
    if history.last().and_then(|m| m.get("role")).and_then(|v| v.as_str()) == Some("user") {
        let last = history.pop().unwrap_or(json!({}));
        let merged = format!("{}\n{}", last.get("content").and_then(|v| v.as_str()).unwrap_or(""), user_text);
        history.push(json!({"role": "user", "content": merged}));
    } else {
        history.push(json!({"role": "user", "content": user_text}));
    }
    let (history, ctx_report) = crate::context::prepare_history(history);
    let mut ctx_value = ctx_report.to_value();
    ctx_value["thread_id"] = json!(thread_id);
    ctx_value["session_id"] = json!(session);
    ctx_value["experts"] = json!(ids);

    let _ = threads::append_message(
        &st.paths,
        &thread_id,
        "user",
        &body.message,
        json!({"attachments": body.attachments, "experts": ids}),
    );
    threads::mark_running(&thread_id, true);
    th.state = "running".into();
    th.last_text = body.message.clone();
    let _ = threads::save(&st.paths, &mut th);

    // live sink: every event goes out the moment the agent produces it
    let (tx, rx) = mpsc::unbounded_channel::<agent::EventOut>();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let live = agent::Live { sink: Some(tx.clone()), stop: Some(stop.clone()) };
    let stop_for_drop = stop.clone();
    // POST /api/threads/{id}/cancel writes the registry; mirror it into the run's stop flag
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop_w = stop.clone();
        let fin_w = finished.clone();
        let tid_w = thread_id.clone();
        tokio::spawn(async move {
            while !fin_w.load(std::sync::atomic::Ordering::Relaxed) {
                if threads::cancel_requested(&tid_w) {
                    stop_w.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
    }
    let st2 = st.clone();
    let llm = st.llm.clone();
    let confirm_ok = body.confirm_ok;
    let tid2 = thread_id.clone();
    let paths2 = st.paths.clone();
    let ctx_for_task = ctx_value.clone();
    tokio::spawn(async move {
        let send = |ev: agent::EventOut| {
            let _ = tx.send(ev);
        };
        send(("context".into(), ctx_for_task));
        if ctx_report.compressed {
            send(("status".into(), json!({"phase": "compress", "text": ctx_report.note()})));
        }
        let mut final_state = "done".to_string();
        let record = |evs: &[agent::EventOut], final_state: &mut String| {
            for (name, data) in evs {
                if name == "done" {
                    let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        let _ = threads::append_message(&paths2, &tid2, "assistant", text, json!({
                            "expert": data.get("expert").cloned().unwrap_or(json!("")),
                            "citations": data.get("citations").cloned().unwrap_or(json!([])),
                            "deliverables": data.get("deliverables").cloned().unwrap_or(json!([])),
                            "stopped": data.get("stopped").cloned().unwrap_or(json!(false)),
                        }));
                    }
                    if data.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false) {
                        *final_state = "cancelled".into();
                    }
                }
            }
        };
        let result: Result<(), llm::LlmError> = async {
            let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed) || threads::cancel_requested(&tid2);
            if ids.is_empty() {
                let evs = agent::run_plain(history, &llm, &live).await?;
                record(&evs, &mut final_state);
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
                send(("status".into(), json!({"phase": "harness", "text": "一人公司成套 · harness steps（一次，不按专家重复）"})));
                if let Some(files) = run_v.get("files") {
                    send(("file".into(), json!({"deliverables": files})));
                }
                send(("token".into(), json!({"text": run_v.to_string()})));
                let done = json!({
                    "mode": "firm",
                    "harness": true,
                    "runtime": run_v.get("mode").cloned().unwrap_or(json!("steps")),
                    "text": run_v.to_string(),
                    "citations": [],
                    "deliverables": run_v.get("files").cloned().unwrap_or(json!([])),
                });
                record(&[("done".into(), done.clone())], &mut final_state);
                send(("done".into(), done));
                return Ok(());
            }
            let n = ids.len();
            for (i, eid) in ids.iter().enumerate() {
                if stopped() {
                    final_state = "cancelled".into();
                    send(("status".into(), json!({"phase": "stopped", "text": "已按要求停止"})));
                    break;
                }
                let Some(exp) = store::get_expert(&st2.paths, eid) else {
                    continue;
                };
                if n > 1 {
                    send(("status".into(), json!({"phase": "queue", "text": format!("独立专家 {}/{}：{}", i + 1, n, exp.name)})));
                }
                match agent::run_expert(&st2.paths, &exp, history.clone(), confirm_ok, &session, &llm, &live).await {
                    Ok(evs) => record(&evs, &mut final_state),
                    Err(e) if e.is_stopped() => {
                        final_state = "cancelled".into();
                        send(("status".into(), json!({"phase": "stopped", "text": "已按要求停止"})));
                        send(("done".into(), json!({"mode": "expert", "expert": exp.id, "text": "", "citations": [], "deliverables": [], "stopped": true})));
                        break;
                    }
                    Err(e) => {
                        final_state = "failed".into();
                        let _ = threads::append_message(&paths2, &tid2, "assistant", &format!("（中断：{e}）"), json!({"expert": exp.id, "error": e.to_string()}));
                        send(("error".into(), json!({"text": e.to_string(), "expert": exp.id, "recoverable": true, "deliverables": threads::session_files(&paths2, &session)})));
                    }
                }
            }
            Ok::<(), llm::LlmError>(())
        }
        .await;
        if let Err(e) = result {
            final_state = if e.is_stopped() { "cancelled".into() } else { "failed".into() };
            let _ = threads::append_message(&paths2, &tid2, "assistant", &format!("（中断：{e}）"), json!({"error": e.to_string()}));
            send(("error".into(), json!({"text": e.to_string(), "recoverable": true})));
        }
        if let Some(mut cur) = threads::load(&paths2, &tid2) {
            cur.state = final_state;
            cur.cancel_requested = false;
            let _ = threads::save(&paths2, &mut cur);
        }
        threads::mark_running(&tid2, false);
        threads::clear_cancel(&tid2);
        finished.store(true, std::sync::atomic::Ordering::Relaxed);
        // dropping `tx` here closes the SSE stream
    });

    let stop_on_drop = StopOnDrop(stop_for_drop);
    let stream = UnboundedReceiverStream::new(rx).map(move |(name, data)| {
        let _keep = &stop_on_drop; // moved into the stream: dropped (client gone) => stop flag raised
        let payload = serde_json::to_string(&data).unwrap_or_else(|_| "{}".into());
        Ok::<Event, Infallible>(Event::default().event(name).data(payload))
    });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(sse_ping_interval()).text("ping"))
        .into_response())
}

/// SSE heartbeat interval while the model is silent (CIVIL_SSE_PING_SEC, default 15 s; same knob as demo/app.py).
fn sse_ping_interval() -> std::time::Duration {
    let secs = std::env::var("CIVIL_SSE_PING_SEC")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| *n > 0.0)
        .unwrap_or(15.0);
    std::time::Duration::from_secs_f64(secs)
}

/// Raises the run's stop flag when the SSE stream is dropped — only under CIVIL_DETACH=stop.
/// Default (`continue`): a client that drops mid-run (phone lock screen, wifi blip) does not cancel the
/// run; it finishes, the transcript gets the reply, and the page re-syncs on return.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

fn detach_stops_run() -> bool {
    std::env::var("CIVIL_DETACH").map(|v| v.trim().eq_ignore_ascii_case("stop")).unwrap_or(false)
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if detach_stops_run() {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
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
