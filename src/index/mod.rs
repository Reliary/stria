mod schema;
mod extract;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use sha2::{Sha256, Digest};
use fxhash::{FxHashMap, FxHasher};

use crate::zone::{self, COMMON_KEYWORDS};

/// Fast hash of a phrase string to u64. Collision risk for 85K phrases: ~1 in 2^32.
/// This eliminates String allocations in the 200K-entry occs accumulator.
fn phrase_hash(s: &str) -> u64 {
    let mut h = FxHasher::default();
    s.hash(&mut h);
    h.finish()
}

/// Packed occurrence entry: [is_def, zone_code, count, first_line_no]
type OccEntry = [i32; 4];

/// Thread-local result from a parallel extraction worker.
struct WorkerResult {
    occs: FxHashMap<(u64, i64), OccEntry>,
    phrase_strings: HashMap<u64, String>,
    left_ctx: HashMap<String, HashMap<String, u32>>,
    phrase_df: HashMap<u64, u32>,
    token_lens: HashMap<i64, u32>,
    content_lens: HashMap<i64, usize>,
    comment_ratios: HashMap<i64, f64>,
    total_phrases: u32,
}

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

    // Determine changed files: skip SHA-256 on first build (all files already changed)
    let mut changed_files: Vec<(String, String)> = Vec::new();
    let mut all_digests: HashMap<String, String> = HashMap::new();

    if digest_cache.is_empty() {
        // First build — every file is changed, skip expensive SHA-256 per file
        for rel in &files {
            all_digests.insert(rel.clone(), String::new());
            changed_files.push((rel.clone(), String::new()));
        }
    } else {
        for rel in &files {
            let fpath = repo.join(rel);
            let dig = sha256_file(&fpath).unwrap_or_default();
            all_digests.insert(rel.clone(), dig.clone());
            if digest_cache.get(rel) != Some(&dig) {
                changed_files.push((rel.clone(), dig));
            }
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
    schema::create_schema(&db).map_err(|e| format!("schema: {}", e))?;
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;
         PRAGMA mmap_size = 268435456;"
    ).map_err(|e| format!("pragma: {}", e))?;

    let is_full_rebuild = changed_files.len() as f64 >= files.len() as f64 * 0.3 || !db_path.exists() || digest_cache.is_empty();

    // Sequential or parallel extraction
    let source_files: Vec<String> = if is_full_rebuild {
        files.clone()
    } else {
        changed_files.iter().map(|(rel, _)| rel.clone()).collect()
    };

    // Accumulators — plain maps, merged from WorkerResult after parallel extraction
    let mut occs: FxHashMap<(u64, i64), OccEntry> = FxHashMap::default();
    let mut phrase_strings: HashMap<u64, String> = HashMap::new();
    let mut phrase_left_ctx: HashMap<String, HashMap<String, u32>> = HashMap::new();
    let mut phrase_df_counter: HashMap<u64, u32> = HashMap::new();
    let mut file_token_lens: HashMap<i64, u32> = HashMap::new();
    let mut file_content_lens: HashMap<i64, usize> = HashMap::new();
    let mut file_comment_ratios: HashMap<i64, f64> = HashMap::new();
    let mut global_total_phrases: u32 = 0;

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
    {
        let tx = db.unchecked_transaction().map_err(|e| format!("tx file_map: {}", e))?;
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO file_map (id, file_path) VALUES (?1, ?2)")
            .map_err(|e| format!("prepare file_map: {}", e))?;
        for (fid, rel) in &file_map {
            stmt.execute(params![fid, rel]).map_err(|e| format!("insert file_map: {}", e))?;
        }
        drop(stmt);
        tx.commit().map_err(|e| format!("commit file_map: {}", e))?;
    }
    fs::write("/tmp/eh_progress.txt", "file_map done\n").ok();

    // Determine which files to extract
    let extract_set: HashSet<&str> = if is_full_rebuild {
        files.iter().map(|s| s.as_str()).collect()
    } else {
        changed_files.iter().map(|(rel, _)| rel.as_str()).collect()
    };

    // Parallel extraction — single pass over ALL source files.
    // Rayon handles work-stealing across all available cores.
    let n_files = source_files.len();
    let chunk_size = (n_files / rayon::current_num_threads().max(1)).max(1);
    fs::write("/tmp/eh_progress.txt", format!("extraction: {} files, {} chunks, {} threads\n", n_files, (n_files + chunk_size - 1) / chunk_size, rayon::current_num_threads())).ok();

    let repo_arc = std::sync::Arc::new(repo);
    let file_map_arc = std::sync::Arc::new(file_map.clone());
    let file_id_map: HashMap<&str, i64> = file_map.iter().map(|(id, rel)| (rel.as_str(), *id)).collect();
    let file_id_map_arc = std::sync::Arc::new(file_id_map);

    let results: Vec<WorkerResult> = source_files.par_chunks(chunk_size)
        .filter_map(|chunk| -> Option<WorkerResult> {
            if chunk.is_empty() { return None; }
            let repo = repo_arc.as_path();
            let file_id_map = file_id_map_arc.as_ref();
            let mut local_occs = FxHashMap::default();
            let mut local_phrase_strings: HashMap<u64, String> = HashMap::new();
            let mut local_left_ctx: HashMap<String, HashMap<String, u32>> = HashMap::new();
            let mut local_phrase_df: HashMap<u64, u32> = HashMap::new();
            let mut local_ftl: HashMap<i64, u32> = HashMap::new();
            let mut local_fcl: HashMap<i64, usize> = HashMap::new();
            let mut local_fcr: HashMap<i64, f64> = HashMap::new();
            let mut local_total = 0u32;

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
                let mut file_phrases_fast = HashSet::<u64>::new();
                let mut token_len = 0u32;
                let mut comment_lines = 0u32;

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
                        let ph = phrase_hash(phrase);

                        *local_phrase_df.entry(ph).or_insert(0) += 1;

                        let df_count = local_phrase_df.get(&ph).copied().unwrap_or(0);
                        let lc_key = phrase.to_lowercase();
                        if !local_left_ctx.contains_key(&lc_key) && df_count <= 5 {
                            let before = &line[..m.start()].trim();
                            if let Some(lc) = before.split_whitespace().last() {
                                let lc = lc.chars().take(30).collect::<String>().to_lowercase();
                                local_left_ctx.entry(lc_key).or_default().entry(lc).and_modify(|c| *c += 1).or_insert(1);
                            }
                        }

                        let is_def = if zone == 0 && crate::zone::is_definition(phrase, line, m.start()) { 1 } else { 0 };

                        let key = (ph, fid);
                        let entry = local_occs.entry(key).or_insert([is_def, zone as i32, 0, line_no as i32]);
                        if is_def > entry[0] { entry[0] = is_def; }
                        if zone == 0 { entry[1] = 0; }
                        entry[2] += 1;

                        if !local_phrase_strings.contains_key(&ph) && !file_phrases_fast.contains(&ph) {
                            local_phrase_strings.entry(ph).or_insert_with(|| phrase.to_string());
                        }

                        if file_phrases_fast.insert(ph) {
                            token_len += 1;
                        }
                    }
                }

                local_ftl.insert(fid, token_len);
                local_fcl.insert(fid, content_len);
                local_fcr.insert(fid, if lines.is_empty() { 0.0 } else { comment_lines as f64 / lines.len() as f64 });
                local_total += token_len;
            }

            Some(WorkerResult {
                occs: local_occs,
                phrase_strings: local_phrase_strings,
                left_ctx: local_left_ctx,
                phrase_df: local_phrase_df,
                token_lens: local_ftl,
                content_lens: local_fcl,
                comment_ratios: local_fcr,
                total_phrases: local_total,
            })
        }).collect();

    fs::write("/tmp/eh_progress.txt", format!("extraction done: {} worker results, {} occs\n", results.len(), results.iter().map(|r| r.occs.len()).sum::<usize>())).ok();

    // Merge all results into global accumulators
    for wr in results {
        for (k, v) in wr.occs {
            occs.entry(k).or_insert(v);
        }
        for (k, v) in wr.left_ctx {
            let entry = phrase_left_ctx.entry(k).or_default();
            for (ck, cv) in v {
                *entry.entry(ck).or_insert(0) += cv;
            }
        }
        for (k, v) in wr.phrase_df {
            *phrase_df_counter.entry(k).or_insert(0) += v;
        }
        for (k, v) in wr.phrase_strings {
            phrase_strings.entry(k).or_insert(v);
        }
        file_token_lens.extend(wr.token_lens);
        file_content_lens.extend(wr.content_lens);
        file_comment_ratios.extend(wr.comment_ratios);
        global_total_phrases += wr.total_phrases;
    }

    fs::write("/tmp/eh_progress.txt", format!("merge done: {} occs, bulk writing SQLite...\n", occs.len())).ok();

    // Pre-compute LCEP thresholds for is_def overrides
    let lcep_thresholds: HashMap<u64, f64> = phrase_left_ctx.iter()
        .filter_map(|(pl, ctx_counts)| {
            let total: u32 = ctx_counts.values().sum();
            if total < 3 { return None; }
            let entropy: f64 = ctx_counts.values()
                .map(|c| { let p = *c as f64 / total as f64; -p * p.log2() })
                .sum();
            let ph = phrase_hash(pl);
            Some((ph, entropy))
        }).collect();

    // Bulk INSERT using prepared statement in single transaction
    {
        let tx = db.unchecked_transaction().map_err(|e| format!("tx: {}", e))?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO phrase_occ (phrase, file_id, is_def, zone, count, line_nos, zone_int)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ).map_err(|e| format!("prepare: {}", e))?;

            let mut row_count = 0u32;
            for ((ph, fid), [is_def_orig, zone_code, count, first_line]) in occs.iter() {
                let phrase = phrase_strings.get(ph)
                    .map(|s| s.as_str())
                    .unwrap_or("__missing__");
                let mut is_def = *is_def_orig;
                let df = phrase_df_counter.get(ph).copied().unwrap_or(0);

                if *zone_code == 0 {
                    if let Some(entropy) = lcep_thresholds.get(ph) {
                        if df < 20 && *entropy < 1.0 { is_def = 2; }
                        else if df < 20 && *entropy < 2.0 { is_def = is_def.max(1); }
                        else if df >= 20 && *entropy > 2.5 { is_def = 0; }
                    }
                    if is_def == 0 && !lcep_thresholds.contains_key(ph) && df < 10 { is_def = -1; }
                }

                let zone_str = if *zone_code == 0 { "code" } else { "prose" };
                let zi = if *zone_code == 0 { 0i32 } else { 1i32 };
                let line_blob = first_line.to_le_bytes().to_vec();
                stmt.execute(rusqlite::params![phrase, fid, is_def, zone_str, count, line_blob, zi])
                    .map_err(|e| format!("insert: {}", e))?;

                row_count += 1;
                if row_count % 1_000_000 == 0 {
                    fs::write("/tmp/eh_progress.txt", format!("inserting... {}M rows\n", row_count / 1_000_000)).ok();
                }
            }
            drop(stmt);
        }
        tx.commit().map_err(|e| format!("commit: {}", e))?;
    }
    fs::write("/tmp/eh_progress.txt", "SQLite commit done, rebuilding PK index...\n").ok();

    // Rebuild phrase_occ as WITHOUT ROWID for fast query lookups
    schema::rebuild_primary_key(&db).map_err(|e| format!("rebuild pk: {}", e))?;
    fs::write("/tmp/eh_progress.txt", "PK rebuild done, stats...\n").ok();

    // File stats
    {
        let mut stats_stmt = db.prepare(
            "INSERT OR REPLACE INTO file_stats (file_id, token_len, content_len, comment_ratio)
             VALUES (?1, ?2, ?3, ?4)"
        ).map_err(|e| format!("prepare stats: {}", e))?;
        for (fid, _) in &file_map {
            let tl = file_token_lens.get(fid).copied().unwrap_or(0) as f64;
            let cl = file_content_lens.get(fid).copied().unwrap_or(0) as f64;
            let cr = file_comment_ratios.get(fid).copied().unwrap_or(0.0);
            stats_stmt.execute(params![fid, tl, cl, cr])
                .map_err(|e| format!("stats: {}", e))?;
        }
        drop(stats_stmt);

        // Uniqueness ratio — only for repos under 2M phrase_occ rows
        // For large repos, this query is too expensive on first build
        let occ_count: i64 = db.query_row("SELECT COUNT(*) FROM phrase_occ", [], |r| r.get(0)).unwrap_or(0);
        if occ_count < 2_000_000 {
            db.execute_batch(
                "CREATE TEMP TABLE phrase_df AS SELECT phrase, COUNT(*) AS df FROM phrase_occ GROUP BY phrase;
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
        }

        let sum_len: u32 = file_token_lens.values().sum();
        let n_files = file_token_lens.len().max(1_usize);
        let avgdl = sum_len as f64 / n_files as f64;

        db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('avgdl', ?1)",
            [avgdl],
        ).map_err(|e| format!("meta: {}", e))?;

        db.execute_batch("ANALYZE").ok();
    }

    // Write digest cache
    if let Ok(s) = serde_json::to_string(&all_digests) {
        fs::write(&digest_path, s).ok();
    }

    let _ = db.close();

    if verbose {
        eprintln!("Phrase index built: {} files, {} phrases", files.len(), occs.len());
    }
    Ok(count_phrases(&db_path).unwrap_or(0))
}

