use std::path::Path;
use std::collections::HashMap;
use rusqlite::{Connection, params};

use crate::zone;

/// BM25 scoring for phrase search.
pub mod bm25;

/// Line-number proximity bonus.
pub mod proximity;

/// Search the phrase index and return ranked file paths with scores.
pub fn search_phrases(
    db_path: &str,
    query: &str,
    top_n: usize,
) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let n_docs: f64 = db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0)).unwrap_or(1.0);
    let avgdl: f64 = db.query_row("SELECT value FROM meta WHERE key='avgdl'", [], |r| r.get(0)).unwrap_or(100.0);

    // Parse query into search terms
    let raw_terms: Vec<String> = zone::extract_phrases(query);
    if raw_terms.is_empty() { return vec![]; }

    // Build search_terms from raw terms (lower + stem variants)
    let mut search_terms: Vec<String> = Vec::new();
    for t in &raw_terms {
        let tl = t.to_lowercase();
        if !search_terms.contains(&tl) {
            search_terms.push(tl.clone());
        }
        // Suffix morphing: "ing"→"", "ed"→"", trailing "s"→""
        if tl.ends_with("ing") {
            let stem = tl[..tl.len()-3].to_string();
            if !search_terms.contains(&stem) { search_terms.push(stem); }
        } else if tl.ends_with('s') && tl.len() > 3 {
            let stem = tl[..tl.len()-1].to_string();
            if !search_terms.contains(&stem) { search_terms.push(stem); }
        }
    }

    // Pre-compute IDF for all search terms
    let mut idf_map: HashMap<String, f64> = HashMap::new();
    for st in &search_terms {
        let df: f64 = db.query_row(
            "SELECT COUNT(*) FROM phrase_occ WHERE phrase = ?1", [st], |r| r.get(0)
        ).unwrap_or(1.0);
        idf_map.insert(st.clone(), bm25::bm25_idf(n_docs, df.max(0.5)));
    }

    let mut file_scores: HashMap<i64, f64> = HashMap::new();
    let mut file_matches: HashMap<i64, Vec<String>> = HashMap::new();

    // Tier 1: Exact match
    for st in &search_terms {
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let mut exact_q = db.prepare(
            "SELECT po.file_id, po.count, po.is_def, po.zone, fs.token_len
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase = ?1"
        ).unwrap();
        let rows: Vec<_> = exact_q.query_map([st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        }).unwrap().filter_map(|r| r.ok()).collect();
        drop(exact_q);

        for (fid, tf, is_def, zone_str, doc_len) in rows {
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_str == "code" { 2.0 } else { 0.25 };
            let def_mult = match is_def {
                2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0,
            };
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult;
            file_matches.entry(fid).or_default().push(st.clone());
        }
    }

    // Tier 2: Prefix match (LIKE 'term%')
    for st in &search_terms {
        let pattern = format!("{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.3;

        let mut prefix_q = db.prepare(
            "SELECT po.file_id, po.is_def, po.zone, fs.token_len
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase LIKE ?1 AND po.phrase != ?2"
        ).unwrap();
        let rows: Vec<_> = prefix_q.query_map(params![&pattern, st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
            ))
        }).unwrap().filter_map(|r| r.ok()).collect();
        drop(prefix_q);

        for (fid, is_def, zone_str, doc_len) in rows {
            let tf = 1.0; // existence-only for prefix
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_str == "code" { 2.0 } else { 0.25 };
            let def_mult = match is_def {
                2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0,
            };
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult * idf_rare;
            file_matches.entry(fid).or_default().push(format!("~{}", st));
        }
    }

    // Sort by score descending
    let mut scored: Vec<(i64, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Map to file paths
    let mut results: Vec<(String, f64)> = Vec::new();
    for (fid, score) in scored.iter().take(top_n) {
        if let Ok(fp) = db.query_row(
            "SELECT file_path FROM file_map WHERE id = ?1", [fid], |r| r.get::<_, String>(0)
        ) {
            results.push((fp, *score));
        }
    }

    db.close().ok();
    results
}
