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
pub fn line_zone(line: &str) -> u8 {
    let s = line.trim();
    if s.is_empty() { return 1; }

    // Comment markers
    if let Some(m) = COMMENT_RE.find(s) {
        let marker = m.as_str().trim();
        if marker.starts_with('#') {
            // C preprocessor is code
            if s.starts_with("#!") || s.starts_with("#include") || s.starts_with("#define")
                || s.starts_with("#if") || s.starts_with("#endif")
                || s.starts_with("#pragma") || s.starts_with("#error") || s.starts_with("#warning")
            {
                return 0;
            }
            return 1;
        }
        return 1; // prose comment
    }

    // Structural character density
    let structural = s.chars().filter(|c| "{}()[]<>;:=|&!@#$%^*-+/?\\".contains(*c)).count();
    let lower = s.chars().filter(|c| c.is_ascii_lowercase()).count();
    let idents = PHRASE_RE.find_iter(s).count();
    let slen = s.len().max(1);
    let prose_ratio = lower as f64 / slen as f64;
    let struct_ratio = structural as f64 / slen as f64;

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

    0 // code
}

/// Grammar-free definition detection: checks if phrase is followed by `(`, `<`, `[`, `=`, `:`.
pub fn is_definition(phrase: &str, line: &str) -> bool {
    let s = line.trim();
    if let Some(idx) = s.find(phrase) {
        // Word boundary before
        if idx > 0 {
            let pc = s.as_bytes()[idx - 1] as char;
            if pc.is_alphanumeric() || pc == '_' || pc == '.' {
                return false;
            }
        }
        let after = s[idx + phrase.len()..].trim();
        // Structural follows
        if after.starts_with('(') || after.starts_with('<') || after.starts_with('[')
            || after.starts_with('=') || after.starts_with(':') || after.starts_with('{')
            || after.starts_with("->")
        {
            return true;
        }
        // Lowercase keyword + structural char
        if let Some(p) = after.find(|c: char| c == '(' || c == '{' || c == '<' || c == '[') {
            let prefix = &after[..p].trim();
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                return true;
            }
        }
    }
    false
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
