mod schema;
mod extract;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use sha2::{Sha256, Digest};

use crate::zone::{self, COMMON_KEYWORDS};

/// Packed occurrence entry: [is_def, zone_code, count]
type OccEntry = [i32; 3];

/// Build the phrase index for a repo.
/// Incremental: uses SHA-256 digest cache to skip unchanged files.
pub fn build_phrase_index(repo_path: &str, out_dir: &Path, verbose: bool) -> Result<usize, String> {
    let repo = PathBuf::from(repo_path);
    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir: {}", e))?;

    let db_path = out_dir.join("phrases.sqlite");
    let digest_path = out_dir.join("digest_cache.json");

    // Load digest cache
    let mut digest_cache: HashMap<String, String> = HashMap::new();
    if digest_path.exists() {
        if let Ok(s) = fs::read_to_string(&digest_path) {
            if let Ok(v) = serde_json::from_str::<HashMap<String, String>>(&s) {
                digest_cache = v;
            }
        }
    }

    // Collect files
    let files = collect_source_files(&repo);
    if files.is_empty() { return Ok(0); }

    // Determine changed files
    let mut changed_files: Vec<(String, String)> = Vec::new(); // (rel_path, digest)
    let mut all_digests: HashMap<String, String> = HashMap::new();

    for rel in &files {
        let fpath = repo.join(rel);
        let dig = sha256_file(&fpath).unwrap_or_default();
        all_digests.insert(rel.clone(), dig.clone());
        if digest_cache.get(rel) != Some(&dig) {
            changed_files.push((rel.clone(), dig));
        }
    }

    // If nothing changed, return early
    if !changed_files.is_empty() || !db_path.exists() || digest_cache.is_empty() {
        // Proceed with build
    } else {
        if verbose { eprintln!("Phrase index up to date: 0 changed files"); }
        return count_phrases(&db_path);
    }

    let db = Connection::open(&db_path).map_err(|e| format!("db: {}", e))?;
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;"
    ).map_err(|e| format!("pragma: {}", e))?;

    let is_full_rebuild = changed_files.len() as f64 >= files.len() as f64 * 0.3 || !db_path.exists() || digest_cache.is_empty();

    // Sequential or parallel extraction
    let source_files: Vec<String> = if is_full_rebuild {
        files.clone()
    } else {
        changed_files.iter().map(|(rel, _)| rel.clone()).collect()
    };

    // Accumulators
    let occs = Arc::new(Mutex::new(HashMap::<(String, i64), OccEntry>::new()));
    let left_ctx = Arc::new(Mutex::new(HashMap::<String, HashMap<String, u32>>::new()));
    let phrase_df = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    let file_token_lens = Arc::new(Mutex::new(HashMap::<i64, u32>::new()));
    let file_content_lens = Arc::new(Mutex::new(HashMap::<i64, usize>::new()));
    let file_comment_ratios = Arc::new(Mutex::new(HashMap::<i64, f64>::new()));
    let global_total_phrases = Arc::new(Mutex::new(0u32));
    let global_file_map = Arc::new(Mutex::new(Vec::<(i64, String)>::new()));

    let chunk_size = (source_files.len() / rayon::current_num_threads().max(1)).max(1);
    let chunks: Vec<&[String]> = source_files.chunks(chunk_size).collect();

    // Build file_map for new files
    let mut file_map: Vec<(i64, String)> = Vec::new();
    if is_full_rebuild {
        if let Ok(_) = db.execute("DELETE FROM file_map", []) {}
        for (i, rel) in files.iter().enumerate() {
            file_map.push(((i + 1) as i64, rel.clone()));
        }
    } else {
        // Load existing IDs, add new
        let mut existing: HashMap<String, i64> = HashMap::new();
        if let Ok(mut get_files_q) = db.prepare("SELECT id, file_path FROM file_map") {
            if let Ok(rows) = get_files_q.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    existing.insert(row.1, row.0);
                }
            }
        }
        // Get max ID
        let max_id: i64 = db.query_row("SELECT COALESCE(MAX(id), 0) FROM file_map", [], |r| r.get(0)).unwrap_or(0);
        let mut next_id = max_id + 1;
        for rel in &files {
            if let Some(id) = existing.get(rel) {
                file_map.push((*id, rel.clone()));
            } else {
                file_map.push((next_id, rel.clone()));
                next_id += 1;
            }
        }
        // Remove old entries for changed files
        for (rel, _) in &changed_files {
            if let Some(fid) = existing.get(rel) {
                if let Ok(_) = db.execute("DELETE FROM phrase_occ WHERE file_id = ?1", [fid]) {}
                if let Ok(_) = db.execute("DELETE FROM file_stats WHERE file_id = ?1", [fid]) {}
            }
        }
    }

    // Insert file_map entries
    for (fid, rel) in &file_map {
        let _ = db.execute("INSERT OR REPLACE INTO file_map (id, file_path) VALUES (?1, ?2)", params![fid, rel]);
    }

    // Determine which files to extract
    let extract_set: HashSet<&str> = if is_full_rebuild {
        files.iter().map(|s| s.as_str()).collect()
    } else {
        changed_files.iter().map(|(rel, _)| rel.as_str()).collect()
    };

    let repo_arc = Arc::new(repo);
    let file_map_arc = Arc::new(file_map.clone());

    // Parallel extraction
    source_files.par_chunks(chunk_size).for_each(|chunk| {
        let repo = repo_arc.clone();
        let file_map_ref = file_map_arc.clone();
        let occs_local = Arc::clone(&occs);
        let left_ctx_local = Arc::clone(&left_ctx);
        let phrase_df_local = Arc::clone(&phrase_df);
        let ftl_local = Arc::clone(&file_token_lens);
        let fcl_local = Arc::clone(&file_content_lens);
        let fcr_local = Arc::clone(&file_comment_ratios);
        let total_local = Arc::clone(&global_total_phrases);
        let fm_local = Arc::clone(&global_file_map);

        // Build file_id map for this chunk
        let file_id_map: HashMap<&str, i64> = file_map_ref.iter().map(|(id, rel)| (rel.as_str(), *id)).collect();

        for rel in chunk {
            let fpath = repo.join(rel);
            let text = match fs::read_to_string(&fpath) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let fid = match file_id_map.get(rel.as_str()) {
                Some(id) => *id,
                None => continue,
            };

            let lines: Vec<&str> = text.lines().collect();
            let content_len = text.len();
            let mut file_phrases_fast = HashSet::<String>::new();
            let mut token_len = 0u32;
            let mut comment_lines = 0u32;
            let mut phrase_lc: HashMap<String, HashMap<String, u32>> = HashMap::new();
            let mut phrase_df_local_map: HashMap<String, u32> = HashMap::new();

            for (line_no, line) in lines.iter().enumerate() {
                let s = line.trim();
                if s.starts_with('#') || s.starts_with("//") || s.starts_with("/*")
                    || s.starts_with('*') || s.starts_with("<!--") || s.starts_with('>')
                {
                    comment_lines += 1;
                }
                let zone = zone::line_zone(line);
                for m in crate::zone::PHRASE_RE.find_iter(line) {
                    let phrase = m.as_str();
                    if COMMON_KEYWORDS.contains(&phrase) { continue; }
                    let pl = phrase.to_lowercase();

                    let is_def = if zone == 0 && crate::zone::is_definition(phrase, line) { 1 } else { 0 };

                    // Track df
                    *phrase_df_local_map.entry(pl.clone()).or_insert(0) += 1;

                    // LCEP tracking (simplified: track first 5)
                    if !phrase_lc.contains_key(&pl) && phrase_df_local_map.get(&pl).copied().unwrap_or(0) <= 5 {
                        // Find left-context word
                        let before = &line[..m.start()].trim();
                        if let Some(lc) = before.split_whitespace().last() {
                            let lc = lc.chars().take(30).collect::<String>().to_lowercase();
                            phrase_lc.entry(pl.clone()).or_default().entry(lc).and_modify(|c| *c += 1).or_insert(1);
                        }
                    }

                    // Accumulate
                    let key = (phrase.to_string(), fid);
                    let mut occs_map = occs_local.lock().unwrap();
                    let entry = occs_map.entry(key).or_insert([is_def, zone as i32, 0]);
                    if is_def > entry[0] { entry[0] = is_def; }
                    if zone == 0 { entry[1] = 0; }
                    entry[2] += 1;
                    drop(occs_map);

                    if file_phrases_fast.insert(phrase.to_string()) {
                        token_len += 1;
                    }
                }
            }

            // Thread-local accumulators
            {
                let mut left = left_ctx_local.lock().unwrap();
                for (k, v) in phrase_lc {
                    let entry = left.entry(k).or_default();
                    for (ck, cv) in v {
                        *entry.entry(ck).or_insert(0) += cv;
                    }
                }
            }
            {
                let mut pdf = phrase_df_local.lock().unwrap();
                for (k, v) in phrase_df_local_map {
                    *pdf.entry(k).or_insert(0) += v;
                }
            }
            {
                let mut ftl = ftl_local.lock().unwrap();
                ftl.insert(fid, token_len);
            }
            {
                let mut fcl = fcl_local.lock().unwrap();
                fcl.insert(fid, content_len);
            }
            {
                let mut fcr = fcr_local.lock().unwrap();
                let ratio = if lines.is_empty() { 0.0 } else { comment_lines as f64 / lines.len() as f64 };
                fcr.insert(fid, ratio);
            }
            {
                let mut tl = total_local.lock().unwrap();
                *tl += token_len;
            }
        }
    });

    // Merge and insert
    let mut rows: Vec<(String, i64, i32, String, i32)> = Vec::new();
    let occs_map = occs.lock().unwrap();
    for ((phrase, fid), [is_def, zone_code, count]) in occs_map.iter() {
        let zone_str = if *zone_code == 0 { "code" } else { "prose" };
        rows.push((phrase.clone(), *fid, *is_def, zone_str.to_string(), *count));
    }
    drop(occs_map);

    let total_phrases: u32 = {
        let tl = global_total_phrases.lock().unwrap();
        *tl
    };

    // Insert into SQLite
    let tx = db.unchecked_transaction().map_err(|e| format!("tx: {}", e))?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO phrase_occ (phrase, file_id, is_def, zone, count, line_nos)
             VALUES (?1, ?2, ?3, ?4, ?5, '')"
        ).map_err(|e| format!("prepare: {}", e))?;

        for (phrase, fid, is_def, zone_str, count) in &rows {
            stmt.execute(params![phrase, fid, is_def, zone_str, count])
                .map_err(|e| format!("insert: {}", e))?;
        }
    }
    tx.commit().map_err(|e| format!("commit: {}", e))?;

    // Insert file_stats
    {
        let ftl = file_token_lens.lock().unwrap();
        let fcl = file_content_lens.lock().unwrap();
        let fcr = file_comment_ratios.lock().unwrap();
        let mut stmt = db.prepare(
            "INSERT OR REPLACE INTO file_stats (file_id, token_len, content_len, comment_ratio)
             VALUES (?1, ?2, ?3, ?4)"
        ).map_err(|e| format!("prepare stats: {}", e))?;
        for (fid, _) in &file_map {
            let tl = ftl.get(fid).copied().unwrap_or(0) as f64;
            let cl = fcl.get(fid).copied().unwrap_or(0) as f64;
            let cr = fcr.get(fid).copied().unwrap_or(0.0);
            stmt.execute(params![fid, tl, cl, cr])
                .map_err(|e| format!("stats: {}", e))?;
        }
    }

    // Definition uniqueness ratio
    db.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS phrase_df AS SELECT phrase, COUNT(*) AS df FROM phrase_occ GROUP BY phrase;
         UPDATE file_stats SET unique_def_count = (
             SELECT COUNT(*) FROM phrase_occ po
             JOIN phrase_df ON phrase_df.phrase = po.phrase
             WHERE po.file_id = file_stats.file_id AND po.is_def = 1 AND phrase_df.df = 1
         );
         UPDATE file_stats SET total_def_count = (
             SELECT COUNT(*) FROM phrase_occ po WHERE po.file_id = file_stats.file_id AND po.is_def = 1
         );
         DROP TABLE phrase_df;"
    ).map_err(|e| format!("uniqueness: {}", e))?;

    // Compute avgdl
    let ftl = file_token_lens.lock().unwrap();
    let sum_len: u32 = ftl.values().sum();
    let n_files = ftl.len().max(1_usize);
    let avgdl = sum_len as f64 / n_files as f64;
    drop(ftl);

    db.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('avgdl', ?1)",
        [avgdl],
    ).map_err(|e| format!("meta: {}", e))?;

    // ANALYZE
    db.execute_batch("ANALYZE").ok();

    // Write digest cache
    if let Ok(s) = serde_json::to_string(&all_digests) {
        fs::write(&digest_path, s).ok();
    }

    let _ = db.close();

    if verbose {
        eprintln!("Phrase index built: {} files, {} phrases", files.len(), rows.len());
    }

    Ok(total_phrases as usize)
}

