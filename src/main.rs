#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod zone;
pub mod search;
pub mod lcep;
pub mod structural_risk;
pub mod index;

use std::path::Path;
use std::collections::HashMap;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "event-horizon", about = "Grammar-free structural codebase search")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build phrase index
    Build {
        #[arg(long)]
        repo: String,
    },
    /// Search phrase index
    Search {
        #[arg(long)]
        horizon: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value = "10")]
        top_n: usize,
    },
    /// Start MCP server
    Serve {
        #[arg(long)]
        repo: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { repo } => {
            let repo_path = Path::new(&repo);
            let out_dir = repo_path.join(".horizon");
            match index::build_phrase_index(&repo, &out_dir, true) {
                Ok(n) => println!("Built {} phrases", n),
                Err(e) => eprintln!("Build failed: {}", e),
            }
        }
        Commands::Search { horizon, query, top_n } => {
            let db_path = Path::new(&horizon).join("phrases.sqlite");
            let results = search::search_phrases(
                db_path.to_str().unwrap(),
                &query,
                top_n,
            );
            for (fp, score) in &results {
                println!("{:.4}  {}", score, fp);
            }
        }
        Commands::Serve { repo } => {
            let repo_path = Path::new(&repo);
            let out_dir = repo_path.join(".horizon");
            if !out_dir.join("phrases.sqlite").exists() {
                match index::build_phrase_index(&repo, &out_dir, false) {
                    Ok(n) => eprintln!("Index built: {} phrases", n),
                    Err(e) => {
                        eprintln!("Index build failed: {}", e);
                        return;
                    }
                }
            }
            eprintln!("Event Horizon MCP server starting for repo: {}", repo);
            mcp_server(&repo);
        }
    }
}

