/// BM25 scoring for phrase search.
/// Uses K1=1.2, b=0.75 with logarithmic TF scaling.

use std::sync::LazyLock;
use regex::Regex;

// Regex for phrase extraction used across all modules
pub static PHRASE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{3,}").unwrap()
});

pub fn bm25_idf(n_docs: f64, df: f64) -> f64 {
    ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln()
}

pub fn bm25_score(idf: f64, tf: f64, doc_len: f64, avgdl: f64) -> f64 {
    let k1 = 1.2;
    let b = 0.75;
    let log_tf = (1.0 + tf).ln();
    idf * (log_tf * (k1 + 1.0)) / (log_tf + k1 * (1.0 - b + b * doc_len / avgdl))
}
