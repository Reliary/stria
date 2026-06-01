use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::index::schema;
use crate::zone;

/// BM25 scoring for phrase search.
pub mod bm25;

/// Line-number proximity bonus.
pub mod proximity;

/// Path-role multiplier: /src/ = 2.0, tests = 0.6, deps/vendor = 0.3
/// Normalizes \ to / for cross-platform comparison.
fn path_role_mult(file_path: &str) -> f64 {
    let fp = file_path.replace('\\', "/");
    if fp.contains("/src/") || fp.starts_with("src/") {
        return 2.0;
    }
    if fp.contains("/test")
        || fp.contains("/spec")
        || fp.starts_with("test")
        || fp.starts_with("spec")
    {
        return 0.6;
    }
    if fp.contains("/deps/")
        || fp.contains("/vendor/")
        || fp.starts_with("vendor/")
        || fp.starts_with("deps/")
    {
        return 0.3;
    }
    if fp.contains("/scripts/") || fp.starts_with("scripts/") {
        return 0.6;
    }
    if fp.ends_with(".md") || fp.ends_with(".rst") || fp.ends_with(".txt") {
        return 0.25;
    }
    1.0
}

/// Filename coverage multiplier: files whose path contains more query terms get a boost.
/// This is principled — filenames are the most compact, intentional identifiers for code.
fn filename_coverage(file_path: &str, _search_terms: &[String], raw_terms: &[String]) -> f64 {
    let fp_lower = file_path.to_lowercase();
    let mut matched = 0usize;
    for t in raw_terms {
        let tl = t.to_lowercase();
        if fp_lower.contains(&tl)
            || fp_lower.contains(&tl.replace('_', "-"))
            || fp_lower.contains(&tl.replace('-', "_"))
        {
            matched += 1;
        }
    }
    if matched == 0 || raw_terms.is_empty() {
        return 1.0;
    }
    1.0 + matched as f64 / raw_terms.len() as f64
}

/// Unpack first_line from 4-byte little-endian BLOB
#[allow(dead_code)]
fn unpack_first_line(blob: &[u8]) -> Option<i32> {
    if blob.len() < 4 {
        return None;
    }
    Some(i32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]))
}

