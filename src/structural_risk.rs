// Structural risk functions: entangle diffusion, blast radius, verify candidates
// Lifted from Quale's MIT-licensed codebase.

use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::index::schema;

/// Find files that call/use a given identifier via phrase index.
pub fn who_calls(db_path: &str, name: &str) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("eh:error: who_calls: db open failed: {}", e);
            return vec![];
        }
    };
    let mut results = Vec::new();

    // Find definition file(s) with is_def=1 — unpack in Rust
    let def_fids: Vec<i64> = {
        let mut stmt = match db.prepare(
            "SELECT po.file_id, po.flags FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             WHERE p.phrase = ?1 LIMIT 5",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("eh:warn: who_calls: prepare failed: {}", e);
                return vec![];
            }
        };
        let rows: Vec<(i64, i32)> = match stmt.query_map([name], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            Ok((fid, schema::unpack_is_def(f)))
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                eprintln!("eh:warn: who_calls: query failed for '{}': {}", name, e);
                vec![]
            }
        };
        rows.into_iter()
            .filter(|(_, is_def)| *is_def == 1)
            .map(|(fid, _)| fid)
            .collect()
    };

    if let Some(&fid) = def_fids.first() {
        let mut stmt = match db.prepare(
            "SELECT fm.file_path,
                    COALESCE(oc.count, 1) as effective_count
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_map fm ON fm.id = po.file_id
             LEFT JOIN count_overflow oc ON oc.phrase_id = po.phrase_id AND oc.file_id = po.file_id
             WHERE p.phrase = ?1 AND po.file_id != ?2
             LIMIT 20",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("eh:warn: who_calls: second prepare failed: {}", e);
                return vec![];
            }
        };
        let second_rows: Vec<(String, f64)> =
            match stmt.query_map(rusqlite::params![name, fid], |r| {
                let fp = r.get::<_, String>(0)?;
                let count = r.get::<_, f64>(1)?;
                Ok((fp, count))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    eprintln!("eh:warn: who_calls: second query failed: {}", e);
                    vec![]
                }
            };
        for row in second_rows {
            results.push(row);
        }
    }

    db.close().ok();
    results
}

/// Trace calls through N hops. Hop 1 = direct callers (same as who_calls).
/// Hop 2+ follows phrase_occ edges: files sharing distinctive vocabulary
/// with the caller are "callers of callers."
pub fn trace_callers(db_path: &str, name: &str, depth: u32) -> Vec<(String, f64)> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results: Vec<(String, f64)> = Vec::new();

    // Hop 1: direct definitions and callers
    let mut current = who_calls(db_path, name);
    for (fp, score) in &current {
        if seen.insert(fp.clone()) {
            results.push((fp.clone(), *score));
        }
    }

    // Hops 2+: find files that share distinctive vocabulary with each caller
    // (latent coupling through phrase_occ edges, not direct references)
    for _ in 2..=depth {
        let mut next_results: Vec<(String, f64)> = Vec::new();
        for (caller_fp, _) in &current {
            // Find files sharing rare definition phrases with this caller
            let deps = latent_deps(db_path, caller_fp);
            for (fp, score) in &deps {
                if seen.insert(fp.clone()) {
                    // Diminishing weight per hop (0.5^hop)
                    let hop_weight = 0.5_f64.powi((depth as i32) - 1);
                    next_results.push((fp.clone(), score * hop_weight));
                }
            }
        }
        if next_results.is_empty() {
            break;
        }
        results.extend(next_results.iter().cloned());
        current = next_results;
    }

    results
}

