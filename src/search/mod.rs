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
        if fp_lower.contains(t) { return 1.5; }
    }
    1.0
}

/// Unpack first_line from 4-byte little-endian BLOB
fn unpack_first_line(blob: &[u8]) -> Option<i32> {
    if blob.len() < 4 { return None; }
    Some(i32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]))
}

/// Proximity bonus from first_line entries.
/// Multiplicative: if matched terms cluster within 10 lines, 1.0-1.5x boost.
fn proximity_mult(line_sets: &[Vec<i32>], max_gap: i32) -> f64 {
    if line_sets.len() < 2 { return 1.0; }
    let mut min_dist = i32::MAX;
    for i in 0..line_sets.len() {
        for j in (i + 1)..line_sets.len() {
            for &a in &line_sets[i] {
                for &b in &line_sets[j] {
                    let dist = if a > b { a - b } else { b - a };
                    if dist < min_dist { min_dist = dist; }
                }
            }
        }
    }
    if min_dist <= max_gap {
        1.0 + (max_gap - min_dist + 1) as f64 / max_gap as f64 * 0.5
    } else {
        1.0
    }
}

/// Get directory parent for module cluster scoring
fn file_module(file_path: &str) -> String {
    if let Some(idx) = file_path.rfind('/') {
        file_path[..idx].to_string()
    } else {
        String::new()
    }
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
    let mut file_line_sets: HashMap<i64, HashMap<String, Vec<i32>>> = HashMap::new();
    let mut file_tiers: HashMap<i64, u8> = HashMap::new(); // 0=exact, 1=prefix, 2=substr
    let mut file_phrases_matched: HashMap<i64, u32> = HashMap::new();

    // Tier 1: Exact match BM25
    for st in &search_terms {
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let mut exact_q = match db.prepare(
            "SELECT po.file_id, po.count, po.is_def, po.zone_int, fs.token_len,
                    fs.unique_def_count, fs.total_def_count, fs.comment_ratio,
                    po.line_nos
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase = ?1"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rows_result = exact_q.query_map([st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, i32>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, f64>(7)?,
                r.get::<_, Vec<u8>>(8)?,
            ))
        }).unwrap();
        for row in rows_result.filter_map(|r| r.ok()) {
            let (fid, tf, is_def, zone_int, doc_len, uniq_def, total_def, comment_ratio, line_nos) = row;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else { 1.0 };
            // Comment penalty: high comment ratio reduces score
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult * uniq_mult * comment_mult;
            file_tiers.entry(fid).or_insert(0);
            *file_phrases_matched.entry(fid).or_insert(0) += 1;
            if let Some(ln) = line_nos.first() {
                let line = *ln as i32; // first byte is sufficient for first_line since first_line ≤ 255 for small files... no.
                // Actually unpack the 4-byte LE
                if let Some(first_line) = unpack_first_line(&line_nos) {
                    file_line_sets.entry(fid).or_default()
                        .entry(st.clone()).or_default()
                        .push(first_line);
                }
            }
        }
    }

    // Tier 2: Prefix match
    for st in &search_terms {
        let pattern = format!("{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.3;

        let mut prefix_q = match db.prepare(
            "SELECT po.file_id, po.is_def, po.zone_int, fs.token_len,
                    fs.unique_def_count, fs.total_def_count, fs.comment_ratio
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase LIKE ?1 AND po.phrase != ?2"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let prefix_rows = prefix_q.query_map(params![&pattern, st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, f64>(6)?,
            ))
        }).unwrap();
        for row in prefix_rows.filter_map(|r| r.ok()) {
            let (fid, is_def, zone_int, doc_len, uniq_def, total_def, comment_ratio) = row;
            let tf = 1.0;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else { 1.0 };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            let contrib = score * zone_mult * def_mult * uniq_mult * comment_mult * idf_rare;
            *file_scores.entry(fid).or_insert(0.0) += contrib;
            file_tiers.entry(fid).or_insert(1);
            *file_phrases_matched.entry(fid).or_insert(0) += 1;
        }
    }

    // Tier 3: Substring match
    for st in &search_terms {
        if st.len() < 4 { continue; }
        let pattern = format!("%{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.15;
        let excl_prefix = format!("{}%", st);

        let mut sub_q = match db.prepare(
            "SELECT po.file_id, po.count, po.is_def, po.zone_int, fs.token_len,
                    fs.comment_ratio
             FROM phrase_occ po JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE po.phrase LIKE ?1 AND po.phrase NOT LIKE ?2 AND po.phrase != ?3
             LIMIT 200"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let sub_rows = sub_q.query_map(params![&pattern, &excl_prefix, st], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, i32>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, f64>(5)?,
            ))
        }).unwrap();
        for row in sub_rows.filter_map(|r| r.ok()) {
            let (fid, tf, is_def, zone_int, doc_len, comment_ratio) = row;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            let contrib = score * zone_mult * def_mult * comment_mult * idf_rare;
            *file_scores.entry(fid).or_insert(0.0) += contrib;
            file_tiers.entry(fid).or_insert(2);
            *file_phrases_matched.entry(fid).or_insert(0) += 1;
        }
    }

    // Phase 4: Concentration bonus — files matching more distinct query terms get boost
    let total_search_terms = search_terms.len() as f64;
    if total_search_terms > 1.0 {
        for (fid, score) in file_scores.iter_mut() {
            let matched = file_phrases_matched.get(fid).copied().unwrap_or(0) as f64;
            let concentration = (matched.min(total_search_terms)) / total_search_terms;
            *score *= 1.0 + concentration * 0.5;
        }
    }

    // Sort by base score
    let mut scored: Vec<(i64, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Collect top candidates for final scoring with path-role, filename boost, proximity, module bonus
    let candidate_count = (top_n * 5).min(scored.len());
    let top_fids: Vec<i64> = scored.iter().take(candidate_count).map(|(fid, _)| *fid).collect();

    // Resolve file paths
    let mut fp_map: HashMap<i64, String> = HashMap::new();
    if !top_fids.is_empty() {
        let placeholders: Vec<String> = top_fids.iter().map(|_| "?".to_string()).collect();
        let sql = format!("SELECT id, file_path FROM file_map WHERE id IN ({})", placeholders.join(","));
        if let Ok(mut stmt) = db.prepare(&sql) {
            let params: Vec<&dyn rusqlite::types::ToSql> = top_fids.iter()
                .map(|id| id as &dyn rusqlite::types::ToSql).collect();
            if let Ok(rows) = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() { fp_map.insert(row.0, row.1); }
            }
        }
    }

    // Compute module cluster bonus
    let mut module_scores: HashMap<String, f64> = HashMap::new();
    for (fid, base_score) in &scored[..candidate_count.min(20)] {
        if let Some(fp) = fp_map.get(fid) {
            let mod_path = file_module(fp);
            *module_scores.entry(mod_path).or_insert(0.0) += base_score;
        }
    }

    // Apply all multipliers
    let mut results: Vec<(String, f64)> = Vec::new();
    let mut top_fps: Vec<String> = Vec::new();
    for (fid, base_score) in &scored[..candidate_count] {
        if let Some(fp) = fp_map.get(fid) {
            let mut final_score = *base_score;

            // Path-role multiplier
            final_score *= path_role_mult(fp);

            // Filename boost
            final_score *= filename_boost(fp, &search_terms);

            // Module cluster bonus: +10% if file's module is a top module
            let mod_path = file_module(fp);
            if let Some(mod_score) = module_scores.get(&mod_path) {
                if *mod_score > 0.0 {
                    final_score *= 1.0 + (mod_score / n_docs).min(0.1);
                }
            }

            // Proximity bonus: across matched terms
            if let Some(line_sets) = file_line_sets.get(fid) {
                let lines: Vec<Vec<i32>> = line_sets.values().cloned().collect();
                if !lines.is_empty() {
                    final_score *= proximity_mult(&lines, 10);
                }
            }

            top_fps.push(fp.clone());
            results.push((fp.clone(), final_score));
        }
    }

    // Re-sort with all multipliers
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(top_n);

    db.close().ok();
    results
}
