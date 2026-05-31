use rusqlite::Connection;

pub(crate) fn create_new_db(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;
         PRAGMA mmap_size = 268435456;
         PRAGMA temp_store = MEMORY;
         PRAGMA lock_timeout = 5000;",
    )?;
    create_tables(db)
}

pub(crate) fn open_existing_db(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;
         PRAGMA mmap_size = 268435456;
         PRAGMA temp_store = MEMORY;
         PRAGMA lock_timeout = 5000;",
    )
}

fn create_tables(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_map (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS phrases (
            id INTEGER PRIMARY KEY,
            phrase TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS phrase_occ (
            phrase_id INTEGER,
            file_id INTEGER,
            flags BLOB NOT NULL,
            line_nos BLOB NOT NULL,
            PRIMARY KEY (phrase_id, file_id)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS count_overflow (
            phrase_id INTEGER,
            file_id INTEGER,
            count INTEGER NOT NULL,
            PRIMARY KEY (phrase_id, file_id)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS file_stats (
            file_id INTEGER PRIMARY KEY,
            token_len INTEGER DEFAULT 0,
            content_len INTEGER DEFAULT 0,
            unique_def_count INTEGER DEFAULT 0,
            total_def_count INTEGER DEFAULT 0,
            comment_ratio REAL DEFAULT 0.0
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value REAL
        );",
    )?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn rebuild_primary_key(_db: &Connection) -> rusqlite::Result<()> {
    Ok(())
}

// --- Packing helpers ---
// is_def: [-1, 0, 1, 2] → packed: [0, 1, 2, 3] (add 1, 2 bits)
// zone_int: [0, 1] → packed: [0, 1] (1 bit)
// count: [0..30] direct, 31 = overflow (5 bits)
const COUNT_OVERFLOW: u8 = 31;

pub(crate) fn pack_flags(is_def: i32, zone_int: i32, count: u32) -> [u8; 1] {
    let is_def_packed = ((is_def + 1) as u8) & 0x03; // 2 bits: -1→0, 0→1, 1→2, 2→3
    let zone_packed = (zone_int as u8) & 0x01; // 1 bit
    let count_packed = if count <= 30 {
        count as u8
    } else {
        COUNT_OVERFLOW
    };
    [is_def_packed | (zone_packed << 2) | (count_packed << 3)]
}

pub(crate) fn unpack_is_def(flags: u8) -> i32 {
    ((flags & 0x03) as i32) - 1
}

pub(crate) fn unpack_zone_int(flags: u8) -> i32 {
    ((flags >> 2) & 0x01) as i32
}

pub(crate) fn unpack_count(flags: u8) -> u32 {
    (flags >> 3) as u32
}

#[allow(dead_code)]
pub(crate) fn is_count_overflow(flags: u8) -> bool {
    unpack_count(flags) >= COUNT_OVERFLOW as u32
}

pub(crate) fn pack_line_nos(line: u32) -> [u8; 2] {
    (line as u16).to_le_bytes()
}

pub(crate) fn unpack_line_nos(blob: &[u8]) -> u32 {
    if blob.len() >= 2 {
        u16::from_le_bytes([blob[0], blob[1]]) as u32
    } else {
        0
    }
}
