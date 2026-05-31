use std::collections::HashMap;
use rusqlite::{Connection, params};

use crate::zone;
use crate::index::schema;

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
/// Automatically re-queries with individual terms when the top result
/// doesn't match all query terms (indicating a concentration problem).
/// Search the phrase index with full feature parity.
/// Automatically re-queries with individual terms when the top result
/// doesn't match all query terms, and falls back to file path LIKE matching.
pub fn search_phrases(
    db_path: &str,
    query: &str,
    top_n: usize,
) -> Vec<(String, f64)> {
    let mut results = _search_phrases(db_path, query, top_n);
    let raw_terms: Vec<String> = zone::extract_phrases(query);
    
    // Step 1: if top result doesn't contain all query terms in its path, try IDF re-query
    let needs_requery = if let Some((top_fp, _)) = results.first() {
        let top_lower = top_fp.to_lowercase();
        let terms_lower: Vec<String> = raw_terms.iter().map(|t| t.to_lowercase()).collect();
        !terms_lower.iter().all(|t| top_lower.contains(t.as_str()))
    } else { true };
    
    if needs_requery && raw_terms.len() >= 2 {
        if let Ok(idf_db) = Connection::open(db_path) {
            let n_docs: f64 = idf_db.query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0)).unwrap_or(1.0);
            let term_idfs: Vec<(usize, f64)> = raw_terms.iter().enumerate().map(|(i, t)| {
                let df: f64 = idf_db.query_row(
                    "SELECT COUNT(*) FROM phrase_occ po JOIN phrases p ON p.id = po.phrase_id WHERE p.phrase = ?1",
                    [t], |r| r.get(0)
                ).unwrap_or(0.0);
                (i, (if df > 0.0 { ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln() } else { 10.0_f64 }))
            }).collect();
            
            let best_idx = term_idfs.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| *i).unwrap_or(0);
            let best_idf = term_idfs.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(_, idf)| *idf).unwrap_or(0.0);
            
            if best_idf > 1.0 {
                let other_terms: Vec<String> = raw_terms.iter().enumerate()
                    .filter(|(i, _)| *i != best_idx)
                    .map(|(_, t)| t.to_lowercase()).collect();
                
                let mut reranked: Vec<(String, f64)> = _search_phrases(db_path, &raw_terms[best_idx], top_n * 5).iter()
                    .map(|(fp, score)| {
                        let count = 1.0 + other_terms.iter()
                            .filter(|ot| fp.to_lowercase().contains(ot.as_str())).count() as f64;
                        (fp.clone(), score * (1.0 + count / raw_terms.len() as f64))
                    }).collect();
                reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                reranked.truncate(top_n);
                
                if reranked.first().map(|(_, s)| *s).unwrap_or(0.0) > results.first().map(|(_, s)| *s).unwrap_or(0.0) * 1.1 {
                    results = reranked;
                }
            }
        }
    }
    
    // Step 2: always add file path LIKE matches as an independent bonus tier.
    // Scores path matches proportional to the top vocabulary score × term coverage.
    // This catches {array_list.zig, kernel.ex, janitor_test.go, gen_server.erl} etc.
    let top_vocab_score = results.first().map(|(_, s)| *s).unwrap_or(100.0);
    if let Ok(fp_db) = Connection::open(db_path) {
        for t in &raw_terms {
            let tl = t.to_lowercase();
            for variant in [&tl, &tl.replace('_', "-"), &tl.replace('-', "_")] {
                if let Ok(mut stmt) = fp_db.prepare(
                    "SELECT file_path FROM file_map WHERE LOWER(file_path) LIKE ?1 LIMIT 3"
                ) {
                    let pattern = format!("%{}%", variant);
                    if let Ok(rows) = stmt.query_map([&pattern], |r| r.get::<_, String>(0)) {
                        for row in rows.flatten() {
                            let row_lower = row.to_lowercase();
                            let term_hits = raw_terms.iter()
                                .filter(|t| row_lower.contains(&t.to_lowercase()))
                                .count() as f64;
                            let coverage = term_hits / raw_terms.len() as f64;
                            let path_score = top_vocab_score * coverage.max(0.3);
                            
                            // Boost existing results or add new ones
                            if let Some(existing) = results.iter_mut().find(|(fp, _)| fp.as_str() == row) {
                                existing.1 = existing.1.max(path_score);
                            } else {
                                results.push((row, path_score));
                            }
                        }
                    }
                }
            }
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_n);
    }
    
    results
}

