// LCEP: Left-Context Entropy Projection
// Grammar-free definition detection via Shannon entropy of left-context words.
// A phrase with low-entropy left context (always preceded by "func", "struct")
// is likely a definition. High-entropy = usage.

use std::collections::HashMap;

/// Compute Shannon entropy of left-context distribution.
/// Returns None if fewer than 3 samples (unreliable).
pub fn left_context_entropy(ctx_counts: &HashMap<String, u32>) -> Option<f64> {
    let total: u32 = ctx_counts.values().sum();
    if total < 3 {
        return None;
    }

    let entropy: f64 = ctx_counts
        .values()
        .map(|c| {
            let p = *c as f64 / total as f64;
            -p * p.log2()
        })
        .sum();
    Some(entropy)
}

/// Classify a phrase as definition based on left-context entropy and document frequency.
/// Returns is_def score: 2 (strong def), 1 (likely def), 0 (usage), -1 (unknown).
pub fn classify_definition(phrase_df_count: u32, left_ctx_entropy: Option<f64>) -> i32 {
    match left_ctx_entropy {
        Some(entropy) if phrase_df_count < 20 && entropy < 1.0 => 2, // strong def
        Some(entropy) if phrase_df_count < 20 && entropy < 2.0 => 1, // likely def
        Some(entropy) if phrase_df_count >= 20 && entropy > 2.5 => 0, // usage (common term)
        Some(_) => 1,                       // default to definition for sampled phrases
        None if phrase_df_count < 10 => -1, // unknown, weak signal
        None => 0,                          // no data = usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn entropy_empty() {
        let m: HashMap<String, u32> = HashMap::new();
        assert_eq!(left_context_entropy(&m), None);
    }

    #[test]
    fn entropy_few_samples() {
        let mut m = HashMap::new();
        m.insert("fn".to_string(), 1);
        assert_eq!(left_context_entropy(&m), None);
    }

    #[test]
    fn entropy_zero_entropy() {
        let mut m = HashMap::new();
        m.insert("fn".to_string(), 10);
        let e = left_context_entropy(&m).unwrap();
        assert!(
            (e - 0.0).abs() < 1e-10,
            "single context should have zero entropy: {}",
            e
        );
    }

    #[test]
    fn entropy_maximum() {
        let mut m = HashMap::new();
        for i in 0..8 {
            m.insert(format!("ctx_{}", i), 1);
        }
        let e = left_context_entropy(&m).unwrap();
        assert!(
            (e - 3.0).abs() < 0.01,
            "8 equally-likely contexts should give 3.0 bits: {}",
            e
        );
    }

    #[test]
    fn entropy_varied() {
        let mut m = HashMap::new();
        m.insert("fn".to_string(), 8);
        m.insert("let".to_string(), 2);
        let e = left_context_entropy(&m).unwrap();
        assert!(
            e > 0.0 && e < 1.0,
            "unbalanced distribution should give < 1.0: {}",
            e
        );
    }

    #[test]
    fn classify_strong_def_low_entropy() {
        assert_eq!(classify_definition(5, Some(0.5)), 2);
    }

    #[test]
    fn classify_likely_def_medium_entropy() {
        assert_eq!(classify_definition(5, Some(1.5)), 1);
    }

    #[test]
    fn classify_usage_high_entropy_common() {
        assert_eq!(classify_definition(50, Some(3.0)), 0);
    }

    #[test]
    fn classify_default_def_for_sampled() {
        assert_eq!(classify_definition(50, Some(1.5)), 1);
    }

    #[test]
    fn classify_unknown_low_df_no_entropy() {
        assert_eq!(classify_definition(5, None), -1);
    }

    #[test]
    fn classify_high_df_no_entropy() {
        assert_eq!(classify_definition(50, None), 0);
    }

    #[test]
    fn classify_boundary_df_20() {
        // Exactly at boundary
        assert_eq!(classify_definition(20, Some(0.5)), 1);
        assert_eq!(classify_definition(19, Some(0.5)), 2);
    }

    #[test]
    fn classify_boundary_entropy_1_0() {
        assert_eq!(classify_definition(15, Some(0.99)), 2);
        assert_eq!(classify_definition(15, Some(1.01)), 1);
    }
}
