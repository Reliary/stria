use rusqlite::Connection;

pub fn create_schema(db: &Connection) -> rusqlite::Result<()> {
    // For bulk loading, create a simple heap table (no PK constraint).
    // After bulk insert, we'll rebuild as WITHOUT ROWID for fast lookups.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_map (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS phrase_occ (
            phrase TEXT,
            file_id INTEGER,
            is_def INTEGER DEFAULT 0,
            zone TEXT DEFAULT 'code',
            count INTEGER DEFAULT 1,
            line_nos BLOB,
            zone_int INTEGER DEFAULT 0
        );
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
    // Add zone_int column if it doesn't exist (migration from old schema)
    db.execute_batch("ALTER TABLE phrase_occ ADD COLUMN zone_int INTEGER DEFAULT 0;").ok();
    Ok(())
}

/// After bulk insert, add a B-tree index for fast lookups.
pub fn rebuild_primary_key(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS phrase_occ_phrase_idx ON phrase_occ(phrase, file_id);
         ANALYZE;"
    )?;
    Ok(())
}
