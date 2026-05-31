pub(crate) mod schema;

use fxhash::{FxHashMap, FxHasher};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::zone;

fn progress(msg: &str) {
    let p = std::env::temp_dir().join("eh_progress.txt");
    std::fs::write(p, msg).ok();
}

/// Fast hash of a phrase string to u64. Collision risk for 85K phrases: ~1 in 2^32.
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
    digests: HashMap<String, String>,
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
    if files.is_empty() {
        return Ok(0);
    }

    // Determine changed files: skip SHA-256 on first build (all files already changed)
    let mut changed_files: Vec<(String, String)> = Vec::new();
    let mut all_digests: HashMap<String, String> = HashMap::new();

    if digest_cache.is_empty() {
        for rel in &files {
            changed_files.push((rel.clone(), String::new()));
        }
        // Write mtime cache on first build so second build uses mtime fast path
        let mut mtimes: HashMap<String, (u64, u32)> = HashMap::with_capacity(files.len());
        for rel in &files {
            let fpath = repo.join(rel);
            let mtime = fpath
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| (d.as_secs(), d.subsec_nanos()))
                        .unwrap_or((0, 0))
                })
                .unwrap_or((0, 0));
            mtimes.insert(rel.clone(), mtime);
        }
        if let Ok(s) = serde_json::to_string(&mtimes) {
            fs::write(out_dir.join("mtime_cache.json"), s).ok();
        }
    } else {
        // Mtime-based fast path: skip SHA-256 for files whose mtime hasn't changed
        let mut mtimes: HashMap<String, (u64, u32)> = HashMap::new();
        if let Ok(s) = fs::read_to_string(out_dir.join("mtime_cache.json")) {
            if let Ok(v) = serde_json::from_str::<HashMap<String, (u64, u32)>>(&s) {
                mtimes = v;
            }
        }
        for rel in &files {
            let fpath = repo.join(rel);
            let mtime = fpath
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| (d.as_secs(), d.subsec_nanos()))
                        .unwrap_or((0, 0))
                })
                .unwrap_or((0, 0));
            if mtimes.get(rel) == Some(&mtime) && digest_cache.contains_key(rel) {
                // mtime unchanged → reuse cached digest
                if let Some(cached) = digest_cache.get(rel) {
                    all_digests.insert(rel.clone(), cached.clone());
                }
            } else {
                // mtime changed → compute SHA-256
                let dig = sha256_file(&fpath).unwrap_or_default();
                all_digests.insert(rel.clone(), dig.clone());
                changed_files.push((rel.clone(), dig));
            }
            mtimes.insert(rel.clone(), mtime);
        }
        // Write mtime cache for next run
        if let Ok(s) = serde_json::to_string(&mtimes) {
            fs::write(out_dir.join("mtime_cache.json"), s).ok();
        }
    }

    // If nothing changed, return early
    if !changed_files.is_empty() || !db_path.exists() || digest_cache.is_empty() {
        // Proceed with build
    } else {
        if verbose {
            eprintln!("Phrase index up to date: 0 changed files");
        }
        return count_phrases(&db_path);
    }

    let is_full_rebuild = !db_path.exists()
        || digest_cache.is_empty()
        || (changed_files.len() as f64) >= (files.len() as f64 * 0.3);

    let source_files: Vec<String> = if is_full_rebuild {
        files.clone()
    } else {
        changed_files.iter().map(|(rel, _)| rel.clone()).collect()
    };

    // Accumulators — pre-allocated during merge phase after extraction completes
    let mut occs: FxHashMap<(u64, i64), OccEntry> = FxHashMap::default();
    let mut phrase_strings: HashMap<u64, String> = HashMap::new();
    let mut phrase_left_ctx: HashMap<String, HashMap<String, u32>> = HashMap::new();
    let mut phrase_df_counter: HashMap<u64, u32> = HashMap::new();
    let mut file_token_lens: HashMap<i64, u32> = HashMap::new();
    let mut file_content_lens: HashMap<i64, usize> = HashMap::new();
    let mut file_comment_ratios: HashMap<i64, f64> = HashMap::new();

    // Build file_map
    let mut file_map: Vec<(i64, String)> = Vec::new();
    if is_full_rebuild {
        for (i, rel) in files.iter().enumerate() {
            file_map.push(((i + 1) as i64, rel.clone()));
        }
    } else {
        let mut existing: HashMap<String, i64> = HashMap::new();
        if let Ok(db_temp) = Connection::open(&db_path) {
            if let Ok(mut get_files_q) = db_temp.prepare("SELECT id, file_path FROM file_map") {
                if let Ok(rows) =
                    get_files_q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                {
                    for row in rows.flatten() {
                        existing.insert(row.1, row.0);
                    }
                }
            }
        }
        let max_id: i64 = if let Ok(db_temp) = Connection::open(&db_path) {
            db_temp
                .query_row("SELECT COALESCE(MAX(id), 0) FROM file_map", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0)
        } else {
            0
        };
        let mut next_id = max_id + 1;
        for rel in &files {
            if let Some(id) = existing.get(rel) {
                file_map.push((*id, rel.clone()));
            } else {
                file_map.push((next_id, rel.clone()));
                next_id += 1;
            }
        }
    }

    // Open DB: temp file for full rebuild (atomic swap at end), real DB for incremental
    let temp_path = out_dir.join("phrases.tmp.sqlite");
    let (db, is_swap) = if is_full_rebuild {
        let db = Connection::open(&temp_path).map_err(|e| format!("db: {}", e))?;
        schema::create_new_db(&db).map_err(|e| format!("schema: {}", e))?;
        (db, true)
    } else {
        let db = Connection::open(&db_path).map_err(|e| format!("db: {}", e))?;
        schema::open_existing_db(&db).map_err(|e| format!("pragmas: {}", e))?;
        schema::create_new_db(&db).ok(); // create tables if they don't exist
        (db, false)
    };

    // Delete changed file data for incremental rebuild
    if !is_full_rebuild {
        for (rel, _) in &changed_files {
            if let Ok(mut get_id) = db.prepare("SELECT id FROM file_map WHERE file_path = ?1") {
                if let Ok(fid) = get_id.query_row(params![rel], |r| r.get::<_, i64>(0)) {
                    let _ = db.execute("DELETE FROM phrase_occ WHERE file_id = ?1", [fid]);
                    let _ = db.execute("DELETE FROM file_stats WHERE file_id = ?1", [fid]);
                }
            }
        }
    }

    // Insert file_map entries
    {
        let tx = db
            .unchecked_transaction()
            .map_err(|e| format!("tx file_map: {}", e))?;
        let mut stmt = tx
            .prepare("INSERT OR REPLACE INTO file_map (id, file_path) VALUES (?1, ?2)")
            .map_err(|e| format!("prepare file_map: {}", e))?;
        for (fid, rel) in &file_map {
            stmt.execute(params![fid, rel])
                .map_err(|e| format!("insert file_map: {}", e))?;
        }
        drop(stmt);
        tx.commit().map_err(|e| format!("commit file_map: {}", e))?;
    }
    progress("file_map done");

    // Parallel extraction
    let n_files = source_files.len();
    let n_workers = rayon::current_num_threads();
    let chunk_size = (n_files / n_workers.max(1)).max(1);
    progress(&format!(
        "extraction: {} files, {} threads",
        n_files, n_workers
    ));

    let repo_arc = std::sync::Arc::new(repo);
    let file_id_map: HashMap<&str, i64> = file_map
        .iter()
        .map(|(id, rel)| (rel.as_str(), *id))
        .collect();
    let file_id_map_arc = std::sync::Arc::new(file_id_map);

    let results: Vec<WorkerResult> = source_files
        .par_chunks(chunk_size)
        .filter_map(|chunk| -> Option<WorkerResult> {
            if chunk.is_empty() {
                return None;
            }
            let repo = repo_arc.as_path();
            let file_id_map = file_id_map_arc.as_ref();
            let est = chunk.len() * 80;
            let mut local_occs: FxHashMap<(u64, i64), OccEntry> =
                FxHashMap::with_capacity_and_hasher(est, Default::default());
            let mut local_phrase_strings: HashMap<u64, String> = HashMap::with_capacity(est / 4);
            let mut local_left_ctx: HashMap<String, HashMap<String, u32>> = HashMap::new();
            let mut local_phrase_df: HashMap<u64, u32> = HashMap::with_capacity(est / 4);
            let mut local_ftl: HashMap<i64, u32> = HashMap::with_capacity(chunk.len());
            let mut local_fcl: HashMap<i64, usize> = HashMap::with_capacity(chunk.len());
            let mut local_fcr: HashMap<i64, f64> = HashMap::with_capacity(chunk.len());
            let mut local_digests: HashMap<String, String> = HashMap::with_capacity(chunk.len());
            let track_lcep = n_files <= 5000;

            for rel in chunk {
                let fpath = repo.join(rel);
                let text = match fs::read_to_string(&fpath) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                // Compute SHA-256 during extraction — file is already in memory
                {
                    let mut hasher = Sha256::new();
                    hasher.update(text.as_bytes());
                    local_digests.insert(rel.clone(), format!("{:x}", hasher.finalize()));
                }
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
                    if s.starts_with('#')
                        || s.starts_with("//")
                        || s.starts_with("/*")
                        || s.starts_with('*')
                        || s.starts_with("<!--")
                        || s.starts_with('>')
                    {
                        comment_lines += 1;
                    }
                    let zone = zone::line_zone(line);
                    for (start, phrase) in zone::scan_identifiers(line) {
                        let ph = phrase_hash(phrase);

                        *local_phrase_df.entry(ph).or_insert(0) += 1;

                        let df_count = local_phrase_df.get(&ph).copied().unwrap_or(0);
                        let lc_key = phrase.to_lowercase();
                        if track_lcep && !local_left_ctx.contains_key(&lc_key) && df_count <= 5 {
                            let before = &line[..start].trim();
                            if let Some(lc) = before.split_whitespace().last() {
                                let lc = lc.chars().take(30).collect::<String>().to_lowercase();
                                local_left_ctx
                                    .entry(lc_key)
                                    .or_default()
                                    .entry(lc)
                                    .and_modify(|c| *c += 1)
                                    .or_insert(1);
                            }
                        }

                        let is_def = if zone == 0 && crate::zone::is_definition(phrase, line, start)
                        {
                            1
                        } else {
                            0
                        };

                        let key = (ph, fid);
                        let entry = local_occs.entry(key).or_insert([
                            is_def,
                            zone as i32,
                            0,
                            line_no as i32,
                        ]);
                        if is_def > entry[0] {
                            entry[0] = is_def;
                        }
                        if zone == 0 {
                            entry[1] = 0;
                        }
                        entry[2] += 1;

                        if !local_phrase_strings.contains_key(&ph)
                            && !file_phrases_fast.contains(&ph)
                        {
                            local_phrase_strings
                                .entry(ph)
                                .or_insert_with(|| phrase.to_string());
                        }

                        if file_phrases_fast.insert(ph) {
                            token_len += 1;
                        }
                    }
                }

                local_ftl.insert(fid, token_len);
                local_fcl.insert(fid, content_len);
                local_fcr.insert(
                    fid,
                    if lines.is_empty() {
                        0.0
                    } else {
                        comment_lines as f64 / lines.len() as f64
                    },
                );
            }

            Some(WorkerResult {
                occs: local_occs,
                phrase_strings: local_phrase_strings,
                left_ctx: local_left_ctx,
                phrase_df: local_phrase_df,
                token_lens: local_ftl,
                content_lens: local_fcl,
                comment_ratios: local_fcr,
                digests: local_digests,
            })
        })
        .collect();

    progress(&format!(
        "extraction done: {} worker results",
        results.len()
    ));

    // Pre-allocate to prevent rehash thrash (24M entries × 12 rehashes = 12s)
    let est_occs: usize = std::cmp::max(n_files * 20, results.iter().map(|wr| wr.occs.len()).sum());
    occs.reserve(est_occs);
    phrase_df_counter.reserve(est_occs / 4);
    phrase_strings.reserve(est_occs / 4);
    file_token_lens.reserve(n_files);
    file_content_lens.reserve(n_files);
    file_comment_ratios.reserve(n_files);

    // Merge all results — pre-allocated occs handles 24M entries without rehash
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
        for (k, v) in wr.digests {
            all_digests.insert(k, v);
        }
        file_token_lens.extend(wr.token_lens);
        file_content_lens.extend(wr.content_lens);
        file_comment_ratios.extend(wr.comment_ratios);
    }

    progress(&format!(
        "merge done: {} occs, building phrase table...",
        occs.len()
    ));

    // Build phrase table: assign integer IDs to unique phrases
    // Sort by phrase string for deterministic output
    let mut phrase_list: Vec<(u64, String)> = phrase_strings.into_iter().collect();
    phrase_list.sort_by(|a, b| a.1.cmp(&b.1));

    // phrase_hash → phrase_id mapping (1-based)
    let mut phrase_to_id: HashMap<u64, i64> = HashMap::with_capacity(phrase_list.len());
    {
        let tx = db
            .unchecked_transaction()
            .map_err(|e| format!("tx phrases: {}", e))?;
        {
            // On full rebuild, phrases are known-unique from Rust dedup (no UNIQUE check needed)
            let insert_sql = if is_full_rebuild {
                "INSERT INTO phrases (id, phrase) VALUES (?1, ?2)"
            } else {
                "INSERT OR IGNORE INTO phrases (id, phrase) VALUES (?1, ?2)"
            };
            let mut stmt = tx
                .prepare(insert_sql)
                .map_err(|e| format!("prepare phrases: {}", e))?;
            for (idx, (ph, phrase)) in phrase_list.iter().enumerate() {
                let pid = (idx + 1) as i64;
                stmt.execute(params![pid, phrase])
                    .map_err(|e| format!("insert phrase: {}", e))?;
                phrase_to_id.insert(*ph, pid);
            }
            drop(stmt);
        }
        tx.commit().map_err(|e| format!("commit phrases: {}", e))?;
    }

    // LCEP thresholds (for small repos only)
    let lcep_thresholds: HashMap<u64, f64> = phrase_left_ctx
        .iter()
        .filter_map(|(pl, ctx_counts)| {
            let total: u32 = ctx_counts.values().sum();
            if total < 3 {
                return None;
            }
            let entropy: f64 = ctx_counts
                .values()
                .map(|c| {
                    let p = *c as f64 / total as f64;
                    -p * p.log2()
                })
                .sum();
            let ph = phrase_hash(pl);
            Some((ph, entropy))
        })
        .collect();

    // Sort occs by (phrase_id, file_id) for sequential WITHOUT ROWID B-tree fill.
    // Pre-compute phrase_ids so sorting is pure integer comparison (no HashMap lookups).
    let mut sorted: Vec<((i64, i64), [i32; 4])> = Vec::with_capacity(occs.len());
    let mut df_map: HashMap<i64, u32> = HashMap::with_capacity(phrase_to_id.len());
    let mut entropy_by_pid: HashMap<i64, f64> = HashMap::new();
    for ((ph, fid), entry) in occs {
        let pid = phrase_to_id.get(&ph).copied().unwrap_or(0);
        sorted.push(((pid, fid), entry));
        *df_map.entry(pid).or_insert(0) += 1;
        if let Some(entropy) = lcep_thresholds.get(&ph) {
            entropy_by_pid.entry(pid).or_insert(*entropy);
        }
    }
    sorted.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.0 .1.cmp(&b.0 .1)));
    progress(&format!(
        "sorted {} entries, inserting phrase_occ...",
        sorted.len()
    ));

    // Compute def stats in Rust before sharding (eliminates need to pass back from shards)
    let mut total_def_counts: HashMap<i64, u32> = HashMap::with_capacity(file_map.len());
    let mut unique_def_counts: HashMap<i64, u32> = HashMap::with_capacity(file_map.len());
    for ((pid, fid), [is_def_orig, zone_code, _count, _first_line]) in &sorted {
        let mut is_def = *is_def_orig;
        let df = df_map.get(pid).copied().unwrap_or(0);
        if *zone_code == 0 {
            if let Some(entropy) = entropy_by_pid.get(pid) {
                if df < 20 && *entropy < 1.0 {
                    is_def = 2;
                } else if df < 20 && *entropy < 2.0 {
                    is_def = is_def.max(1);
                } else if df >= 20 && *entropy > 2.5 {
                    is_def = 0;
                }
            }
        }
        if is_def >= 1 {
            *total_def_counts.entry(*fid).or_insert(0) += 1;
            if df == 1 {
                *unique_def_counts.entry(*fid).or_insert(0) += 1;
            }
        }
    }

    // Bulk INSERT phrase_occ with packed format
    {
        let tx = db
            .unchecked_transaction()
            .map_err(|e| format!("tx: {}", e))?;
        let mut occ_stmt = tx.prepare(
            "INSERT INTO phrase_occ (phrase_id, file_id, flags, line_nos) VALUES (?1, ?2, ?3, ?4)"
        ).map_err(|e| format!("prepare: {}", e))?;
        let mut overflow_stmt = tx
            .prepare("INSERT INTO count_overflow (phrase_id, file_id, count) VALUES (?1, ?2, ?3)")
            .map_err(|e| format!("prepare overflow: {}", e))?;

        for ((pid, fid), [is_def_orig, zone_code, count, first_line]) in &sorted {
            let mut is_def = *is_def_orig;
            let df = df_map.get(pid).copied().unwrap_or(0);
            if *zone_code == 0 {
                if let Some(entropy) = entropy_by_pid.get(pid) {
                    if df < 20 && *entropy < 1.0 {
                        is_def = 2;
                    } else if df < 20 && *entropy < 2.0 {
                        is_def = is_def.max(1);
                    } else if df >= 20 && *entropy > 2.5 {
                        is_def = 0;
                    }
                }
            }
            if is_def >= 1 {
                *total_def_counts.entry(*fid).or_insert(0) += 1;
                if df == 1 {
                    *unique_def_counts.entry(*fid).or_insert(0) += 1;
                }
            }

            let flags = schema::pack_flags(is_def, *zone_code, *count as u32);
            let line_bytes = schema::pack_line_nos(*first_line as u32);
            occ_stmt
                .execute(rusqlite::params![
                    pid,
                    fid,
                    flags.as_slice(),
                    line_bytes.as_slice()
                ])
                .map_err(|e| format!("insert: {}", e))?;

            if *count > 30 {
                overflow_stmt
                    .execute(rusqlite::params![pid, fid, count])
                    .map_err(|e| format!("overflow insert: {}", e))?;
            }
        }
        drop(occ_stmt);
        drop(overflow_stmt);
        tx.commit().map_err(|e| format!("commit: {}", e))?;
    }
    progress("phrase_occ done, stats...");

    // File stats (using Rust-computed def counts, no SQL needed)
    {
        let mut stats_stmt = db.prepare(
            "INSERT OR REPLACE INTO file_stats (file_id, token_len, content_len, comment_ratio, unique_def_count, total_def_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ).map_err(|e| format!("prepare stats: {}", e))?;
        for (fid, _) in &file_map {
            let tl = file_token_lens.get(fid).copied().unwrap_or(0) as f64;
            let cl = file_content_lens.get(fid).copied().unwrap_or(0) as f64;
            let cr = file_comment_ratios.get(fid).copied().unwrap_or(0.0);
            let udc = *unique_def_counts.get(fid).unwrap_or(&0);
            let tdc = *total_def_counts.get(fid).unwrap_or(&0);
            stats_stmt
                .execute(params![fid, tl, cl, cr, udc, tdc])
                .map_err(|e| format!("stats: {}", e))?;
        }
        drop(stats_stmt);

        let sum_len: u32 = file_token_lens.values().sum();
        let n_stats_files = file_token_lens.len().max(1_usize);
        let avgdl = sum_len as f64 / n_stats_files as f64;

        db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('avgdl', ?1)",
            [avgdl],
        )
        .map_err(|e| format!("meta: {}", e))?;

        db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('build_time', ?1)",
            [std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64()],
        )
        .map_err(|e| format!("meta: {}", e))?;

        // Skip ANALYZE on large repos — no benefit for search, costs 5s+ on kernel
        if n_files < 5000 {
            db.execute_batch("ANALYZE").ok();
        }
    }

    // Write digest cache
    if let Ok(s) = serde_json::to_string(&all_digests) {
        fs::write(&digest_path, s).ok();
    }

    let _ = db.close();

    // Atomic swap for full rebuilds (WAL mode)
    if is_swap {
        let temp_path = out_dir.join("phrases.tmp.sqlite");
        // WAL checkpoint: copy WAL into main file
        if let Ok(chk_db) = Connection::open(&temp_path) {
            chk_db
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .ok();
            let _ = chk_db.close();
        }
        // Overwrite final DB with temp DB (WAL shutdown ensures clean copy)
        let _ = fs::remove_file(&db_path);
        if fs::rename(&temp_path, &db_path).is_ok() {}
    }

    if verbose {
        eprintln!(
            "Phrase index built: {} files, {} phrases",
            files.len(),
            phrase_list.len()
        );
    }
    Ok(count_phrases(&db_path).unwrap_or(0))
}