fn _search_phrases(
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
        // Expand underscore-delimited: "hub_risk_report" → sub-words + adjacent pairs
        if tl.contains('_') || tl.contains('-') {
            let sub: Vec<&str> = tl.split(|c| c == '_' || c == '-').filter(|s| s.len() >= 3).collect();
            for s in &sub {
                if !search_terms.iter().any(|x| x == s) { search_terms.push(s.to_string()); }
            }
            for pair in sub.windows(2) {
                let joined = format!("{}_{}", pair[0], pair[1]);
                if joined.len() >= 4 && !search_terms.contains(&joined) { search_terms.push(joined); }
            }
        }
    }

    // Pre-compute IDF — all queries go through phrases table JOIN
    let mut idf_map: HashMap<String, f64> = HashMap::new();
    for st in &search_terms {
        let df: f64 = db.query_row(
            "SELECT COUNT(*) FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             WHERE p.phrase = ?1", [st], |r| r.get(0)
        ).unwrap_or(1.0);
        idf_map.insert(st.clone(), bm25::bm25_idf(n_docs, df.max(0.5)));
    }

    let mut file_scores: HashMap<i64, f64> = HashMap::new();
    let mut file_line_sets: HashMap<i64, HashMap<String, Vec<i32>>> = HashMap::new();
    let mut file_tiers: HashMap<i64, u8> = HashMap::new();
    let mut file_idf_sum: HashMap<i64, f64> = HashMap::new();

    // Tier 1: Exact match BM25
    for st in &search_terms {
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let mut exact_q = match db.prepare(
            "SELECT po.file_id, po.flags, po.line_nos, fs.token_len,
                    fs.unique_def_count, fs.total_def_count, fs.comment_ratio,
                    COALESCE(oc.count, 1) as effective_count
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_stats fs ON fs.file_id = po.file_id
             LEFT JOIN count_overflow oc ON oc.phrase_id = po.phrase_id AND oc.file_id = po.file_id
             WHERE p.phrase = ?1"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let rows_result = exact_q.query_map([st], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let line_blob = r.get::<_, Vec<u8>>(2)?;
            let doc_len = r.get::<_, f64>(3)?;
            let uniq_def = r.get::<_, i64>(4)?;
            let total_def = r.get::<_, i64>(5)?;
            let comment_ratio = r.get::<_, f64>(6)?;
            let overflow_count = r.get::<_, u32>(7)?;
            let f = if flags.len() >= 1 { flags[0] } else { 0 };
            let is_def = schema::unpack_is_def(f);
            let zone_int = schema::unpack_zone_int(f);
            let base_count = schema::unpack_count(f);
            let tf = if base_count >= 31 { overflow_count as f64 } else { base_count as f64 };
            let first_line = schema::unpack_line_nos(&line_blob) as i32;
            Ok((fid, tf, is_def, zone_int, doc_len, uniq_def, total_def, comment_ratio, first_line))
        }).unwrap();
        for row in rows_result.filter_map(|r| r.ok()) {
            let (fid, tf, is_def, zone_int, doc_len, uniq_def, total_def, comment_ratio, first_line) = row;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else { 1.0 };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            *file_scores.entry(fid).or_insert(0.0) += score * zone_mult * def_mult * uniq_mult * comment_mult;
            file_tiers.entry(fid).or_insert(0);
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
            if first_line > 0 {
                file_line_sets.entry(fid).or_default()
                    .entry(st.clone()).or_default()
                    .push(first_line);
            }
        }
    }

    // Tier 2: Prefix match
    for st in &search_terms {
        let pattern = format!("{}%", st);
        let idf = idf_map.get(st).copied().unwrap_or(1.0);
        let idf_rare = idf.powf(1.5) * 0.3;

        let mut prefix_q = match db.prepare(
            "SELECT po.file_id, po.flags, fs.token_len,
                    fs.unique_def_count, fs.total_def_count, fs.comment_ratio
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE p.phrase LIKE ?1 AND p.phrase != ?2"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let prefix_rows = prefix_q.query_map(params![&pattern, st], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let doc_len = r.get::<_, f64>(2)?;
            let uniq_def = r.get::<_, i64>(3)?;
            let total_def = r.get::<_, i64>(4)?;
            let comment_ratio = r.get::<_, f64>(5)?;
            let f = if flags.len() >= 1 { flags[0] } else { 0 };
            Ok((fid, schema::unpack_is_def(f), schema::unpack_zone_int(f), doc_len, uniq_def, total_def, comment_ratio))
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
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
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
            "SELECT po.file_id, po.flags, fs.token_len,
                    fs.comment_ratio
             FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             JOIN file_stats fs ON fs.file_id = po.file_id
             WHERE p.phrase LIKE ?1 AND p.phrase NOT LIKE ?2 AND p.phrase != ?3
             LIMIT 200"
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let sub_rows = sub_q.query_map(params![&pattern, &excl_prefix, st], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let doc_len = r.get::<_, f64>(2)?;
            let comment_ratio = r.get::<_, f64>(3)?;
            let f = if flags.len() >= 1 { flags[0] } else { 0 };
            Ok((fid, schema::unpack_is_def(f), schema::unpack_zone_int(f), doc_len, comment_ratio))
        }).unwrap();
        for row in sub_rows.filter_map(|r| r.ok()) {
            let (fid, is_def, zone_int, doc_len, comment_ratio) = row;
            let tf = 1.0;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def { 2 => 8.0, 1 => 5.0, -1 => 2.0, _ => 1.0 };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            let contrib = score * zone_mult * def_mult * comment_mult * idf_rare;
            *file_scores.entry(fid).or_insert(0.0) += contrib;
            file_tiers.entry(fid).or_insert(2);
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
        }
    }

    // Concentration bonus: files matching high-IDF terms get a boost.
    // Low-IDF terms (like 'test' appearing in 50%+ of files) contribute less.
    let total_idf_sum: f64 = search_terms.iter()
        .filter_map(|st| idf_map.get(st.as_str()))
        .sum();
    if total_idf_sum > 0.0 && search_terms.len() > 1 {
        for (fid, score) in file_scores.iter_mut() {
            let matched_sum = file_idf_sum.get(fid).copied().unwrap_or(0.0);
            let concentration = (matched_sum.min(total_idf_sum)) / total_idf_sum;
            *score *= 1.0 + concentration * concentration;
        }
    }

    // Sort by base score
    let mut scored: Vec<(i64, f64)> = file_scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Collect top candidates
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

    // Module cluster bonus
    let mut module_scores: HashMap<String, f64> = HashMap::new();
    for (fid, base_score) in &scored[..candidate_count.min(20)] {
        if let Some(fp) = fp_map.get(fid) {
            let mod_path = file_module(fp);
            *module_scores.entry(mod_path).or_insert(0.0) += base_score;
        }
    }

    // Apply all multipliers
    let mut results: Vec<(String, f64)> = Vec::new();
    for (fid, base_score) in &scored[..candidate_count] {
        if let Some(fp) = fp_map.get(fid) {
            let mut final_score = *base_score;
            final_score *= path_role_mult(fp);
            final_score *= filename_boost(fp, &search_terms);
            let mod_path = file_module(fp);
            if let Some(mod_score) = module_scores.get(&mod_path) {
                if *mod_score > 0.0 {
                    final_score *= 1.0 + (mod_score / n_docs).min(0.1);
                }
            }
            if let Some(line_sets) = file_line_sets.get(fid) {
                let lines: Vec<Vec<i32>> = line_sets.values().cloned().collect();
                if !lines.is_empty() {
                    final_score *= proximity_mult(&lines, 10);
                }
            }
            results.push((fp.clone(), final_score));
        }
    }

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(top_n);

    db.close().ok();
    results
}
