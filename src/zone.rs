use regex::Regex;
use std::sync::LazyLock;

/// Shared regex for identifier extraction (grammar-free, 3+ char identifiers)
pub static PHRASE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{3,}").unwrap()
});

static COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(//|#|--|%|/\*|\*)").unwrap()
});

/// Classify a line as 'code' (0) or 'prose' (1).
/// Grammar-free: uses structural character frequency + comment markers.
/// Byte-based for cache efficiency: no UTF-8 decoding on ASCII source code.
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
    let mut idents = 0u32;
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

    // Approximate ident count via PHRASE_RE (still needed for accuracy)
    idents = PHRASE_RE.find_iter(s).count() as u32;

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

    // First non-whitespace char after the phrase
    let after_slice = &bytes[end..];
    let first_non_space = after_slice.iter().position(|&b| b != b' ' && b != b'\t');
    let pos = first_non_space.map(|p| end + p).unwrap_or(bytes.len());

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

pub const COMMON_KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "return", "break", "continue", "switch", "case",
    "default", "try", "catch", "finally", "throw", "throws", "function", "class",
    "struct", "interface", "enum", "import", "export", "from", "as", "let", "const",
    "var", "def", "pass", "yield", "await", "async", "public", "private", "protected",
    "static", "new", "this", "super", "self", "true", "false", "null", "undefined",
    "none", "nil", "void", "int", "string", "bool", "boolean", "float", "double",
    "elif", "in", "is", "not", "and", "or", "lambda", "global", "nonlocal", "del", "with",
    "the", "that", "for", "then", "also", "var", "let", "mut", "val",
    "set", "get", "has", "not", "too", "via", "use", "pub", "mod", "type",
    "impl", "trait", "fn", "where", "as", "ref", "enum", "union", "async",
    "await", "yield", "loop", "match", "self", "super",
    "crate", "default", "derive", "inline", "override", "virtual", "explicit",
    "auto", "register", "extern", "template", "typename",
    "namespace", "using", "dynamic", "sealed", "abstract", "readonly",
];
