use std::sync::LazyLock;

/// Shared regex for identifier extraction (grammar-free, 3+ char identifiers)
pub static PHRASE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{2,}").unwrap()
});

/// Classify a line as 'code' (0) or 'prose' (1).
/// Grammar-free: uses structural character frequency + comment markers.
/// Byte-based for cache efficiency: no UTF-8 decoding on ASCII source code.
/// Uses inline byte DFA instead of regex for identifier counting (~10x faster).
pub fn line_zone(line: &str) -> u8 {
    let s = line.trim();
    if s.is_empty() { return 1; }

    let bytes = s.as_bytes();

    // Comment markers (byte prefix check)
    if bytes.len() >= 1 && bytes[0] == b'/' {
        if bytes.len() >= 2 && (bytes[1] == b'/' || bytes[1] == b'*') { return 1; }
    }
    if bytes.len() >= 1 && bytes[0] == b'#' {
        if bytes.len() >= 2 && (bytes[1] == b'!') { return 0; }
        // Check for C preprocessor
        if bytes.len() > 2 {
            let w2 = if bytes.len() > 2 { bytes[1..3].as_ref() } else { &[] };
            if matches!(w2, b"in" | b"de" | b"if" | b"en" | b"pr" | b"er" | b"wa") {
                return 0;
            }
        }
        return 1;
    }
    if bytes.starts_with(b"*") || bytes.starts_with(b"<!--") || bytes.starts_with(b">") {
        return 1;
    }

    // Structural character density using byte iteration (no UTF-8 decode)
    let mut structural = 0u32;
    let mut lower = 0u32;
    let slen = s.len().max(1) as f64;

    for &b in bytes {
        match b {
            b'a'..=b'z' => lower += 1,
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b'<' | b'>'
            | b';' | b':' | b'=' | b'|' | b'&' | b'!' | b'@' | b'#'
            | b'$' | b'%' | b'^' | b'*' | b'-' | b'+' | b'/' | b'?' | b'\\' => structural += 1,
            _ => {}
        }
    }

    // Count identifiers using inline byte DFA (~10x faster than regex)
    let mut idents = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let mut count = 1u32;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
                count += 1;
            }
            if count >= 3 { idents += 1; }
        } else {
            i += 1;
        }
    }

    if slen > 0.0 {
        let prose_ratio = lower as f64 / slen;
        let struct_ratio = structural as f64 / slen;

        if prose_ratio > 0.65 && struct_ratio < 0.08 && idents < 3 { return 1; }
        if idents == 0 { return 1; }

        // Multi-word English prose
        let words: Vec<&str> = s.split_whitespace().collect();
        if !words.is_empty() {
            let avg = words.iter().map(|w| w.len()).sum::<usize>() as f64 / words.len() as f64;
            if prose_ratio > 0.5 && avg < 6.0 && struct_ratio < 0.05 && idents < 2 {
                return 1;
            }
        }
    }

    0 // code
}

/// Grammar-free definition detection: checks if phrase is followed by `(`, `<`, `[`, `=`, `:`.
/// Uses byte operations and the known match position — no `find()` scan, no UTF-8 decode.
/// Scans forward past word characters to catch type declarations like `struct {`, `interface {`.
pub fn is_definition(phrase: &str, line: &str, match_start: usize) -> bool {
    let bytes = line.as_bytes();
    let end = match_start + phrase.len();
    if end >= bytes.len() { return false; }

    // Word boundary before the phrase
    if match_start > 0 {
        let prev = bytes[match_start - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' {
            return false;
        }
    }

    // Scan forward: skip whitespace, then word characters (reserved words like struct/extends),
    // then check for structural definition marker.
    // Catches: type Foo struct { | class Foo extends Bar { | trait Foo where Self:
    let mut pos = end;
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    while pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
        pos += 1;
    }
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    if pos < bytes.len() {
        let c = bytes[pos];
        if matches!(c, b'(' | b'<' | b'[' | b'=' | b':' | b'{') { return true; }
        if c == b'-' && pos + 1 < bytes.len() && bytes[pos + 1] == b'>' { return true; }
    }

    false
}

/// Legacy string-based signature for external callers that don't have match_start.
/// Uses `find()` to locate the phrase first.
pub fn is_definition_str(phrase: &str, line: &str) -> bool {
    if let Some(idx) = line.find(phrase) {
        is_definition(phrase, line, idx)
    } else {
        false
    }
}

/// Extract identifier phrases from text.
pub fn extract_phrases(text: &str) -> Vec<String> {
    PHRASE_RE.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// Hand-written byte scanner for [a-zA-Z_][a-zA-Z0-9_]{2,}.
/// Processes at memory bandwidth (~2GB/s) vs regex DFA (~200MB/s).
/// Zero allocations, zero state machine overhead.
pub fn scan_identifiers(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    std::iter::from_fn(move || {
        while i < len {
            // Scan for start: [a-zA-Z_]
            let b = bytes[i];
            if b.is_ascii_alphabetic() || b == b'_' {
                let start = i;
                i += 1;
                // Consume: [a-zA-Z0-9_]{3,}
                let mut count = 1u32;
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                    count += 1;
                }
                if count >= 3 {
                    return Some((start, &text[start..i]));
                }
            } else {
                i += 1;
            }
        }
        None
    })
}