/// Find latent dependencies: files in different modules sharing rare vocabulary.
pub fn latent_deps(db_path: &str, file: &str) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("eh:error: latent_deps: db open failed: {}", e);
            return vec![];
        }
    };

    let fid: Option<i64> = db
        .query_row(
            "SELECT id FROM file_map WHERE file_path = ?1",
            [file],
            |r| r.get(0),
        )
        .ok();

    if fid.is_none() {
        db.close().ok();
        return vec![];
    }
    let fid = fid.unwrap();
    let fp = file.to_string();
    let module = std::path::Path::new(&fp)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    // Find rare phrases (df <= 3) that this file defines — unpack is_def in Rust
    let mut rare_q = db
        .prepare(
            "SELECT p.phrase, po.flags FROM phrase_occ po
         JOIN phrases p ON p.id = po.phrase_id
         WHERE po.file_id = ?1
         AND (SELECT COUNT(*) FROM phrase_occ po2 WHERE po2.phrase_id = po.phrase_id) <= 3
         LIMIT 50",
        )
        .unwrap();
    let rare_phrases: Vec<String> = rare_q
        .query_map([fid], |r| {
            let phrase = r.get::<_, String>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            Ok((phrase, schema::unpack_is_def(f)))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|(_, is_def)| *is_def == 1)
        .map(|(phrase, _)| phrase)
        .collect();
    drop(rare_q);

    if rare_phrases.is_empty() {
        db.close().ok();
        return vec![];
    }

    let mut score_map: HashMap<String, f64> = HashMap::new();
    for phrase in &rare_phrases {
        let mut stmt = db
            .prepare(
                "SELECT fm.file_path
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_map fm ON fm.id = po.file_id
             WHERE p.phrase = ?1 AND po.file_id != ?2
             LIMIT 10",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![phrase, fid], |r| r.get::<_, String>(0))
            .unwrap();
        for row in rows.flatten() {
            let other_module = std::path::Path::new(&row)
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if other_module != module {
                *score_map.entry(row).or_insert(0.0) += 1.0;
            }
        }
    }

    db.close().ok();
    let mut results: Vec<(String, f64)> = score_map.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(20);
    results
}

/// Compute blast radius: files sharing vocabulary with a given file
pub fn blast_radius(db_path: &str, file: &str) -> Vec<(String, f64)> {
    let (db, fid) = match open_db_and_file(db_path, file) {
        Some((d, f)) => (d, f),
        None => return vec![],
    };

    // Get distinctive phrases (is_def > 0) — unpack in Rust
    let mut phrase_q = db
        .prepare(
            "SELECT p.phrase, po.flags,
                COALESCE(oc.count, 1) as effective_count
         FROM phrase_occ po
         JOIN phrases p ON p.id = po.phrase_id
         LEFT JOIN count_overflow oc ON oc.phrase_id = po.phrase_id AND oc.file_id = po.file_id
         WHERE po.file_id=?1 LIMIT 50",
        )
        .unwrap();
    let phrases: Vec<(String, f64)> = phrase_q
        .query_map([fid], |r| {
            let phrase = r.get::<_, String>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let count = r.get::<_, f64>(2)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            let is_def = schema::unpack_is_def(f);
            Ok((phrase, count, is_def))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|(_, _, is_def)| *is_def > 0)
        .map(|(phrase, count, _)| (phrase, count))
        .collect();
    drop(phrase_q);

    if phrases.is_empty() {
        return vec![];
    }

    let mut other_scores: HashMap<i64, f64> = HashMap::new();
    for (phrase, count) in &phrases {
        let mut q = db
            .prepare(
                "SELECT po.file_id FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             WHERE p.phrase=?1 AND po.file_id!=?2 LIMIT 10",
            )
            .unwrap();
        let rows: Vec<i64> = q
            .query_map(params![phrase, fid], |r| r.get::<_, i64>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        drop(q);
        for ofid in rows {
            *other_scores.entry(ofid).or_insert(0.0) += count;
        }
    }

    let mut results: Vec<(String, f64)> = Vec::new();
    for (ofid, score) in other_scores {
        if let Some(path) = resolve_file_path(&db, ofid) {
            results.push((path, score));
        }
    }
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(20);
    results
}

fn open_db_and_file(db_path: &str, file: &str) -> Option<(Connection, i64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("eh:error: db open failed: {}", e);
            return None;
        }
    };
    match db.query_row("SELECT id FROM file_map WHERE file_path=?1", [file], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(fid) => Some((db, fid)),
        Err(e) => {
            eprintln!("eh:warn: file not found in index: '{}': {}", file, e);
            None
        }
    }
}

fn resolve_file_path(db: &Connection, fid: i64) -> Option<String> {
    db.query_row("SELECT file_path FROM file_map WHERE id=?1", [fid], |r| {
        r.get(0)
    })
    .ok()
}

/// Build a file index: ordered list of all file paths for ID-based encoding.
/// The agent receives this once via hologram_orient and references files by
/// 0-based position in subsequent hologram_task calls — saving ~50 tokens
/// per response vs repeating full paths.
pub fn build_file_index(db_path: &str) -> Vec<String> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let mut stmt = match db.prepare("SELECT file_path FROM file_map ORDER BY id") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let files: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    // db closes on drop
    files
}

