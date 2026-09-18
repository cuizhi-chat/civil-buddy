use axum::body::Body;
use axum::http::{Request, StatusCode};
use civil_workbench::agent::{build_expert_prompt, finish_if_no_deliverable, LlmMode};
use civil_workbench::api::{app, AppState};
use civil_workbench::catalog::seed;
use civil_workbench::config::Paths;
use civil_workbench::kbio::resolve_rel;
use civil_workbench::packs::{self, ToolCtx};
use civil_workbench::rag::search_kb;
use civil_workbench::tier_map;
use civil_workbench::store::resolve_mentions;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn paths() -> Paths {
    Paths::detect()
}

fn state() -> AppState {
    AppState::live(paths())
}

async fn send(st: AppState, req: Request<Body>) -> (StatusCode, String) {
    let res = app(st).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn test_index_and_static() {
    let (st, body) = send(
        state(),
        Request::builder().uri("/").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("Civil Buddy"));
    assert!(body.contains("全企业") || body.contains("任意专家") || body.contains("本岗知识"));
    assert!(body.contains("ctxBar"));
    assert!(body.contains(".xlsx"), "{body}");
    assert!(body.contains("提问不写盘") || body.contains("写一份"), "{body}");
    let (js_st, js) = send(
        state(),
        Request::builder().uri("/static/app.js").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(js_st, StatusCode::OK);
    assert!(js.contains("reloadCatalog"));
    assert!(js.contains("能聊能跑"));
}

#[tokio::test]
async fn test_catalog_sixteen() {
    let (st, body) = send(
        state(),
        Request::builder().uri("/api/catalog").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["categories"].as_array().unwrap().len(), 16);
    let experts = v["experts"].as_array().unwrap();
    assert_eq!(experts.len(), 66, "catalog must list the seed 66");
    let ids: Vec<&str> = experts.iter().filter_map(|e| e["id"].as_str()).collect();
    for need in ["interior", "facade", "civil-defense", "hydraulic", "port", "pack-ship"] {
        assert!(ids.contains(&need), "{need}");
    }
}

#[tokio::test]
async fn test_studio_tree_and_read() {
    let (st, body) = send(
        state(),
        Request::builder().uri("/api/studio/tree").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let tree: Value = serde_json::from_str(&body).unwrap();
    assert!(tree.get("company").is_some());
    assert_eq!(tree["categories"].as_array().unwrap().len(), 16);
    let (st, body) = send(
        state(),
        Request::builder()
            .uri("/api/studio/file?path=company/ask-anyone.md")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let got: Value = serde_json::from_str(&body).unwrap();
    let content = got["content"].as_str().unwrap_or("");
    assert!(content.contains("任何人都可以向你提问") || content.contains("全企业"));
}

#[test]
fn test_path_traversal_blocked() {
    let p = paths();
    assert!(resolve_rel(&p, "../secrets.md").is_none());
    assert!(resolve_rel(&p, "company/../../config.py").is_none());
}

#[tokio::test]
async fn test_studio_rejects_traversal() {
    let (st, _) = send(
        state(),
        Request::builder()
            .uri("/api/studio/file?path=../README.md")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(
        state(),
        Request::builder()
            .method("PUT")
            .uri("/api/studio/file")
            .header("content-type", "application/json")
            .body(Body::from(json!({"path":"../x.md","content":"nope"}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_kb_list_has_layers() {
    let (st, body) = send(
        state(),
        Request::builder().uri("/api/kb/structure").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let layers: std::collections::HashSet<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["layer"].as_str())
        .collect();
    assert!(layers.contains("expert"));
    assert!(layers.contains("category"));
    assert!(layers.contains("company"));
}

#[tokio::test]
async fn test_unknown_expert_404() {
    let (st, _) = send(
        state(),
        Request::builder()
            .uri("/api/kb/not-a-real-expert")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[test]
fn test_mentions_do_not_summon_every_construction_sentence() {
    let ids = resolve_mentions(&paths(), "财务上施工发票备注栏怎么写？");
    assert!(!ids.iter().any(|i| i == "construction"));
}

#[test]
fn test_explicit_mention_still_works() {
    let ids = resolve_mentions(&paths(), "请 @施工方案 写临边提纲");
    assert!(ids.iter().any(|i| i == "construction"));
    let ids2 = resolve_mentions(&paths(), "召唤危大识别：临边要不要论证");
    assert!(ids2.iter().any(|i| i == "method-hazard"));
    assert!(!ids2.iter().any(|i| i == "construction"));
}

#[tokio::test]
async fn test_studio_crud_roundtrip() {
    let path = "company/_rust_roundtrip.md";
    let (st, body) = send(
        state(),
        Request::builder()
            .method("PUT")
            .uri("/api/studio/file")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"path": path, "content": "# tmp\nhello-kb\n"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = send(
        state(),
        Request::builder()
            .uri(format!("/api/studio/file?path={path}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("hello-kb"));
    let (st, _) = send(
        state(),
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/studio/file?path={path}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(
        state(),
        Request::builder()
            .uri(format!("/api/studio/file?path={path}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_invalid_expert_id_rejected() {
    let (st, _) = send(
        state(),
        Request::builder()
            .method("POST")
            .uri("/api/studio/experts")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"id":"Bad ID","name":"x","category":"design"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_chat_plain_when_no_explicit_summon() {
    let st = AppState {
        paths: paths(),
        llm: LlmMode::FakePlain {
            text: "PLAIN".into(),
        },
        force_has_key: Some(true),
        auth_token: None,
    };
    let (code, body) = send(
        st,
        Request::builder()
            .method("POST")
            .uri("/api/chat")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"message":"配合比和发票有什么关系","expert_ids":[]}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("PLAIN"), "{body}");
    assert!(body.contains("event: context") || body.contains("\"used\""), "{body}");
}

#[test]
fn test_upload_text_roundtrip() {
    let p = paths();
    let sid = format!("uploadtest-{}", std::process::id());
    let meta = civil_workbench::attach::save_upload(
        &p,
        &sid,
        "评分点.txt",
        "技术标 40 分，必须编制临边专项方案。".as_bytes(),
    )
    .expect("save");
    assert_eq!(meta["parse"].as_str(), Some("builtin"));
    let id = meta["id"].as_str().unwrap();
    let listed = civil_workbench::attach::list_uploads(&p, &sid);
    assert!(listed.iter().any(|f| f["id"] == id));
    let text = civil_workbench::attach::read_upload(&p, &sid, id, 0, 2000).unwrap();
    assert!(text.contains("技术标"));
    let bundled = civil_workbench::attach::bundle_for_prompt(&p, &sid, &[id.to_string()], "抽出评分点");
    assert!(bundled.contains("抽出评分点"));
    assert!(bundled.contains("临边"));
}

#[tokio::test]
async fn test_kb_files_have_zh_display_names() {
    let (st, body) = send(
        state(),
        Request::builder()
            .uri("/api/kb/bid-parse")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let files = v["files"].as_array().expect("files");
    let web = files.iter().find(|f| {
        f["path"]
            .as_str()
            .unwrap_or("")
            .replace('\\', "/")
            .ends_with("bid-parse/web-knowledge.md")
    });
    let web = web.expect("bid-parse web-knowledge");
    assert_eq!(web["display"].as_str(), Some("联网核对要点"));
    assert_eq!(web["layer_label"].as_str(), Some("本岗知识"));
    let shared = files.iter().find(|f| {
        f["path"]
            .as_str()
            .unwrap_or("")
            .replace('\\', "/")
            .contains("/_shared/web-knowledge.md")
    });
    assert_eq!(
        shared.expect("shared")["layer_label"].as_str(),
        Some("大类共享")
    );
}

#[tokio::test]
async fn test_health_exposes_context_policy() {
    let (st, body) = send(
        state(),
        Request::builder().uri("/api/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["context"]["limit"].as_u64().unwrap_or(0) >= 1000);
    assert!(v["context"]["compress_at"].as_u64().unwrap_or(0) > 0);
    assert_eq!(v["context"]["compress_pct"].as_u64(), Some(70));
    assert!(v["has_key"].is_boolean(), "{body}");
    assert_eq!(v["has_key"], v["deepseek"]);
    assert_eq!(v["harness"]["default_mode"].as_str(), Some("steps"));
    assert_eq!(v["harness"]["expert_runtime"].as_str(), Some("understand"));
    assert_eq!(v["harness"]["summoned_default"].as_str(), Some("chat"));
    assert_eq!(v["parse"]["policy"].as_str(), Some("auto"));
    assert!(v["parse"].get("mineru").is_some());
}

#[test]
fn test_sixteen_categories() {
    let ids: Vec<&str> = seed().categories.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), 16);
    let set: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(set.len(), 16);
}

#[test]
fn test_qa_prompt_allows_ask_and_other_depts() {
    let exp = seed().experts.iter().find(|e| e.id == "structure").unwrap();
    let prompt = build_expert_prompt(exp, false);
    assert!(prompt.contains("不懂") || prompt.contains("提问"));
    assert!(prompt.contains("全企业任何人都可以向你提问"));
    assert!(!prompt.contains("不要只聊天不交稿"));
    assert!(prompt.contains("可以只聊天"));
    assert!(prompt.contains("改召唤那位专家"));
}

#[test]
fn test_high_risk_deliverable_still_gated() {
    let exp = seed().experts.iter().find(|e| e.id == "construction").unwrap();
    let prompt = build_expert_prompt(exp, false);
    assert!(prompt.contains("我明白，将由持证人员签认"));
    assert!(prompt.contains("纯提问（A）不受确认门阻挡"));
}

#[test]
fn test_retrieve_not_tied_to_caller_department() {
    let p = paths();
    let hits = search_kb(&p, "structure", "design", "荷载", 6);
    for h in &hits {
        assert!(["expert", "category", "company"].contains(&h.layer.as_str()));
        if h.layer == "expert" {
            assert!(h.path.starts_with("design/structure/"));
        }
    }
}

#[test]
fn test_switch_expert_hits_new_private_and_shared() {
    let p = paths();
    let a = search_kb(&p, "architecture", "design", "防火分区", 6);
    let b = search_kb(&p, "lab-mix", "lab", "配合比", 6);
    for h in a.iter().filter(|h| h.layer == "expert") {
        assert!(h.path.replace('\\', "/").contains("/architecture/"));
    }
    for h in b.iter().filter(|h| h.layer == "expert") {
        assert!(h.path.replace('\\', "/").contains("/lab-mix/"));
    }
    for h in b.iter().filter(|h| h.layer == "category") {
        assert!(h.path.replace('\\', "/").starts_with("lab/_shared/"));
    }
}

#[test]
fn test_every_expert_private_and_category_shared_nonstub() {
    let p = paths();
    assert_eq!(seed().categories.len(), 16);
    for e in &seed().experts {
        let priv_dir = p.kb_root.join(&e.category).join(&e.id);
        let shared = p.kb_root.join(&e.category).join("_shared");
        assert!(priv_dir.is_dir(), "{}", e.id);
        assert!(shared.is_dir(), "{}", e.category);
        let mut body_bytes = 0u64;
        let mut body_n = 0usize;
        if let Ok(rd) = std::fs::read_dir(&priv_dir) {
            for ent in rd.flatten() {
                let path = ent.path();
                if !path.is_file() {
                    continue;
                }
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !matches!(ext.as_str(), "md" | "txt") {
                    continue;
                }
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.eq_ignore_ascii_case("readme.md") {
                    continue;
                }
                body_n += 1;
                body_bytes += path.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        assert!(body_n > 0, "{}", e.id);
        assert!(body_bytes >= 400, "{} bytes={body_bytes}", e.id);
        assert!(shared.join("ask-from-others.md").is_file(), "{}", e.category);
        let web = priv_dir.join("web-knowledge.md");
        assert!(web.is_file(), "missing web-knowledge {}", e.id);
        let body = std::fs::read_to_string(&web).unwrap_or_default();
        assert!(body.len() >= 400, "thin web-knowledge {} {}", e.id, body.len());
        assert!(
            body.contains("http://") || body.contains("https://"),
            "no url in web-knowledge {}",
            e.id
        );
        assert!(
            body.contains("2026-08-14"),
            "web-knowledge {} missing 2026-08-14 pass",
            e.id
        );
        let shared_web = shared.join("web-knowledge.md");
        assert!(shared_web.is_file(), "missing category shared web-knowledge {}", e.category);
        let sw = std::fs::read_to_string(&shared_web).unwrap_or_default();
        assert!(sw.contains("2026-08-14") && (sw.contains("http://") || sw.contains("https://")), "{}", e.category);
    }
    let portals = p.kb_root.join("company").join("web-portals.md");
    let pb = std::fs::read_to_string(&portals).unwrap_or_default();
    assert!(pb.contains("2026-08-14") && pb.contains("APPBCA-2026-12"));
}

#[test]
fn test_construction_prompt_lists_dedicated_mcp_tools() {
    let exp = seed().experts.iter().find(|e| e.id == "construction").unwrap();
    let prompt = build_expert_prompt(exp, true);
    assert!(prompt.contains("construction__scheme_draft"));
    assert!(prompt.contains("construction__scan_forbidden"));
    assert!(prompt.contains("web-knowledge.md"));
    assert!(prompt.contains("web-portals.md"));
    assert!(prompt.contains("APPBCA-2026-12"));
    assert!(!prompt.contains("method-hazard__judge_hazard"));
    assert!(!prompt.contains("construction__judge_hazard"));
}

#[test]
fn test_pack_judge_hazard_writes_file() {
    let p = paths();
    let mut ctx = ToolCtx::new(
        p,
        "method-hazard",
        "construction",
        "high",
        true,
        "rust-pack-test",
    );
    let out = packs::execute(
        &mut ctx,
        "method-hazard__judge_hazard",
        &json!({"work_type":"临边防护","height_m":3.2,"description":"人行道栏杆"}),
    );
    assert!(out.contains("已写入"), "{out}");
    assert!(!ctx.deliverables.is_empty());
    let path = ctx.out_dir.join("危大判定书.md");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("危大"));
    assert!(!text.contains("可以开工"));
}

#[test]
fn test_high_risk_pack_blocked_without_confirm() {
    let p = paths();
    let mut ctx = ToolCtx::new(p, "construction", "construction", "high", false, "rust-gate");
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({"project_name":"滨河路","work_scope":"临边"}),
    );
    assert!(out.contains("拒绝写盘"), "{out}");
}

#[test]
fn test_mcp_tool_catalog_covers_sixteen_packs() {
    let all = packs::all_tools_filtered(None);
    let names: Vec<&str> = all.iter().map(|t| t.name).collect();
    assert!(names.contains(&"search_kb"));
    for pack in [
        "bid",
        "design",
        "bim",
        "planning",
        "construction",
        "hse",
        "commercial",
        "procurement",
        "plant",
        "lab",
        "finance",
        "docs",
        "hr",
        "admin",
        "it",
        "people",
    ] {
        let has = seed().experts.iter().filter(|e| e.category == pack).any(|e| {
            packs::visible_tool_names(&e.id).iter().any(|n| {
                !matches!(
                    n.as_str(),
                    "search_kb"
                        | "read_kb"
                        | "list_kb"
                        | "write_deliverable"
                        | "web_search"
                        | "web_open"
                        | "list_attachments"
                        | "read_attachment"
                        | "import_local"
                        | "firm__bid_pack"
                )
            })
        });
        assert!(has, "missing dedicated/category tools for {pack}");
    }
    let only = packs::all_tools_filtered(Some("construction"));
    assert!(only.iter().any(|t| t.name == "construction__scheme_draft"));
    assert!(only.iter().any(|t| t.name == "construction__scan_forbidden"));
    assert!(!only.iter().any(|t| t.name.starts_with("bid__")));
    assert!(!only.iter().any(|t| t.name == "method-hazard__judge_hazard"));
}

#[test]
fn test_three_tier_visibility_construction_vs_method_hazard() {
    let c = packs::visible_tool_names("construction");
    let h = packs::visible_tool_names("method-hazard");
    for common in [
        "search_kb",
        "read_kb",
        "list_kb",
        "write_deliverable",
        "web_search",
        "web_open",
        "list_attachments",
        "read_attachment",
        "import_local",
        "firm__bid_pack",
    ] {
        assert!(c.iter().any(|n| n == common), "{common} missing on construction");
        assert!(h.iter().any(|n| n == common), "{common} missing on method-hazard");
    }
    assert!(c.iter().any(|n| n == "construction__scan_forbidden"));
    assert!(h.iter().any(|n| n == "construction__scan_forbidden"));
    assert!(c.iter().any(|n| n == "construction__scheme_draft"));
    assert!(c.iter().any(|n| n == "construction__fill_scheme_docx"));
    assert!(!c.iter().any(|n| n == "method-hazard__judge_hazard"));
    assert!(h.iter().any(|n| n == "method-hazard__judge_hazard"));
    assert!(!h.iter().any(|n| n == "construction__scheme_draft"));
    assert!(!h.iter().any(|n| n == "construction__fill_scheme_docx"));
}

#[test]
fn test_wrong_expert_exclusive_refused() {
    let p = paths();
    let mut hazard = ToolCtx::new(p.clone(), "method-hazard", "construction", "high", true, "refuse-x");
    let out = packs::execute(
        &mut hazard,
        "construction__scheme_draft",
        &json!({"project_name":"Tuas site","work_scope":"edge protection"}),
    );
    assert!(out.contains("拒绝"), "{out}");
    assert!(out.contains("construction"), "{out}");

    let mut scheme = ToolCtx::new(p, "construction", "construction", "high", true, "refuse-h");
    let out2 = packs::execute(
        &mut scheme,
        "method-hazard__judge_hazard",
        &json!({"work_type":"临边"}),
    );
    assert!(out2.contains("拒绝"), "{out2}");
    assert!(
        packs::refuse_exclusive("construction", "method-hazard__judge_hazard").is_some()
    );
}

#[test]
fn test_compare_purchase_pm_daily_sg_titles() {
    let p = paths();
    let mut c = ToolCtx::new(p.clone(), "proc-compare", "procurement", "low", true, "iter-cmp");
    let co = packs::execute(&mut c, "proc-compare__table", &json!({"item":"rebar","vendors":"A"}));
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(c.out_dir.join("比价表草稿.md")).unwrap();
    assert!(ct.contains("GeBIZ") && ct.contains("[A001]"), "{ct}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "proc-plan", "procurement", "low", true, "iter-cmp-sib"),
        "proc-compare__table",
        &json!({"item":"x"}),
    )
    .contains("拒绝"));

    let mut pl = ToolCtx::new(p.clone(), "proc-plan", "procurement", "low", true, "iter-pp");
    let po = packs::execute(&mut pl, "proc-plan__schedule", &json!({"items":"rebar"}));
    assert!(po.contains("已写入"), "{po}");
    let pt = std::fs::read_to_string(pl.out_dir.join("采购计划表.md")).unwrap();
    assert!(pt.contains("CRS") && pt.contains("[A001]"), "{pt}");
    let mut pcn = ToolCtx::new(p.clone(), "proc-plan", "procurement", "low", true, "iter-pp-cn");
    let pco = packs::execute(&mut pcn, "proc-plan__schedule", &json!({"items":"rebar","jurisdiction":"CN"}));
    assert!(pco.contains("已写入"), "{pco}");
    let pct = std::fs::read_to_string(pcn.out_dir.join("采购计划表.md")).unwrap();
    assert!(pct.contains("辖区：CN"), "{pct}");
    assert!(!pct.contains("CRS"), "{pct}");

    let mut d = ToolCtx::new(p.clone(), "pm-daily", "people", "low", true, "iter-pmd");
    let dout = packs::execute(&mut d, "pm-daily__log", &json!({"progress":"edge prep"}));
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(d.out_dir.join("项目日报草稿.md")).unwrap();
    assert!(dt.contains("BCA") || dt.contains("site records"), "{dt}");
    assert!(dt.contains("辖区：SG"), "{dt}");

    let mut w = ToolCtx::new(p, "worker-brief", "people", "low", true, "iter-wb-zone");
    let wo = packs::execute(&mut w, "worker-brief__talk", &json!({"work_today":"edge"}));
    assert!(wo.contains("已写入"), "{wo}");
    let wt = std::fs::read_to_string(w.out_dir.join("班前白话稿.md")).unwrap();
    assert!(wt.contains("辖区：SG"), "{wt}");
}

#[test]
fn test_bim_clash_and_vendor_sg_titles() {
    let p = paths();
    let mut b = ToolCtx::new(p.clone(), "bim-coord", "bim", "low", true, "iter-clash");
    let bo = packs::execute(&mut b, "bim-coord__clash", &json!({"issues":"riser vs beam","disciplines":"str/mep"}));
    assert!(bo.contains("已写入"), "{bo}");
    let bt = std::fs::read_to_string(b.out_dir.join("碰撞协调纪要.md")).unwrap();
    assert!(bt.contains("IFC+SG") || bt.contains("CORENET"), "{bt}");
    let mut cn = ToolCtx::new(p.clone(), "bim-coord", "bim", "low", true, "iter-clash-cn");
    let cnot = packs::execute(&mut cn, "bim-coord__clash", &json!({"issues":"x","jurisdiction":"CN"}));
    assert!(cnot.contains("已写入"), "{cnot}");
    let ct = std::fs::read_to_string(cn.out_dir.join("碰撞协调纪要.md")).unwrap();
    assert!(ct.contains("辖区：CN"), "{ct}");
    assert!(!ct.contains("CORENET"), "{ct}");
    assert!(!ct.contains("IFC+SG"), "{ct}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "bim-qto", "bim", "low", true, "iter-clash-sib"),
        "bim-coord__clash",
        &json!({"issues":"x"}),
    )
    .contains("拒绝"));

    let mut v = ToolCtx::new(p.clone(), "proc-vendor", "procurement", "low", true, "iter-vendor");
    let vo = packs::execute(&mut v, "proc-vendor__eval", &json!({"vendor":"local fab"}));
    assert!(vo.contains("已写入"), "{vo}");
    let vt = std::fs::read_to_string(v.out_dir.join("供方评价表头.md")).unwrap();
    assert!(vt.contains("GeBIZ") && vt.contains("[A001]"), "{vt}");
    let mut vcn = ToolCtx::new(p, "proc-vendor", "procurement", "low", true, "iter-vendor-cn");
    let vco = packs::execute(&mut vcn, "proc-vendor__eval", &json!({"vendor":"x","jurisdiction":"CN"}));
    assert!(vco.contains("已写入"), "{vco}");
    let vct = std::fs::read_to_string(vcn.out_dir.join("供方评价表头.md")).unwrap();
    assert!(vct.contains("辖区：CN"), "{vct}");
    assert!(!vct.contains("GeBIZ"), "{vct}");
}

#[test]
fn test_claim_and_mix_sg_titles() {
    let p = paths();
    let mut c = ToolCtx::new(p.clone(), "claim", "commercial", "low", true, "iter-claim");
    let co = packs::execute(&mut c, "claim__notice", &json!({"event":"late drawings"}));
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(c.out_dir.join("索赔意向草稿.md")).unwrap();
    assert!(ct.contains("SG") && ct.contains("Security of Payment"), "{ct}");
    assert!(ct.contains("TBD") || ct.contains("[A001]") || ct.contains("UNSPECIFIED"), "{ct}");
    assert!(ct.contains("意向通知必备"), "{ct}");
    assert!(ct.contains("证据清单"), "{ct}");
    assert!(ct.contains("条款原文待贴") || ct.contains("待贴"), "{ct}");
    assert!(!ct.contains("可以开工"), "{ct}");
    let mut ev = ToolCtx::new(p.clone(), "claim", "commercial", "low", true, "iter-claim-ev");
    let evo = packs::execute(
        &mut ev,
        "claim__notice",
        &json!({"event":"late drawings","evidence":"监理通知 NCR-1；停工令 8月1日","jurisdiction":"SG"}),
    );
    assert!(evo.contains("已写入"), "{evo}");
    let et = std::fs::read_to_string(ev.out_dir.join("索赔意向草稿.md")).unwrap();
    assert!(et.contains("NCR-1"), "{et}");
    assert!(et.contains("停工令"), "{et}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "cost", "commercial", "low", true, "iter-claim-sib"),
        "claim__notice",
        &json!({"event":"x"}),
    )
    .contains("拒绝"));

    let mut m = ToolCtx::new(p, "lab-mix", "lab", "high", true, "iter-mix");
    let mo = packs::execute(&mut m, "lab-mix__report", &json!({"material":"C40","has_trial_data":false}));
    assert!(mo.contains("已写入"), "{mo}");
    let mt = std::fs::read_to_string(m.out_dir.join("配比报告提纲.md")).unwrap();
    assert!(mt.contains("SAC") && mt.contains("不给施工配合比"), "{mt}");
}

#[test]
fn test_quality_sample_emergency_sg_titles() {
    let p = paths();
    let mut q = ToolCtx::new(p.clone(), "quality", "hse", "high", true, "iter-q");
    let qo = packs::execute(&mut q, "quality__lot", &json!({"inspection_lot":"slab-1","items":"cover"}));
    assert!(qo.contains("已写入"), "{qo}");
    let qt = std::fs::read_to_string(q.out_dir.join("质量检查表.md")).unwrap();
    assert!(qt.contains("SG") && qt.contains("CONQUAS") && qt.contains("[A001]"), "{qt}");
    assert!(!qt.contains("可以开工"), "{qt}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "safety-brief", "hse", "high", true, "iter-q-sib"),
        "quality__lot",
        &json!({"inspection_lot":"x"}),
    )
    .contains("拒绝"));

    let mut s = ToolCtx::new(p.clone(), "lab-sample", "lab", "high", true, "iter-sample");
    let so = packs::execute(&mut s, "lab-sample__list", &json!({"materials":"concrete"}));
    assert!(so.contains("已写入"), "{so}");
    let st = std::fs::read_to_string(s.out_dir.join("取样送检清单.md")).unwrap();
    assert!(st.contains("SAC") && st.contains("[A001]"), "{st}");

    let mut e = ToolCtx::new(p, "emergency", "hse", "high", true, "iter-em");
    let eo = packs::execute(&mut e, "emergency__plan", &json!({"scenario":"fire"}));
    assert!(eo.contains("已写入"), "{eo}");
    let et = std::fs::read_to_string(e.out_dir.join("应急预案提纲.md")).unwrap();
    assert!(et.contains("SCDF") && et.contains("SG"), "{et}");
}

#[test]
fn test_hr_labor_sg_titles() {
    let p = paths();
    assert!(packs::visible_tool_names("hr-labor").iter().any(|n| n == "hr-labor__check"));
    assert!(!packs::visible_tool_names("hr-recruit").iter().any(|n| n == "hr-labor__check"));
    let mut ctx = ToolCtx::new(p.clone(), "hr-labor", "hr", "low", true, "iter-labor");
    let out = packs::execute(&mut ctx, "hr-labor__check", &json!({"contract_type":"work permit"}));
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("劳动合同检查表.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("Employment Act") || text.contains("Key Employment Terms"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    let mut sib = ToolCtx::new(p, "hr-recruit", "hr", "low", true, "iter-labor-sib");
    assert!(packs::execute(&mut sib, "hr-labor__check", &json!({"contract_type":"x"})).contains("拒绝"));
}

#[test]
fn test_finance_tax_and_env_sg_titles() {
    let p = paths();
    assert!(packs::visible_tool_names("finance-tax").iter().any(|n| n == "finance-tax__calendar"));
    assert!(!packs::visible_tool_names("finance-fund").iter().any(|n| n == "finance-tax__calendar"));
    let mut tax = ToolCtx::new(p.clone(), "finance-tax", "finance", "low", true, "iter-tax");
    let out = packs::execute(&mut tax, "finance-tax__calendar", &json!({}));
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(tax.out_dir.join("税务检查表.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("GST") && text.contains("IRAS"), "{text}");
    assert!(text.contains("9%"), "{text}");
    assert!(text.contains("申报期空栏") || text.contains("空栏"), "{text}");
    assert!(!text.contains("增值税"), "{text}");
    assert!(text.contains("辖区：SG"), "{text}");

    let mut book = ToolCtx::new(p.clone(), "finance-book", "finance", "low", true, "iter-book-sg");
    let bo = packs::execute(&mut book, "finance-book__check", &json!({"period":"2026-08"}));
    assert!(bo.contains("已写入"), "{bo}");
    let bt = std::fs::read_to_string(book.out_dir.join("核算检查表.md")).unwrap();
    assert!(bt.contains("辖区：SG"), "{bt}");
    assert!(bt.contains("GST") || bt.contains("IRAS"), "{bt}");
    assert!(!bt.contains("增值税"), "{bt}");

    let mut dual = ToolCtx::new(p.clone(), "finance-tax", "finance", "low", true, "iter-tax-dual");
    let dout = packs::execute(&mut dual, "finance-tax__calendar", &json!({"jurisdiction":"DUAL","other_jurisdiction":"CN"}));
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(dual.out_dir.join("税务检查表.md")).unwrap();
    assert!(dt.contains("DUAL（SG + CN）"), "{dt}");
    assert!(dt.contains("GST（SG 栏）"), "{dt}");
    assert!(dt.contains("增值税（另一辖区栏）"), "{dt}");
    assert!(text.contains("[A001]"), "{text}");
    let mut sib = ToolCtx::new(p.clone(), "finance-fund", "finance", "low", true, "iter-tax-sib");
    assert!(packs::execute(&mut sib, "finance-tax__calendar", &json!({})).contains("拒绝"));

    let mut env = ToolCtx::new(p.clone(), "env", "hse", "low", true, "iter-env");
    let eout = packs::execute(&mut env, "env__list", &json!({"site":"Tuas","jurisdiction":"SG"}));
    assert!(eout.contains("已写入"), "{eout}");
    let et = std::fs::read_to_string(env.out_dir.join("环保文明清单.md")).unwrap();
    assert!(et.contains("NEA") && et.contains("UNSPECIFIED"), "{et}");
    assert!(!et.contains("可以开工"), "{et}");
    let mut env_sib = ToolCtx::new(p, "quality", "hse", "high", true, "iter-env-sib");
    assert!(packs::execute(&mut env_sib, "env__list", &json!({"site":"x"})).contains("拒绝"));
}

#[test]
fn test_every_category_has_scan_forbidden() {
    for c in &seed().categories {
        let shared = tier_map::category_shared(&c.id);
        assert!(
            shared.iter().any(|t| t.ends_with("__scan_forbidden")),
            "{} missing category scan",
            c.id
        );
        let expert = packs::default_expert(&c.id);
        assert!(
            packs::visible_tool_names(expert)
                .iter()
                .any(|n| n.ends_with("__scan_forbidden")),
            "{expert} in {} cannot see scan",
            c.id
        );
    }
}

#[test]
fn test_yibiao_map_covers_every_seed_expert() {
    let mapped: std::collections::HashSet<&str> = tier_map::all_expert_maps()
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(tier_map::yibiao_pipeline(), ["parse", "outline", "qa", "kb", "write"]);
    for e in &seed().experts {
        assert!(mapped.contains(e.id.as_str()), "unmapped expert {}", e.id);
        let em = tier_map::expert_map(&e.id).unwrap();
        assert!(!em.yibiao.is_empty(), "{}", e.id);
        assert!(!em.exclusive.is_empty(), "{}", e.id);
    }
    let c = tier_map::expert_map("construction").unwrap();
    assert!(c.aligned);
    assert!(c.exclusive.iter().any(|t| t == "construction__scheme_draft"));
    for e in &seed().experts {
        let em = tier_map::expert_map(&e.id).unwrap();
        assert!(em.aligned, "{} should be aligned", e.id);
        let names = packs::visible_tool_names(&e.id);
        for tool in &em.exclusive {
            assert!(
                names.iter().any(|n| n == tool),
                "{} missing exclusive {tool}",
                e.id
            );
        }
    }
}

#[test]
fn test_bid_parse_exclusive_writes_and_sibling_refused() {
    let p = paths();
    let parse = packs::visible_tool_names("bid-parse");
    let comp = packs::visible_tool_names("bid-compliance");
    assert!(parse.iter().any(|n| n == "bid-parse__extract"));
    assert!(!comp.iter().any(|n| n == "bid-parse__extract"));
    assert!(!parse.iter().any(|n| n == "bid__parse_tender"));

    let mut ctx = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, "iter-bid-parse");
    let out = packs::execute(
        &mut ctx,
        "bid-parse__extract",
        &json!({
            "project_name": "Tuas warehouse tender",
            "jurisdiction": "SG",
            "tender_text": "Technical score 40\nPQM quality 20\nmethod statement for working at height required\ncalendar days 180"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"));
    assert!(text.contains("PQM") || text.contains("Technical score"), "{text}");
    assert!(text.contains("GeBIZ") && text.contains("CSOC"), "{text}");
    assert!(!text.contains("GB 50"));
    assert!(!text.contains("JGJ"));

    let mut dual = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, "iter-bid-dual");
    let dout = packs::execute(
        &mut dual,
        "bid-parse__extract",
        &json!({"tender_text":"PQM 20","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(dual.out_dir.join("招标解析表.md")).unwrap();
    assert!(dt.contains("DUAL（SG + CN）"), "{dt}");

    let mut sib = ToolCtx::new(p, "bid-compliance", "bid", "low", true, "iter-bid-sib");
    let refused = packs::execute(
        &mut sib,
        "bid-parse__extract",
        &json!({"tender_text": "x"}),
    );
    assert!(refused.contains("拒绝"), "{refused}");
}

#[test]
fn test_bid_parse_copies_itt_scoring_and_method_statement() {
    let p = paths();
    let tender = "\
INVITATION TO TENDER — Changi T5 airside package\n\
Evaluation criteria (PQM):\n\
- Quality 40%\n\
- Price 60%\n\
Tenderers shall submit a method statement for working at height.\n\
Required: 临边防护专项方案\n\
Time for Completion: 180 days\n\
BCA workhead CW01\n\
Two Envelope: technical and price separately\n\
Item 1 Drainage m 120";
    let mut ctx = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, "itt-facts");
    let out = packs::execute(
        &mut ctx,
        "bid-parse__extract",
        &json!({
            "project_name": "Changi T5 airside",
            "jurisdiction": "SG",
            "tender_text": tender
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains("Quality 40%"), "{text}");
    assert!(text.contains("临边防护专项方案"), "{text}");
    assert!(text.contains("method statement for working at height"), "{text}");
    assert!(text.contains("Price Quality Method (PQM) Framework"), "{text}");
    assert!(!text.contains("未在原文检出评分点"), "{text}");
    assert!(!text.contains("未在原文检出专项要求"), "{text}");
    assert!(text.contains("评标权重表"), "{text}");
    assert!(text.contains("CW01"), "{text}");
    assert!(text.contains("Two Envelope"), "{text}");
    let score_block = text.split("## Workhead").next().unwrap_or("");
    assert!(
        !score_block.contains("40% - 60%") && !score_block.contains("40%–60%"),
        "{score_block}"
    );
    let mut sib = ToolCtx::new(p, "bid-tech", "bid", "low", true, "itt-sib");
    assert!(packs::execute(
        &mut sib,
        "bid-parse__extract",
        &json!({"tender_text": tender})
    )
    .contains("拒绝"));
}

#[test]
fn test_bid_parse_does_not_invent_bca_pqm_band() {
    let p = paths();
    let tender = "Quality 35%\nPrice 65%\nmethod statement for working at height required";
    let mut ctx = ToolCtx::new(p, "bid-parse", "bid", "low", true, "pqm-band");
    let out = packs::execute(
        &mut ctx,
        "bid-parse__extract",
        &json!({"project_name": "Tuas", "jurisdiction": "SG", "tender_text": tender}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains("Quality 35%"), "{text}");
    assert!(text.contains("Price 65%"), "{text}");
    assert!(text.contains("Price Quality Method (PQM) Framework"), "{text}");
    let score_block = text.split("## 必须编制").next().unwrap_or("");
    assert!(
        !score_block.contains("40% - 60%") && !score_block.contains("40%–60%"),
        "{score_block}"
    );
}

#[test]
fn test_bid_parse_extracts_from_uploaded_attachment() {
    let p = paths();
    let sid = format!(
        "attach-itt-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let body = "Technical score 42\nRequired 临边防护专项方案\nmethod statement for working at height";
    let meta = civil_workbench::attach::save_upload(&p, &sid, "itt.txt", body.as_bytes()).unwrap();
    assert!(meta.get("id").is_some());
    let mut ctx = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, &sid);
    let out = packs::execute(
        &mut ctx,
        "bid-parse__extract",
        &json!({"project_name": "T5", "jurisdiction": "SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains("Technical score 42"), "{text}");
    assert!(text.contains("临边防护专项方案"), "{text}");
    let mut empty = ToolCtx::new(p, "bid-parse", "bid", "low", true, "no-src-01");
    let refuse = packs::execute(&mut empty, "bid-parse__extract", &json!({}));
    assert!(refuse.contains("拒绝"), "{refuse}");
    assert!(!empty.out_dir.join("招标解析表.md").is_file());
}

#[test]
fn test_bid_tech_dual_and_sibling() {
    let p = paths();
    assert!(packs::visible_tool_names("bid-tech").iter().any(|n| n == "bid-tech__expand"));
    assert!(!packs::visible_tool_names("bid-parse").iter().any(|n| n == "bid-tech__expand"));
    let mut ctx = ToolCtx::new(p.clone(), "bid-tech", "bid", "low", true, "iter-tech-dual");
    let out = packs::execute(
        &mut ctx,
        "bid-tech__expand",
        &json!({"scoring_points":"method","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("技术标目录草稿.md")).unwrap();
    assert!(text.contains("DUAL（SG + CN）"), "{text}");
    assert!(text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("GeBIZ") || text.contains("PQM"), "{text}");
    let mut sib = ToolCtx::new(p, "bid-parse", "bid", "low", true, "iter-tech-sib");
    assert!(
        packs::execute(&mut sib, "bid-tech__expand", &json!({"scoring_points":"x"})).contains("拒绝")
    );
}

#[test]
fn test_method_hazard_sg_exclusive() {
    let p = paths();
    let mut ctx = ToolCtx::new(p.clone(), "method-hazard", "construction", "high", true, "iter-hazard-sg");
    let out = packs::execute(
        &mut ctx,
        "method-hazard__judge_hazard",
        &json!({"work_type":"excavation","excavation_depth_m":2.0,"jurisdiction":"SG","description":"Tuas pit"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("危大判定书.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("危大"));
    assert!(text.contains("PTW") || text.contains("WSH"));
    assert!(text.contains("CSOC"), "{text}");
    assert!(!text.contains("可以开工"));
    assert!(!text.contains("报审通过"));

    let mut blocked = ToolCtx::new(p.clone(), "method-hazard", "construction", "high", false, "iter-hazard-gate");
    let gate = packs::execute(
        &mut blocked,
        "method-hazard__judge_hazard",
        &json!({"work_type":"excavation","jurisdiction":"SG"}),
    );
    assert!(gate.contains("拒绝写盘"), "{gate}");

    let mut dual = ToolCtx::new(p.clone(), "method-hazard", "construction", "high", true, "iter-hazard-dual");
    let dout = packs::execute(
        &mut dual,
        "method-hazard__judge_hazard",
        &json!({"work_type":"excavation","excavation_depth_m":2.0,"jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(dual.out_dir.join("危大判定书.md")).unwrap();
    assert!(dt.contains("DUAL（SG + CN）"), "{dt}");
    assert!(dt.contains("PTW") || dt.contains("WSH"), "{dt}");

    let mut sib = ToolCtx::new(p, "survey", "construction", "high", true, "iter-hazard-sib");
    let refused = packs::execute(
        &mut sib,
        "method-hazard__judge_hazard",
        &json!({"work_type":"excavation"}),
    );
    assert!(refused.contains("拒绝"), "{refused}");
}

#[test]
fn test_cost_takeoff_exclusive_writes_and_sibling_refused() {
    let p = paths();
    let cost = packs::visible_tool_names("cost");
    let var = packs::visible_tool_names("variation");
    assert!(cost.iter().any(|n| n == "cost__takeoff"));
    assert!(!var.iter().any(|n| n == "cost__takeoff"));

    let mut ctx = ToolCtx::new(p.clone(), "cost", "commercial", "low", true, "iter-cost");
    let out = packs::execute(
        &mut ctx,
        "cost__takeoff",
        &json!({"project_name":"Tuas slab","items":"edge protection\nconcrete","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("工程量拆分表.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("TBD"));
    assert!(text.contains("[A001]"));
    assert!(text.contains("PSSCOC"), "{text}");
    assert!(!text.contains("GB 50"));
    assert!(!text.contains("JGJ"));

    let mut dual = ToolCtx::new(p.clone(), "cost", "commercial", "low", true, "iter-cost-dual");
    let dout = packs::execute(
        &mut dual,
        "cost__takeoff",
        &json!({"items":"rebar","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(dual.out_dir.join("工程量拆分表.md")).unwrap();
    assert!(dt.contains("DUAL（SG + CN）"), "{dt}");
    assert!(dt.contains("TBD"), "{dt}");

    let mut cn = ToolCtx::new(p.clone(), "cost", "commercial", "low", true, "iter-cost-cn");
    let cnot = packs::execute(
        &mut cn,
        "cost__takeoff",
        &json!({"items":"rebar","jurisdiction":"CN"}),
    );
    assert!(cnot.contains("已写入"), "{cnot}");
    let ct = std::fs::read_to_string(cn.out_dir.join("工程量拆分表.md")).unwrap();
    assert!(ct.contains("辖区：CN"), "{ct}");
    assert!(!ct.contains("PSSCOC"), "{ct}");
    assert!(!ct.contains("GeBIZ"), "{ct}");

    let mut sib = ToolCtx::new(p, "variation", "commercial", "low", true, "iter-cost-sib");
    let refused = packs::execute(
        &mut sib,
        "cost__takeoff",
        &json!({"items":"x"}),
    );
    assert!(refused.contains("拒绝"), "{refused}");
}

#[test]
fn test_cn_outline_omits_sg_portal_titles() {
    let p = paths();
    let mut ctx = ToolCtx::new(p.clone(), "plumbing", "design", "low", true, "iter-plumb-cn");
    let out = packs::execute(
        &mut ctx,
        "plumbing__memo",
        &json!({"scope":"stack","jurisdiction":"CN"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("给排水专业说明草稿.md")).unwrap();
    assert!(text.contains("辖区：CN"), "{text}");
    assert!(!text.contains("PUB"), "{text}");
    assert!(!text.contains("SS 636"), "{text}");
    assert!(!text.contains("Fire Code 2023"), "{text}");
    assert!(text.contains("给水排水") || text.contains("设计标准"), "{text}");

    let mut sg = ToolCtx::new(p.clone(), "plumbing", "design", "low", true, "iter-plumb-sg");
    let so = packs::execute(
        &mut sg,
        "plumbing__memo",
        &json!({"scope":"stack","jurisdiction":"SG"}),
    );
    assert!(so.contains("已写入"), "{so}");
    let st = std::fs::read_to_string(sg.out_dir.join("给排水专业说明草稿.md")).unwrap();
    assert!(st.contains("PUB") || st.contains("SS 636"), "{st}");

    let mut sib = ToolCtx::new(p, "hvac", "design", "low", true, "iter-plumb-sib");
    assert!(
        packs::execute(&mut sib, "plumbing__memo", &json!({"scope":"x","jurisdiction":"CN"}))
            .contains("拒绝")
    );
}

#[test]
fn test_hse_lab_geotech_dual_banners() {
    let p = paths();
    let mut g = ToolCtx::new(p.clone(), "geotech", "design", "high", true, "iter-geo-dual");
    let go = packs::execute(
        &mut g,
        "geotech__brief",
        &json!({"scope":"pad","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(go.contains("已写入"), "{go}");
    let gt = std::fs::read_to_string(g.out_dir.join("岩土勘察提纲.md")).unwrap();
    assert!(gt.contains("DUAL（SG + CN）"), "{gt}");
    assert!(gt.contains("GeoSS") || gt.contains("AGS"), "{gt}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "structure", "design", "high", true, "iter-geo-dual-sib"),
        "geotech__brief",
        &json!({"scope":"x","jurisdiction":"DUAL"}),
    )
    .contains("拒绝"));

    let mut q = ToolCtx::new(p.clone(), "quality", "hse", "high", true, "iter-q-dual");
    let qo = packs::execute(
        &mut q,
        "quality__lot",
        &json!({"inspection_lot":"slab","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(qo.contains("已写入"), "{qo}");
    let qt = std::fs::read_to_string(q.out_dir.join("质量检查表.md")).unwrap();
    assert!(qt.contains("DUAL（SG + CN）"), "{qt}");
    assert!(qt.contains("CONQUAS"), "{qt}");

    let mut qcn = ToolCtx::new(p.clone(), "quality", "hse", "high", true, "iter-q-cn");
    let qco = packs::execute(
        &mut qcn,
        "quality__lot",
        &json!({"inspection_lot":"slab","jurisdiction":"CN"}),
    );
    assert!(qco.contains("已写入"), "{qco}");
    let qct = std::fs::read_to_string(qcn.out_dir.join("质量检查表.md")).unwrap();
    assert!(qct.contains("辖区：CN"), "{qct}");
    assert!(!qct.contains("CONQUAS"), "{qct}");

    let mut lab = ToolCtx::new(p.clone(), "lab-mix", "lab", "high", true, "iter-mix-dual");
    let lo = packs::execute(
        &mut lab,
        "lab-mix__report",
        &json!({"material":"C40","has_trial_data":false,"jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(lo.contains("已写入"), "{lo}");
    let lt = std::fs::read_to_string(lab.out_dir.join("配比报告提纲.md")).unwrap();
    assert!(lt.contains("DUAL（SG + CN）"), "{lt}");
    assert!(lt.contains("不给施工配合比"), "{lt}");
    assert!(lt.contains("SAC"), "{lt}");

    let mut lcn = ToolCtx::new(p.clone(), "lab-mix", "lab", "high", true, "iter-mix-cn");
    let lco = packs::execute(
        &mut lcn,
        "lab-mix__report",
        &json!({"material":"C40","has_trial_data":false,"jurisdiction":"CN"}),
    );
    assert!(lco.contains("已写入"), "{lco}");
    let lct = std::fs::read_to_string(lcn.out_dir.join("配比报告提纲.md")).unwrap();
    assert!(lct.contains("辖区：CN"), "{lct}");
    assert!(!lct.contains("SAC"), "{lct}");

    let mut labr = ToolCtx::new(p, "hr-labor", "hr", "low", true, "iter-labor-dual");
    let ho = packs::execute(
        &mut labr,
        "hr-labor__check",
        &json!({"contract_type":"wp","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(ho.contains("已写入"), "{ho}");
    let ht = std::fs::read_to_string(labr.out_dir.join("劳动合同检查表.md")).unwrap();
    assert!(ht.contains("DUAL（SG + CN）"), "{ht}");
    assert!(ht.contains("Employment Act"), "{ht}");
}

#[test]
fn test_more_writers_default_sg_zone() {
    let p = paths();
    let mut wh = ToolCtx::new(p.clone(), "warehouse", "plant", "low", true, "iter-wh-zone");
    let wo = packs::execute(&mut wh, "warehouse__log", &json!({"item":"rebar"}));
    assert!(wo.contains("已写入"), "{wo}");
    let wt = std::fs::read_to_string(wh.out_dir.join("收发存台账.md")).unwrap();
    assert!(wt.contains("辖区：SG"), "{wt}");
    assert!(wt.contains("Factory Notification"), "{wt}");
    let mut whcn = ToolCtx::new(p.clone(), "warehouse", "plant", "low", true, "iter-wh-cn");
    let wco = packs::execute(&mut whcn, "warehouse__log", &json!({"item":"rebar","jurisdiction":"CN"}));
    assert!(wco.contains("已写入"), "{wco}");
    let wct = std::fs::read_to_string(whcn.out_dir.join("收发存台账.md")).unwrap();
    assert!(wct.contains("辖区：CN"), "{wct}");
    assert!(!wct.contains("Factory Notification"), "{wct}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "equip", "plant", "high", true, "iter-wh-sib"),
        "warehouse__log",
        &json!({"item":"x"}),
    )
    .contains("拒绝"));

    let mut fund = ToolCtx::new(p.clone(), "finance-fund", "finance", "low", true, "iter-fund-zone");
    let fo = packs::execute(&mut fund, "finance-fund__plan", &json!({"period":"2026-08"}));
    assert!(fo.contains("已写入"), "{fo}");
    let ft = std::fs::read_to_string(fund.out_dir.join("资金计划草稿.md")).unwrap();
    assert!(ft.contains("辖区：SG"), "{ft}");

    let mut mat = ToolCtx::new(p.clone(), "material-site", "plant", "low", true, "iter-mat-zone");
    let mo = packs::execute(&mut mat, "material-site__recon", &json!({"items":"rebar"}));
    assert!(mo.contains("已写入"), "{mo}");
    let mt = std::fs::read_to_string(mat.out_dir.join("材料耗用核算表头.md")).unwrap();
    assert!(mt.contains("辖区：SG"), "{mt}");
    assert!(packs::execute(
        &mut ToolCtx::new(p.clone(), "warehouse", "plant", "low", true, "iter-mat-sib"),
        "material-site__recon",
        &json!({"items":"x"}),
    )
    .contains("拒绝"));

    let mut ops = ToolCtx::new(p, "it-ops", "it", "low", true, "iter-ops-zone");
    let oo = packs::execute(&mut ops, "it-ops__runbook", &json!({"system":"gate"}));
    assert!(oo.contains("已写入"), "{oo}");
    let ot = std::fs::read_to_string(ops.out_dir.join("运维手册提纲.md")).unwrap();
    assert!(ot.contains("辖区：SG"), "{ot}");
}

#[test]
fn test_dual_jurisdiction_and_default_sg_zone() {
    let p = paths();
    let mut dual = ToolCtx::new(p.clone(), "fire-protect", "design", "high", true, "iter-dual");
    let out = packs::execute(
        &mut dual,
        "fire-protect__brief",
        &json!({"scope": "Tuas hydrant", "jurisdiction": "DUAL"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(dual.out_dir.join("消防专篇提纲.md")).unwrap();
    assert!(text.contains("DUAL"), "{text}");
    assert!(text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("另一辖区"), "{text}");
    assert!(!text.contains("增值税"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");

    let mut named = ToolCtx::new(p.clone(), "fire-protect", "design", "high", true, "iter-dual-cn");
    let out2 = packs::execute(
        &mut named,
        "fire-protect__brief",
        &json!({"scope": "pad", "jurisdiction": "DUAL", "other_jurisdiction": "CN"}),
    );
    assert!(out2.contains("已写入"), "{out2}");
    let t2 = std::fs::read_to_string(named.out_dir.join("消防专篇提纲.md")).unwrap();
    assert!(t2.contains("DUAL（SG + CN）"), "{t2}");

    let mut clash = ToolCtx::new(p.clone(), "bim-coord", "bim", "low", true, "iter-clash-zone");
    let cout = packs::execute(&mut clash, "bim-coord__clash", &json!({"issues":"riser"}));
    assert!(cout.contains("已写入"), "{cout}");
    let ct = std::fs::read_to_string(clash.out_dir.join("碰撞协调纪要.md")).unwrap();
    assert!(ct.contains("辖区：SG"), "{ct}");

    let mut sib = ToolCtx::new(p, "architecture", "design", "low", true, "iter-dual-sib");
    assert!(
        packs::execute(&mut sib, "fire-protect__brief", &json!({"scope":"x","jurisdiction":"DUAL"}))
            .contains("拒绝")
    );
}

#[test]
fn test_architecture_memo_sg_and_sibling() {
    let p = paths();
    assert!(packs::visible_tool_names("architecture").iter().any(|n| n == "architecture__memo"));
    assert!(!packs::visible_tool_names("plumbing").iter().any(|n| n == "architecture__memo"));
    let mut ctx = ToolCtx::new(p.clone(), "architecture", "design", "low", true, "iter-arch");
    let out = packs::execute(
        &mut ctx,
        "architecture__memo",
        &json!({"discipline":"architecture","scope":"Tuas warehouse","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("architecture专业说明草稿.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("Reportable Matters"), "{text}");
    let mut sib = ToolCtx::new(p, "plumbing", "design", "low", true, "iter-arch-sib");
    assert!(
        packs::execute(
            &mut sib,
            "architecture__memo",
            &json!({"discipline":"x","scope":"x"}),
        )
        .contains("拒绝")
    );
}

#[test]
fn test_structure_calc_sg_titles() {
    let p = paths();
    assert!(packs::visible_tool_names("structure").iter().any(|n| n == "structure__calc_outline"));
    assert!(!packs::visible_tool_names("geotech").iter().any(|n| n == "structure__calc_outline"));
    let mut ctx = ToolCtx::new(p.clone(), "structure", "design", "high", true, "iter-str");
    let out = packs::execute(&mut ctx, "structure__calc_outline", &json!({"system":"RC frame","jurisdiction":"SG"}));
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("结构计算书提纲.md")).unwrap();
    assert!(text.contains("SG") && text.contains("Accredited Checker"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(!text.contains("JGJ"), "{text}");
    let mut sib = ToolCtx::new(p, "geotech", "design", "high", true, "iter-str-sib");
    assert!(packs::execute(&mut sib, "structure__calc_outline", &json!({"system":"x"})).contains("拒绝"));
}

#[test]
fn test_geotech_brief_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("geotech").iter().any(|n| n == "geotech__brief"));
    assert!(!packs::visible_tool_names("structure").iter().any(|n| n == "geotech__brief"));
    let mut ctx = ToolCtx::new(p.clone(), "geotech", "design", "high", true, "iter-geotech");
    let out = packs::execute(
        &mut ctx,
        "geotech__brief",
        &json!({"scope":"Tuas warehouse pad","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("岩土勘察提纲.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"));
    assert!(text.contains("GeoSS") || text.contains("AGS"), "{text}");
    assert!(!text.contains("可以开工"));
    let mut sib = ToolCtx::new(p, "architecture", "design", "low", true, "iter-geotech-sib");
    assert!(packs::execute(&mut sib, "geotech__brief", &json!({"scope":"x"})).contains("拒绝"));
}

#[test]
fn test_dispatch_daily_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("dispatch").iter().any(|n| n == "dispatch__daily"));
    assert!(!packs::visible_tool_names("survey").iter().any(|n| n == "dispatch__daily"));
    let mut ctx = ToolCtx::new(p.clone(), "dispatch", "construction", "low", true, "iter-dispatch");
    let out = packs::execute(
        &mut ctx,
        "dispatch__daily",
        &json!({"progress":"edge protection prep","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("调度日报草稿.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]"));
    assert!(text.contains("## 1 报头"), "{text}");
    assert!(text.contains("危大/高处/临边等敏感作业清单"), "{text}");
    assert!(text.contains("method-hazard"), "{text}");
    assert!(!text.contains("可以开工"));
    let mut hot = ToolCtx::new(p.clone(), "dispatch", "construction", "low", true, "iter-dispatch-edge");
    let hout = packs::execute(
        &mut hot,
        "dispatch__daily",
        &json!({"progress":"临边防护白班","jurisdiction":"SG"}),
    );
    assert!(hout.contains("已写入"), "{hout}");
    let ht = std::fs::read_to_string(hot.out_dir.join("调度日报草稿.md")).unwrap();
    assert!(ht.contains("临边"), "{ht}");
    assert!(ht.contains("method-hazard"), "{ht}");
    let mut sib = ToolCtx::new(p, "construction", "construction", "high", true, "iter-dispatch-sib");
    assert!(packs::execute(&mut sib, "dispatch__daily", &json!({"progress":"x"})).contains("拒绝"));
}

#[test]
fn test_survey_record_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("survey").iter().any(|n| n == "survey__record"));
    assert!(!packs::visible_tool_names("construction").iter().any(|n| n == "survey__record"));
    assert!(!packs::visible_tool_names("dispatch").iter().any(|n| n == "survey__record"));
    let mut ctx = ToolCtx::new(p.clone(), "survey", "construction", "high", true, "iter-survey");
    let out = packs::execute(
        &mut ctx,
        "survey__record",
        &json!({"work_item":"slab opening set-out","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("测量记录口径.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"));
    assert!(text.contains("SVY21") || text.contains("SHD"), "{text}");
    assert!(text.contains("## 4 已知起算"), "{text}");
    assert!(text.contains("## 11 附录"), "{text}");
    assert!(!text.contains("CP99"), "{text}");
    assert!(!text.contains("可以开工"));
    let sid = format!(
        "wb-sv-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    civil_workbench::attach::save_upload(
        &p,
        &sid,
        "points.txt",
        "控制点 CP01 东 12345.67 北 23456.89\n".as_bytes(),
    )
    .unwrap();
    let mut with_pt = ToolCtx::new(p.clone(), "survey", "construction", "high", true, &sid);
    let pout = packs::execute(
        &mut with_pt,
        "survey__record",
        &json!({"work_item":"slab opening set-out","jurisdiction":"SG"}),
    );
    assert!(pout.contains("已写入"), "{pout}");
    let pt = std::fs::read_to_string(with_pt.out_dir.join("测量记录口径.md")).unwrap();
    assert!(pt.contains("CP01"), "{pt}");
    assert!(pt.contains("12345.67"), "{pt}");
    assert!(pt.contains("23456.89"), "{pt}");
    let mut blocked = ToolCtx::new(p.clone(), "survey", "construction", "high", false, "iter-survey-gate");
    assert!(packs::execute(
        &mut blocked,
        "survey__record",
        &json!({"work_item":"x","jurisdiction":"SG"})
    )
    .contains("拒绝写盘"));
    let mut sib = ToolCtx::new(p, "construction", "construction", "high", true, "iter-survey-sib");
    assert!(packs::execute(&mut sib, "survey__record", &json!({"work_item":"x"})).contains("拒绝"));
}

#[test]
fn test_equip_ledger_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("equip").iter().any(|n| n == "equip__ledger"));
    assert!(!packs::visible_tool_names("warehouse").iter().any(|n| n == "equip__ledger"));
    let mut blocked = ToolCtx::new(p.clone(), "equip", "plant", "high", false, "iter-equip-hitl");
    assert!(packs::execute(&mut blocked, "equip__ledger", &json!({"equipment":"tower crane"})).contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "equip", "plant", "high", true, "iter-equip");
    let out = packs::execute(&mut ctx, "equip__ledger", &json!({"equipment":"tower crane"}));
    assert!(out.contains("已写入"), "{out}");
    let et = std::fs::read_to_string(ctx.out_dir.join("设备台账.md")).unwrap();
    assert!(et.contains("SG") && et.contains("MOM") && et.contains("[A001]"), "{et}");
    assert!(et.contains("tower crane"), "{et}");
    assert!(et.contains("进场验收") && et.contains("证件") && et.contains("维保"), "{et}");
    assert!(et.contains("无证件不编进场结论"), "{et}");
    assert!(!et.contains("可以开工"), "{et}");
    let mut cert = ToolCtx::new(p.clone(), "equip", "plant", "high", true, "iter-equip-cert");
    let co = packs::execute(
        &mut cert,
        "equip__ledger",
        &json!({"equipment":"塔吊","certs":"合格证 TS-88","jurisdiction":"SG"}),
    );
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(cert.out_dir.join("设备台账.md")).unwrap();
    assert!(ct.contains("塔吊"), "{ct}");
    assert!(ct.contains("TS-88"), "{ct}");
    assert!(ct.contains("用户给定"), "{ct}");
    let mut sib = ToolCtx::new(p, "warehouse", "plant", "low", true, "iter-equip-sib");
    assert!(packs::execute(&mut sib, "equip__ledger", &json!({"equipment":"x"})).contains("拒绝"));
}

#[test]
fn test_warehouse_log_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("warehouse").iter().any(|n| n == "warehouse__log"));
    assert!(!packs::visible_tool_names("equip").iter().any(|n| n == "warehouse__log"));
    let mut ctx = ToolCtx::new(p.clone(), "warehouse", "plant", "low", true, "iter-wh-t036");
    let out = packs::execute(&mut ctx, "warehouse__log", &json!({"item":"rebar","jurisdiction":"SG"}));
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("收发存台账.md")).unwrap();
    assert!(text.contains("rebar"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("无盘点不编盈亏"), "{text}");
    assert!(text.contains("Factory Notification"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut qty = ToolCtx::new(p.clone(), "warehouse", "plant", "low", true, "iter-wh-qty");
    let qo = packs::execute(
        &mut qty,
        "warehouse__log",
        &json!({"item":"钢筋 入库 12吨","jurisdiction":"SG"}),
    );
    assert!(qo.contains("已写入"), "{qo}");
    let qt = std::fs::read_to_string(qty.out_dir.join("收发存台账.md")).unwrap();
    assert!(qt.contains("钢筋"), "{qt}");
    assert!(qt.contains("12吨"), "{qt}");
    let mut sib = ToolCtx::new(p, "equip", "plant", "high", true, "iter-wh-t036-sib");
    assert!(packs::execute(&mut sib, "warehouse__log", &json!({"item":"x"})).contains("拒绝"));
}

#[test]
fn test_material_site_recon_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("material-site").iter().any(|n| n == "material-site__recon"));
    assert!(!packs::visible_tool_names("warehouse").iter().any(|n| n == "material-site__recon"));
    let mut ctx = ToolCtx::new(p.clone(), "material-site", "plant", "low", true, "iter-ms-t036");
    let out = packs::execute(
        &mut ctx,
        "material-site__recon",
        &json!({"items":"rebar","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("材料耗用核算表头.md")).unwrap();
    assert!(text.contains("rebar"), "{text}");
    assert!(text.contains("应耗"), "{text}");
    assert!(text.contains("领料"), "{text}");
    assert!(text.contains("节超"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("无盘点不编盈亏"), "{text}");
    assert!(text.contains("Factory Notification"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut qty = ToolCtx::new(p.clone(), "material-site", "plant", "low", true, "iter-ms-qty");
    let qo = packs::execute(
        &mut qty,
        "material-site__recon",
        &json!({"items":"钢筋 应耗 10吨 领料 12吨","jurisdiction":"SG"}),
    );
    assert!(qo.contains("已写入"), "{qo}");
    let qt = std::fs::read_to_string(qty.out_dir.join("材料耗用核算表头.md")).unwrap();
    assert!(qt.contains("钢筋"), "{qt}");
    assert!(qt.contains("10吨"), "{qt}");
    assert!(qt.contains("12吨"), "{qt}");
    let mut sib = ToolCtx::new(p, "warehouse", "plant", "low", true, "iter-ms-sib");
    assert!(packs::execute(&mut sib, "material-site__recon", &json!({"items":"x"})).contains("拒绝"));
}

#[test]
fn test_purchase_plan_split_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("proc-plan").iter().any(|n| n == "proc-plan__schedule"));
    assert!(!packs::visible_tool_names("proc-compare").iter().any(|n| n == "proc-plan__schedule"));
    let mut ctx = ToolCtx::new(p.clone(), "proc-plan", "procurement", "low", true, "iter-pp-t037");
    let out = packs::execute(
        &mut ctx,
        "proc-plan__schedule",
        &json!({"items":"rebar","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("采购计划表.md")).unwrap();
    assert!(text.contains("rebar"), "{text}");
    assert!(text.contains("甲供") && text.contains("甲指") && text.contains("自采"), "{text}");
    assert!(text.contains("待划"), "{text}");
    assert!(text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("CRS"), "{text}");
    assert!(text.contains("GeBIZ"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("一律提前"), "{text}");
    let mut split = ToolCtx::new(p.clone(), "proc-plan", "procurement", "low", true, "iter-pp-split");
    let so = packs::execute(
        &mut split,
        "proc-plan__schedule",
        &json!({"items":"钢筋 甲供；水泥 自采","jurisdiction":"SG"}),
    );
    assert!(so.contains("已写入"), "{so}");
    let st = std::fs::read_to_string(split.out_dir.join("采购计划表.md")).unwrap();
    assert!(st.contains("钢筋"), "{st}");
    assert!(st.contains("水泥"), "{st}");
    let gong = st.split("### 甲供").nth(1).unwrap().split("###").next().unwrap();
    let zi = st.split("### 自采").nth(1).unwrap().split("###").next().unwrap();
    assert!(gong.contains("钢筋"), "{gong}");
    assert!(zi.contains("水泥"), "{zi}");
    let mut sib = ToolCtx::new(p, "proc-compare", "procurement", "low", true, "iter-pp-t037-sib");
    assert!(packs::execute(&mut sib, "proc-plan__schedule", &json!({"items":"x"})).contains("拒绝"));
}

#[test]
fn test_compare_table_multi_col_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("proc-compare").iter().any(|n| n == "proc-compare__table"));
    assert!(!packs::visible_tool_names("proc-plan").iter().any(|n| n == "proc-compare__table"));
    let mut ctx = ToolCtx::new(p.clone(), "proc-compare", "procurement", "low", true, "iter-cmp-t037");
    let out = packs::execute(
        &mut ctx,
        "proc-compare__table",
        &json!({"item":"rebar","vendors":"A","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    assert!(out.contains("扫描通过"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("比价表草稿.md")).unwrap();
    assert!(text.contains("rebar"), "{text}");
    assert!(text.contains("| A |"), "{text}");
    assert!(text.contains("规格响应") && text.contains("到货期") && text.contains("质保") && text.contains("付款"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("待制度定"), "{text}");
    assert!(text.contains("GeBIZ"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("现定标"), "{text}");
    let mut two = ToolCtx::new(p.clone(), "proc-compare", "procurement", "low", true, "iter-cmp-vendors");
    let to = packs::execute(
        &mut two,
        "proc-compare__table",
        &json!({"item":"钢筋","vendors":"甲；乙","jurisdiction":"SG"}),
    );
    assert!(to.contains("已写入"), "{to}");
    let tt = std::fs::read_to_string(two.out_dir.join("比价表草稿.md")).unwrap();
    assert!(tt.contains("甲") && tt.contains("乙"), "{tt}");
    assert!(tt.contains("不足三家"), "{tt}");
    let mut sib = ToolCtx::new(p, "proc-plan", "procurement", "low", true, "iter-cmp-t037-sib");
    assert!(packs::execute(&mut sib, "proc-compare__table", &json!({"item":"x"})).contains("拒绝"));
}

#[test]
fn test_vendor_eval_access_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("proc-vendor").iter().any(|n| n == "proc-vendor__eval"));
    assert!(!packs::visible_tool_names("proc-compare").iter().any(|n| n == "proc-vendor__eval"));
    let mut ctx = ToolCtx::new(p.clone(), "proc-vendor", "procurement", "low", true, "iter-pv-t037");
    let out = packs::execute(
        &mut ctx,
        "proc-vendor__eval",
        &json!({"vendor":"local fab","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("供方评价表头.md")).unwrap();
    assert!(text.contains("local fab"), "{text}");
    assert!(text.contains("准入") && text.contains("考察") && text.contains("短名单"), "{text}");
    assert!(text.contains("待核"), "{text}");
    assert!(text.contains("分数"), "{text}");
    assert!(text.contains("GeBIZ"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("中标"), "{text}");
    let mut two = ToolCtx::new(p.clone(), "proc-vendor", "procurement", "low", true, "iter-pv-two");
    let to = packs::execute(
        &mut two,
        "proc-vendor__eval",
        &json!({"vendor":"甲；乙","jurisdiction":"SG"}),
    );
    assert!(to.contains("已写入"), "{to}");
    let tt = std::fs::read_to_string(two.out_dir.join("供方评价表头.md")).unwrap();
    let acc = tt.split("## 3 准入").nth(1).unwrap().split("## 4").next().unwrap();
    assert!(acc.contains("甲") && acc.contains("乙"), "{acc}");
    let mut sib = ToolCtx::new(p, "proc-compare", "procurement", "low", true, "iter-pv-t037-sib");
    assert!(packs::execute(&mut sib, "proc-vendor__eval", &json!({"vendor":"x"})).contains("拒绝"));
}

#[test]
fn test_finance_book_check_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("finance-book").iter().any(|n| n == "finance-book__check"));
    assert!(!packs::visible_tool_names("finance-fund").iter().any(|n| n == "finance-book__check"));
    let mut ctx = ToolCtx::new(p.clone(), "finance-book", "finance", "low", true, "iter-fb-t038");
    let out = packs::execute(
        &mut ctx,
        "finance-book__check",
        &json!({"period":"2026-08","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("核算检查表.md")).unwrap();
    assert!(text.contains("2026-08"), "{text}");
    assert!(text.contains("报销勾选") && text.contains("科目对照") && text.contains("对账缺口"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("发票查验"), "{text}");
    assert!(text.contains("人工费"), "{text}");
    assert!(text.contains("GST") || text.contains("IRAS"), "{text}");
    assert!(!text.contains("增值税"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("账已平"), "{text}");
    let mut gap = ToolCtx::new(p.clone(), "finance-book", "finance", "low", true, "iter-fb-gap");
    let go = packs::execute(
        &mut gap,
        "finance-book__check",
        &json!({"period":"2026-08","notes":"收发存未盘点","jurisdiction":"SG"}),
    );
    assert!(go.contains("已写入"), "{go}");
    let gt = std::fs::read_to_string(gap.out_dir.join("核算检查表.md")).unwrap();
    assert!(gt.contains("收发存"), "{gt}");
    assert!(gt.contains("不编盈亏"), "{gt}");
    let mut sib = ToolCtx::new(p, "finance-fund", "finance", "low", true, "iter-fb-t038-sib");
    assert!(packs::execute(&mut sib, "finance-book__check", &json!({"period":"x"})).contains("拒绝"));
}

#[test]
fn test_finance_fund_windows_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("finance-fund").iter().any(|n| n == "finance-fund__plan"));
    assert!(!packs::visible_tool_names("finance-book").iter().any(|n| n == "finance-fund__plan"));
    let mut ctx = ToolCtx::new(p.clone(), "finance-fund", "finance", "low", true, "iter-ff-t038");
    let out = packs::execute(
        &mut ctx,
        "finance-fund__plan",
        &json!({"period":"2026-08","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("资金计划草稿.md")).unwrap();
    assert!(text.contains("2026-08"), "{text}");
    assert!(text.contains("收入栏") && text.contains("支出栏"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("不构成付款指令"), "{text}");
    assert!(text.contains("无法试算"), "{text}");
    assert!(text.contains("CPF"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("资金充足"), "{text}");
    let mut io = ToolCtx::new(p.clone(), "finance-fund", "finance", "low", true, "iter-ff-io");
    let ioo = packs::execute(
        &mut io,
        "finance-fund__plan",
        &json!({"period":"2026-08","notes":"收入 业主进度款；支出 钢筋","jurisdiction":"SG"}),
    );
    assert!(ioo.contains("已写入"), "{ioo}");
    let iot = std::fs::read_to_string(io.out_dir.join("资金计划草稿.md")).unwrap();
    let inc = iot.split("## 5 收入栏").nth(1).unwrap().split("## 6").next().unwrap();
    let exp = iot.split("## 6 支出栏").nth(1).unwrap().split("## 7").next().unwrap();
    assert!(inc.contains("业主进度款"), "{inc}");
    assert!(exp.contains("钢筋"), "{exp}");
    let mut sib = ToolCtx::new(p, "finance-book", "finance", "low", true, "iter-ff-t038-sib");
    assert!(packs::execute(&mut sib, "finance-fund__plan", &json!({"period":"x"})).contains("拒绝"));
}

#[test]
fn test_worker_brief_three_part_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("worker-brief").iter().any(|n| n == "worker-brief__talk"));
    assert!(!packs::visible_tool_names("pm-daily").iter().any(|n| n == "worker-brief__talk"));
    let mut ctx = ToolCtx::new(p.clone(), "worker-brief", "people", "low", true, "iter-wb-t039");
    let out = packs::execute(
        &mut ctx,
        "worker-brief__talk",
        &json!({"work_today":"edge","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("班前白话稿.md")).unwrap();
    assert!(text.contains("edge"), "{text}");
    assert!(text.contains("今天干什么") && text.contains("哪儿会掉") && text.contains("三步怎么干"), "{text}");
    assert!(text.contains("toolbox"), "{text}");
    assert!(!text.contains("毫米"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sized = ToolCtx::new(p.clone(), "worker-brief", "people", "low", true, "iter-wb-mm");
    let so = packs::execute(
        &mut sized,
        "worker-brief__talk",
        &json!({"work_today":"临边 1200mm","jurisdiction":"SG"}),
    );
    assert!(so.contains("已写入"), "{so}");
    let st = std::fs::read_to_string(sized.out_dir.join("班前白话稿.md")).unwrap();
    assert!(st.contains("临边"), "{st}");
    assert!(st.contains("1200mm"), "{st}");
    let mut sib = ToolCtx::new(p, "pm-daily", "people", "low", true, "iter-wb-t039-sib");
    assert!(packs::execute(&mut sib, "worker-brief__talk", &json!({"work_today":"x"})).contains("拒绝"));
}

#[test]
fn test_pm_daily_weather_site_no_pct_labor() {
    let p = paths();
    assert!(packs::visible_tool_names("pm-daily").iter().any(|n| n == "pm-daily__log"));
    assert!(!packs::visible_tool_names("worker-brief").iter().any(|n| n == "pm-daily__log"));
    let mut ctx = ToolCtx::new(p.clone(), "pm-daily", "people", "low", true, "iter-pmd-t039");
    let out = packs::execute(
        &mut ctx,
        "pm-daily__log",
        &json!({"progress":"edge prep","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("项目日报草稿.md")).unwrap();
    assert!(text.contains("edge prep"), "{text}");
    assert!(text.contains("天气待填"), "{text}");
    assert!(text.contains("出勤待填"), "{text}");
    let img = text.split("## 4 形象进度").nth(1).unwrap().split("## 5").next().unwrap();
    assert!(!img.contains('%'), "{img}");
    assert!(text.contains("BCA") && text.contains("site record"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut wx = ToolCtx::new(p.clone(), "pm-daily", "people", "low", true, "iter-pmd-wx");
    let wo = packs::execute(
        &mut wx,
        "pm-daily__log",
        &json!({"progress":"临边","weather":"晴","labor":"木工","jurisdiction":"SG"}),
    );
    assert!(wo.contains("已写入"), "{wo}");
    let wt = std::fs::read_to_string(wx.out_dir.join("项目日报草稿.md")).unwrap();
    let weather_sec = wt.split("## 2 天气").nth(1).unwrap().split("## 3").next().unwrap();
    assert!(weather_sec.contains("晴"), "{weather_sec}");
    assert!(!weather_sec.contains("天气待填"), "{weather_sec}");
    let labor_sec = wt.split("## 5 出勤").nth(1).unwrap().split("## 6").next().unwrap();
    assert!(labor_sec.contains("木工"), "{labor_sec}");
    assert!(!labor_sec.contains("出勤待填"), "{labor_sec}");
    let mut pct = ToolCtx::new(p.clone(), "pm-daily", "people", "low", true, "iter-pmd-pct");
    let po = packs::execute(
        &mut pct,
        "pm-daily__log",
        &json!({"progress":"临边 完成30%","jurisdiction":"SG"}),
    );
    assert!(po.contains("已写入"), "{po}");
    let pt = std::fs::read_to_string(pct.out_dir.join("项目日报草稿.md")).unwrap();
    let img2 = pt.split("## 4 形象进度").nth(1).unwrap().split("## 5").next().unwrap();
    assert!(!img2.contains('%'), "{img2}");
    let mut cn = ToolCtx::new(p.clone(), "pm-daily", "people", "low", true, "iter-pmd-cn");
    let co = packs::execute(
        &mut cn,
        "pm-daily__log",
        &json!({"progress":"临边","jurisdiction":"CN"}),
    );
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(cn.out_dir.join("项目日报草稿.md")).unwrap();
    assert!(ct.contains("辖区：CN"), "{ct}");
    assert!(!ct.contains("BCA"), "{ct}");
    assert!(!ct.contains("site record"), "{ct}");
    let mut sib = ToolCtx::new(p, "worker-brief", "people", "low", true, "iter-pmd-t039-sib");
    assert!(packs::execute(&mut sib, "pm-daily__log", &json!({"progress":"x"})).contains("拒绝"));
}

#[test]
fn test_variation_form_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("variation").iter().any(|n| n == "variation__form"));
    assert!(!packs::visible_tool_names("cost").iter().any(|n| n == "variation__form"));
    let mut ctx = ToolCtx::new(p.clone(), "variation", "commercial", "low", true, "iter-var");
    let out = packs::execute(
        &mut ctx,
        "variation__form",
        &json!({"event_facts":"extra edge protection at Tuas","basis":"UNSPECIFIED","qty_note":"待计量"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("签证单草稿.md")).unwrap();
    assert!(text.contains("TBD") || text.contains("待填") || text.contains("UNSPECIFIED"));
    assert!(text.contains("PSSCOC") || text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("文件类型判定"), "{text}");
    assert!(text.contains("签认栏"), "{text}");
    assert!(text.contains("变更编号待填") || text.contains("依据"), "{text}");
    assert!(!text.contains("见图"), "{text}");
    assert!(!text.contains("可以开工"));
    let mut numbered = ToolCtx::new(p.clone(), "variation", "commercial", "low", true, "iter-var-vo");
    let nout = packs::execute(
        &mut numbered,
        "variation__form",
        &json!({"event_facts":"临边栏杆 变更编号 VO-12","jurisdiction":"SG"}),
    );
    assert!(nout.contains("已写入"), "{nout}");
    let nt = std::fs::read_to_string(numbered.out_dir.join("签证单草稿.md")).unwrap();
    assert!(nt.contains("VO-12"), "{nt}");
    assert!(nt.contains("工程签证"), "{nt}");
    assert!(!nt.contains("见图"), "{nt}");
    let mut sib = ToolCtx::new(p, "cost", "commercial", "low", true, "iter-var-sib");
    assert!(packs::execute(&mut sib, "variation__form", &json!({"event_facts":"x"})).contains("拒绝"));
}

#[test]
fn test_subcontract_sheet_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("subcontract").iter().any(|n| n == "subcontract__sheet"));
    assert!(!packs::visible_tool_names("cost").iter().any(|n| n == "subcontract__sheet"));
    let mut ctx = ToolCtx::new(p.clone(), "subcontract", "commercial", "low", true, "iter-sub");
    let out = packs::execute(
        &mut ctx,
        "subcontract__sheet",
        &json!({"package":"模板 120m2","qty_note":"钢筋 2.5t","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("分包结算表头.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("本期完成"), "{text}");
    assert!(text.contains("模板"), "{text}");
    assert!(text.contains("120"), "{text}");
    assert!(text.contains("钢筋"), "{text}");
    assert!(text.contains("2.5"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("PSSCOC") || text.contains("SOP Act"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "cost", "commercial", "low", true, "iter-sub-sib");
    assert!(packs::execute(&mut sib, "subcontract__sheet", &json!({"package":"x"})).contains("拒绝"));
}

#[test]
fn test_interim_measure_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("interim").iter().any(|n| n == "interim__measure"));
    assert!(!packs::visible_tool_names("cost").iter().any(|n| n == "interim__measure"));
    let mut ctx = ToolCtx::new(p.clone(), "interim", "commercial", "low", true, "iter-int");
    let out = packs::execute(
        &mut ctx,
        "interim__measure",
        &json!({"period":"2026-08","qty_note":"模板 120m2","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("验工计价草稿.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("计量草表"), "{text}");
    assert!(text.contains("上期末开累"), "{text}");
    assert!(text.contains("监理审"), "{text}");
    assert!(text.contains("业主核"), "{text}");
    assert!(text.contains("模板"), "{text}");
    assert!(text.contains("120"), "{text}");
    assert!(text.contains("TBD"), "{text}");
    assert!(text.contains("Security of Payment"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("报审通过"), "{text}");
    let mut sib = ToolCtx::new(p, "cost", "commercial", "low", true, "iter-int-sib");
    assert!(packs::execute(&mut sib, "interim__measure", &json!({"period":"x"})).contains("拒绝"));
}

#[test]
fn test_plan_master_network_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("plan-master").iter().any(|n| n == "plan-master__network"));
    assert!(!packs::visible_tool_names("plan-lookahead").iter().any(|n| n == "plan-master__network"));
    let mut ctx = ToolCtx::new(p.clone(), "plan-master", "planning", "low", true, "iter-pm");
    let out = packs::execute(
        &mut ctx,
        "plan-master__network",
        &json!({"level":"master","milestones":"主体结构；封顶","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("计划骨架.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("WBS") || text.contains("工作分解"), "{text}");
    assert!(text.contains("紧前"), "{text}");
    assert!(text.contains("主体结构"), "{text}");
    assert!(text.contains("封顶"), "{text}");
    assert!(text.contains("关键线路=待计算") || text.contains("关键线路待计算"), "{text}");
    assert!(text.contains("PSSCOC"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "plan-lookahead", "planning", "low", true, "iter-pm-sib");
    assert!(packs::execute(&mut sib, "plan-master__network", &json!({"level":"master"})).contains("拒绝"));
}

#[test]
fn test_plan_lookahead_week_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("plan-lookahead").iter().any(|n| n == "plan-lookahead__week"));
    assert!(!packs::visible_tool_names("plan-master").iter().any(|n| n == "plan-lookahead__week"));
    let mut ctx = ToolCtx::new(p.clone(), "plan-lookahead", "planning", "low", true, "iter-pl");
    let out = packs::execute(
        &mut ctx,
        "plan-lookahead__week",
        &json!({
            "window": "第1–4周",
            "works": "3层砌筑",
            "constraints": "塔吊未到",
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("周月计划骨架.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("第1周"), "{text}");
    assert!(text.contains("第4周"), "{text}");
    assert!(text.contains("3层砌筑"), "{text}");
    assert!(text.contains("塔吊未到"), "{text}");
    assert!(text.contains("制约未清") || text.contains("不得写入本周承诺"), "{text}");
    let sec = text.split("## 5 周承诺").nth(1).unwrap().split("## 6").next().unwrap();
    assert!(sec.contains("不得写入本周承诺"), "{sec}");
    assert!(!sec.contains("3层砌筑"), "{sec}");
    assert!(text.contains("Last Planner"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("可以复工"), "{text}");
    let mut ctx2 = ToolCtx::new(p.clone(), "plan-lookahead", "planning", "low", true, "iter-pl-ok");
    let out2 = packs::execute(
        &mut ctx2,
        "plan-lookahead__week",
        &json!({
            "window": "四周",
            "works": "3层砌筑",
            "constraints": "制约已清",
            "jurisdiction": "CN"
        }),
    );
    assert!(out2.contains("已写入"), "{out2}");
    let text2 = std::fs::read_to_string(ctx2.out_dir.join("周月计划骨架.md")).unwrap();
    let sec2 = text2.split("## 5 周承诺").nth(1).unwrap().split("## 6").next().unwrap();
    assert!(sec2.contains("3层砌筑"), "{sec2}");
    assert!(!sec2.contains("不得写入本周承诺"), "{sec2}");
    assert!(text2.contains("不是工期签证"), "{text2}");
    let mut sib = ToolCtx::new(p, "plan-master", "planning", "low", true, "iter-pl-sib");
    assert!(packs::execute(&mut sib, "plan-lookahead__week", &json!({"window":"x"})).contains("拒绝"));
}

#[test]
fn test_plan_resource_peak_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("plan-resource").iter().any(|n| n == "plan-resource__peak"));
    assert!(!packs::visible_tool_names("plan-master").iter().any(|n| n == "plan-resource__peak"));
    let mut ctx = ToolCtx::new(p.clone(), "plan-resource", "planning", "low", true, "iter-pr");
    let out = packs::execute(
        &mut ctx,
        "plan-resource__peak",
        &json!({
            "trades": "木工",
            "equipment": "塔吊",
            "items": "钢筋",
            "window": "W32",
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("资源峰值表头.md")).unwrap();
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("劳动力负荷表头"), "{text}");
    assert!(text.contains("施工机具负荷表头"), "{text}");
    assert!(text.contains("主要材料与周转料表头"), "{text}");
    let labor = text.split("## 3 劳动力负荷表头").nth(1).unwrap().split("## 4").next().unwrap();
    let plant = text.split("## 4 施工机具负荷表头").nth(1).unwrap().split("## 5").next().unwrap();
    let mat = text.split("## 5 主要材料与周转料表头").nth(1).unwrap().split("## 6").next().unwrap();
    assert!(labor.contains("木工"), "{labor}");
    assert!(plant.contains("塔吊"), "{plant}");
    assert!(mat.contains("钢筋"), "{mat}");
    assert!(labor.contains("TBD"), "{labor}");
    assert!(text.contains("C-Score") || text.contains("Buildability"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("已满足施工需要"), "{text}");
    assert!(!text.contains("已安排进场"), "{text}");
    let mut ctx2 = ToolCtx::new(p.clone(), "plan-resource", "planning", "low", true, "iter-pr-qty");
    let out2 = packs::execute(
        &mut ctx2,
        "plan-resource__peak",
        &json!({"trades": "木工20人", "jurisdiction": "CN"}),
    );
    assert!(out2.contains("已写入"), "{out2}");
    let text2 = std::fs::read_to_string(ctx2.out_dir.join("资源峰值表头.md")).unwrap();
    let labor2 = text2.split("## 3 劳动力负荷表头").nth(1).unwrap().split("## 4").next().unwrap();
    assert!(labor2.contains("木工"), "{labor2}");
    assert!(labor2.contains("20人"), "{labor2}");
    assert!(labor2.contains("用户给定"), "{labor2}");
    assert!(text2.contains("劳动定额") || text2.contains("不编工日"), "{text2}");
    let mut sib = ToolCtx::new(p, "plan-lookahead", "planning", "low", true, "iter-pr-sib");
    assert!(packs::execute(&mut sib, "plan-resource__peak", &json!({"trades":"x"})).contains("拒绝"));
}

#[test]
fn test_lab_mix_report_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("lab-mix").iter().any(|n| n == "lab-mix__report"));
    assert!(!packs::visible_tool_names("lab-sample").iter().any(|n| n == "lab-mix__report"));
    let mut blocked = ToolCtx::new(p.clone(), "lab-mix", "lab", "high", false, "iter-mix-hitl");
    assert!(packs::execute(
        &mut blocked,
        "lab-mix__report",
        &json!({"material":"C40","has_trial_data":false}),
    )
    .contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "lab-mix", "lab", "high", true, "iter-mix-t033");
    let out = packs::execute(
        &mut ctx,
        "lab-mix__report",
        &json!({"material":"C40","has_trial_data":false,"jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("配比报告提纲.md")).unwrap();
    assert!(text.contains("初步（理论）配合比"), "{text}");
    assert!(text.contains("基准配合比"), "{text}");
    assert!(text.contains("试验室配合比"), "{text}");
    assert!(text.contains("施工配合比"), "{text}");
    assert!(text.contains("C40"), "{text}");
    assert!(text.contains("不给施工配合比"), "{text}");
    assert!(text.contains("SAC"), "{text}");
    assert!(!text.contains("JGJ"), "{text}");
    assert!(!text.contains("GB 50"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("已具备开盘条件"), "{text}");
    let mut ctx2 = ToolCtx::new(p.clone(), "lab-mix", "lab", "high", true, "iter-mix-trial");
    let out2 = packs::execute(
        &mut ctx2,
        "lab-mix__report",
        &json!({"material":"C40","has_trial_data":true,"jurisdiction":"SG"}),
    );
    assert!(out2.contains("已写入"), "{out2}");
    let t2 = std::fs::read_to_string(ctx2.out_dir.join("配比报告提纲.md")).unwrap();
    let row = t2.split("| 施工配合比 |").nth(1).unwrap().split('\n').next().unwrap();
    assert!(row.contains("试验室签认"), "{row}");
    assert!(!row.contains("不给施工配合比"), "{row}");
    let mut sib = ToolCtx::new(p, "lab-sample", "lab", "high", true, "iter-mix-sib");
    assert!(packs::execute(&mut sib, "lab-mix__report", &json!({"material":"x"})).contains("拒绝"));
}

#[test]
fn test_lab_sample_list_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("lab-sample").iter().any(|n| n == "lab-sample__list"));
    assert!(!packs::visible_tool_names("lab-record").iter().any(|n| n == "lab-sample__list"));
    let mut blocked = ToolCtx::new(p.clone(), "lab-sample", "lab", "high", false, "iter-ls-hitl");
    assert!(packs::execute(&mut blocked, "lab-sample__list", &json!({"materials":"钢筋"})).contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "lab-sample", "lab", "high", true, "iter-ls");
    let out = packs::execute(
        &mut ctx,
        "lab-sample__list",
        &json!({"materials":"钢筋","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("取样送检清单.md")).unwrap();
    assert!(text.contains("类别"), "{text}");
    assert!(text.contains("部位"), "{text}");
    assert!(text.contains("见证人"), "{text}");
    assert!(text.contains("升级路径"), "{text}");
    assert!(text.contains("钢筋"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("（空）"), "{text}");
    assert!(text.contains("SAC"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("已取样合格"), "{text}");
    let mut sib = ToolCtx::new(p, "lab-mix", "lab", "high", true, "iter-ls-sib");
    assert!(packs::execute(&mut sib, "lab-sample__list", &json!({"materials":"x"})).contains("拒绝"));
}

#[test]
fn test_lab_record_ledger_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("lab-record").iter().any(|n| n == "lab-record__ledger"));
    assert!(!packs::visible_tool_names("lab-sample").iter().any(|n| n == "lab-record__ledger"));
    let mut ctx = ToolCtx::new(p.clone(), "lab-record", "lab", "low", true, "iter-lr");
    let out = packs::execute(
        &mut ctx,
        "lab-record__ledger",
        &json!({"samples":"cube-1","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("试验台账骨架.md")).unwrap();
    assert!(text.contains("报告编号"), "{text}");
    assert!(text.contains("仪器检定"), "{text}");
    assert!(text.contains("结论"), "{text}");
    assert!(text.contains("cube-1"), "{text}");
    assert!(text.contains("待核"), "{text}");
    assert!(text.contains("SAC"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut cn = ToolCtx::new(p.clone(), "lab-record", "lab", "low", true, "iter-lr-cn");
    let co = packs::execute(
        &mut cn,
        "lab-record__ledger",
        &json!({"samples":"cube-1","jurisdiction":"CN"}),
    );
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(cn.out_dir.join("试验台账骨架.md")).unwrap();
    assert!(ct.contains("质量检测"), "{ct}");
    assert!(!ct.contains("SAC"), "{ct}");
    let mut sib = ToolCtx::new(p, "lab-sample", "lab", "high", true, "iter-lr-sib");
    assert!(packs::execute(&mut sib, "lab-record__ledger", &json!({"samples":"x"})).contains("拒绝"));
}

#[test]
fn test_supervision_reply_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("supervision").iter().any(|n| n == "supervision__reply"));
    let mut ctx = ToolCtx::new(p.clone(), "supervision", "docs", "low", true, "iter-sv");
    let out = packs::execute(
        &mut ctx,
        "supervision__reply",
        &json!({"notice":"NCR-1 钢筋保护层","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("监理回复草稿.md")).unwrap();
    assert!(text.contains("来文要点复述"), "{text}");
    assert!(text.contains("拟办"), "{text}");
    assert!(text.contains("证据目录"), "{text}");
    assert!(text.contains("NCR-1"), "{text}");
    assert!(text.contains("C-forms"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("可以复工"), "{text}");
    let mut stop = ToolCtx::new(p.clone(), "supervision", "docs", "low", true, "iter-sv-stop");
    let so = packs::execute(
        &mut stop,
        "supervision__reply",
        &json!({"notice":"暂停令","jurisdiction":"CN"}),
    );
    assert!(so.contains("已写入"), "{so}");
    let st = std::fs::read_to_string(stop.out_dir.join("监理回复草稿.md")).unwrap();
    assert!(st.contains("只出目录"), "{st}");
    assert!(st.contains("监理规范"), "{st}");
    assert!(!st.contains("可以复工"), "{st}");
    let mut sib = ToolCtx::new(p, "admin-doc", "admin", "low", true, "iter-sv-sib");
    assert!(packs::execute(&mut sib, "supervision__reply", &json!({"notice":"x"})).contains("拒绝"));
}

#[test]
fn test_safety_brief_talk_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("safety-brief").iter().any(|n| n == "safety-brief__talk"));
    let mut blocked = ToolCtx::new(p.clone(), "safety-brief", "hse", "high", false, "iter-sb-hitl");
    assert!(packs::execute(&mut blocked, "safety-brief__talk", &json!({"work_item":"临边"})).contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "safety-brief", "hse", "high", true, "iter-sb");
    let out = packs::execute(
        &mut ctx,
        "safety-brief__talk",
        &json!({"work_item":"临边","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("安全交底草稿.md")).unwrap();
    assert!(text.contains("## 1 封面"), "{text}");
    assert!(text.contains("## 11 签字栏"), "{text}");
    assert!(text.contains("临边"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("电话"), "{text}");
    assert!(text.contains("toolbox"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut cn = ToolCtx::new(p.clone(), "safety-brief", "hse", "high", true, "iter-sb-cn");
    let co = packs::execute(
        &mut cn,
        "safety-brief__talk",
        &json!({"work_item":"临边","jurisdiction":"CN"}),
    );
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(cn.out_dir.join("安全交底草稿.md")).unwrap();
    assert!(ct.contains("安全技术交底"), "{ct}");
    assert!(!ct.contains("toolbox"), "{ct}");
    let mut sib = ToolCtx::new(p, "quality", "hse", "high", true, "iter-sb-sib");
    assert!(packs::execute(&mut sib, "safety-brief__talk", &json!({"work_item":"x"})).contains("拒绝"));
}

#[test]
fn test_quality_lot_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("quality").iter().any(|n| n == "quality__lot"));
    let mut blocked = ToolCtx::new(p.clone(), "quality", "hse", "high", false, "iter-q-hitl");
    assert!(packs::execute(&mut blocked, "quality__lot", &json!({"inspection_lot":"slab","items":"cover"})).contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "quality", "hse", "high", true, "iter-q-t035");
    let out = packs::execute(
        &mut ctx,
        "quality__lot",
        &json!({"inspection_lot":"slab-1","items":"cover","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("质量检查表.md")).unwrap();
    assert!(text.contains("主控项目检查栏"), "{text}");
    assert!(text.contains("一般项目检查栏"), "{text}");
    assert!(text.contains("隐蔽专项"), "{text}");
    assert!(text.contains("未检"), "{text}");
    assert!(text.contains("cover"), "{text}");
    assert!(text.contains("CONQUAS"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "env", "hse", "low", true, "iter-q-sib");
    assert!(packs::execute(&mut sib, "quality__lot", &json!({"inspection_lot":"x"})).contains("拒绝"));
}

#[test]
fn test_env_list_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("env").iter().any(|n| n == "env__list"));
    let mut ctx = ToolCtx::new(p.clone(), "env", "hse", "low", true, "iter-env-t035");
    let out = packs::execute(
        &mut ctx,
        "env__list",
        &json!({"site":"Tuas","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("环保文明清单.md")).unwrap();
    assert!(text.contains("扬尘"), "{text}");
    assert!(text.contains("弃土"), "{text}");
    assert!(text.contains("污水"), "{text}");
    assert!(text.contains("夜间"), "{text}");
    assert!(text.contains("市容"), "{text}");
    assert!(text.contains("UNSPECIFIED"), "{text}");
    assert!(text.contains("NEA"), "{text}");
    assert!(text.contains("Earth Control"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "quality", "hse", "high", true, "iter-env-t035-sib");
    assert!(packs::execute(&mut sib, "env__list", &json!({"site":"x"})).contains("拒绝"));
}

#[test]
fn test_emergency_plan_exclusive() {
    let p = paths();
    assert!(packs::visible_tool_names("emergency").iter().any(|n| n == "emergency__plan"));
    assert!(!packs::visible_tool_names("env").iter().any(|n| n == "emergency__plan"));
    let mut blocked = ToolCtx::new(p.clone(), "emergency", "hse", "high", false, "iter-em-hitl");
    assert!(packs::execute(&mut blocked, "emergency__plan", &json!({"scenario":"fire"})).contains("拒绝"));
    let mut ctx = ToolCtx::new(p.clone(), "emergency", "hse", "high", true, "iter-em-t035");
    let out = packs::execute(
        &mut ctx,
        "emergency__plan",
        &json!({"scenario":"fire","jurisdiction":"SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("应急预案提纲.md")).unwrap();
    assert!(text.contains("综合预案目录"), "{text}");
    assert!(text.contains("专项预案目录"), "{text}");
    assert!(text.contains("演练计划与记录表头"), "{text}");
    assert!(text.contains("火灾爆炸"), "{text}");
    assert!(text.contains("本轮点名"), "{text}");
    assert!(text.contains("[A001]"), "{text}");
    assert!(text.contains("SCDF"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    assert!(!text.contains("演练合格"), "{text}");
    let mut cn = ToolCtx::new(p.clone(), "emergency", "hse", "high", true, "iter-em-cn-t035");
    let co = packs::execute(
        &mut cn,
        "emergency__plan",
        &json!({"scenario":"fire","jurisdiction":"CN"}),
    );
    assert!(co.contains("已写入"), "{co}");
    let ct = std::fs::read_to_string(cn.out_dir.join("应急预案提纲.md")).unwrap();
    assert!(ct.contains("应急预案"), "{ct}");
    assert!(!ct.contains("SCDF"), "{ct}");
    let mut sib = ToolCtx::new(p, "env", "hse", "low", true, "iter-em-sib");
    assert!(packs::execute(&mut sib, "emergency__plan", &json!({"scenario":"x"})).contains("拒绝"));
}

#[test]
fn test_bid_compliance_exclusive_and_scan_sg_mix() {
    let p = paths();
    assert!(packs::visible_tool_names("bid-compliance").iter().any(|n| n == "bid-compliance__gaps"));
    assert!(!packs::visible_tool_names("bid-parse").iter().any(|n| n == "bid-compliance__gaps"));
    let mut ctx = ToolCtx::new(p.clone(), "bid-compliance", "bid", "low", true, "iter-gaps");
    let out = packs::execute(
        &mut ctx,
        "bid-compliance__gaps",
        &json!({"required_items":"method statement\nCSOC","response_notes":"method statement attached"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let gaps = std::fs::read_to_string(ctx.out_dir.join("响应缺口清单.md")).unwrap();
    assert!(gaps.contains("辖区：SG"), "{gaps}");
    assert!(gaps.contains("GeBIZ") || gaps.contains("PQM"), "{gaps}");
    let mut dual = ToolCtx::new(p.clone(), "bid-compliance", "bid", "low", true, "iter-gaps-dual");
    let dout = packs::execute(
        &mut dual,
        "bid-compliance__gaps",
        &json!({"required_items":"CSOC","jurisdiction":"DUAL","other_jurisdiction":"CN"}),
    );
    assert!(dout.contains("已写入"), "{dout}");
    let dt = std::fs::read_to_string(dual.out_dir.join("响应缺口清单.md")).unwrap();
    assert!(dt.contains("DUAL（SG + CN）"), "{dt}");
    let mut cn = ToolCtx::new(p.clone(), "bid-compliance", "bid", "low", true, "iter-gaps-cn");
    let cnot = packs::execute(
        &mut cn,
        "bid-compliance__gaps",
        &json!({"required_items":"x","jurisdiction":"CN"}),
    );
    assert!(cnot.contains("已写入"), "{cnot}");
    let ctext = std::fs::read_to_string(cn.out_dir.join("响应缺口清单.md")).unwrap();
    assert!(ctext.contains("辖区：CN"), "{ctext}");
    assert!(!ctext.contains("GeBIZ"), "{ctext}");
    let mut sib = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, "iter-gaps-sib");
    assert!(packs::execute(&mut sib, "bid-compliance__gaps", &json!({"required_items":"x"})).contains("拒绝"));

    let mut sc = ToolCtx::new(p.clone(), "construction", "construction", "high", true, "iter-scan");
    let draft = "辖区：SG\n引用建办质〔2018〕31 号和 JGJ 80\n增值税\n";
    std::fs::write(sc.out_dir.join("mix.md"), draft).unwrap();
    let scan = packs::execute(&mut sc, "construction__scan_forbidden", &json!({"filename":"mix.md"}));
    assert!(scan.contains("扫描未通过"), "{scan}");
    assert!(scan.contains("JGJ") || scan.contains("建办质"), "{scan}");
    assert!(scan.contains("增值税"), "{scan}");

    let mut blocked = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, "iter-write-gate");
    let refuse = packs::execute(
        &mut blocked,
        "write_deliverable",
        &json!({"filename":"bad.md","markdown":"辖区：SG\n可以开工\nJGJ 80\n"}),
    );
    assert!(refuse.contains("拒绝写盘"), "{refuse}");
    assert!(refuse.contains("可以开工") || refuse.contains("JGJ"), "{refuse}");
    assert!(!blocked.out_dir.join("bad.md").is_file(), "bad draft must not land");

    let mut clean = ToolCtx::new(p, "bid-parse", "bid", "low", true, "iter-write-ok");
    let ok = packs::execute(
        &mut clean,
        "write_deliverable",
        &json!({"filename":"ok.md","markdown":"辖区：SG\n[A001] 待填\n条款 UNSPECIFIED\n"}),
    );
    assert!(ok.contains("已写入"), "{ok}");
    assert!(clean.out_dir.join("ok.md").is_file());
}

#[test]
fn test_search_kb_boosts_web_knowledge() {
    let p = paths();
    let hits = search_kb(&p, "construction", "construction", "PTW", 8);
    assert!(
        hits.iter().any(|h| h.path.replace('\\', "/").ends_with("web-knowledge.md")),
        "expected web-knowledge hit, got {:?}",
        hits.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let fire = search_kb(&p, "fire-protect", "design", "Fire Code", 8);
    assert!(
        fire.iter().any(|h| h.path.replace('\\', "/").contains("fire-protect")
            && h.path.replace('\\', "/").ends_with("web-knowledge.md")),
        "expected fire-protect web-knowledge, got {:?}",
        fire.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let portals = search_kb(&p, "construction", "construction", "CORENET", 8);
    assert!(
        portals.iter().any(|h| h.path.replace('\\', "/").ends_with("web-portals.md")),
        "expected company web-portals, got {:?}",
        portals.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let gst = search_kb(&p, "finance-tax", "finance", "GST IRAS", 8);
    assert!(
        gst.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected GST web hit, got {:?}",
        gst.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let conquas = search_kb(&p, "quality", "hse", "CONQUAS", 8);
    assert!(
        conquas.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected CONQUAS web hit, got {:?}",
        conquas.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let portals = std::fs::read_to_string(p.kb_root.join("company").join("web-portals.md")).unwrap_or_default();
    assert!(
        portals.contains("APPBCA-2026-12") && portals.contains("5,000"),
        "company portals must record CORENET X 2026-10-01 GFA≥5000 narrowing: {portals}"
    );
    let labor = search_kb(&p, "hr-labor", "hr", "劳动合同法", 8);
    assert!(
        labor.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")),
        "expected CN labor web hit, got {:?}",
        labor.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let ct06 = search_kb(&p, "lab-mix", "lab", "CT 06", 8);
    assert!(
        ct06.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected CT 06 web hit, got {:?}",
        ct06.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let bdas = search_kb(&p, "plan-resource", "planning", "Buildability", 8);
    assert!(
        bdas.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected Buildability web hit, got {:?}",
        bdas.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let pss = search_kb(&p, "cost", "commercial", "PSSCOC-lite", 8);
    assert!(
        pss.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected PSSCOC-lite web hit, got {:?}",
        pss.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let rm = search_kb(&p, "architecture", "design", "Reportable Matters", 8);
    assert!(
        rm.iter().any(|h| h.path.replace('\\', "/").contains("web-portals")
            || h.path.replace('\\', "/").contains("web-knowledge")),
        "expected Reportable Matters web hit, got {:?}",
        rm.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let cx = search_kb(&p, "bim-deliver", "bim", "APPBCA-2026-12 CORENET", 8);
    assert!(
        cx.iter().any(|h| h.path.replace('\\', "/").contains("web-knowledge")
            || h.path.replace('\\', "/").contains("web-portals")),
        "expected CORENET circular web hit, got {:?}",
        cx.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let sg = search_kb(&p, "construction", "construction", "PTW Singapore WSH", 8);
    assert!(
        !sg.iter()
            .take(3)
            .any(|h| h.path.replace('\\', "/").contains("order-37")),
        "SG query should not top-rank 37-order notes: {:?}",
        sg.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
}

#[test]
fn test_sg_construction_scheme_deliverable() {
    assert_eq!(packs::normalize_jurisdiction(""), "SG");
    assert_eq!(packs::normalize_jurisdiction("singapore"), "SG");
    let p = paths();
    let mut ctx = ToolCtx::new(
        p,
        "construction",
        "construction",
        "high",
        true,
        "sg-site-align",
    );
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({
            "project_name": "Tuas South Boulevard warehouse",
            "work_scope": "edge protection / working at height on slab opening",
            "jurisdiction": "SG",
            "site_name": "Tuas South site",
            "known_facts": "RC slab opening, no measured height given",
            "unknowns": "guardrail height and platform rating not surveyed"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let path = ctx.out_dir.join("专项方案-AI草稿.md");
    let text = std::fs::read_to_string(&path).expect("sg draft");
    assert!(text.contains("SG"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"));
    assert!(!text.contains("可以开工"));
    assert!(!text.contains("报审通过"));
    assert!(
        !text.split_whitespace().any(|w| {
            let u = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            (u.starts_with("SS") || u.starts_with("CP")) && u.chars().any(|c| c.is_ascii_digit())
        }),
        "invented SS/CP number in {text}"
    );
    assert!(!text.contains("GB 50"));
    assert!(!text.contains("JGJ"));
}

#[test]
fn test_scheme_dual_zone_banner() {
    let p = paths();
    let mut ctx = ToolCtx::new(p.clone(), "construction", "construction", "high", true, "iter-scheme-dual");
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({
            "project_name": "Tuas warehouse",
            "work_scope": "edge protection",
            "jurisdiction": "DUAL",
            "other_jurisdiction": "CN"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("专项方案-AI草稿.md")).unwrap();
    assert!(text.contains("DUAL（SG + CN）"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "survey", "construction", "high", true, "iter-scheme-dual-sib");
    assert!(
        packs::execute(
            &mut sib,
            "construction__scheme_draft",
            &json!({"project_name":"x","work_scope":"x","jurisdiction":"DUAL"}),
        )
        .contains("拒绝")
    );
}

#[test]
fn test_sg_construction_scheme_blocked_without_confirm() {
    let p = paths();
    let mut ctx = ToolCtx::new(p, "construction", "construction", "high", false, "sg-gate");
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({
            "project_name": "Jurong Island tank farm",
            "work_scope": "working at height",
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("拒绝写盘"), "{out}");
    assert!(out.contains("我明白，将由持证人员签认"));
}

#[test]
fn test_scheme_draft_copies_user_height_and_site() {
    let p = paths();
    let mut ctx = ToolCtx::new(p.clone(), "construction", "construction", "high", true, "scheme-facts");
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({
            "project_name": "滨河路人行道维修",
            "work_scope": "临边防护",
            "site_name": "滨河路人行道",
            "height_m": 3.2,
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("专项方案-AI草稿.md")).unwrap();
    assert!(text.contains("滨河路人行道"), "{text}");
    assert!(text.contains("3.2"), "{text}");
    assert!(text.contains("临边防护"), "{text}");
    assert!(!text.contains("（用户未提供，整节待填）"), "{text}");
    assert!(!text.contains("37号令"), "{text}");
    assert!(!text.contains("JGJ"), "{text}");
    let mut sib = ToolCtx::new(p, "survey", "construction", "high", true, "scheme-facts-sib");
    assert!(packs::execute(
        &mut sib,
        "construction__scheme_draft",
        &json!({"project_name":"滨河路人行道维修","work_scope":"临边防护","height_m":3.2})
    )
    .contains("拒绝"));
}

#[test]
fn test_finish_if_no_deliverable_writes_bid_parse_from_user_blob() {
    let p = paths();
    let exp = seed()
        .experts
        .iter()
        .find(|e| e.id == "bid-parse")
        .cloned()
        .unwrap();
    let mut ctx = ToolCtx::new(p, "bid-parse", "bid", "low", true, "auto-finish-bid");
    let history = vec![json!({
        "role": "user",
        "content": "Technical score 41\n必须编制临边防护专项方案\nmethod statement for working at height"
    })];
    let out = finish_if_no_deliverable(&exp, &history, &mut ctx).expect("auto write");
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains("Technical score 41"), "{text}");
    assert!(text.contains("临边防护专项方案"), "{text}");
}

#[test]
fn test_pack_ship_plan_copies_materials_no_invented_xyz() {
    let p = paths();
    assert!(packs::visible_tool_names("pack-ship").iter().any(|n| n == "pack-ship__plan"));
    assert!(packs::visible_tool_names("pack-ship").iter().any(|n| n == "pack-ship__health"));
    assert!(!packs::visible_tool_names("warehouse").iter().any(|n| n == "pack-ship__plan"));
    assert!(!packs::visible_tool_names("warehouse").iter().any(|n| n == "pack-ship__health"));
    let mut ctx = ToolCtx::new(p.clone(), "pack-ship", "plant", "low", true, "pack-ship-1");
    let out = packs::execute(
        &mut ctx,
        "pack-ship__plan",
        &json!({
            "project_name": "Tuas steel batch",
            "materials": "H-beam 12m x 20 pcs, no unit weight given",
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("装箱作业单.md")).unwrap();
    assert!(text.contains("H-beam 12m x 20 pcs"), "{text}");
    assert!(text.contains("CTU Code"), "{text}");
    assert!(text.contains("UNSPECIFIED") || text.contains("packing-agent"), "{text}");
    assert!(!text.contains("(0.0, 0.0, 0.0)"), "{text}");
    assert!(!text.contains("可以开工"), "{text}");
    let mut sib = ToolCtx::new(p, "warehouse", "plant", "low", true, "pack-ship-sib");
    assert!(
        packs::execute(
            &mut sib,
            "pack-ship__plan",
            &json!({"materials": "H-beam 12m x 20 pcs"})
        )
        .contains("拒绝")
    );
    let parsed = civil_workbench::packing_bridge::summarize_pipeline_json(
        &json!({
            "ok": true,
            "summary": {"n0": 2, "containers_used": 2, "boxes": 4, "can_fit": true, "ship_ok": true, "phase": "done"}
        }),
        "unit",
    );
    assert!(parsed.ok);
    assert_eq!(parsed.n0, "2");
    assert_eq!(parsed.can_fit, "true");
    assert!(parsed.markdown().contains("n0: 2"));
}

#[test]
fn test_import_local_and_firm_bid_pack() {
    let p = paths();
    let src = p.demo_root.join("data").join("firm-sample-itt.txt");
    let _ = std::fs::create_dir_all(src.parent().unwrap());
    std::fs::write(
        &src,
        "INVITATION TO TENDER — Tuas warehouse\n\
Evaluation criteria (PQM):\n\
- Quality 40%\n\
- Price 60%\n\
Tenderers shall submit a method statement for working at height.\n\
Required: 临边防护专项方案\n\
Time for Completion: 180 days\n\
BCA workhead CW01\n\
Two Envelope: technical and price separately\n\
Item 1 Drainage m 120\n",
    )
    .unwrap();
    let sid = format!("firm-local-{}", std::process::id());
    let imported = civil_workbench::attach::import_local(&p, &sid, src.to_str().unwrap()).unwrap();
    assert!(!imported.is_empty());
    assert!(civil_workbench::attach::allow_local_path(&p, r"D:\layout").is_err());
    assert!(civil_workbench::attach::allow_local_path(&p, r"D:\layout\tender.pdf").is_err());

    let v = civil_workbench::firm::run_bid_job(
        &p,
        &sid,
        &json!({
            "project_name": "Tuas warehouse",
            "jurisdiction": "SG",
            "path": src.to_string_lossy(),
        }),
    );
    assert_eq!(v["ok"], true, "{v}");
    let files = v["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["name"] == "招标解析表.md"), "{v}");
    assert!(files.iter().any(|f| f["name"] == "技术标目录草稿.md"), "{v}");
    assert!(files.iter().any(|f| f["name"] == "响应缺口清单.md"), "{v}");
    assert!(files.iter().any(|f| f["name"] == "成套作业单.md"), "{v}");
    assert!(files.iter().any(|f| f["name"] == "价表-待填.md"), "{v}");
    let parse_path = files
        .iter()
        .find(|f| f["name"] == "招标解析表.md")
        .unwrap()["path"]
        .as_str()
        .unwrap();
    let text = std::fs::read_to_string(parse_path).unwrap();
    assert!(text.contains("Quality 40%"), "{text}");
    assert!(text.contains("临边防护专项方案"), "{text}");
    assert!(text.contains("CW01"), "{text}");
    assert!(text.contains("Two Envelope"), "{text}");
    let price = std::fs::read_to_string(
        std::path::Path::new(v["job_dir"].as_str().unwrap()).join("价表-待填.md"),
    )
    .unwrap();
    assert!(price.contains("Drainage"), "{price}");
    assert!(price.contains("UNSPECIFIED"), "{price}");
    let unit_cells: Vec<&str> = price
        .lines()
        .filter(|l| l.starts_with('|') && l.contains("Drainage"))
        .collect();
    assert!(!unit_cells.is_empty(), "{price}");
    for line in &unit_cells {
        let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
        assert!(cols.iter().any(|c| *c == "UNSPECIFIED"), "{line}");
        assert!(!line.contains("$"), "{line}");
        assert!(!line.contains("S$"), "{line}");
    }
    let gaps = files
        .iter()
        .find(|f| f["name"] == "响应缺口清单.md")
        .and_then(|f| f["path"].as_str())
        .unwrap();
    let gap_text = std::fs::read_to_string(gaps).unwrap();
    assert!(gap_text.contains("workhead") || gap_text.contains("CW01"), "{gap_text}");
    assert!(gap_text.contains("信封") || gap_text.contains("Two Envelope"), "{gap_text}");
    assert!(!gap_text.contains("可以投标"), "{gap_text}");
    let index = std::fs::read_to_string(
        std::path::Path::new(v["job_dir"].as_str().unwrap()).join("成套作业单.md"),
    )
    .unwrap();
    assert!(index.contains("招标解析表.md"), "{index}");

    let mut as_tech = ToolCtx::new(p, "bid-tech", "bid", "low", true, "firm-as-tech");
    let out = packs::execute(
        &mut as_tech,
        "firm__bid_pack",
        &json!({"tender_text": "Quality 33%\nmethod statement for working at height", "project_name": "solo"}),
    );
    assert!(out.contains("招标解析表") || out.contains("\"ok\":true"), "{out}");
}

#[test]
fn test_harness_steps_trace_hitl_and_shadow() {
    let p = paths();
    let tender = "\
Quality 40%\n\
Price 60%\n\
Two Envelope: technical and price separately\n\
BCA workhead CW01\n\
Item 1 Drainage m 120\n\
method statement for working at height\n\
临边防护专项方案";
    let ticket = civil_workbench::harness::Ticket {
        session: "harness-run-01".into(),
        project: "Tuas".into(),
        jurisdiction: "SG".into(),
        brief: tender.into(),
        path: String::new(),
        confirm_ok: false,
    };
    let run = civil_workbench::harness::run_bid_steps(&p, ticket.clone());
    assert!(run.error.is_none(), "{:?}", run.error);
    assert_eq!(run.mode, "steps");
    assert_eq!(run.illegal_count(), 0);
    assert!(run.hitl.required);
    assert!(run.hitl.pending);
    assert!(run.run_dir.join("trace.json").is_file());
    assert!(run.steps.iter().any(|s| s.tool == "bid-parse__extract" && s.ok));
    assert!(run.steps.iter().any(|s| s.name == "scheme_gate"));
    let ev = civil_workbench::harness::shadow_eval(&p, ticket);
    assert_eq!(ev["illegal_tool_calls"], 0, "{ev}");
    assert_eq!(ev["agree_core"], true, "{ev}");
    assert_eq!(ev["ok"], true, "{ev}");
}

#[test]
fn test_iter18_remaining_exclusives_write_and_sibling_refused() {
    let p = paths();
    let cases: &[(&str, &str, &str, &str, &str, &str, Value)] = &[
        (
            "fire-protect",
            "design",
            "high",
            "fire-protect__brief",
            "architecture",
            "消防专篇提纲.md",
            json!({"scope": "Tuas warehouse", "systems": "hydrant", "jurisdiction": "SG"}),
        ),
        (
            "plan-resource",
            "planning",
            "low",
            "plan-resource__peak",
            "plan-master",
            "资源峰值表头.md",
            json!({"trades": "formwork", "window": "W32", "jurisdiction": "SG"}),
        ),
        (
            "bim-deliver",
            "bim",
            "low",
            "bim-deliver__lod",
            "bim-coord",
            "BIM交付清单.md",
            json!({"stage": "Design gateway", "lod": "UNSPECIFIED", "jurisdiction": "SG"}),
        ),
        (
            "plumbing",
            "design",
            "low",
            "plumbing__memo",
            "hvac",
            "给排水专业说明草稿.md",
            json!({"scope": "sanitary stack", "jurisdiction": "SG"}),
        ),
        (
            "bridge",
            "design",
            "high",
            "bridge__outline",
            "tunnel",
            "桥梁提纲.md",
            json!({"span_note": "pending survey", "jurisdiction": "SG"}),
        ),
        (
            "proc-vendor",
            "procurement",
            "low",
            "proc-vendor__eval",
            "proc-plan",
            "供方评价表头.md",
            json!({"vendor": "local fabricator"}),
        ),
        (
            "it-app",
            "it",
            "low",
            "it-app__srs",
            "it-ops",
            "需求说明书骨架.md",
            json!({"system": "site access app", "users": "gate"}),
        ),
        (
            "hr-train",
            "hr",
            "low",
            "hr-train__plan",
            "hr-recruit",
            "培训计划骨架.md",
            json!({"audience": "new workers", "topics": "CSOC"}),
        ),
        (
            "material-site",
            "plant",
            "low",
            "material-site__recon",
            "warehouse",
            "材料耗用核算表头.md",
            json!({"items": "rebar\nconcrete", "notes": "no stocktake"}),
        ),
        (
            "civil-defense",
            "design",
            "high",
            "civil-defense__brief",
            "architecture",
            "人防专篇提纲.md",
            json!({"scope": "storey shelter", "jurisdiction": "SG"}),
        ),
        (
            "facade",
            "design",
            "high",
            "facade__brief",
            "architecture",
            "幕墙专篇提纲.md",
            json!({"system": "unitized", "jurisdiction": "SG"}),
        ),
        (
            "landscape",
            "design",
            "low",
            "landscape__memo",
            "architecture",
            "景观专业说明草稿.md",
            json!({"scope": "streetscape", "jurisdiction": "SG"}),
        ),
        (
            "hydraulic",
            "design",
            "high",
            "hydraulic__outline",
            "port",
            "水利提纲.md",
            json!({"scope": "drain", "jurisdiction": "SG"}),
        ),
        (
            "port",
            "design",
            "high",
            "port__outline",
            "hydraulic",
            "港航提纲.md",
            json!({"scope": "berth", "jurisdiction": "SG"}),
        ),
        (
            "tunnel",
            "design",
            "high",
            "tunnel__outline",
            "bridge",
            "隧道提纲.md",
            json!({"method": "cut and cover", "jurisdiction": "SG"}),
        ),
        (
            "municipal",
            "design",
            "low",
            "municipal__memo",
            "traffic",
            "市政道路原则.md",
            json!({"scope": "access road", "jurisdiction": "SG"}),
        ),
        (
            "traffic",
            "design",
            "low",
            "traffic__skeleton",
            "municipal",
            "交通报告骨架.md",
            json!({"corridor": "Pioneer", "jurisdiction": "SG"}),
        ),
        (
            "intel-weak",
            "design",
            "low",
            "intel-weak__memo",
            "electrical",
            "弱电专业说明草稿.md",
            json!({"systems": "access", "jurisdiction": "SG"}),
        ),
        (
            "interior",
            "design",
            "low",
            "interior__schedule",
            "architecture",
            "室内界面表.md",
            json!({"rooms": "lobby", "jurisdiction": "SG"}),
        ),
        (
            "hvac",
            "design",
            "low",
            "hvac__memo",
            "plumbing",
            "暖通专业说明草稿.md",
            json!({"scope": "ACMV", "jurisdiction": "SG"}),
        ),
        (
            "electrical",
            "design",
            "low",
            "electrical__memo",
            "hvac",
            "电气专业说明草稿.md",
            json!({"scope": "MSB", "jurisdiction": "SG"}),
        ),
        (
            "steel",
            "design",
            "high",
            "steel__memo",
            "structure",
            "钢结构专业说明草稿.md",
            json!({"system": "portal", "jurisdiction": "SG"}),
        ),
        (
            "design-coord",
            "design",
            "low",
            "design-coord__minutes",
            "architecture",
            "设计会审纪要.md",
            json!({"issues": "riser clash", "jurisdiction": "SG"}),
        ),
    ];
    for (expert, cat, risk, tool, sibling, file, args) in cases {
        assert!(
            packs::visible_tool_names(expert).iter().any(|n| n == tool),
            "{expert} missing {tool}"
        );
        assert!(
            !packs::visible_tool_names(sibling).iter().any(|n| n == tool),
            "{sibling} must not see {tool}"
        );
        let mut ctx = ToolCtx::new(
            p.clone(),
            expert,
            cat,
            risk,
            true,
            &format!("iter18-{expert}"),
        );
        let out = packs::execute(&mut ctx, tool, args);
        assert!(out.contains("已写入"), "{expert} {tool} {out}");
        let text = std::fs::read_to_string(ctx.out_dir.join(file)).unwrap_or_default();
        assert!(
            text.contains("[A001]") || text.contains("TBD") || text.contains("待填") || text.contains("UNSPECIFIED"),
            "{expert} missing pending marker: {text}"
        );
        assert!(!text.contains("可以开工"), "{expert} {text}");
        assert!(!text.contains("报审通过"), "{expert} {text}");
        assert!(!text.contains("GB 50"), "{expert}");
        if *expert == "bim-deliver" {
            assert!(
                text.contains("CORENET") || text.contains("IFC+SG"),
                "{expert} {text}"
            );
        }
        if *expert == "plan-resource" {
            assert!(
                text.contains("C-Score") || text.contains("定额") || text.contains("工日"),
                "{expert} {text}"
            );
        }
        if *expert == "fire-protect" {
            assert!(
                text.contains("Fire Code") || text.contains("Fire Safety"),
                "{expert} {text}"
            );
        }
        if *expert == "civil-defense" {
            assert!(
                text.contains("TRHS") || text.contains("Shelter"),
                "{expert} {text}"
            );
        }
        if *expert == "facade" {
            assert!(
                text.contains("Envelope") || text.contains("Fire Code"),
                "{expert} {text}"
            );
        }
        if *expert == "landscape" {
            assert!(text.contains("NParks"), "{expert} {text}");
        }
        if *expert == "hydraulic" {
            assert!(text.contains("PUB"), "{expert} {text}");
        }
        if *expert == "port" {
            assert!(text.contains("MPA") || text.contains("PUB"), "{expert} {text}");
        }
        if *expert == "tunnel" {
            assert!(
                text.contains("LTA") || text.contains("WSH") || text.contains("CPFPRT"),
                "{expert} {text}"
            );
        }
        if *expert == "municipal" || *expert == "traffic" {
            assert!(text.contains("LTA"), "{expert} {text}");
        }
        if *expert == "intel-weak" {
            assert!(
                text.contains("PDPA") || text.contains("COPIF") || text.contains("弱电"),
                "{expert} {text}"
            );
        }
        if *expert == "interior" {
            assert!(
                text.contains("Accessibility") || text.contains("CONQUAS"),
                "{expert} {text}"
            );
        }
        if *expert == "design-coord" {
            assert!(
                text.contains("CORENET") || text.contains("APPBCA"),
                "{expert} {text}"
            );
        }
        if *expert == "it-app" {
            assert!(
                text.contains("PDPA") || text.contains("CII") || text.contains("CSA"),
                "{expert} {text}"
            );
        }
        if *expert == "plan-resource" {
            assert!(
                text.contains("C-Score") || text.contains("Buildability"),
                "{expert} {text}"
            );
        }
        if *expert == "hvac" {
            assert!(text.contains("SS 553") || text.contains("Fire Code"), "{expert} {text}");
        }
        if *expert == "electrical" {
            assert!(text.contains("SS 638") || text.contains("Fire Code"), "{expert} {text}");
        }
        if *expert == "steel" {
            assert!(text.contains("SS EN 1993") || text.contains("SS EN"), "{expert} {text}");
        }
        if *expert == "plumbing" {
            assert!(text.contains("PUB") || text.contains("SS 636"), "{expert} {text}");
        }
        if text.contains("辖区：SG") {
            assert!(
                text.contains("UNSPECIFIED") || text.contains("[A001]"),
                "{expert} SG draft missing pending marker"
            );
            assert!(!text.contains("JGJ"), "{expert} SG draft leaked JGJ");
            assert!(!text.contains("37 号令"), "{expert} SG draft leaked 37");
        }
        let mut sib = ToolCtx::new(
            p.clone(),
            sibling,
            cat,
            "low",
            true,
            &format!("iter18-{expert}-sib"),
        );
        assert!(
            packs::execute(&mut sib, tool, args).contains("拒绝"),
            "{sibling} should be refused {tool}"
        );
    }
}

#[test]
fn test_design_scan_forbidden_shared_and_sg_mix() {
    assert!(packs::visible_tool_names("fire-protect")
        .iter()
        .any(|n| n == "design__scan_forbidden"));
    assert!(packs::visible_tool_names("architecture")
        .iter()
        .any(|n| n == "design__scan_forbidden"));
    assert!(!packs::visible_tool_names("construction")
        .iter()
        .any(|n| n == "design__scan_forbidden"));
    let p = paths();
    let mut ctx = ToolCtx::new(p, "fire-protect", "design", "high", true, "iter-design-scan");
    std::fs::write(ctx.out_dir.join("mix.md"), "辖区：SG\n引用 JGJ 80 和 37 号令\n").unwrap();
    let scan = packs::execute(&mut ctx, "design__scan_forbidden", &json!({"filename": "mix.md"}));
    assert!(scan.contains("扫描未通过"), "{scan}");
    assert!(scan.contains("JGJ") || scan.contains("37"), "{scan}");
    for (expert, tool) in [
        ("bim-coord", "bim__scan_forbidden"),
        ("plan-master", "planning__scan_forbidden"),
        ("safety-brief", "hse__scan_forbidden"),
        ("cost", "commercial__scan_forbidden"),
        ("bid-parse", "bid__scan_forbidden"),
        ("proc-plan", "procurement__scan_forbidden"),
        ("equip", "plant__scan_forbidden"),
        ("lab-mix", "lab__scan_forbidden"),
        ("finance-tax", "finance__scan_forbidden"),
        ("supervision", "docs__scan_forbidden"),
        ("hr-recruit", "hr__scan_forbidden"),
        ("admin-doc", "admin__scan_forbidden"),
        ("it-ops", "it__scan_forbidden"),
        ("worker-brief", "people__scan_forbidden"),
    ] {
        assert!(
            packs::visible_tool_names(expert).iter().any(|n| n == tool),
            "{expert} missing {tool}"
        );
        assert!(
            !packs::visible_tool_names("construction").iter().any(|n| n == tool),
            "construction must not see {tool}"
        );
    }
}

#[test]
fn test_every_exclusive_execute_sg_and_high_risk_gate() {
    let p = paths();
    let mut map = serde_json::Map::new();
    for (k, v) in [
        ("scope", "Tuas site"),
        ("systems", "hydrant"),
        ("system", "site app"),
        ("open_items", "pending"),
        ("known_facts", "none"),
        ("tender_text", "Technical score 10"),
        ("required_items", "method statement"),
        ("scoring_points", "method"),
        ("project_name", "Tuas warehouse"),
        ("work_scope", "edge protection"),
        ("jurisdiction", "SG"),
        ("work_type", "excavation"),
        ("work_item", "set-out"),
        ("progress", "prep"),
        ("items", "rebar"),
        ("event_facts", "extra rail"),
        ("event", "delay"),
        ("package", "formwork"),
        ("period", "2026-08"),
        ("material", "concrete"),
        ("materials", "concrete"),
        ("inspection_lot", "lot-1"),
        ("scenario", "fire"),
        ("site", "Tuas"),
        ("discipline", "architecture"),
        ("issues", "clash at riser"),
        ("filters", "walls"),
        ("level", "master"),
        ("window", "W32"),
        ("trades", "formwork"),
        ("item", "rebar"),
        ("equipment", "tower crane"),
        ("notice", "NCR-1"),
        ("role", "site PE"),
        ("contract_type", "employment"),
        ("audience", "new workers"),
        ("topics", "CSOC"),
        ("doc_type", "memo"),
        ("subject", "PTW"),
        ("work_today", "edge"),
        ("vendor", "local"),
        ("samples", "cube-1"),
        ("rooms", "lobby"),
        ("span_note", "pending survey"),
        ("method", "cut and cover"),
        ("corridor", "Pioneer Rd"),
        ("stage", "Design"),
        ("lod", "UNSPECIFIED"),
        ("users", "gate"),
    ] {
        map.insert(k.to_string(), Value::String(v.to_string()));
    }
    let blob = Value::Object(map);
    for e in &seed().experts {
        let em = tier_map::expert_map(&e.id).unwrap();
        for tool in &em.exclusive {
            if tool.contains("fill_scheme")
                || matches!(
                    tool.as_str(),
                    "pack-ship__list" | "pack-ship__health" | "pack-ship__export"
                )
            {
                continue;
            }
            let mut ctx = ToolCtx::new(
                p.clone(),
                &e.id,
                &e.category,
                &e.risk,
                true,
                &format!("all-ex-{}", e.id),
            );
            let out = packs::execute(&mut ctx, tool, &blob);
            assert!(
                out.contains("已写入") || out.contains("fill_scheme"),
                "{} {} => {out}",
                e.id,
                tool
            );
        }
        if e.risk == "high" {
            let tool = em.exclusive.iter().find(|t| !t.contains("fill_scheme")).unwrap();
            let mut blocked = ToolCtx::new(
                p.clone(),
                &e.id,
                &e.category,
                "high",
                false,
                &format!("gate-{}", e.id),
            );
            let gate = packs::execute(&mut blocked, tool, &blob);
            assert!(
                gate.contains("拒绝写盘"),
                "{} {} should gate: {gate}",
                e.id,
                tool
            );
        }
    }
}

#[test]
fn test_cn_writers_omit_sg_only_titles() {
    let p = paths();
    let cases: &[(&str, &str, &str, &str, serde_json::Value, &[&str])] = &[
        (
            "structure",
            "design",
            "structure__calc_outline",
            "结构计算书提纲.md",
            json!({"system": "RC", "jurisdiction": "CN"}),
            &["Accredited Checker", "Structural Plan"],
        ),
        (
            "survey",
            "construction",
            "survey__record",
            "测量记录口径.md",
            json!({"work_item": "set-out", "jurisdiction": "CN"}),
            &["SVY21", "SHD"],
        ),
        (
            "safety-brief",
            "hse",
            "safety-brief__talk",
            "安全交底草稿.md",
            json!({"work_item": "edge", "jurisdiction": "CN"}),
            &["toolbox"],
        ),
        (
            "emergency",
            "hse",
            "emergency__plan",
            "应急预案提纲.md",
            json!({"scenario": "fire", "jurisdiction": "CN"}),
            &["SCDF"],
        ),
        (
            "env",
            "hse",
            "env__list",
            "环保文明清单.md",
            json!({"site": "site-a", "jurisdiction": "CN"}),
            &["NEA", "Earth Control"],
        ),
        (
            "subcontract",
            "commercial",
            "subcontract__sheet",
            "分包结算表头.md",
            json!({"package": "formwork", "jurisdiction": "CN"}),
            &["PSSCOC", "SOP Act"],
        ),
        (
            "worker-brief",
            "people",
            "worker-brief__talk",
            "班前白话稿.md",
            json!({"work_today": "rebar", "jurisdiction": "CN"}),
            &["toolbox"],
        ),
        (
            "pm-daily",
            "people",
            "pm-daily__log",
            "项目日报草稿.md",
            json!({"progress": "edge", "jurisdiction": "CN"}),
            &["BCA", "site record"],
        ),
        (
            "supervision",
            "docs",
            "supervision__reply",
            "监理回复草稿.md",
            json!({"notice": "n1", "jurisdiction": "CN"}),
            &["C-forms"],
        ),
        (
            "lab-record",
            "lab",
            "lab-record__ledger",
            "试验台账骨架.md",
            json!({"samples": "cube", "jurisdiction": "CN"}),
            &["SAC"],
        ),
        (
            "bid-tech",
            "bid",
            "bid-tech__expand",
            "技术标目录草稿.md",
            json!({"scoring_points": "method", "jurisdiction": "CN"}),
            &["GeBIZ"],
        ),
        (
            "dispatch",
            "construction",
            "dispatch__daily",
            "调度日报草稿.md",
            json!({"progress": "rebar", "jurisdiction": "CN"}),
            &["construction site records"],
        ),
        (
            "material-site",
            "plant",
            "material-site__recon",
            "材料耗用核算表头.md",
            json!({"items": "rebar", "jurisdiction": "CN"}),
            &["Factory Notification"],
        ),
        (
            "proc-plan",
            "procurement",
            "proc-plan__schedule",
            "采购计划表.md",
            json!({"items": "rebar", "jurisdiction": "CN"}),
            &["CRS", "GeBIZ"],
        ),
        (
            "proc-compare",
            "procurement",
            "proc-compare__table",
            "比价表草稿.md",
            json!({"item": "rebar", "vendors": "A", "jurisdiction": "CN"}),
            &["GeBIZ", "PQM"],
        ),
        (
            "proc-vendor",
            "procurement",
            "proc-vendor__eval",
            "供方评价表头.md",
            json!({"vendor": "local fab", "jurisdiction": "CN"}),
            &["GeBIZ", "GTP"],
        ),
        (
            "interior",
            "design",
            "interior__schedule",
            "室内界面表.md",
            json!({"rooms": "lobby", "jurisdiction": "CN"}),
            &["CONQUAS", "Accessibility"],
        ),
        (
            "design-coord",
            "design",
            "design-coord__minutes",
            "设计会审纪要.md",
            json!({"issues": "riser", "jurisdiction": "CN"}),
            &["CORENET", "APPBCA"],
        ),
        (
            "bim-deliver",
            "bim",
            "bim-deliver__lod",
            "BIM交付清单.md",
            json!({"stage": "DG", "jurisdiction": "CN"}),
            &["CORENET", "APPBCA", "IFC+SG"],
        ),
        (
            "hr-labor",
            "hr",
            "hr-labor__check",
            "劳动合同检查表.md",
            json!({"contract_type": "劳务", "jurisdiction": "CN"}),
            &["Employment Act", "Fair Consideration"],
        ),
        (
            "finance-book",
            "finance",
            "finance-book__check",
            "核算检查表.md",
            json!({"period": "2026-08", "jurisdiction": "CN"}),
            &["GST", "IRAS"],
        ),
        (
            "finance-fund",
            "finance",
            "finance-fund__plan",
            "资金计划草稿.md",
            json!({"period": "2026-08", "jurisdiction": "CN"}),
            &["CPF", "GST"],
        ),
        (
            "civil-defense",
            "design",
            "civil-defense__brief",
            "人防专篇提纲.md",
            json!({"scope": "basement", "jurisdiction": "CN"}),
            &["TRHS", "THSS", "Household"],
        ),
        (
            "hvac",
            "design",
            "hvac__memo",
            "暖通专业说明草稿.md",
            json!({"scope": "ACMV", "jurisdiction": "CN"}),
            &["SS 553", "Fire Code"],
        ),
        (
            "landscape",
            "design",
            "landscape__memo",
            "景观专业说明草稿.md",
            json!({"scope": "streetscape", "jurisdiction": "CN"}),
            &["NParks"],
        ),
        (
            "municipal",
            "design",
            "municipal__memo",
            "市政道路原则.md",
            json!({"scope": "road", "jurisdiction": "CN"}),
            &["LTA"],
        ),
        (
            "tunnel",
            "design",
            "tunnel__outline",
            "隧道提纲.md",
            json!({"method": "NATM", "jurisdiction": "CN"}),
            &["CPFPRT", "LTA"],
        ),
        (
            "architecture",
            "design",
            "architecture__memo",
            "architecture专业说明草稿.md",
            json!({"discipline": "architecture", "scope": "tower", "jurisdiction": "CN"}),
            &["Reportable Matters"],
        ),
        (
            "fire-protect",
            "design",
            "fire-protect__brief",
            "消防专篇提纲.md",
            json!({"scope": "hydrant", "jurisdiction": "CN"}),
            &["Fire Code", "Fire Safety"],
        ),
        (
            "variation",
            "commercial",
            "variation__form",
            "签证单草稿.md",
            json!({"event": "extra rebar", "jurisdiction": "CN"}),
            &["PSSCOC", "REDAS"],
        ),
        (
            "claim",
            "commercial",
            "claim__notice",
            "索赔意向草稿.md",
            json!({"event": "delay", "jurisdiction": "CN"}),
            &["Security of Payment", "PSSCOC"],
        ),
        (
            "geotech",
            "design",
            "geotech__brief",
            "岩土勘察提纲.md",
            json!({"scope": "pad", "jurisdiction": "CN"}),
            &["GeoSS", "AGS"],
        ),
        (
            "equip",
            "plant",
            "equip__ledger",
            "设备台账.md",
            json!({"equipment": "crane", "jurisdiction": "CN"}),
            &["MOM lifting", "approved crane"],
        ),
    ];
    for (expert, cat, tool, file, args, forbidden) in cases {
        let risk = if matches!(
            *expert,
            "structure" | "survey" | "safety-brief" | "supervision" | "lab-record"
        ) {
            "high"
        } else {
            "low"
        };
        let mut ctx = ToolCtx::new(
            p.clone(),
            expert,
            cat,
            risk,
            true,
            &format!("iter66-cn-{expert}"),
        );
        let out = packs::execute(&mut ctx, tool, args);
        assert!(out.contains("已写入"), "{expert} {tool} {out}");
        let text = std::fs::read_to_string(ctx.out_dir.join(file)).unwrap_or_default();
        assert!(text.contains("辖区：CN"), "{expert} {text}");
        for token in *forbidden {
            assert!(
                !text.contains(token),
                "{expert} CN draft leaked {token}: {text}"
            );
        }
        if *expert == "hr-labor" {
            assert!(
                text.contains("劳动合同法") || text.contains("劳动法"),
                "{expert} {text}"
            );
        }
        if *expert == "finance-book" {
            assert!(text.contains("增值税法") || text.contains("会计法"), "{expert} {text}");
        }
        if *expert == "civil-defense" {
            assert!(text.contains("人民防空"), "{expert} {text}");
        }
        if *expert == "hvac" {
            assert!(text.contains("供暖通风") || text.contains("空气调节"), "{expert} {text}");
        }
        if *expert == "landscape" {
            assert!(text.contains("城市绿地"), "{expert} {text}");
        }
        if *expert == "municipal" {
            assert!(text.contains("城市道路"), "{expert} {text}");
        }
        if *expert == "tunnel" {
            assert!(text.contains("公路隧道"), "{expert} {text}");
        }
        if *expert == "interior" {
            assert!(text.contains("装修") || text.contains("防火规范"), "{expert} {text}");
        }
        if *expert == "design-coord" {
            assert!(text.contains("施工图"), "{expert} {text}");
        }
        if *expert == "bim-deliver" {
            assert!(text.contains("建筑信息模型") || text.contains("交付标准"), "{expert} {text}");
        }
        if *expert == "fire-protect" {
            assert!(text.contains("建筑设计防火规范"), "{expert} {text}");
        }
        if *expert == "safety-brief" {
            assert!(text.contains("安全技术交底"), "{expert} {text}");
        }
        if *expert == "emergency" {
            assert!(text.contains("应急预案"), "{expert} {text}");
        }
        if *expert == "supervision" {
            assert!(text.contains("监理规范"), "{expert} {text}");
        }
        if *expert == "lab-record" {
            assert!(text.contains("质量检测"), "{expert} {text}");
        }
        if *expert == "worker-brief" {
            assert!(text.contains("班前会"), "{expert} {text}");
        }
        if *expert == "dispatch" {
            assert!(text.contains("危大"), "{expert} {text}");
        }
        if *expert == "geotech" {
            assert!(text.contains("岩土勘察"), "{expert} {text}");
        }
        if *expert == "structure" {
            assert!(text.contains("混凝土结构"), "{expert} {text}");
        }
        if *expert == "survey" {
            assert!(text.contains("工程测量"), "{expert} {text}");
        }
        if *expert == "equip" {
            assert!(text.contains("特种设备"), "{expert} {text}");
        }
    }
}

#[test]
fn test_understand_chat_or_run() {
    use civil_workbench::agent::{is_packish, understand, Intent};
    assert_eq!(understand("什么是 GST"), Intent::Chat);
    assert_eq!(understand("IRAS 发票是什么意思"), Intent::Chat);
    assert_eq!(understand("临边防护算不算危大？要不要专家论证？"), Intent::Chat);
    assert_eq!(understand("新加坡现在 GST 税率多少？"), Intent::Chat);
    assert_eq!(understand("写临边防护方案讨论提纲"), Intent::Run);
    assert_eq!(
        understand("按虚构滨河路人行道维修，写临边与洞口防护专项方案讨论提纲。"),
        Intent::Run
    );
    assert_eq!(understand("一人公司成套投标"), Intent::Run);
    assert_eq!(understand("先解释 GST 再出一份税务日历"), Intent::Both);
    assert_eq!(understand("评标是什么"), Intent::Chat);
    assert_eq!(understand("ITT 是什么意思"), Intent::Chat);
    assert_eq!(understand("workhead 算不算资质？"), Intent::Chat);
    assert_eq!(understand("招标文件评标办法怎么理解"), Intent::Chat);
    assert_eq!(
        understand("Quality 33%\nPrice 67%\nBCA workhead CW01\nTwo Envelope"),
        Intent::Run
    );
    assert!(is_packish("一人公司成套投标"));
}

#[test]
fn test_every_seed_expert_is_harness_steps() {
    let p = paths();
    let brief = "Tuas warehouse drainage 作业提纲 scope: inspection lot-1";
    let mut fails = Vec::new();
    for exp in &seed().experts {
        let ticket = civil_workbench::harness::Ticket {
            session: format!("hex-{}", exp.id),
            project: format!("H-{}", exp.id),
            jurisdiction: "SG".into(),
            brief: brief.into(),
            path: String::new(),
            confirm_ok: true,
        };
        let run = civil_workbench::harness::run_expert_steps(&p, exp, ticket.clone());
        if run.error.is_some() {
            fails.push(format!("{} error={:?}", exp.id, run.error));
            continue;
        }
        if run.mode != "steps" {
            fails.push(format!("{} mode={}", exp.id, run.mode));
        }
        if run.illegal_count() != 0 {
            fails.push(format!("{} illegal={}", exp.id, run.illegal_count()));
        }
        if run.files.is_empty() && !run.hitl.pending {
            fails.push(format!("{} no write", exp.id));
        }
        if !run.run_dir.join("trace.json").is_file() {
            fails.push(format!("{} missing trace", exp.id));
        }
        let ev = civil_workbench::harness::shadow_eval_expert(&p, exp, ticket);
        if ev["ok"] != true || ev["illegal_tool_calls"] != 0 {
            fails.push(format!("{} shadow {ev}", exp.id));
        }
    }
    assert!(fails.is_empty(), "{fails:?}");
}

#[test]
fn test_high_risk_expert_harness_hitl_no_write() {
    let p = paths();
    let exp = seed()
        .experts
        .iter()
        .find(|e| e.id == "construction")
        .cloned()
        .unwrap();
    assert_eq!(exp.risk, "high");
    let ticket = civil_workbench::harness::Ticket {
        session: "hex-hitl-construction".into(),
        project: "滨河路".into(),
        jurisdiction: "CN".into(),
        brief: "临边与洞口防护专项方案讨论提纲".into(),
        path: String::new(),
        confirm_ok: false,
    };
    let run = civil_workbench::harness::run_expert_steps(&p, &exp, ticket);
    assert_eq!(run.illegal_count(), 0);
    assert!(run.hitl.required);
    assert!(run.hitl.pending);
    assert!(run.files.is_empty(), "{:?}", run.files);
    assert!(run.steps.iter().any(|s| s.name == "hitl_gate"));
}

#[tokio::test]
async fn test_harness_expert_api_and_shadow() {
    let (gst_st, gst_body) = send(
        state(),
        Request::builder()
            .method("POST")
            .uri("/api/harness/expert")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "session_id": "hex-api-gst",
                    "expert_id": "finance-tax",
                    "brief": "什么是 GST",
                    "confirm_ok": false
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(gst_st, StatusCode::OK, "{gst_body}");
    let gst: Value = serde_json::from_str(&gst_body).unwrap();
    assert_eq!(gst["intent"], "chat", "{gst}");
    assert_eq!(gst["wrote"], false, "{gst}");
    assert_eq!(gst["submit_blocked"], true);
    assert!(gst["reply"].as_str().unwrap_or("").contains("9%"), "{gst}");
    assert!(gst["files"].as_array().map(|a| a.is_empty()).unwrap_or(false), "{gst}");

    let (st, body) = send(
        state(),
        Request::builder()
            .method("POST")
            .uri("/api/harness/expert")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "session_id": "hex-api-bid-parse",
                    "expert_id": "bid-parse",
                    "project_name": "T5",
                    "jurisdiction": "SG",
                    "brief": "Quality 33%\nPrice 67%\nBCA workhead CW01\nTwo Envelope",
                    "confirm_ok": true
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["harness"], true);
    assert_eq!(v["mode"], "steps");
    assert_eq!(v["illegal_tool_calls"], 0);
    assert!(v["files"].as_array().map(|a| !a.is_empty()).unwrap_or(false), "{v}");

    let (st, body) = send(
        state(),
        Request::builder()
            .method("POST")
            .uri("/api/eval/shadow-expert")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "session_id": "hex-api-shadow-cost",
                    "expert_id": "cost",
                    "project_name": "T5",
                    "brief": "drainage m 120",
                    "confirm_ok": true
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ev: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ev["ok"], true, "{ev}");
    assert_eq!(ev["illegal_tool_calls"], 0);
    assert_eq!(ev["expert"], "cost");
}

const SCHEME_CHAPTERS: &[&str] = &[
    "封面与文件控制",
    "草稿与责任声明",
    "工程概况",
    "编制依据",
    "施工部署与工艺",
    "质量",
    "安全与应急",
    "环保与文明施工",
    "资源计划",
    "验收与资料",
    "附录",
];

#[test]
fn test_construction_eleven_chapters() {
    let p = paths();
    let blocked = {
        let mut ctx = ToolCtx::new(
            p.clone(),
            "construction",
            "construction",
            "high",
            false,
            "wb-scheme-hitl",
        );
        let out = packs::execute(
            &mut ctx,
            "construction__scheme_draft",
            &json!({
                "project_name": "滨河路人行道维修",
                "work_scope": "临边防护",
                "jurisdiction": "SG"
            }),
        );
        assert!(out.contains("拒绝写盘"), "{out}");
        assert!(!ctx.out_dir.join("专项方案-AI草稿.md").is_file());
        out
    };
    assert!(blocked.contains("我明白，将由持证人员签认"));

    let mut ctx = ToolCtx::new(
        p,
        "construction",
        "construction",
        "high",
        true,
        "wb-scheme-11",
    );
    let out = packs::execute(
        &mut ctx,
        "construction__scheme_draft",
        &json!({
            "project_name": "滨河路人行道维修",
            "work_scope": "临边防护",
            "site_name": "滨河路人行道",
            "height_m": 3.2,
            "jurisdiction": "SG"
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let path = ctx.out_dir.join("专项方案-AI草稿.md");
    let text = std::fs::read_to_string(&path).expect("scheme draft");
    for (i, title) in SCHEME_CHAPTERS.iter().enumerate() {
        let heading = format!("## {} {title}", i + 1);
        assert!(text.contains(&heading), "missing {heading} in {text}");
    }
    assert!(!text.contains("可以开工"), "{text}");
    assert!(text.contains("[A001]") || text.contains("UNSPECIFIED"), "{text}");
    let fill = packs::execute(
        &mut ctx,
        "construction__fill_scheme_docx",
        &json!({
            "project_name": "滨河路人行道维修",
            "jurisdiction": "SG"
        }),
    );
    assert!(
        fill.contains("fill_scheme") || fill.contains("docx_pending") || fill.contains("已写入"),
        "{fill}"
    );
}

#[test]
fn test_bid_parse_from_attached_tender() {
    let p = paths();
    let sid = format!(
        "wb-bid-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let needle = "Quality 41% — Jurong tank farm method statement for working at height";
    let body = format!(
        "INVITATION TO TENDER — Jurong Island tank farm\n\
Evaluation criteria (PQM):\n\
- {needle}\n\
- Price 59%\n\
Required: 临边防护专项方案\n\
Time for Completion: 150 days\n"
    );
    civil_workbench::attach::save_upload(&p, &sid, "jurong-itt.txt", body.as_bytes()).unwrap();
    let mut ctx = ToolCtx::new(p.clone(), "bid-parse", "bid", "low", true, &sid);
    let out = packs::execute(
        &mut ctx,
        "bid-parse__extract",
        &json!({"project_name": "Jurong Island tank farm", "jurisdiction": "SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("招标解析表.md")).unwrap();
    assert!(text.contains(needle), "{text}");
    assert!(text.contains("临边防护专项方案"), "{text}");
    assert!(!text.contains("可以投标"), "{text}");
}

#[test]
fn test_pack_ship_disconnected_unspecified() {
    let p = paths();
    let mut ctx = ToolCtx::new(p, "pack-ship", "plant", "low", true, "wb-pack-off");
    let out = packs::execute(
        &mut ctx,
        "pack-ship__plan",
        &json!({
            "project_name": "Tuas steel batch",
            "materials": "H-beam 12m x 20 pcs, no unit weight given",
            "jurisdiction": "SG",
            "connected": false
        }),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("装箱作业单.md")).unwrap();
    assert!(text.contains("H-beam 12m x 20 pcs"), "{text}");
    for (k, line) in [
        ("utilization", "utilization=UNSPECIFIED"),
        ("can_fit", "can_fit=UNSPECIFIED"),
        ("mid50", "mid50=UNSPECIFIED"),
        ("系固待办", "系固待办=UNSPECIFIED"),
    ] {
        assert!(text.contains(line), "missing {k} in {text}");
    }
    assert!(!text.contains("(0.0, 0.0, 0.0)"), "{text}");
}

#[test]
fn test_civil_spreadsheet_ingest_no_invented_price() {
    let p = paths();
    let sid = format!(
        "wb-xlsx-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let needle = "C30临边梁 C-LN-01";
    let xlsx = civil_workbench::attach::pack_minimal_xlsx(&[needle, "12m"]).expect("xlsx");
    civil_workbench::attach::save_upload(&p, &sid, "广联达导出.xlsx", &xlsx).unwrap();
    let mut ctx = ToolCtx::new(p, "cost", "commercial", "low", true, &sid);
    let out = packs::execute(
        &mut ctx,
        "cost__takeoff",
        &json!({"project_name": "滨河路", "jurisdiction": "SG"}),
    );
    assert!(out.contains("已写入"), "{out}");
    let text = std::fs::read_to_string(ctx.out_dir.join("工程量拆分表.md")).unwrap();
    assert!(text.contains(needle), "{text}");
    assert!(text.contains("UNSPECIFIED"), "{text}");
    assert!(!text.contains("350"), "{text}");
    assert!(!text.contains("综合单价 | 120"), "{text}");
    let price_cells: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(needle))
        .collect();
    assert!(!price_cells.is_empty(), "{text}");
    for line in price_cells {
        assert!(line.contains("UNSPECIFIED"), "{line}");
        assert!(!line.contains("S$"), "{line}");
    }
}

#[test]
fn test_harness_gst_question_no_write() {
    let p = paths();
    let exp = seed()
        .experts
        .iter()
        .find(|e| e.id == "finance-tax")
        .cloned()
        .unwrap();
    let ticket = civil_workbench::harness::Ticket {
        session: "wb-gst-chat".into(),
        project: "GST".into(),
        jurisdiction: "SG".into(),
        brief: "什么是 GST".into(),
        path: String::new(),
        confirm_ok: false,
    };
    let run = civil_workbench::harness::run_turn(&p, &exp, ticket);
    assert_eq!(run.intent, "chat");
    assert!(run.files.is_empty(), "{:?}", run.files);
    assert!(run.reply.contains("9%"), "{}", run.reply);
    let v = run.to_value();
    assert_eq!(v["wrote"], false);
    assert_eq!(v["submit_blocked"], true);
    assert!(!p
        .out_root
        .join("wb-gst-chat")
        .join("finance-tax")
        .join("税务检查表.md")
        .is_file());
}

#[tokio::test]
async fn test_harness_tender_question_no_write() {
    let p = paths();
    let exp = seed()
        .experts
        .iter()
        .find(|e| e.id == "bid-parse")
        .cloned()
        .unwrap();
    let questions = ["评标是什么", "ITT 是什么意思", "workhead 算不算资质？"];
    for (i, brief) in questions.iter().enumerate() {
        let session = format!("wb-q-bid-{i}");
        let draft = p
            .out_root
            .join(&session)
            .join("bid-parse")
            .join("招标解析表.md");
        let _ = std::fs::remove_file(&draft);
        let ticket = civil_workbench::harness::Ticket {
            session: session.clone(),
            project: "问".into(),
            jurisdiction: "SG".into(),
            brief: (*brief).into(),
            path: String::new(),
            confirm_ok: true,
        };
        let run = civil_workbench::harness::run_turn(&p, &exp, ticket);
        assert_eq!(run.intent, "chat", "{brief} {:?}", run.intent);
        assert!(run.files.is_empty(), "{brief} {:?}", run.files);
        assert_eq!(run.to_value()["wrote"], false, "{brief}");
        assert!(!draft.is_file(), "{brief} wrote {}", draft.display());
    }

    let (st, body) = send(
        state(),
        Request::builder()
            .method("POST")
            .uri("/api/harness/expert")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "session_id": "wb-http-q-bid",
                    "expert_id": "bid-parse",
                    "brief": "评标是什么",
                    "confirm_ok": true
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["intent"], "chat", "{v}");
    assert_eq!(v["wrote"], false, "{v}");
    assert!(
        v["files"].as_array().map(|a| a.is_empty()).unwrap_or(false),
        "{v}"
    );
    assert!(!p
        .out_root
        .join("wb-http-q-bid")
        .join("bid-parse")
        .join("招标解析表.md")
        .is_file());
}

// ---------- v0.7: threads / config / auth / downloads (parity with demo/tests/test_stream_resilience.py) ----------

fn fake_state(text: &str) -> AppState {
    AppState {
        paths: paths(),
        llm: LlmMode::FakePlain { text: text.into() },
        force_has_key: Some(true),
        auth_token: None,
    }
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn sse_events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|b| !b.trim().is_empty() && !b.trim_start().starts_with(':'))
        .filter_map(|block| {
            let mut name = "message".to_string();
            let mut data = None;
            for line in block.lines() {
                if let Some(n) = line.strip_prefix("event: ") {
                    name = n.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = serde_json::from_str::<Value>(d).ok();
                }
            }
            data.map(|d| (name, d))
        })
        .collect()
}

#[tokio::test]
async fn test_health_capabilities_declare_threads_and_config() {
    let (st, body) = send(state(), Request::builder().uri("/api/health").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let caps = &v["capabilities"];
    assert_eq!(caps["backend"], "rust");
    for k in ["upload", "threads", "thread_messages", "thread_files", "cancel", "config", "heartbeat", "file_events"] {
        assert_eq!(caps[k], true, "{k}");
    }
}

#[tokio::test]
async fn test_config_get_and_set_modes() {
    let (st, body) = send(state(), Request::builder().uri("/api/config").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["sandbox_modes"].as_array().unwrap().len() == 2);
    assert_eq!(v["confirm_sentence"], "我明白，将由持证人员签认");
    let (st, _) = send(state(), post_json("/api/config", json!({"sandbox": "nope"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, body) = send(state(), post_json("/api/config", json!({"approval": "never"}))).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["approval"], "never");
    std::env::set_var("CIVIL_APPROVAL", "on-request");
}

#[tokio::test]
async fn test_chat_creates_thread_and_transcript() {
    let (code, body) = send(
        fake_state("临边防护三米"),
        post_json("/api/chat", json!({"message": "什么是临边", "expert_ids": []})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let evs = sse_events(&body);
    let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first().copied(), Some("context"), "{names:?}");
    assert!(names.contains(&"token") && names.last().copied() == Some("done"), "{names:?}");
    let ctx = &evs[0].1;
    let tid = ctx["thread_id"].as_str().expect("thread_id in context").to_string();
    assert!(ctx["session_id"].as_str().is_some());

    let (st, body) = send(state(), Request::builder().uri(format!("/api/threads/{tid}/messages")).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["content"], "临边防护三米");
    assert_eq!(v["state"], "done");

    // second turn on the same thread keeps the transcript growing
    let (code, body) = send(
        fake_state("再说一次"),
        post_json("/api/chat", json!({"message": "再来", "expert_ids": [], "thread_id": tid})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(sse_events(&body)[0].1["thread_id"], tid);
    let (_, body) = send(state(), Request::builder().uri(format!("/api/threads/{tid}/messages")).body(Body::empty()).unwrap()).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 4);

    let (st, _) = send(state(), Request::builder().uri(format!("/api/threads/{tid}/files")).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(state(), Request::builder().method("DELETE").uri(format!("/api/threads/{tid}")).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(state(), Request::builder().uri(format!("/api/threads/{tid}")).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_threads_create_list_cancel() {
    let (st, body) = send(state(), post_json("/api/threads", json!({"title": "新对话"}))).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let tid = v["thread_id"].as_str().unwrap().to_string();
    let (st, body) = send(state(), Request::builder().uri("/api/threads").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["threads"].as_array().unwrap().iter().any(|t| t["thread_id"] == tid));
    let (st, body) = send(state(), post_json(&format!("/api/threads/{tid}/cancel"), json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["state"], "cancelled");
    let (st, _) = send(state(), post_json("/api/threads/nope/cancel", json!({}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let _ = send(state(), Request::builder().method("DELETE").uri(format!("/api/threads/{tid}")).body(Body::empty()).unwrap()).await;
}

#[tokio::test]
async fn test_background_thread_turn_records_reply() {
    let (st, body) = send(
        fake_state("后台答复"),
        post_json("/api/threads", json!({"text": "什么是 GST", "background": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let tid = v["thread_id"].as_str().unwrap().to_string();
    assert_eq!(v["state"], "running");
    let mut state_seen = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, body) = send(state(), Request::builder().uri(format!("/api/threads/{tid}")).body(Body::empty()).unwrap()).await;
        let v: Value = serde_json::from_str(&body).unwrap();
        state_seen = v["state"].as_str().unwrap_or("").to_string();
        if state_seen == "done" {
            assert_eq!(v["last_reply"], "后台答复");
            break;
        }
    }
    assert_eq!(state_seen, "done");
    let _ = send(state(), Request::builder().method("DELETE").uri(format!("/api/threads/{tid}")).body(Body::empty()).unwrap()).await;
}

#[tokio::test]
async fn test_download_headers_rfc5987_and_inline() {
    let p = paths();
    let dir = p.out_root.join("dltest-rs").join("construction");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("专项方案-AI草稿.md");
    std::fs::write(&f, "# 草稿\n").unwrap();
    let res = app(state())
        .oneshot(Request::builder().uri(format!("/api/file?path={}", urlenc(&f.to_string_lossy()))).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cd = res.headers().get("content-disposition").unwrap().to_str().unwrap().to_string();
    assert!(cd.starts_with("attachment"), "{cd}");
    assert!(cd.contains("filename*=UTF-8''%E4%B8%93"), "{cd}");
    let res = app(state())
        .oneshot(Request::builder().uri("/api/file?path=dltest-rs/construction/%E4%B8%93%E9%A1%B9%E6%96%B9%E6%A1%88-AI%E8%8D%89%E7%A8%BF.md&inline=1").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/plain"));
    assert!(res.headers().get("content-disposition").unwrap().to_str().unwrap().starts_with("inline"));
    let (st, _) = send(state(), Request::builder().uri("/api/file?path=../../README.md").body(Body::empty()).unwrap()).await;
    assert_ne!(st, StatusCode::OK);
    let _ = std::fs::remove_dir_all(p.out_root.join("dltest-rs"));
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn test_token_gate_when_civil_token_set() {
    let gated = || AppState { auth_token: Some("s3cret".into()), ..state() };
    let (st, _) = send(gated(), Request::builder().uri("/api/catalog").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = send(gated(), Request::builder().uri("/").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "page must load so it can ask for the token");
    let (st, body) = send(gated(), Request::builder().uri("/api/health").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "health is open (tells the page auth is on)");
    assert!(body.contains("\"auth\":true"), "{body}");
    let (st, _) = send(gated(), Request::builder().uri("/api/catalog").header("cookie", "cb_token=s3cret").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(gated(), Request::builder().uri("/api/catalog").header("authorization", "Bearer s3cret").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(gated(), Request::builder().uri("/api/catalog?token=wrong").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = send(state(), Request::builder().uri("/api/catalog").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "no token configured = open");
}
