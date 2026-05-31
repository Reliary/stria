#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod index;
pub mod lcep;
pub mod search;
pub mod setup;
pub mod structural_risk;
pub mod zone;

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::Path;

#[derive(Parser)]
#[command(name = "stria", about = "Grammar-free structural codebase search")]
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
    /// Watch repo for changes and rebuild index incrementally
    Watch {
        #[arg(long)]
        repo: String,
    },
    /// Detect agents and add stria MCP server entry
    Setup {
        #[arg(long, default_value = "false")]
        yes: bool,
    },
    /// Remove stria from agent configurations
    Remove {
        #[arg(long, default_value = "false")]
        yes: bool,
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
        Commands::Search {
            horizon,
            query,
            top_n,
        } => {
            let db_path = Path::new(&horizon).join("phrases.sqlite");
            let fallback = std::env::temp_dir().join("nonexistent.sqlite");
            let db_str = db_path.to_str().unwrap_or(fallback.to_str().unwrap_or(""));
            let results = search::search_phrases(db_str, &query, top_n);
            for (fp, score) in &results {
                println!("{:.4}  {}", score, fp);
            }
        }
        Commands::Serve { repo } => {
            let repo_path = Path::new(&repo);
            let canonical = repo_path
                .canonicalize()
                .unwrap_or_else(|_| repo_path.to_path_buf());
            let out_dir = canonical.join(".horizon");
            if !out_dir.join("phrases.sqlite").exists() {
                match index::build_phrase_index(
                    canonical.to_str().unwrap_or(&repo),
                    &out_dir,
                    false,
                ) {
                    Ok(n) => eprintln!("Index built: {} phrases", n),
                    Err(e) => {
                        eprintln!("Index build failed: {}", e);
                        return;
                    }
                }
            }
            eprintln!(
                "stria MCP server starting for repo: {}",
                canonical.display()
            );
            mcp_server(canonical.to_str().unwrap_or(&repo).to_string());
        }
        Commands::Watch { repo } => {
            let repo_path = Path::new(&repo);
            let canonical = repo_path
                .canonicalize()
                .unwrap_or_else(|_| repo_path.to_path_buf());
            let out_dir = canonical.join(".horizon");
            if !out_dir.join("phrases.sqlite").exists() {
                match index::build_phrase_index(
                    canonical.to_str().unwrap_or(&repo),
                    &out_dir,
                    false,
                ) {
                    Ok(n) => eprintln!("Index built: {} phrases", n),
                    Err(e) => {
                        eprintln!("Index build failed: {}", e);
                        return;
                    }
                }
            }
            eprintln!("Watching repo for changes: {}", canonical.display());
            if let Err(e) = index::watch_changes(canonical.to_str().unwrap_or(&repo), &out_dir) {
                eprintln!("Watch error: {}", e);
            }
        }
        Commands::Setup { yes } => {
            setup::run_setup(yes);
        }
        Commands::Remove { yes } => {
            setup::run_remove(yes);
        }
    }
}

