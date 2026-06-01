use serde_json::{json, Value};
use std::path::Path;

struct AgentConfig {
    name: &'static str,
    path: &'static str,
}

const AGENTS: &[AgentConfig] = &[
    AgentConfig {
        name: "OpenCode",
        path: "~/.config/opencode/opencode.json",
    },
    AgentConfig {
        name: "Claude Code",
        path: "~/.claude/settings.json",
    },
    AgentConfig {
        name: "Cursor",
        path: "~/.cursor/mcp.json",
    },
    AgentConfig {
        name: "Windsurf",
        path: "~/.codeium/windsurf/mcp_config.json",
    },
];

fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}/{}", home, rest);
        }
    }
    p.to_string()
}

fn stria_binary() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "stria".to_string())
}

fn has_stria(cfg: &Value, agent: &str) -> bool {
    match agent {
        "OpenCode" => cfg.get("stria").is_some(),
        _ => cfg.pointer("/mcpServers/stria").is_some(),
    }
}

fn add_stria(mut cfg: Value, agent: &str) -> Value {
    let binary = stria_binary();
    match agent {
        "OpenCode" => {
            let entry = cfg
                .get("stria")
                .cloned()
                .unwrap_or_else(|| json!({"type": "local", "command": [binary, "serve"]}));
            cfg.as_object_mut()
                .unwrap()
                .insert("stria".to_string(), entry);
        }
        _ => {
            let entry = if let Some(s) = cfg.pointer_mut("/mcpServers/stria") {
                s.clone()
            } else {
                json!({"command": binary, "args": ["serve"]})
            };
            if let Some(obj) = cfg.as_object_mut() {
                let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
                servers
                    .as_object_mut()
                    .unwrap()
                    .insert("stria".to_string(), entry);
            }
        }
    }
    cfg
}

fn remove_stria(mut cfg: Value, agent: &str) -> Value {
    match agent {
        "OpenCode" => {
            if let Some(obj) = cfg.as_object_mut() {
                obj.remove("stria");
            }
        }
        _ => {
            if let Some(obj) = cfg.as_object_mut() {
                if let Some(servers) = obj.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
                    servers.remove("stria");
                }
            }
        }
    }
    cfg
}

/// Detect agents that are installed but do NOT have stria configured.
pub fn detect_agents_without_stria() -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    for a in AGENTS {
        let expanded = expand_path(a.path);
        if Path::new(&expanded).exists() {
            if let Ok(content) = std::fs::read_to_string(&expanded) {
                if let Ok(cfg) = serde_json::from_str::<Value>(&content) {
                    if !has_stria(&cfg, a.name) {
                        found.push((a.name, expanded));
                    }
                }
            }
        }
    }
    found
}

/// Detect agents that have stria configured.
pub fn find_configured_agents() -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    for a in AGENTS {
        let expanded = expand_path(a.path);
        if Path::new(&expanded).exists() {
            if let Ok(content) = std::fs::read_to_string(&expanded) {
                if let Ok(cfg) = serde_json::from_str::<Value>(&content) {
                    if has_stria(&cfg, a.name) {
                        found.push((a.name, expanded));
                    }
                }
            }
        }
    }
    found
}

/// Add .stria/ to .gitignore for a repo.
/// Returns true if .gitignore was modified.
pub fn add_to_gitignore(repo_path: &str) -> bool {
    let gitignore_path = std::path::Path::new(repo_path).join(".gitignore");
    let stria_dir = ".stria";

    // Read existing or start fresh
    let mut content = if gitignore_path.exists() {
        match std::fs::read_to_string(&gitignore_path) {
            Ok(c) => c,
            Err(_) => return false,
        }
    } else {
        String::new()
    };

    // Check if .stria/ is already listed
    for line in content.lines() {
        if line.trim() == stria_dir {
            return false; // already present
        }
    }

    // Append
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(stria_dir);
    content.push('\n');

    match std::fs::write(&gitignore_path, content) {
        Ok(_) => {
            eprintln!("  Added {} to .gitignore", stria_dir);
            true
        }
        Err(e) => {
            eprintln!("  Warning: could not write .gitignore: {}", e);
            false
        }
    }
}

pub fn run_setup(yes: bool) {
    let found = detect_agents_without_stria();
    if found.is_empty() {
        // Check if already configured
        let configured = find_configured_agents();
        if !configured.is_empty() {
            let names: Vec<&str> = configured.iter().map(|(n, _)| *n).collect();
            eprintln!("stria already configured for: {}", names.join(", "));
        } else {
            eprintln!("No supported agents found. Install one of:");
            for a in AGENTS {
                eprintln!("  {}", a.name);
            }
        }
        return;
    }

    let mut added = 0;
    for (name, path) in &found {
        let proceed = yes || {
            eprint!("Add stria MCP server to {}? [Y/n]: ", name);
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or(0);
            !input.trim().eq_ignore_ascii_case("n")
        };
        if !proceed {
            continue;
        }

        let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
        let cfg: Value = serde_json::from_str(&content).unwrap_or(json!({}));
        let updated = add_stria(cfg, name);
        if let Ok(text) = serde_json::to_string_pretty(&updated) {
            if std::fs::write(path, text).is_ok() {
                added += 1;
                eprintln!("  Added to {}", name);
            }
        }
    }

    if added > 0 {
        eprintln!("\nstria added to {} agent(s). Restart your agent.", added);
    }
}

pub fn run_remove(yes: bool) {
    let configured = find_configured_agents();
    if configured.is_empty() {
        eprintln!("No agents with stria configuration found.");
        return;
    }

    for (name, path) in &configured {
        let proceed = yes || {
            eprint!("Remove stria from {}? [y/N]: ", name);
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or(0);
            input.trim().eq_ignore_ascii_case("y")
        };
        if !proceed {
            continue;
        }

        let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
        let cfg: Value = serde_json::from_str(&content).unwrap_or(json!({}));
        let updated = remove_stria(cfg, name);
        if let Ok(text) = serde_json::to_string_pretty(&updated) {
            if std::fs::write(path, text).is_ok() {
                eprintln!("  Removed from {}", name);
            }
        }
    }
}
