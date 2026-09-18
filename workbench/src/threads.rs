//! Conversation threads, shared on disk with the Python workbench.
//!
//! Layout (identical to `packing_assistant/runtime/threads.py`):
//!   <out_root>/_threads/<thread_id>.json            thread record
//!   <out_root>/_threads/<thread_id>.messages.jsonl  transcript, one row per turn
//! Either backend can open a thread the other one wrote.

use crate::config::Paths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_MESSAGES: usize = 400;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CivilThread {
    pub thread_id: String,
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub skill: String,
    #[serde(default = "idle")]
    pub state: String,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub last_text: String,
    #[serde(default)]
    pub last_reply: String,
    #[serde(default)]
    pub hitl_pending: bool,
    #[serde(default)]
    pub wrote: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub updated_at: f64,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub n_messages: u64,
}

fn idle() -> String {
    "idle".into()
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn boot_time() -> f64 {
    static BOOT: OnceLock<f64> = OnceLock::new();
    *BOOT.get_or_init(now)
}

fn cancel_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

fn running_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.out_root.join("_threads")
}

fn safe_id(thread_id: &str) -> String {
    let s: String = thread_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    if s.is_empty() {
        "t".into()
    } else {
        s
    }
}

pub fn path(paths: &Paths, thread_id: &str) -> PathBuf {
    dir(paths).join(format!("{}.json", safe_id(thread_id)))
}

fn messages_path(paths: &Paths, thread_id: &str) -> PathBuf {
    dir(paths).join(format!("{}.messages.jsonl", safe_id(thread_id)))
}

fn atomic_write(target: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = target.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, target).map_err(|e| e.to_string())
}

pub fn save(paths: &Paths, th: &mut CivilThread) -> Result<(), String> {
    th.updated_at = now();
    let text = serde_json::to_string_pretty(th).map_err(|e| e.to_string())?;
    atomic_write(&path(paths, &th.thread_id), &text)
}

pub fn load(paths: &Paths, thread_id: &str) -> Option<CivilThread> {
    let raw = fs::read_to_string(path(paths, thread_id)).ok()?;
    let mut th: CivilThread = serde_json::from_str(&raw).ok()?;
    if th.thread_id.is_empty() {
        th.thread_id = thread_id.to_string();
    }
    if th.session_id.is_empty() {
        th.session_id = th.thread_id.clone();
    }
    Some(th)
}

pub fn new_thread(paths: &Paths, title: &str, confirm: bool) -> Result<CivilThread, String> {
    let tid = format!("t-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let title = title.trim();
    let mut th = CivilThread {
        thread_id: tid.clone(),
        session_id: tid,
        title: if title.is_empty() { "新对话".into() } else { title.chars().take(80).collect() },
        skill: String::new(),
        state: "idle".into(),
        confirm,
        last_text: String::new(),
        last_reply: String::new(),
        hitl_pending: false,
        wrote: false,
        artifacts: vec![],
        error: String::new(),
        created_at: now(),
        updated_at: now(),
        cancel_requested: false,
        n_messages: 0,
    };
    save(paths, &mut th)?;
    Ok(th)
}

pub fn mark_running(thread_id: &str, on: bool) {
    let mut set = running_set().lock().unwrap_or_else(|e| e.into_inner());
    if on {
        set.insert(thread_id.to_string());
    } else {
        set.remove(thread_id);
    }
}

pub fn is_running(thread_id: &str) -> bool {
    running_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(thread_id)
}

/// Threads newest first. A `running` record older than this process with no live task
/// behind it is reported (and persisted) as `stale`: nothing can finish it any more.
pub fn list(paths: &Paths) -> Vec<CivilThread> {
    let Ok(rd) = fs::read_dir(dir(paths)) else {
        return vec![];
    };
    let mut out: Vec<CivilThread> = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.ends_with(".json") || name.contains(".tmp") {
            continue;
        }
        let stem = name.trim_end_matches(".json");
        let Some(mut th) = load(paths, stem) else {
            continue;
        };
        if th.state == "running" && !is_running(&th.thread_id) && th.updated_at < boot_time() {
            th.state = "stale".into();
            if th.error.is_empty() {
                th.error = "工作台重启，任务没有跑完；请重新发送".into();
            }
            let _ = save(paths, &mut th);
        }
        out.push(th);
    }
    out.sort_by(|a, b| b.updated_at.partial_cmp(&a.updated_at).unwrap_or(std::cmp::Ordering::Equal));
    out
}

pub fn append_message(paths: &Paths, thread_id: &str, role: &str, content: &str, extra: Value) -> Result<(), String> {
    if thread_id.is_empty() {
        return Ok(());
    }
    let mut row = json!({"role": role, "content": content, "ts": now()});
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            row[k] = v.clone();
        }
    }
    let p = messages_path(paths, thread_id);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    use std::io::Write;
    let mut fh = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| e.to_string())?;
    writeln!(fh, "{}", row).map_err(|e| e.to_string())?;
    if let Some(mut th) = load(paths, thread_id) {
        th.n_messages += 1;
        if role == "user" && (th.title.is_empty() || th.title == "新对话") {
            let t: String = content.replace('\n', " ").chars().take(40).collect();
            if !t.is_empty() {
                th.title = t;
            }
        }
        save(paths, &mut th)?;
    }
    Ok(())
}

