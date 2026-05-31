use regex::Regex;
/// BM25 scoring for phrase search.
/// Uses K1=1.2, b=0.75 with logarithmic TF scaling.
use std::sync::LazyLock;

// Regex for phrase extraction used across all modules
pub static PHRASE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{3,}").unwrap());

pub fn bm25_idf(n_docs: f64, df: f64) -> f64 {
    ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln()
}

pub fn bm25_score(idf: f64, tf: f64, doc_len: f64, avgdl: f64) -> f64 {
    let k1 = 1.2;
    let b = 0.75;
    let log_tf = (1.0 + tf).ln();
    idf * (log_tf * (k1 + 1.0)) / (log_tf + k1 * (1.0 - b + b * doc_len / avgdl))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_idf_all_docs_matched() {
        let idf = bm25_idf(100.0_f64, 100.0_f64);
        let expected = ((100.0_f64 - 100.0_f64 + 0.5) / (100.0_f64 + 0.5) + 1.0).ln();
        assert!((idf - expected).abs() < 1e-10, "idf={}", idf);
    }

    #[test]
    fn bm25_idf_single_doc() {
        let idf = bm25_idf(100.0_f64, 1.0_f64);
        let expected = ((100.0_f64 - 1.0_f64 + 0.5) / (1.0_f64 + 0.5) + 1.0).ln();
        assert!(
            (idf - expected).abs() < 1e-10,
            "idf={} expected={}",
            idf,
            expected
        );
    }

    #[test]
    fn bm25_idf_no_matches() {
        let idf = bm25_idf(100.0_f64, 0.0_f64);
        assert!(idf > 0.0, "idf should be positive for zero df");
        let expected = ((100.0_f64 - 0.0_f64 + 0.5) / (0.0_f64 + 0.5) + 1.0).ln();
        assert!(
            (idf - expected).abs() < 1e-10,
            "idf={} expected={}",
            idf,
            expected
        );
    }

    #[test]
    fn bm25_idf_zero_docs() {
        let idf = bm25_idf(0.0, 1.0);
        assert!(!idf.is_nan(), "idf should not be NaN");
    }

    #[test]
    fn bm25_score_zero_tf() {
        let s = bm25_score(2.0, 0.0, 50.0, 100.0);
        assert!(
            (s - 0.0).abs() < 1e-10,
            "zero tf should give zero score, got {}",
            s
        );
    }

    #[test]
    fn bm25_score_single_term() {
        let s = bm25_score(2.0, 1.0, 50.0, 100.0);
        assert!(s > 0.0 && s < 5.0, "score out of range: {}", s);
    }

    #[test]
    fn bm25_score_high_tf() {
        let s1 = bm25_score(2.0, 1.0, 50.0, 100.0);
        let s2 = bm25_score(2.0, 100.0, 50.0, 100.0);
        assert!(
            s2 > s1,
            "higher tf should give higher score: {} vs {}",
            s2,
            s1
        );
    }

    #[test]
    fn bm25_score_short_doc_boost() {
        let short = bm25_score(2.0, 1.0, 10.0, 100.0);
        let long = bm25_score(2.0, 1.0, 1000.0, 100.0);
        assert!(
            short > long,
            "short doc should score higher: {} vs {}",
            short,
            long
        );
    }

    #[test]
    fn bm25_score_zero_avgdl() {
        let s = bm25_score(2.0, 1.0, 10.0, 0.0);
        assert!(
            !s.is_nan() && !s.is_infinite(),
            "zero avgdl should not crash: {}",
            s
        );
    }

    #[test]
    fn bm25_score_idf_ordering() {
        let high = bm25_score(5.0, 1.0, 50.0, 100.0);
        let low = bm25_score(1.0, 1.0, 50.0, 100.0);
        assert!(
            high > low,
            "higher idf should score higher: {} vs {}",
            high,
            low
        );
    }
}