fn collect_source_files(repo: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let skip_dirs: HashSet<&str> = [".git", "node_modules", "vendor", "dist", "build", "target",
        "__pycache__", ".horizon", ".reliary", "third_party", "deps"].into();
    let skip_prefixes: HashSet<&str> = ["scripts/", "tools/", "examples/", "testdata/",
        "generated/", "artifacts/", "migrations/"].into();

    fn walk(dir: &Path, base: &Path, skip_dirs: &HashSet<&str>,
            skip_prefixes: &HashSet<&str>, files: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
                if skip_prefixes.iter().any(|p| rel.starts_with(p)) { continue; }
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if skip_dirs.contains(name.as_str()) { continue; }
                    walk(&path, base, skip_dirs, skip_prefixes, files);
                } else if path.is_file() {
                    if let Some(ext) = path.extension() {
                        let ext = ext.to_string_lossy().to_lowercase();
                        if matches!(ext.as_str(), "ts"|"tsx"|"js"|"jsx"|"go"|"py"|"rs"|"c"|"h"
                            |"cpp"|"hpp"|"java"|"kt"|"swift"|"rb"|"php"|"scala"|"clj"
                            |"erl"|"hrl"|"ex"|"exs"|"zig"|"nim"|"nix"|"tcl"
                            |"elm"|"hs"|"ml"|"mli"|"fs"|"v"|"purs"
                            |"md"|"rst"|"txt"|"yaml"|"yml"|"toml"|"json"|"html"|"css"
                            |"sh"|"bash"|"zsh"|"fish"|"makefile"|"dockerfile"
                            |"sql"|"graphql"|"proto"|"lua"
                        ) {
                            files.push(rel);
                        }
                    }
                }
            }
        }
    }

    walk(repo, repo, &skip_dirs, &skip_prefixes, &mut files);
    files.sort();
    files
}

pub fn count_phrases(db_path: &Path) -> Result<usize, String> {
    if let Ok(db) = Connection::open(db_path) {
        if let Ok(n) = db.query_row("SELECT COUNT(DISTINCT phrase) FROM phrase_occ", [], |r| r.get::<_, i64>(0)) {
            return Ok(n as usize);
        }
    }
    Ok(0)
}

fn sha256_file(path: &Path) -> Result<String, io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}
