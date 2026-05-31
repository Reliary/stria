use rusqlite::Connection;

pub fn create_new_db(db: &Connection) -> rusqlite::Result<()> {
    // page_size must be set BEFORE any table creation
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;
         PRAGMA mmap_size = 268435456;
         PRAGMA temp_store = MEMORY;
         PRAGMA lock_timeout = 5000;"
    )?;
    create_tables(db)
}

pub fn open_existing_db(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -200000;
         PRAGMA mmap_size = 268435456;
         PRAGMA temp_store = MEMORY;
         PRAGMA lock_timeout = 5000;"
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
            is_def INTEGER DEFAULT 0,
            zone_int INTEGER DEFAULT 0,
            count INTEGER DEFAULT 1,
            line_nos BLOB,
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
        );"
    )?;
    // Migration: old schema may have phrase TEXT + zone TEXT columns
    db.execute_batch("ALTER TABLE phrase_occ ADD COLUMN zone_int INTEGER DEFAULT 0;").ok();
    Ok(())
}

pub fn rebuild_primary_key(_db: &Connection) -> rusqlite::Result<()> {
    Ok(())
}
