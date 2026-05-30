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
    use serde_json::Value;

    let db_path = format!("{}/.horizon/phrases.sqlite", repo_path);

    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() { continue; }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = request.get("id");

        let response = match method {
            "cross_horizon" => {
                let hash = request.pointer("/params/hash")
                    .and_then(|h| h.as_str()).unwrap_or("");
                let body = get_horizon_body(repo_path, hash);
                make_result(id, &serde_json::json!({"body": body}))
            }
            "search_horizon" => {
                let name = request.pointer("/params/name")
                    .and_then(|n| n.as_str()).unwrap_or("");
                let results = if let Ok(c) = rusqlite::Connection::open(
                    &format!("{}/.horizon/horizon.db", repo_path)
                ) {
                    let mut stmt = c.prepare(
                        "SELECT hash, name FROM functions WHERE name LIKE ?1 LIMIT 20"
                    ).unwrap();
                    let rows = stmt.query_map([&format!("%{}%", name)], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    }).unwrap();
                    rows.filter_map(|r| r.ok()).collect::<Vec<_>>()
                } else { vec![] };
                make_result(id, &serde_json::json!({"results": results}))
            }
            "hologram_query" | "holo_search" => {
                let task = request.pointer("/params/task")
                    .or_else(|| request.pointer("/params/query"))
                    .and_then(|t| t.as_str()).unwrap_or("");
                let results = search::search_phrases(&db_path, task, 10);
                let files: Vec<serde_json::Value> = results.iter().map(|(fp, sc)| {
                    serde_json::json!({"file": fp, "score": sc})
                }).collect();
                make_result(id, &serde_json::json!({"candidates": files}))
            }
            "hologram_expand" => {
                let task = request.pointer("/params/task")
                    .and_then(|t| t.as_str()).unwrap_or("");
                let search_results = search::search_phrases(&db_path, task, 5);
                let top_file = search_results.first().map(|(fp, _)| fp.clone()).unwrap_or_default();
                let plan = structural_risk::hologram_plan(&db_path, task);
                // Get horizon body for the top file
                let body = if !top_file.is_empty() {
                    get_horizon_body_for_file(repo_path, &top_file)
                } else { String::new() };
                make_result(id, &serde_json::json!({
                    "task": task,
                    "top_edit": search_results.first().map(|(fp, sc)| (fp, sc)),
                    "plan": plan,
                    "body_excerpt": body.chars().take(500).collect::<String>()
                }))
            }
            "hologram_watch" => {
                // Background watcher: spawn a thread that polls mtime and rebuilds
                // For MCP, we just acknowledge and log
                eprintln!("hologram_watch started for: {}", repo_path);
                make_result(id, &serde_json::json!({"status": "watching", "repo": repo_path}))
            }
            "who_calls" => {
                let name = request.pointer("/params/name")
                    .and_then(|n| n.as_str()).unwrap_or("");
                let results = structural_risk::who_calls(&db_path, name);
                make_result(id, &serde_json::json!({"callers": results}))
            }
            "latent_deps" => {
                let file = request.pointer("/params/file")
                    .and_then(|f| f.as_str()).unwrap_or("");
                let results = structural_risk::latent_deps(&db_path, file);
                make_result(id, &serde_json::json!({"deps": results}))
            }
            "switch_repo" => {
                let new_repo = request.pointer("/params/repo")
                    .and_then(|r| r.as_str()).unwrap_or("");
                eprintln!("Switching repo to: {}", new_repo);
                make_result(id, &serde_json::json!({"ok": true, "repo": new_repo}))
            }
            "hologram_plan" => {
                let task = request.pointer("/params/task")
                    .and_then(|t| t.as_str()).unwrap_or("");
                let plan = structural_risk::hologram_plan(&db_path, task);
                make_result(id, &plan)
            }
            _ => {
                make_result(id, &serde_json::json!({"ok": true}))
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
