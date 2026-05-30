// Structural risk functions: entangle diffusion, blast radius, verify candidates
// Lifted from Quale's MIT-licensed codebase.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use rusqlite::Connection;

/// Find files that call/use a given identifier via phrase index.
pub fn who_calls(db_path: &str, name: &str) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let mut results = Vec::new();

    // Get file_id for the definition
    let def_fid: Option<i64> = db.query_row(
        "SELECT po.file_id FROM phrase_occ po
         JOIN file_map fm ON fm.id = po.file_id
         WHERE po.phrase = ?1 AND po.is_def = 1
         LIMIT 1",
        [name],
        |r| r.get(0),
    ).ok();

    if let Some(fid) = def_fid {
        // Find files that share phrases + check co-change if entangle cache exists
        let mut stmt = db.prepare(
            "SELECT fm.file_path, po.count
             FROM phrase_occ po
             JOIN file_map fm ON fm.id = po.file_id
             WHERE po.phrase = ?1 AND po.file_id != ?2
             ORDER BY po.count DESC
             LIMIT 20"
        ).unwrap();
        let rows = stmt.query_map(rusqlite::params![name, fid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
            ))
        }).unwrap();
        for row in rows.flatten() {
            results.push(row);
        }
    }

    db.close().ok();
    results
}

/// Find latent dependencies: files in different modules sharing rare vocabulary.
pub fn latent_deps(db_path: &str, file: &str) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    // Get file_id
    let fid: Option<i64> = db.query_row(
        "SELECT id FROM file_map WHERE file_path = ?1",
        [file],
        |r| r.get(0),
    ).ok();

    if fid.is_none() { db.close().ok(); return vec![]; }
    let fid = fid.unwrap();

    // Get file's phrase set path
    let fp = file.to_string();
    let module = fp.rsplitn(2, '/').last().unwrap_or(&fp).to_string();

    // Find rare phrases (df <= 3) that this file defines
    let mut rare_q = db.prepare(
        "SELECT po.phrase FROM phrase_occ po
         WHERE po.file_id = ?1 AND po.is_def = 1
         AND (SELECT COUNT(*) FROM phrase_occ WHERE phrase = po.phrase) <= 3
         LIMIT 50"
    ).unwrap();
    let rare_phrases: Vec<String> = rare_q.query_map([fid], |r| {
        r.get::<_, String>(0)
    }).unwrap().filter_map(|r| r.ok()).collect();
    drop(rare_q);

    if rare_phrases.is_empty() { db.close().ok(); return vec![]; }

    // Find files in OTHER modules sharing these rare phrases
    let mut score_map: HashMap<String, f64> = HashMap::new();
    for phrase in &rare_phrases {
        let mut stmt = db.prepare(
            "SELECT fm.file_path
             FROM phrase_occ po
             JOIN file_map fm ON fm.id = po.file_id
             WHERE po.phrase = ?1 AND po.file_id != ?2
             LIMIT 10"
        ).unwrap();
        let rows = stmt.query_map(rusqlite::params![phrase, fid], |r| {
            r.get::<_, String>(0)
        }).unwrap();
        for row in rows.flatten() {
            let other_fp = row;
            let other_module = other_fp.rsplitn(2, '/').last().unwrap_or(&other_fp).to_string();
            if other_module != module {
                *score_map.entry(other_fp).or_insert(0.0) += 1.0;
            }
        }
    }

    db.close().ok();
    let mut results: Vec<(String, f64)> = score_map.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(20);
    results
}

/// Generate a hologram plan: recommended edit files, verify candidates, risk
pub fn hologram_plan(
    db_path: &str,
    task: &str,
) -> serde_json::Value {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return serde_json::json!({"error": "cannot open db"}),
    };

    let n_docs: f64 = db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0)).unwrap_or(1.0);
    let avgdl: f64 = db.query_row("SELECT value FROM meta WHERE key='avgdl'", [], |r| r.get(0)).unwrap_or(100.0);

    // Simple risk assessment based on keyword matching
    let task_phrases: Vec<String> = crate::zone::extract_phrases(task);
    let task_lower: Vec<String> = task_phrases.iter().map(|p| p.to_lowercase()).collect();

    // Find files matching task
    let mut file_scores: HashMap<String, f64> = HashMap::new();
    for st in &task_lower {
        if let Ok(mut stmt) = db.prepare(
            "SELECT fm.file_path, po.count, po.is_def, fs.token_len
             FROM phrase_occ po
             JOIN file_map fm ON fm.id = po.file_id
             JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase = ?1
             LIMIT 50"
        ) {
            let rows = stmt.query_map([st], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            }).unwrap();
            for row in rows.flatten() {
                let (fp, tf, is_def, doc_len) = row;
                let idf = crate::search::bm25::bm25_idf(n_docs, 5.0); // rough IDF
                let score = crate::search::bm25::bm25_score(idf, tf, doc_len, avgdl);
                let def_mult = if is_def > 0 { 5.0 } else { 1.0 };
                *file_scores.entry(fp).or_insert(0.0) += score * def_mult;
            }
        }
    }

    let mut scored: Vec<(String, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(10);

    // Build plan
    let read_files: Vec<String> = scored.iter().take(3).map(|(fp, _)| fp.clone()).collect();
    let verify_candidates: Vec<String> = scored.iter()
        .filter(|(fp, _)| fp.contains("test"))
        .take(3)
        .map(|(fp, _)| fp.clone())
        .collect();
    let risk = if scored.is_empty() {
        "unknown".to_string()
    } else if scored[0].1 > 10.0 {
        "low".to_string()
    } else {
        "moderate".to_string()
    };

    let coupled: Vec<String> = scored.iter()
        .skip(1).take(4)
        .map(|(fp, _)| fp.clone())
        .collect();

    db.close().ok();

    serde_json::json!({
        "edit": scored.first().map(|(fp, _)| fp),
        "verify": verify_candidates,
        "read_first": read_files,
        "coupled": coupled,
        "risk": risk,
    })
}