/// Proximity bonus from first_line entries.
fn proximity_mult(line_sets: &[Vec<i32>], max_gap: i32) -> f64 {
    if line_sets.len() < 2 {
        return 1.0;
    }
    let mut min_dist = i32::MAX;
    for i in 0..line_sets.len() {
        for j in (i + 1)..line_sets.len() {
            for &a in &line_sets[i] {
                for &b in &line_sets[j] {
                    let dist = if a > b { a - b } else { b - a };
                    if dist < min_dist {
                        min_dist = dist;
                    }
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

/// Search the phrase index.
/// Automatically re-queries with individual terms when the top result
/// doesn't span all query terms (indicating a concentration problem).
pub fn search_phrases(db_path: &str, query: &str, top_n: usize) -> Vec<(String, f64)> {
    let mut results = _search_phrases(db_path, query, top_n);
    let raw_terms: Vec<String> = zone::extract_phrases(query);

    if raw_terms.len() >= 2 {
        let needs_requery = if let Some((top_fp, _)) = results.first() {
            let terms_lower: Vec<String> = raw_terms.iter().map(|t| t.to_lowercase()).collect();
            let top_lower = top_fp.to_lowercase();
            !terms_lower.iter().all(|t| top_lower.contains(t.as_str()))
        } else {
            true
        };

        if needs_requery {
            if let Ok(idf_db) = Connection::open(db_path) {
                let n_docs: f64 = idf_db
                    .query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0))
                    .unwrap_or(1.0);
                let term_idfs: Vec<(usize, f64)> = raw_terms.iter().enumerate().map(|(i, t)| {
                    let df: f64 = idf_db.query_row(
                        "SELECT COUNT(*) FROM phrase_occ po JOIN phrases p ON p.id = po.phrase_id WHERE p.phrase = ?1",
                        [t], |r| r.get(0)
                    ).unwrap_or(0.0);
                    (i, (if df > 0.0 { ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln() } else { 10.0_f64 }))
                }).collect();

                let best_idx = term_idfs
                    .iter()
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| *i)
                    .unwrap_or(0);

                let other_terms: Vec<String> = raw_terms
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != best_idx)
                    .map(|(_, t)| t.to_lowercase())
                    .collect();

                let mut reranked: Vec<(String, f64)> =
                    _search_phrases(db_path, &raw_terms[best_idx], top_n * 5)
                        .iter()
                        .map(|(fp, score)| {
                            let count = 1.0
                                + other_terms
                                    .iter()
                                    .filter(|ot| fp.to_lowercase().contains(ot.as_str()))
                                    .count() as f64;
                            (fp.clone(), score * (1.0 + count / raw_terms.len() as f64))
                        })
                        .collect();
                reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                reranked.truncate(top_n);

                // Use reranked if it changes the top result
                if reranked.first().map(|(fp, _)| fp.as_str())
                    != results.first().map(|(fp, _)| fp.as_str())
                {
                    results = reranked;
                }
            }
        }
    }

    results
}

fn _search_phrases(db_path: &str, query: &str, top_n: usize) -> Vec<(String, f64)> {
    let db = match Connection::open(db_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let n_docs: f64 = db
        .query_row("SELECT COUNT(*) FROM file_map", [], |r| r.get(0))
        .unwrap_or(1.0);
    let avgdl: f64 = db
        .query_row("SELECT value FROM meta WHERE key='avgdl'", [], |r| r.get(0))
        .unwrap_or(100.0);

    let raw_terms: Vec<String> = zone::extract_phrases(query);
    if raw_terms.is_empty() {
        return vec![];
    }
    if raw_terms.len() > 100 {
        eprintln!(
            "eh:warn: search query has {} terms, truncating to 100",
            raw_terms.len()
        );
    }
    let raw_terms: Vec<String> = raw_terms.into_iter().take(100).collect();

    // Build search_terms: original casing + stems
    let mut search_terms: Vec<String> = Vec::new();
    for t in &raw_terms {
        if !search_terms.contains(t) {
            search_terms.push(t.clone());
        }
    }

    // Pre-compute DF estimates for raw query terms. Used for two purposes:
    // 1. DF-gated underscore expansion (known compounds NOT split into sub-words)
    // 2. DF-gated case-variant suppression (lowercase NOT added when original
    //    casing is in DB, preventing double-scoring through prefix tier)
    let mut known_raw: HashMap<String, bool> = HashMap::new();
    for t in &raw_terms {
        let df_est: i64 = db.query_row(
            "SELECT COUNT(*) FROM phrase_occ po JOIN phrases p ON p.id = po.phrase_id WHERE p.phrase = ?1",
            [t], |r| r.get(0)
        ).unwrap_or(0);
        known_raw.insert(t.to_lowercase(), df_est > 0);
    }

    for t in &raw_terms {
        let tl = t.to_lowercase();
        let is_known = known_raw.get(&tl).copied().unwrap_or(false);

        // Case variant: only add lowercase if original casing is NOT in DB.
        // Prevents ArrayList (df=200) and arraylist (df=2) from being independent
        // search terms — the lowercase prefix tier would double-score files already
        // matched by the exact tier.
        if !is_known && !search_terms.contains(&tl) {
            search_terms.push(tl.clone());
        }
        if tl.ends_with("ing") && tl.len() > 4 {
            let s = tl[..tl.len() - 3].to_string();
            if !search_terms.contains(&s) {
                search_terms.push(s);
            }
        } else if tl.ends_with('s') && tl.len() > 3 {
            let s = tl[..tl.len() - 1].to_string();
            if !search_terms.contains(&s) {
                search_terms.push(s);
            }
        }
        if tl.ends_with("ed") && tl.len() > 4 {
            let s = tl[..tl.len() - 2].to_string();
            if !search_terms.contains(&s) {
                search_terms.push(s);
            }
        }
        if tl.ends_with("tion") && tl.len() > 5 {
            let s = tl[..tl.len() - 4].to_string();
            if !search_terms.contains(&s) {
                search_terms.push(s);
            }
        }

        // Underscore sub-word expansion — only for unknown (DF=0) compound terms.
        // Known terms like gen_server (DF=331) must NOT be split into gen+server
        // because the prefix gen% matches 61 unrelated phrases in consumer files.
        if !is_known && (tl.contains('_') || tl.contains('-')) {
            let sub: Vec<&str> = tl.split(['_', '-']).filter(|s| s.len() >= 3).collect();
            for s in &sub {
                if !search_terms.iter().any(|x| x == s) {
                    search_terms.push(s.to_string());
                }
            }
            for pair in sub.windows(2) {
                let joined = format!("{}_{}", pair[0], pair[1]);
                if joined.len() >= 4 && !search_terms.contains(&joined) {
                    search_terms.push(joined);
                }
            }
        }
    }

    // Pre-compute IDF — all queries go through phrases table JOIN
    let mut idf_map: HashMap<String, f64> = HashMap::new();
    for st in &search_terms {
        let df: f64 = db
            .query_row(
                "SELECT COUNT(*) FROM phrase_occ po
             JOIN phrases p ON p.id = po.phrase_id
             WHERE p.phrase = ?1",
                [st],
                |r| r.get(0),
            )
            .unwrap_or(1.0);
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
             WHERE p.phrase = ?1",
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
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            let is_def = schema::unpack_is_def(f);
            let zone_int = schema::unpack_zone_int(f);
            let base_count = schema::unpack_count(f);
            let tf = if base_count >= 31 {
                overflow_count as f64
            } else {
                base_count as f64
            };
            let first_line = schema::unpack_line_nos(&line_blob) as i32;
            Ok((
                fid,
                tf,
                is_def,
                zone_int,
                doc_len,
                uniq_def,
                total_def,
                comment_ratio,
                first_line,
            ))
        });
        let rows_result = match rows_result {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("eh:warn: search: exact query_map failed: {}", e);
                continue;
            }
        };
        for row in rows_result.filter_map(|r| r.ok()) {
            let (
                fid,
                tf,
                is_def,
                zone_int,
                doc_len,
                uniq_def,
                total_def,
                comment_ratio,
                first_line,
            ) = row;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def {
                2 => 8.0,
                1 => 5.0,
                -1 => 2.0,
                _ => 1.0,
            };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else {
                1.0
            };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            *file_scores.entry(fid).or_insert(0.0) +=
                score * zone_mult * def_mult * uniq_mult * comment_mult;
            file_tiers.entry(fid).or_insert(0);
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
            if first_line > 0 {
                file_line_sets
                    .entry(fid)
                    .or_default()
                    .entry(st.clone())
                    .or_default()
                    .push(first_line);
            }
        }
    }

    // Tier 2: Prefix match — MAX aggregation per (file, term).
    // SUM would let files with many prefix variants (e.g. 61 gen_* phrases
    // in asn1ct_constructed_per.erl) drown out files with few variants.
    // MAX ensures each term contributes at most once per file — standard IR
    // practice for expanded query terms with correlated matches.
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
             WHERE p.phrase LIKE ?1 AND p.phrase != ?2
             LIMIT 5000",
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let mut max_per_file: HashMap<i64, f64> = HashMap::new();
        let prefix_result = prefix_q.query_map(params![&pattern, st], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let doc_len = r.get::<_, f64>(2)?;
            let uniq_def = r.get::<_, i64>(3)?;
            let total_def = r.get::<_, i64>(4)?;
            let comment_ratio = r.get::<_, f64>(5)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            Ok((
                fid,
                schema::unpack_is_def(f),
                schema::unpack_zone_int(f),
                doc_len,
                uniq_def,
                total_def,
                comment_ratio,
            ))
        });
        let prefix_rows = match prefix_result {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("eh:warn: search: prefix query_map failed: {}", e);
                continue;
            }
        };
        for row in prefix_rows.filter_map(|r| r.ok()) {
            let (fid, is_def, zone_int, doc_len, uniq_def, total_def, comment_ratio) = row;
            let tf = 1.0;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def {
                2 => 8.0,
                1 => 5.0,
                -1 => 2.0,
                _ => 1.0,
            };
            let uniq_mult = if total_def > 0 && uniq_def > 0 {
                1.0 + (uniq_def as f64 / total_def as f64) * 0.5
            } else {
                1.0
            };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            let contrib = score * zone_mult * def_mult * uniq_mult * comment_mult * idf_rare;
            let current = max_per_file.entry(fid).or_insert(0.0);
            if contrib > *current {
                *current = contrib;
            }
        }
        for (fid, max_contrib) in max_per_file {
            *file_scores.entry(fid).or_insert(0.0) += max_contrib;
            file_tiers.entry(fid).or_insert(1);
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
        }
    }

    // Tier 3: Substring match — same MAX aggregation per (file, term).
    for st in &search_terms {
        if st.len() < 4 {
            continue;
        }
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
              LIMIT 5000",
        ) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let mut max_per_file: HashMap<i64, f64> = HashMap::new();
        let sub_result = sub_q.query_map(params![&pattern, &excl_prefix, st], |r| {
            let fid = r.get::<_, i64>(0)?;
            let flags = r.get::<_, Vec<u8>>(1)?;
            let doc_len = r.get::<_, f64>(2)?;
            let comment_ratio = r.get::<_, f64>(3)?;
            let f = if !flags.is_empty() { flags[0] } else { 0 };
            Ok((
                fid,
                schema::unpack_is_def(f),
                schema::unpack_zone_int(f),
                doc_len,
                comment_ratio,
            ))
        });
        let sub_rows = match sub_result {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("eh:warn: search: substring query_map failed: {}", e);
                continue;
            }
        };
        for row in sub_rows.filter_map(|r| r.ok()) {
            let (fid, is_def, zone_int, doc_len, comment_ratio) = row;
            let tf = 1.0;
            let score = bm25::bm25_score(idf, tf, doc_len, avgdl);
            let zone_mult = if zone_int == 0 { 2.0 } else { 0.25 };
            let def_mult = match is_def {
                2 => 8.0,
                1 => 5.0,
                -1 => 2.0,
                _ => 1.0,
            };
            let comment_mult = (1.0 - comment_ratio * 0.5).max(0.5);
            let contrib = score * zone_mult * def_mult * comment_mult * idf_rare;
            let current = max_per_file.entry(fid).or_insert(0.0);
            if contrib > *current {
                *current = contrib;
            }
        }
        for (fid, max_contrib) in max_per_file {
            *file_scores.entry(fid).or_insert(0.0) += max_contrib;
            file_tiers.entry(fid).or_insert(2);
            *file_idf_sum.entry(fid).or_insert(0.0) += idf;
        }
    }

    // Concentration bonus: files matching high-IDF terms get a boost.
    // Low-IDF terms (like 'test' appearing in 50%+ of files) contribute less.
    let total_idf_sum: f64 = search_terms
        .iter()
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
    let top_fids: Vec<i64> = scored
        .iter()
        .take(candidate_count)
        .map(|(fid, _)| *fid)
        .collect();

    // Resolve file paths
    let mut fp_map: HashMap<i64, String> = HashMap::new();
    if !top_fids.is_empty() {
        let placeholders: Vec<String> = top_fids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT id, file_path FROM file_map WHERE id IN ({})",
            placeholders.join(",")
        );
        if let Ok(mut stmt) = db.prepare(&sql) {
            let params: Vec<&dyn rusqlite::types::ToSql> = top_fids
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            if let Ok(rows) = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    fp_map.insert(row.0, row.1);
                }
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
            final_score *= filename_coverage(fp, &search_terms, &raw_terms);
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
