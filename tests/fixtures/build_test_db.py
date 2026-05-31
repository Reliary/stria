"""Build a small test phrase index for integration tests.
Generates a 3-file corpus with known phrases and is_def flags."""
import sqlite3, json, os, sys, hashlib, struct

OUT = os.path.join(os.path.dirname(__file__), "test_phrases.sqlite")
if os.path.exists(OUT):
    os.remove(OUT)

db = sqlite3.connect(OUT)
db.execute("PRAGMA journal_mode=WAL")
db.execute("PRAGMA synchronous=OFF")

# Schema matches the Rust build output
db.executescript("""
CREATE TABLE IF NOT EXISTS file_map (
    id INTEGER PRIMARY KEY,
    file_path TEXT UNIQUE
);
CREATE TABLE IF NOT EXISTS phrases (
    id INTEGER PRIMARY KEY,
    phrase TEXT UNIQUE
);
CREATE TABLE IF NOT EXISTS phrase_occ (
    phrase_id INTEGER NOT NULL,
    file_id INTEGER NOT NULL,
    flags BLOB DEFAULT (X'01'),
    line_nos BLOB DEFAULT (X'00000000'),
    PRIMARY KEY (phrase_id, file_id)
);
CREATE TABLE IF NOT EXISTS count_overflow (
    phrase_id INTEGER NOT NULL,
    file_id INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (phrase_id, file_id)
);
CREATE TABLE IF NOT EXISTS file_stats (
    file_id INTEGER PRIMARY KEY,
    token_len REAL DEFAULT 10,
    unique_def_count INTEGER DEFAULT 0,
    total_def_count INTEGER DEFAULT 0,
    comment_ratio REAL DEFAULT 0.0
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value REAL
);
""")

# 3 files with known content
files = [
    ("src/spool.ts", "spool upload retry SpoolManager uploadEvents processSpool"),
    ("tests/spool.test.ts", "spool test SpoolManager MockS3Client RateLimitError setupTestSpool"),
    ("tests/helpers/mock_s3.ts", "MockS3Client createDummySpool mockUpload mockDelete createS3Bucket"),
]

file_ids = {}
for path, content in files:
    c = db.execute("INSERT OR REPLACE INTO file_map (file_path) VALUES (?)", (path,))
    fid = c.lastrowid or db.execute("SELECT id FROM file_map WHERE file_path=?", (path,)).fetchone()[0]
    file_ids[path] = fid
    
    # Populate file_stats
    tokens = content.split()
    db.execute(
        "INSERT OR REPLACE INTO file_stats (file_id, token_len, unique_def_count, total_def_count, comment_ratio) VALUES (?, ?, 2, 3, 0.0)",
        (fid, len(tokens))
    )

# Phrases and their occurrences (with is_def flags)
phrases_data = [
    # (phrase, file_path, is_def, count)
    ("SpoolManager", "src/spool.ts", 2, 3),
    ("SpoolManager", "tests/spool.test.ts", 0, 1),
    ("uploadEvents", "src/spool.ts", 1, 2),
    ("processSpool", "src/spool.ts", 1, 2),
    ("spool", "src/spool.ts", 0, 5),
    ("spool", "tests/spool.test.ts", 0, 3),
    ("MockS3Client", "tests/spool.test.ts", 0, 2),
    ("MockS3Client", "tests/helpers/mock_s3.ts", 2, 2),
    ("RateLimitError", "tests/spool.test.ts", 0, 1),
    ("RateLimitError", "tests/helpers/mock_s3.ts", 2, 1),
    ("setupTestSpool", "tests/spool.test.ts", 1, 1),  # defined in test
    ("test", "tests/spool.test.ts", 0, 4),
    ("test", "tests/helpers/mock_s3.ts", 0, 1),
    ("createDummySpool", "tests/helpers/mock_s3.ts", 1, 1),
    ("mockUpload", "tests/helpers/mock_s3.ts", 1, 1),
    ("mockDelete", "tests/helpers/mock_s3.ts", 1, 1),
    ("createS3Bucket", "tests/helpers/mock_s3.ts", 1, 1),
]

# Insert unique phrases first
seen_phrases = {}
for phrase, _, _, _ in phrases_data:
    if phrase not in seen_phrases:
        c = db.execute("INSERT OR REPLACE INTO phrases (phrase) VALUES (?)", (phrase,))
        seen_phrases[phrase] = c.lastrowid or db.execute("SELECT id FROM phrases WHERE phrase=?", (phrase,)).fetchone()[0]

# Insert phrase_occ with flags
for phrase, file_path, is_def, count in phrases_data:
    pid = seen_phrases[phrase]
    fid = file_ids[file_path]
    flags = struct.pack('B', is_def)
    line_nos = struct.pack('<I', 5)  # first_line=5
    
    db.execute(
        "INSERT INTO phrase_occ (phrase_id, file_id, flags, line_nos) VALUES (?, ?, ?, ?)",
        (pid, fid, flags, line_nos)
    )
    
    if count >= 31:
        db.execute(
            "INSERT INTO count_overflow (phrase_id, file_id, count) VALUES (?, ?, ?)",
            (pid, fid, count)
        )

# Meta
avgdl = sum(5 for _ in files) / len(files)
db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('avgdl', ?)", (float(avgdl),))
db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('build_time', ?)", (float(1000000),))

db.commit()
db.close()
print(f"Built test DB at {OUT} with {len(phrases_data)} phrase_occ rows")