fn collect_source_files(repo: &Path) -> Vec<String> {
    use walkdir::WalkDir;
    let mut files = Vec::new();
    let skip_dirs: HashSet<&str> = [
        ".git",
        "node_modules",
        "vendor",
        "dist",
        "build",
        "target",
        ".venv",
        "__pycache__",
        ".next",
        ".tox",
        ".eggs",
        "env",
        ".env",
        "coverage",
        ".reliary",
        ".stria",
        ".gitlab",
        ".circleci",
        ".github",
    ]
    .into();
    let lock_suffixes: HashSet<&str> = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "go.sum",
        "Gemfile.lock",
        "poetry.lock",
        "Pipfile.lock",
        "composer.lock",
        "bun.lockb",
    ]
    .into();
    let valid_exts: HashSet<&str> = [
        ".ts",
        ".tsx",
        ".js",
        ".jsx",
        ".go",
        ".py",
        ".rs",
        ".c",
        ".h",
        ".cpp",
        ".hpp",
        ".java",
        ".kt",
        ".swift",
        ".rb",
        ".php",
        ".scala",
        ".clj",
        ".erl",
        ".hrl",
        ".ex",
        ".exs",
        ".zig",
        ".nim",
        ".nix",
        ".tcl",
        ".elm",
        ".hs",
        ".ml",
        ".mli",
        ".fs",
        ".v",
        ".purs",
        ".md",
        ".rst",
        ".txt",
        ".yaml",
        ".yml",
        ".toml",
        ".json",
        ".html",
        ".css",
        ".sh",
        ".bash",
        ".zsh",
        ".fish",
        ".sql",
        ".graphql",
        ".proto",
        ".lua",
        ".makefile",
        ".dockerfile",
    ]
    .into();

    for entry in WalkDir::new(repo)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if let Some(name) = e.file_name().to_str() {
                !skip_dirs.contains(name) && !name.starts_with('.')
            } else {
                true
            }
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let fname = entry.file_name().to_string_lossy();
        if lock_suffixes.contains(fname.as_ref()) {
            continue;
        }
        if let Some(ext) = entry.path().extension() {
            let ext = format!(".{}", ext.to_string_lossy().to_lowercase());
            if valid_exts.contains(ext.as_str()) {
                let rel = entry
                    .path()
                    .strip_prefix(repo)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .to_string()
                    .replace('\\', "/");
                files.push(rel);
            } else if !fname.to_lowercase().ends_with("makefile")
                && !fname.to_lowercase().ends_with("dockerfile")
            {
                continue;
            }
        }
    }
    files.sort();
    files
}

pub fn count_phrases(db_path: &Path) -> Result<usize, String> {
    if let Ok(db) = Connection::open(db_path) {
        // Try new schema first (phrases table)
        if let Ok(n) = db.query_row("SELECT COUNT(*) FROM phrases", [], |r| r.get::<_, i64>(0)) {
            if n > 0 {
                return Ok(n as usize);
            }
        }
        // Fallback to old schema
        if let Ok(n) = db.query_row("SELECT COUNT(DISTINCT phrase) FROM phrase_occ", [], |r| {
            r.get::<_, i64>(0)
        }) {
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
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

/// Watch a repo for file changes and trigger rebuilds.
/// Cross-platform: uses polling every 2 seconds.
/// The digest cache inside build_phrase_index handles incremental extraction,
/// so unchanged files cost ~0.02s per poll.
pub fn watch_changes(repo: &str, out_dir: &Path) -> Result<(), String> {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match build_phrase_index(repo, out_dir, false) {
            Ok(n) => {
                if n > 0 {
                    eprintln!("eh:info: index updated: {} phrases", n);
                }
            }
            Err(e) => eprintln!("eh:warn: rebuild error: {}", e),
        }
    }
}