/// Find fixture definitions (Dark Matter): capitalized identifiers used in the
/// test file that are defined in a different non-source file (mock, helper, fixture).
/// Returns (identifier_name, definition_file_path) pairs.
pub fn find_fixtures(db_path: &str, source_file: &str, test_file: &str) -> Vec<(String, String)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("eh:error: find_fixtures: db open failed: {}", e);
            return vec![];
        }
    };

    let src_fid: i64 = match db.query_row(
        "SELECT id FROM file_map WHERE file_path=?1",
        [source_file],
        |r| r.get(0),
    ) {
        Ok(id) => id,
        Err(_) => return vec![],
    };
    let test_fid: i64 = match db.query_row(
        "SELECT id FROM file_map WHERE file_path=?1",
        [test_file],
        |r| r.get(0),
    ) {
        Ok(id) => id,
        Err(_) => return vec![],
    };

    // Find capitalized identifiers used in the test file that are defined
    // with is_def=1 in a third file (not source, not test).
    // Filters to code source/test files only using mechanical path rules.
    let mut results: Vec<(String, String)> = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT p.phrase, fm_def.file_path
         FROM phrase_occ po_test
         JOIN phrases p ON p.id = po_test.phrase_id
         JOIN phrase_occ po_def ON po_def.phrase_id = po_test.phrase_id
             AND po_def.file_id NOT IN (?1, ?2)
         JOIN file_map fm_def ON fm_def.id = po_def.file_id
         WHERE po_test.file_id = ?1
           AND (po_def.flags & 1) = 1
           AND p.phrase GLOB '[A-Z]*'
           AND LENGTH(p.phrase) > 3
         GROUP BY p.phrase
         LIMIT 20",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![test_fid, src_fid], |r| {
            let phrase = r.get::<_, String>(0)?;
            let fp = r.get::<_, String>(1)?;
            Ok((phrase, fp))
        }) {
            for row in rows.flatten() {
                let (phrase, fp) = row;
                // Mechanical path filter: skip docs, artifacts, lockfiles, configs
                let fp_norm = fp.replace('\\', "/");
                let skip = fp_norm.ends_with(".md")
                    || fp_norm.ends_with(".json")
                    || fp_norm.ends_with(".yml")
                    || fp_norm.ends_with(".yaml")
                    || fp_norm.ends_with(".toml")
                    || fp_norm.ends_with(".txt")
                    || fp_norm.ends_with(".html")
                    || fp_norm.ends_with(".css")
                    || fp_norm.starts_with("artifacts/")
                    || fp_norm.starts_with("node_modules/")
                    || fp_norm.contains("web/templates")
                    || fp_norm.contains("BACKLOG")
                    || fp_norm.contains("CRITICAL");
                if !skip {
                    results.push((phrase, fp));
                }
            }
        }
    }

    results
}

