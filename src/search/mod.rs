use std::collections::HashMap;
use rusqlite::{Connection, params};

use crate::zone;

/// BM25 scoring for phrase search.
pub mod bm25;

/// Line-number proximity bonus.
pub mod proximity;

/// Path-role multiplier: /src/ = 2.0, tests = 0.6, deps/vendor = 0.3
fn path_role_mult(file_path: &str) -> f64 {
    if file_path.contains("/src/") || file_path.starts_with("src/") { return 2.0; }
    if file_path.contains("/test") || file_path.contains("/spec") || 
       file_path.starts_with("test") || file_path.starts_with("spec") { return 0.6; }
    if file_path.contains("/deps/") || file_path.contains("/vendor/") ||
       file_path.starts_with("vendor/") || file_path.starts_with("deps/") { return 0.3; }
    if file_path.contains("/scripts/") || file_path.starts_with("scripts/") { return 0.6; }
    if file_path.ends_with(".md") || file_path.ends_with(".rst") || file_path.ends_with(".txt") { return 0.25; }
    1.0
}

/// Filename boost: +1.5 if any search term appears in the file's path
fn filename_boost(file_path: &str, terms: &[String]) -> f64 {
    let fp_lower = file_path.to_lowercase();
    for t in terms {
        if fp_lower.contains(t) {
            return 1.5;
        }
    }
    1.0
}

/// Search the phrase index with full feature parity.
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

    let raw_terms: Vec<String> = zone::extract_phrases(query);
    if raw_terms.is_empty() { return vec![]; }

    // Build search_terms: lower + stem variants
    let mut search_terms: Vec<String> = Vec::new();
    for t in &raw_terms {
        let tl = t.to_lowercase();
        if !search_terms.contains(&tl) { search_terms.push(tl.clone()); }
        if tl.ends_with("ing") && tl.len() > 4 {
            let s = tl[..tl.len()-3].to_string();
            if !search_terms.contains(&s) { search_terms.push(s); }
        } else if tl.ends_with('s') && tl.len() > 3 {
            let s = tl[..tl.len()-1].to_string();
            if !search_terms.contains(&s) { search_terms.push(s); }
        }
        if tl.ends_with("ed") && tl.len() > 4 {
            let s = tl[..tl.len()-2].to_string();
            if !search_terms.contains(&s) { search_terms.push(s); }
        }
        if tl.ends_with("tion") && tl.len() > 5 {
            let s = tl[..tl.len()-4].to_string();
            if !search_terms.contains(&s) { search_terms.push(s); }
        }
    }

    // Pre-compute IDF
    let mut idf_map: HashMap<String, f64> = HashMap::new();
    for st in &search_terms {
        let df: f64 = db.query_row(
            "SELECT COUNT(*) FROM phrase_occ WHERE phrase = ?1", [st], |r| r.get(0)
        ).unwrap_or(1.0);
        idf_map.insert(st.clone(), bm25::bm25_idf(n_docs, df.max(0.5)));
    }

    let mut file_scores: HashMap<i64, f64> = HashMap::new();

    // Tier 1: Exact match BM25
    for st in &search_terms {
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let mut exact_q = match db.prepare(
            "SELECT po.file_id, po.count, po.is_def, po.zone, fs.token_len,
                    fs.unique_def_count, fs.total_def_count
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase = ?1"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rows: Vec<_> = exact_q.query_map([st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        }).unwrap().filter_map(|r| r.ok()).collect();
        drop(exact_q);

        for (fid, tf, is_def, zone_str, doc_len, uniq_def, total_def) in rows {
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_str == "code" { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else { 1.0 };
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult * uniq_mult;
        }
    }

    // Tier 2: Prefix match
    for st in &search_terms {
        let pattern = format!("{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.3;

        let mut prefix_q = match db.prepare(
            "SELECT po.file_id, po.is_def, po.zone, fs.token_len,
                    fs.unique_def_count, fs.total_def_count
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase LIKE ?1 AND po.phrase != ?2"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rows: Vec<_> = prefix_q.query_map(params![&pattern, st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        }).unwrap().filter_map(|r| r.ok()).collect();
        drop(prefix_q);

        for (fid, is_def, zone_str, doc_len, uniq_def, total_def) in rows {
            let tf = 1.0;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_str == "code" { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else { 1.0 };
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult * uniq_mult * idf_rare;
        }
    }

    // Tier 3: Substring match (lowest weight, CamelCase bridging)
    for st in &search_terms {
        if st.len() < 4 { continue; }
        let pattern = format!("%{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.15;
        let excl_prefix = format!("{}%", st);

        let mut sub_q = match db.prepare(
            "SELECT po.file_id, po.count, po.is_def, po.zone, fs.token_len
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase LIKE ?1 AND po.phrase NOT LIKE ?2 AND po.phrase != ?3
             LIMIT 200"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rows: Vec<_> = sub_q.query_map(params![&pattern, &excl_prefix, st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        }).unwrap().filter_map(|r| r.ok()).collect();
        drop(sub_q);

        for (fid, tf, is_def, zone_str, doc_len) in rows {
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_str == "code" { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let contrib = score * zone_mult * def_mult * idf_rare;
            *file_scores.entry(fid).or_insert(0.0) += contrib;
        }
    }

    // Collect scored file IDs
    let mut scored: Vec<(i64, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Resolve paths for top candidates and apply path-role + filename multipliers
    let mut results: Vec<(String, f64)> = Vec::new();
    let top_batch = scored.iter().take(top_n).collect::<Vec<_>>();
    for (fid, base_score) in top_batch {
        if let Ok(fp) = db.query_row(
            "SELECT file_path FROM file_map WHERE id = ?1", [fid], |r| r.get::<_, String>(0)
        ) {
            let role_mult = path_role_mult(&fp);
            let fname_mult = filename_boost(&fp, &search_terms);
            results.push((fp, base_score * role_mult * fname_mult));
        }
    }

    // Re-sort with multipliers applied
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    db.close().ok();
    results
}
