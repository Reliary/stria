use rusqlite::Connection;

pub fn create_schema(db: &Connection) -> rusqlite::Result<()> {
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
            PRIMARY KEY (phrase, file_id)
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
    Ok(())
}