fn collect_source_files(repo: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let skip_dirs: HashSet<&str> = [
        ".git", "node_modules", "vendor", "dist", "build", "target",
        ".venv", "__pycache__", ".next", ".tox", ".eggs", "env", ".env",
        "coverage", ".reliary", ".horizon", ".gitlab", ".circleci",
        ".github",
    ].into();
    let lock_suffixes: HashSet<&str> = [
        "package-lock.json", "yarn.lock", "pnpm-lock.yaml",
        "Cargo.lock", "go.sum", "Gemfile.lock", "poetry.lock",
        "Pipfile.lock", "composer.lock", "bun.lockb",
    ].into();
    let valid_exts: HashSet<&str> = [
        ".ts", ".tsx", ".js", ".jsx", ".go", ".py", ".rs", ".c", ".h",
        ".cpp", ".hpp", ".java", ".kt", ".swift", ".rb", ".php", ".scala", ".clj",
        ".erl", ".hrl", ".ex", ".exs", ".zig", ".nim", ".nix", ".tcl",
        ".elm", ".hs", ".ml", ".mli", ".fs", ".v", ".purs",
        ".md", ".rst", ".txt", ".yaml", ".yml", ".toml", ".json", ".html", ".css",
        ".sh", ".bash", ".zsh", ".fish",
        ".sql", ".graphql", ".proto", ".lua",
        ".makefile", ".dockerfile",
    ].into();

    let mut stack: Vec<PathBuf> = vec![repo.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string(); // no follow
                if skip_dirs.contains(name.as_str()) || name.starts_with('.') {
                    continue;
                }
                stack.push(entry.path());
            } else if ft.is_file() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if lock_suffixes.contains(fname.as_str()) {
                    continue;
                }
                if let Some(ext) = entry.path().extension() {
                    let ext = format!(".{}", ext.to_string_lossy().to_lowercase());
                    if !valid_exts.contains(ext.as_str()) {
                        if !fname.to_lowercase().ends_with("makefile") && !fname.to_lowercase().ends_with("dockerfile") {
                            continue;
                        }
                    }
                } else {
                    continue;
                }
                let p = entry.path();
                let rel = p.strip_prefix(repo).unwrap_or(&p).to_string_lossy().to_string();
                files.push(rel);
            }
        }
    }

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