fn mcp_server(repo_path: &str) {
    use std::io::{self, BufRead, Write};
    use serde_json::{json, Value};

    let db_path = format!("{}/.horizon/phrases.sqlite", repo_path);
    let mut initialized = false;

    let tools = json!([
        {"name": "hologram_orient", "description": "One-time session orientation. Returns tool workflow map + language breakdown. Call this first. ~80t.", "inputSchema": {"type": "object", "properties": {}, "required": []}},
        {"name": "hologram_task", "description": "Single composite tool for all code-search needs. Three expansion tiers: default (compact paths, ~80t), expand_plan (+read_order/risk, ~150t), expand_full (+who_calls/latent_deps, ~250t). Use when: editing code, finding test files, checking impact.", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}, "expand_plan": {"type": "boolean"}, "expand_full": {"type": "boolean"}}, "required": ["task"]}},
        {"name": "holo_search", "description": "Find files by conceptual content (not filename). Use when: you know what the code does but not where it lives. Input: free-form query of 1-5 keywords. Returns: top 10 files with relevance scores.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "hologram_plan", "description": "Full execution plan with read_order, edit target, verify candidates, coupled files, risk level. Use when: you need detailed pre-edit guidance beyond what hologram_task default tier provides.", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}},
        {"name": "who_calls", "description": "Find all files that reference a specific identifier. Use when: refactoring a function and need to check callers. Input: exact identifier name (case-sensitive).", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "latent_deps", "description": "Find hidden cross-module dependencies that imports don't reveal. Use when: checking if a refactor reaches outside the current module. Input: file path relative to repo root.", "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]}},
        {"name": "cross_horizon", "description": "Expand a [HORIZON: hash] marker to full function body. Use when: orient output shows an horizon marker and you need to read the function source. Input: the hex hash from the marker.", "inputSchema": {"type": "object", "properties": {"hash": {"type": "string"}}, "required": ["hash"]}},
        {"name": "search_horizon", "description": "Find horizon hashes by function name. Use when: you know a function name and need its hash for cross_horizon. Input: function name or partial name.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "eh_health", "description": "Server health check. Returns DB stats and latency. Use when: connection issues or monitoring.", "inputSchema": {"type": "object", "properties": {}, "required": []}},
    ]);

    for line in io::stdin().lock().lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        if line.trim().is_empty() { continue; }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let req_id = request.get("id");
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                initialized = true;
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "event-horizon", "version": "0.3.0"}
                    }
                })
            }
            "notifications/initialized" => continue,
            "tools/list" => {
                json!({"jsonrpc": "2.0", "id": req_id, "result": {"tools": tools}})
            }
            "tools/call" => {
                let name = request.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                let result = match name {
                    "hologram_orient" => {
                        let n_files: i64 = if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0)).unwrap_or(0)
                        } else { 0 };
                        
                        let mut lang_counts: Vec<(String, i32)> = Vec::new();
                        if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            if let Ok(mut stmt) = db.prepare("SELECT file_path FROM file_map") {
                                let mut ext_counts: HashMap<String, i32> = HashMap::new();
                                if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                                    for row in rows.flatten() {
                                        if let Some(ext) = row.rsplit('.').next() {
                                            if ext.len() <= 6 {
                                                *ext_counts.entry(ext.to_string()).or_insert(0) += 1;
                                            }
                                        }
                                    }
                                }
                                lang_counts = ext_counts.into_iter().collect();
                                lang_counts.sort_by(|a, b| b.1.cmp(&a.1));
                                lang_counts.truncate(8);
                            }
                        }
                        let languages: Vec<Value> = lang_counts.into_iter()
                            .map(|(k, v)| json!({"ext": k, "count": v}))
                            .collect();

                        json!({
                            "schema_version": 1,
                            "n_files": n_files,
                            "languages": languages,
                            "workflows": {
                                "pre_edit": {"tools": ["hologram_task(task=...) -> default tier", "who_calls(name=...) -> if refactoring", "latent_deps(file=...) -> if cross-module"], "description": "Use hologram_task first. who_calls/latent_deps for targeted questions."},
                                "discovery": {"tools": ["holo_search(query=...)"], "description": "Find files when you know the concept but not the location."},
                                "impact": {"tools": ["who_calls(name=...)", "latent_deps(file=...)"], "description": "Check callers and hidden dependencies."}
                            },
                            "tool_guide": {
                                "hologram_task": {"use_when": "editing code and need target files + test candidates", "expand_plan": "adds read_order and blast radius", "expand_full": "adds who_calls chains and latent deps"},
                                "holo_search": {"use_when": "need to find files by concept (not filename)"},
                                "hologram_plan": {"use_when": "need full plan beyond hologram_task default"},
                                "who_calls": {"use_when": "refactoring a specific function/type and need to check callers"},
                                "latent_deps": {"use_when": "need hidden cross-module coupling"},
                                "cross_horizon": {"use_when": "a [HORIZON: hash] marker appears and you need the function body"},
                                "search_horizon": {"use_when": "you know a function name and need its hash"}
                            }
                        })
                    }
                    "hologram_task" => {
                        let task = request.pointer("/params/arguments/task")
                            .and_then(|t| t.as_str()).unwrap_or("");
                        let expand_plan = request.pointer("/params/arguments/expand_plan")
                            .and_then(|v| v.as_bool()).unwrap_or(false);
                        let expand_full = request.pointer("/params/arguments/expand_full")
                            .and_then(|v| v.as_bool()).unwrap_or(false);

                        let search_results = search::search_phrases(&db_path, task, 15);
                        let plan = structural_risk::hologram_plan(&db_path, task);

                        // Default tier: compact paths + verify candidates + risk
                        let top_files: Vec<Value> = search_results.iter().take(5).map(|(fp, _)| {
                            json!(fp)
                        }).collect();

                        let verify: Vec<Value> = plan.get("verify").and_then(|v| v.as_array()).map(|arr| {
                            arr.iter().filter_map(|v| v.as_str()).map(|fp| json!(fp)).collect()
                        }).unwrap_or_default();

                        let edit_path: Option<Value> = search_results.first().map(|(fp, _)| json!(fp));
                        let risk = plan.get("risk").and_then(|v| v.as_str()).unwrap_or("unknown");

                        let mut response = json!({
                            "schema_version": 1,
                            "files": top_files,
                            "edit": edit_path,
                            "verify": verify,
                            "risk": risk,
                        });

                        // Tier 2: expand_plan — add read_order and blast deps
                        if expand_plan {
                            let read_order: Vec<Value> = plan.get("read_first").and_then(|v| v.as_array()).map(|arr| {
                                arr.iter().filter_map(|v| v.as_str()).map(|fp| json!(fp)).collect()
                            }).unwrap_or_default();
                            let coupled: Vec<Value> = plan.get("coupled").and_then(|v| v.as_array()).map(|arr| {
                                arr.iter().filter_map(|v| v.as_str()).map(|fp| json!(fp)).collect()
                            }).unwrap_or_default();
                            let response_obj = response.as_object_mut().unwrap();
                            response_obj.insert("read_order".to_string(), json!(read_order));
                            response_obj.insert("coupled".to_string(), json!(coupled));
                        }

                        // Tier 3: expand_full — add caller chains and latent deps
                        if expand_full {
                            let task_phrases = crate::zone::extract_phrases(task);
                            let mut all_callers: HashMap<String, f64> = HashMap::new();
                            for phrase in &task_phrases {
                                for (fp, score) in structural_risk::who_calls(&db_path, phrase) {
                                    *all_callers.entry(fp).or_insert(0.0) += score;
                                }
                            }
                            let mut caller_vec: Vec<(String, f64)> = all_callers.into_iter().collect();
                            caller_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                            let caller_paths: Vec<Value> = caller_vec.iter().take(5).map(|(fp, _)| json!(fp)).collect();

                            let response_obj = response.as_object_mut().unwrap();
                            response_obj.insert("who_calls".to_string(), json!(caller_paths));

                            if let Some((top_fp, _)) = search_results.first() {
                                let deps = structural_risk::latent_deps(&db_path, top_fp);
                                let dep_paths: Vec<Value> = deps.iter().take(5).map(|(fp, _)| json!(fp)).collect();
                                response_obj.insert("latent_deps".to_string(), json!(dep_paths));
                            }
                        }

                        response
                    }
                    "cross_horizon" => {
                        let hash = request.pointer("/params/arguments/hash")
                            .and_then(|h| h.as_str()).unwrap_or("");
                        let body = get_horizon_body(repo_path, hash);
                        json!({"body": body})
                    }
                    "search_horizon" => {
                        let search_name = request.pointer("/params/arguments/name")
                            .and_then(|n| n.as_str()).unwrap_or("");
                        let results = if let Ok(c) = rusqlite::Connection::open(
                            &format!("{}/.horizon/horizon.db", repo_path)
                        ) {
                            let mut stmt = c.prepare(
                                "SELECT hash, name FROM functions WHERE name LIKE ?1 LIMIT 20"
                            ).unwrap();
                            let rows = stmt.query_map([&format!("%{}%", search_name)], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                            }).unwrap();
                            rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
                        } else { vec![] };
                        json!({"results": results})
                    }
                    "holo_search" => {
                        let query = request.pointer("/params/arguments/query")
                            .and_then(|q| q.as_str()).unwrap_or("");
                        let results = search::search_phrases(&db_path, query, 10);
                        let files: Vec<Value> = results.iter().map(|(fp, sc)| {
                            json!({"file": fp, "score": sc})
                        }).collect();
                        json!({"candidates": files})
                    }
                    "hologram_plan" => {
                        let task = request.pointer("/params/arguments/task")
                            .and_then(|t| t.as_str()).unwrap_or("");
                        let plan = structural_risk::hologram_plan(&db_path, task);
                        plan
                    }
                    "who_calls" => {
                        let call_name = request.pointer("/params/arguments/name")
                            .and_then(|n| n.as_str()).unwrap_or("");
                        let results = structural_risk::who_calls(&db_path, call_name);
                        json!({"callers": results})
                    }
                    "latent_deps" => {
                        let file = request.pointer("/params/arguments/file")
                            .and_then(|f| f.as_str()).unwrap_or("");
                        let results = structural_risk::latent_deps(&db_path, file);
                        json!({"deps": results})
                    }
                    "eh_health" => {
                        let t0 = std::time::Instant::now();
                        let mut n_phrases = 0i64;
                        let mut n_files = 0i64;
                        let mut avgdl = 0.0f64;
                        let mut build_time_f = 0.0f64;
                        if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            n_files = db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0)).unwrap_or(0);
                            n_phrases = db.query_row("SELECT COUNT(*) FROM phrase_occ", [], |r| r.get(0)).unwrap_or(0);
                            avgdl = db.query_row("SELECT COALESCE(value,0) FROM meta WHERE key='avgdl'", [], |r| r.get(0)).unwrap_or(0.0);
                            build_time_f = db.query_row(
                                "SELECT COALESCE(value,0) FROM meta WHERE key='build_time'", [], |r| r.get(0)
                            ).unwrap_or(0.0);
                        }
                        let ms = t0.elapsed().as_secs_f64() * 1000.0;
                        let build_date = if build_time_f > 0.0 {
                            let secs = build_time_f as u64;
                            let d = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
                            format!("{:?}", d)
                        } else {
                            "unknown".to_string()
                        };
                        json!({
                            "status": "ok",
                            "ok": true,
                            "phrases": n_phrases,
                            "files": n_files,
                            "avgdl": avgdl,
                            "build_date": format!("{:.0}", build_time_f),
                            "build_time_readable": build_date,
                            "ms": ms,
                        })
                    }
                    _ => {
                        json!({"error": format!("Unknown tool: {}", name)})
                    }
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "content": [{"type": "text", "text": serde_json::to_string(&result).unwrap_or_default()}]
                    }
                })
            }
            _ => {
                if req_id.is_some() {
                    json!({"jsonrpc": "2.0", "id": req_id, "result": {}})
                } else {
                    continue;
                }
            }
        };

        let resp_json = serde_json::to_string(&response).unwrap_or_default();
        println!("{}", resp_json);
        io::stdout().flush().ok();
    }
}