pub fn load_messages(paths: &Paths, thread_id: &str, limit: usize) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(messages_path(paths, thread_id)) else {
        return vec![];
    };
    let mut rows: Vec<Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|v| matches!(v.get("role").and_then(|r| r.as_str()), Some("user") | Some("assistant")))
        .collect();
    let keep = limit.clamp(1, 2000);
    if rows.len() > keep {
        rows = rows.split_off(rows.len() - keep);
    }
    rows
}

pub fn delete(paths: &Paths, thread_id: &str) -> bool {
    let mut ok = false;
    for p in [path(paths, thread_id), messages_path(paths, thread_id)] {
        if fs::remove_file(p).is_ok() {
            ok = true;
        }
    }
    clear_cancel(thread_id);
    mark_running(thread_id, false);
    ok
}

pub fn request_cancel(paths: &Paths, thread_id: &str) -> Option<Value> {
    let mut th = load(paths, thread_id)?;
    cancel_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(thread_id.to_string());
    th.cancel_requested = true;
    if !is_running(thread_id) && matches!(th.state.as_str(), "idle" | "stale") {
        th.state = "cancelled".into();
    }
    let _ = save(paths, &mut th);
    Some(json!({"ok": true, "thread_id": thread_id, "state": th.state, "dropped": false}))
}

pub fn cancel_requested(thread_id: &str) -> bool {
    !thread_id.is_empty()
        && cancel_set()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(thread_id)
}

pub fn clear_cancel(thread_id: &str) {
    cancel_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(thread_id);
}

/// Every deliverable under <out_root>/<session>/<expert>/ — survives a broken stream.
pub fn session_files(paths: &Paths, session_id: &str) -> Vec<Value> {
    let sid: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if sid.len() < 4 {
        return vec![];
    }
    let root = paths.out_root.join(&sid);
    let Ok(rd) = fs::read_dir(&root) else {
        return vec![];
    };
    let mut rows: Vec<(f64, Value)> = Vec::new();
    for expert_dir in rd.flatten() {
        let ep = expert_dir.path();
        if !ep.is_dir() {
            continue;
        }
        let expert = ep.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let Ok(files) = fs::read_dir(&ep) else {
            continue;
        };
        for f in files.flatten() {
            let fp = f.path();
            let Ok(meta) = fp.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            rows.push((
                mtime,
                json!({
                    "expert": expert,
                    "name": fp.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                    "path": fp.to_string_lossy(),
                    "bytes": meta.len(),
                    "mtime": mtime,
                }),
            ));
        }
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    rows.into_iter().map(|(_, v)| v).collect()
}

pub fn status(paths: &Paths, thread_id: &str) -> Option<Value> {
    let mut th = load(paths, thread_id)?;
    let running = is_running(thread_id);
    if running {
        th.state = "running".into();
    } else if th.state == "running" && th.updated_at < boot_time() {
        th.state = "stale".into();
    }
    let mut v = serde_json::to_value(&th).ok()?;
    v["ok"] = json!(true);
    v["running"] = json!(running);
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        let tmp = std::env::temp_dir().join(format!("cb-threads-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(tmp.join("out")).unwrap();
        Paths::from_demo(tmp)
    }

    #[test]
    fn transcript_roundtrip_and_title() {
        let p = paths();
        let th = new_thread(&p, "", false).unwrap();
        assert_eq!(th.title, "新对话");
        append_message(&p, &th.thread_id, "user", "写一份临边提纲\n第二行", json!({"experts": ["construction"]})).unwrap();
        append_message(&p, &th.thread_id, "assistant", "好的", json!({"expert": "construction"})).unwrap();
        let rows = load_messages(&p, &th.thread_id, 400);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["expert"], "construction");
        let th2 = load(&p, &th.thread_id).unwrap();
        assert_eq!(th2.n_messages, 2);
        assert_eq!(th2.title, "写一份临边提纲 第二行");
        assert!(delete(&p, &th.thread_id));
        assert!(load(&p, &th.thread_id).is_none());
    }

    #[test]
    fn stale_and_cancel() {
        let p = paths();
        let mut th = new_thread(&p, "orphan", false).unwrap();
        th.state = "running".into();
        save(&p, &mut th).unwrap();
        // pretend it was written before this process started
        th.updated_at = boot_time() - 10.0;
        atomic_write(&path(&p, &th.thread_id), &serde_json::to_string(&th).unwrap()).unwrap();
        let rows = list(&p);
        assert_eq!(rows[0].state, "stale");

        let th2 = new_thread(&p, "cancel-me", false).unwrap();
        let got = request_cancel(&p, &th2.thread_id).unwrap();
        assert_eq!(got["state"], "cancelled");
        assert!(cancel_requested(&th2.thread_id));
        clear_cancel(&th2.thread_id);
        assert!(!cancel_requested(&th2.thread_id));
        assert!(request_cancel(&p, "nope").is_none());
    }

    #[test]
    fn same_layout_as_python() {
        let p = paths();
        let th = new_thread(&p, "x", true).unwrap();
        let raw = fs::read_to_string(path(&p, &th.thread_id)).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        for key in [
            "thread_id", "session_id", "title", "skill", "state", "confirm", "last_text", "last_reply",
            "hitl_pending", "wrote", "artifacts", "error", "created_at", "updated_at", "cancel_requested", "n_messages",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }
}