fn mcp_server(initial_repo: String) {
    use serde_json::{json, Value};
    use std::io::{self, BufRead, Write};
    use std::sync::Mutex;

    let current_repo: Mutex<String> = Mutex::new(initial_repo);

    let db_path_of = |repo: &str| -> String {
        std::path::Path::new(repo)
            .join(".horizon")
            .join("phrases.sqlite")
            .to_str()
            .unwrap_or("")
            .to_string()
    };

    let tools = json!([
        {"name": "orient", "description": "One-time session orientation. Returns tool workflow map + language breakdown. Call this first. ~80t.", "inputSchema": {"type": "object", "properties": {}, "required": []}},
        {"name": "code_search", "description": "Single composite tool for all code-search needs. Three expansion tiers: default (compact paths, ~80t), expand_plan (+read_order/risk, ~150t), expand_full (+who_calls/hidden_deps, ~250t). Use when: editing code, finding test files, checking impact.", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}, "expand_plan": {"type": "boolean"}, "expand_full": {"type": "boolean"}}, "required": ["task"]}},
        {"name": "search", "description": "Find files by conceptual content (not filename). Use when: you know what the code does but not where it lives. Input: free-form query of 1-5 keywords. Returns: top 10 files with relevance scores.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "pre_edit", "description": "Full execution plan with read_order, edit target, verify candidates, fixtures, coupled files, risk level. Use when: you need detailed pre-edit guidance beyond what code_search default tier provides.", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}},
        {"name": "who_calls", "description": "Find all files that reference a specific identifier. Use when: refactoring a function and need to check callers. Input: exact identifier name (case-sensitive).", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "trace_callers", "description": "Find callers through N hops. depth=1 is same as who_calls, depth=2 finds callers of callers. Use when: need full impact analysis across module boundaries.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "depth": {"type": "integer", "default": 2}}, "required": ["name"]}},
        {"name": "hidden_deps", "description": "Find hidden cross-module dependencies that imports don't reveal. Use when: checking if a refactor reaches outside the current module. Input: file path relative to repo root.", "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]}},
        {"name": "expand_body", "description": "Expand a [HORIZON: hash] marker to full function body. Use when: orient output shows an horizon marker and you need to read the function source. Input: the hex hash from the marker.", "inputSchema": {"type": "object", "properties": {"hash": {"type": "string"}}, "required": ["hash"]}},
        {"name": "find_hash", "description": "Find horizon hashes by function name. Use when: you know a function name and need its hash for expand_body. Input: function name or partial name.", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "health", "description": "Server health check. Returns DB stats and latency. Use when: connection issues or monitoring.", "inputSchema": {"type": "object", "properties": {}, "required": []}},
        {"name": "switch_repo", "description": "Switch the active repo and rebuild index if needed. Use when: working on multiple projects in one agent session. Input: path to the repo root.", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
    ]);

    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let req_id = request.get("id");
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stria", "version": "0.3.0"}
                    }
                })
            }
            "notifications/initialized" => continue,
            "tools/list" => {
                json!({"jsonrpc": "2.0", "id": req_id, "result": {"tools": tools}})
            }
            "tools/call" => {
                let repo = current_repo.lock().unwrap().clone();
                let db_path = db_path_of(&repo);
                let name = request
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let req_start = std::time::Instant::now();
                const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                let result = match name {
                    "orient" => {
                        let n_files: i64 = if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0))
                                .unwrap_or(0)
                        } else {
                            0
                        };

                        let mut lang_counts: Vec<(String, i32)> = Vec::new();
                        if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            if let Ok(mut stmt) = db.prepare("SELECT file_path FROM file_map") {
                                let mut ext_counts: HashMap<String, i32> = HashMap::new();
                                if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                                    for row in rows.flatten() {
                                        if let Some(ext) = row.rsplit('.').next() {
                                            if ext.len() <= 6 {
                                                *ext_counts.entry(ext.to_string()).or_insert(0) +=
                                                    1;
                                            }
                                        }
                                    }
                                }
                                lang_counts = ext_counts.into_iter().collect();
                                lang_counts.sort_by_key(|b| std::cmp::Reverse(b.1));
                                lang_counts.truncate(8);
                            }
                        }
                        let languages: Vec<Value> = lang_counts
                            .into_iter()
                            .map(|(k, v)| json!({"ext": k, "count": v}))
                            .collect();

                        json!({
                            "schema_version": 1,
                            "n_files": n_files,
                            "languages": languages,
                            "workflows": {
                                "pre_edit": {"tools": ["code_search(task=...) -> default tier", "who_calls(name=...) -> if refactoring", "hidden_deps(file=...) -> if cross-module"], "description": "Use code_search first. who_calls/hidden_deps for targeted questions."},
                                "discovery": {"tools": ["search(query=...)"], "description": "Find files when you know the concept but not the location."},
                                "impact": {"tools": ["who_calls(name=...)", "hidden_deps(file=...)"], "description": "Check callers and hidden dependencies."}
                            },
                            "tool_guide": {
                                "code_search": {"use_when": "editing code and need target files + test candidates", "expand_plan": "adds read_order and blast radius", "expand_full": "adds who_calls chains and hidden deps"},
                                "search": {"use_when": "need to find files by concept (not filename)"},
                                "pre_edit": {"use_when": "need full plan beyond code_search default"},
                                "who_calls": {"use_when": "refactoring a specific function/type and need to check callers"},
                                "hidden_deps": {"use_when": "need hidden cross-module coupling"},
                                "expand_body": {"use_when": "a [HORIZON: hash] marker appears and you need the function body"},
                                "find_hash": {"use_when": "you know a function name and need its hash"}
                            }
                        })
                    }
                    "code_search" => {
                        let task = request
                            .pointer("/params/arguments/task")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let expand_plan = request
                            .pointer("/params/arguments/expand_plan")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let expand_full = request
                            .pointer("/params/arguments/expand_full")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        let search_results = search::search_phrases(&db_path, task, 15);
                        let plan = structural_risk::hologram_plan(&db_path, task);

                        // Default tier: compact paths + verify candidates + risk
                        let top_files: Vec<Value> = search_results
                            .iter()
                            .take(5)
                            .map(|(fp, _)| json!(fp))
                            .collect();

                        let verify: Vec<Value> = plan
                            .get("verify")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|fp| json!(fp))
                                    .collect()
                            })
                            .unwrap_or_default();

                        let edit_path: Option<Value> =
                            search_results.first().map(|(fp, _)| json!(fp));
                        let risk = plan
                            .get("risk")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        let mut response = json!({
                            "schema_version": 1,
                            "files": top_files,
                            "edit": edit_path,
                            "verify": verify,
                            "risk": risk,
                        });

                        // Tier 2: expand_plan — add read_order and blast deps
                        if expand_plan {
                            let read_order: Vec<Value> = plan
                                .get("read_first")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str())
                                        .map(|fp| json!(fp))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let coupled: Vec<Value> = plan
                                .get("coupled")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str())
                                        .map(|fp| json!(fp))
                                        .collect()
                                })
                                .unwrap_or_default();
                            if let Some(response_obj) = response.as_object_mut() {
                                response_obj.insert("read_order".to_string(), json!(read_order));
                                response_obj.insert("coupled".to_string(), json!(coupled));
                                let fixtures = plan
                                    .get("fixtures")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default();
                                response_obj.insert("fixtures".to_string(), json!(fixtures));
                            }
                        }

                        // Tier 3: expand_full — add caller chains and latent deps
                        if expand_full {
                            // Check deadline before expensive work
                            if req_start.elapsed() > TIMEOUT {
                                if let Some(response_obj) = response.as_object_mut() {
                                    response_obj.insert(
                                        "warning".to_string(),
                                        json!("request timed out before expand_full"),
                                    );
                                }
                            } else {
                                let task_phrases = crate::zone::extract_phrases(task);
                                let task_phrases: Vec<String> =
                                    task_phrases.into_iter().take(10).collect();
                                let mut all_callers: HashMap<String, f64> = HashMap::new();
                                for phrase in &task_phrases {
                                    if req_start.elapsed() > TIMEOUT {
                                        break;
                                    }
                                    for (fp, score) in structural_risk::who_calls(&db_path, phrase)
                                    {
                                        *all_callers.entry(fp).or_insert(0.0) += score;
                                    }
                                }
                                let mut caller_vec: Vec<(String, f64)> =
                                    all_callers.into_iter().collect();
                                caller_vec.sort_by(|a, b| {
                                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                                });
                                let caller_paths: Vec<Value> =
                                    caller_vec.iter().take(5).map(|(fp, _)| json!(fp)).collect();

                                if let Some(response_obj) = response.as_object_mut() {
                                    response_obj
                                        .insert("who_calls".to_string(), json!(caller_paths));
                                }

                                if req_start.elapsed() <= TIMEOUT {
                                    if let Some((top_fp, _)) = search_results.first() {
                                        let deps = structural_risk::latent_deps(&db_path, top_fp);
                                        let dep_paths: Vec<Value> =
                                            deps.iter().take(5).map(|(fp, _)| json!(fp)).collect();
                                        if let Some(response_obj) = response.as_object_mut() {
                                            response_obj.insert(
                                                "hidden_deps".to_string(),
                                                json!(dep_paths),
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        response
                    }
                    "expand_body" => {
                        let hash = request
                            .pointer("/params/arguments/hash")
                            .and_then(|h| h.as_str())
                            .unwrap_or("");
                        let mut body = get_horizon_body(&repo, hash);
                        if body.len() > 51200 {
                            body.truncate(51200);
                            body.push_str("\n// [body truncated at 50KB]");
                        }
                        json!({"body": body})
                    }
                    "find_hash" => {
                        let search_name = request
                            .pointer("/params/arguments/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        let horizon_db = std::path::Path::new(&repo)
                            .join(".horizon")
                            .join("horizon.db")
                            .to_str()
                            .unwrap_or("")
                            .to_string();
                        let results: Vec<(String, String)> =
                            if let Ok(c) = rusqlite::Connection::open(&horizon_db) {
                                if let Ok(mut stmt) = c.prepare(
                                    "SELECT hash, name FROM functions WHERE name LIKE ?1 LIMIT 20",
                                ) {
                                    if let Ok(rows) = stmt
                                        .query_map([&format!("%{}%", search_name)], |r| {
                                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                                        })
                                    {
                                        rows.filter_map(|r| r.ok()).collect()
                                    } else {
                                        vec![]
                                    }
                                } else {
                                    vec![]
                                }
                            } else {
                                vec![]
                            };
                        json!({"results": results})
                    }
                    "search" => {
                        let query = request
                            .pointer("/params/arguments/query")
                            .and_then(|q| q.as_str())
                            .unwrap_or("");
                        let results = search::search_phrases(&db_path, query, 10);
                        let files: Vec<Value> = results
                            .iter()
                            .map(|(fp, sc)| json!({"file": fp, "score": sc}))
                            .collect();
                        json!({"candidates": files})
                    }
                    "pre_edit" => {
                        let task = request
                            .pointer("/params/arguments/task")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");

                        structural_risk::hologram_plan(&db_path, task)
                    }
                    "who_calls" => {
                        let call_name = request
                            .pointer("/params/arguments/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        let results = structural_risk::who_calls(&db_path, call_name);
                        json!({"callers": results})
                    }
                    "trace_callers" => {
                        let call_name = request
                            .pointer("/params/arguments/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        let depth = request
                            .pointer("/params/arguments/depth")
                            .and_then(|d| d.as_i64())
                            .unwrap_or(2) as u32;
                        let results = structural_risk::trace_callers(&db_path, call_name, depth);
                        json!({"callers": results})
                    }
                    "hidden_deps" => {
                        let file = request
                            .pointer("/params/arguments/file")
                            .and_then(|f| f.as_str())
                            .unwrap_or("");
                        let results = structural_risk::latent_deps(&db_path, file);
                        json!({"deps": results})
                    }
                    "health" => {
                        let t0 = std::time::Instant::now();
                        let mut n_phrases = 0i64;
                        let mut n_files = 0i64;
                        let mut avgdl = 0.0f64;
                        let mut build_time_f = 0.0f64;
                        if let Ok(db) = rusqlite::Connection::open(&db_path) {
                            n_files = db
                                .query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0))
                                .unwrap_or(0);
                            n_phrases = db
                                .query_row("SELECT COUNT(*) FROM phrase_occ", [], |r| r.get(0))
                                .unwrap_or(0);
                            avgdl = db
                                .query_row(
                                    "SELECT COALESCE(value,0) FROM meta WHERE key='avgdl'",
                                    [],
                                    |r| r.get(0),
                                )
                                .unwrap_or(0.0);
                            build_time_f = db
                                .query_row(
                                    "SELECT COALESCE(value,0) FROM meta WHERE key='build_time'",
                                    [],
                                    |r| r.get(0),
                                )
                                .unwrap_or(0.0);
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
                    "switch_repo" => {
                        let new_path = request
                            .pointer("/params/arguments/path")
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        let canonical = match std::path::Path::new(new_path).canonicalize() {
                            Ok(p) => p.to_string_lossy().to_string(),
                            Err(_) => String::new(),
                        };
                        if canonical.is_empty() {
                            json!({"error": format!("Path not found: {}", new_path)})
                        } else {
                            let out_dir = std::path::Path::new(&canonical).join(".horizon");
                            if !out_dir.join("phrases.sqlite").exists() {
                                match index::build_phrase_index(&canonical, &out_dir, false) {
                                    Ok(n) => {
                                        *current_repo.lock().unwrap() = canonical;
                                        json!({"status": "ok", "phrases": n})
                                    }
                                    Err(e) => {
                                        json!({"error": format!("Index build failed: {}", e)})
                                    }
                                }
                            } else {
                                *current_repo.lock().unwrap() = canonical;
                                json!({"status": "ok"})
                            }
                        }
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
    let db_path = std::path::Path::new(repo_path)
        .join(".horizon")
        .join("horizon.db")
        .to_str()
        .unwrap_or("")
        .to_string();
    if let Ok(c) = rusqlite::Connection::open(&db_path) {
        if let Ok(body) = c.query_row("SELECT body FROM functions WHERE hash = ?1", [hash], |r| {
            r.get::<_, String>(0)
        }) {
            return body;
        }
    }
    String::new()
}

#[allow(dead_code)]
fn get_horizon_body_for_file(repo_path: &str, file_path: &str) -> String {
    let db_path = std::path::Path::new(repo_path)
        .join(".horizon")
        .join("horizon.db")
        .to_str()
        .unwrap_or("")
        .to_string();
    if let Ok(c) = rusqlite::Connection::open(&db_path) {
        // Try to find by matching name pattern from file path
        if let Ok(body) = c.query_row(
            "SELECT body FROM functions WHERE name LIKE ?1 LIMIT 1",
            [&format!("%{}%", file_path.replace('/', "_"))],
            |r| r.get::<_, String>(0),
        ) {
            return body;
        }
    }
    String::new()
}

#[allow(dead_code)]
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