fn get_horizon_body(repo_path: &str, hash: &str) -> String {
    let db_path = format!("{}/.horizon/horizon.db", repo_path);
    if let Ok(c) = rusqlite::Connection::open(&db_path) {
        if let Ok(body) = c.query_row(
            "SELECT body FROM functions WHERE hash = ?1", [hash], |r| r.get::<_, String>(0)
        ) {
            return body;
        }
    }
    String::new()
}

fn get_horizon_body_for_file(repo_path: &str, file_path: &str) -> String {
    let db_path = format!("{}/.horizon/horizon.db", repo_path);
    if let Ok(c) = rusqlite::Connection::open(&db_path) {
        // Try to find by matching name pattern from file path
        if let Ok(body) = c.query_row(
            "SELECT body FROM functions WHERE name LIKE ?1 LIMIT 1",
            [&format!("%{}%", file_path.replace('/', "_"))],
            |r| r.get::<_, String>(0)
        ) {
            return body;
        }
    }
    String::new()
}

fn make_result(id: Option<&serde_json::Value>, data: &serde_json::Value) -> serde_json::Value {
    let mut resp = serde_json::json!({
        "jsonrpc": "2.0",
        "result": data
    });
    if let Some(id_val) = id {
        resp["id"] = id_val.clone();
    } else {
        resp["id"] = serde_json::Value::Null;
    }
    resp
}
