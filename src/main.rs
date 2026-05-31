#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod zone;
pub mod search;
pub mod lcep;
pub mod structural_risk;
pub mod index;

use std::path::Path;
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
        {"name": "holo_search", "description": "Search phrase index for code locations", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "cross_horizon", "description": "Expand [HORIZON: hash] to full function body", "inputSchema": {"type": "object", "properties": {"hash": {"type": "string"}}, "required": ["hash"]}},
        {"name": "search_horizon", "description": "Find horizon hashes by function name", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "hologram_plan", "description": "Structural execution plan with risk and verify candidates", "inputSchema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}},
        {"name": "who_calls", "description": "Find callers of an identifier", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        {"name": "latent_deps", "description": "Find hidden cross-module dependencies", "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]}},
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