/// Find verification candidates: test files related to a target source file
pub fn find_verify_candidates(db_path: &str, file: &str) -> Vec<(String, f64)> {
    let (db, fid) = match open_db_and_file(db_path, file) {
        Some((d, f)) => (d, f),
        None => return vec![],
    };

    // Get overlapping phrases with is_def>0 from test files — unpack in Rust
    let mut stmt = db.prepare(
        "SELECT fm.file_path, po2.flags
         FROM phrase_occ po1
         JOIN phrase_occ po2 ON po2.phrase_id = po1.phrase_id AND po2.file_id != po1.file_id
         JOIN file_map fm ON fm.id = po2.file_id
         WHERE po1.file_id = ?1
           AND (fm.file_path LIKE '%test%' OR fm.file_path LIKE '%spec%' OR fm.file_path LIKE '%__tests__%')
         LIMIT 500"
    ).unwrap();

    let mut overlap_map: HashMap<String, f64> = HashMap::new();
    let rows = stmt
        .query_map([fid], |r| {
            let fp = r.get::<_, String>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            Ok((fp, schema::unpack_is_def(f)))
        })
        .unwrap();
    for row in rows.filter_map(|r| r.ok()) {
        let (fp, is_def) = row;
        if is_def > 0 {
            *overlap_map.entry(fp).or_insert(0.0) += 1.0;
        }
    }

    let mut results: Vec<(String, f64)> = overlap_map.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(10);
    results
}
pub fn hologram_plan(db_path: &str, task: &str) -> serde_json::Value {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return serde_json::json!({"error": "cannot open db"}),
    };

    let n_docs: f64 = db
        .query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0))
        .unwrap_or(1.0);
    let avgdl: f64 = db
        .query_row("SELECT value FROM meta WHERE key='avgdl'", [], |r| r.get(0))
        .unwrap_or(100.0);

    let task_phrases: Vec<String> = crate::zone::extract_phrases(task);
    let task_lower: Vec<String> = task_phrases.iter().map(|p| p.to_lowercase()).collect();

    let mut file_scores: HashMap<String, f64> = HashMap::new();
    for st in &task_lower {
        if let Ok(mut stmt) = db.prepare(
            "SELECT fm.file_path, po.flags, fs.token_len
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_map fm ON fm.id = po.file_id
             JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE p.phrase = ?1
             LIMIT 50",
        ) {
            let rows = stmt
                .query_map([st], |r| {
                    let fp = r.get::<_, String>(0)?;
                    let flags = r.get::<_, Vec<u8>>(1)?;
                    let doc_len = r.get::<_, f64>(2)?;
                    let f = if !flags.is_empty() { flags[0] } else { 0 };
                    let base_count = schema::unpack_count(f);
                    let tf = if base_count >= 31 {
                        1.0
                    } else {
                        base_count as f64
                    };
                    Ok((fp, tf, schema::unpack_is_def(f), doc_len))
                })
                .unwrap();
            for row in rows.flatten() {
                let (fp, tf, is_def, doc_len) = row;
                let idf = crate::search::bm25::bm25_idf(n_docs, 5.0);
                let score = crate::search::bm25::bm25_score(idf, tf, doc_len, avgdl);
                let def_mult = if is_def > 0 { 5.0 } else { 1.0 };
                *file_scores.entry(fp).or_insert(0.0) += score * def_mult;
            }
        }
    }

    let mut scored: Vec<(String, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(10);

    let read_files: Vec<String> = scored.iter().take(3).map(|(fp, _)| fp.clone()).collect();
    let verify_candidates: Vec<String> = scored
        .iter()
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

    let coupled: Vec<String> = scored
        .iter()
        .skip(1)
        .take(4)
        .map(|(fp, _)| fp.clone())
        .collect();

    db.close().ok();

    // Dark Matter Mapper: find fixture definitions for the top verify candidate
    let fixtures: Vec<serde_json::Value> =
        if let Some(edit_file) = scored.first().map(|(fp, _)| fp.clone()) {
            verify_candidates
                .iter()
                .take(2)
                .flat_map(|test_file| {
                    find_fixtures(db_path, &edit_file, test_file)
                        .into_iter()
                        .map(|(phrase, fp)| serde_json::json!({"phrase": phrase, "file": fp}))
                        .collect::<Vec<_>>()
                })
                .collect()
        } else {
            vec![]
        };

    serde_json::json!({
        "edit": scored.first().map(|(fp, _)| fp),
        "verify": verify_candidates,
        "fixtures": fixtures,
        "read_first": read_files,
        "coupled": coupled,
        "risk": risk,
    })
}
