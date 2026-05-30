// LCEP: Left-Context Entropy Projection
// Grammar-free definition detection via Shannon entropy of left-context words.
// A phrase with low-entropy left context (always preceded by "func", "struct") 
// is likely a definition. High-entropy = usage.

use std::collections::HashMap;

/// Compute Shannon entropy of left-context distribution.
/// Returns None if fewer than 3 samples (unreliable).
pub fn left_context_entropy(ctx_counts: &HashMap<String, u32>) -> Option<f64> {
    let total: u32 = ctx_counts.values().sum();
    if total < 3 { return None; }

    let entropy: f64 = ctx_counts.values()
        .map(|c| {
            let p = *c as f64 / total as f64;
            -p * p.log2()
        })
        .sum();
    Some(entropy)
}

/// Classify a phrase as definition based on left-context entropy and document frequency.
/// Returns is_def score: 2 (strong def), 1 (likely def), 0 (usage), -1 (unknown).
pub fn classify_definition(
    phrase_df_count: u32,
    left_ctx_entropy: Option<f64>,
) -> i32 {
    match left_ctx_entropy {
        Some(entropy) if phrase_df_count < 20 && entropy < 1.0 => 2,  // strong def
        Some(entropy) if phrase_df_count < 20 && entropy < 2.0 => 1,  // likely def
        Some(entropy) if phrase_df_count >= 20 && entropy > 2.5 => 0, // usage (common term)
        Some(_) => 1,  // default to definition for sampled phrases
        None if phrase_df_count < 10 => -1,  // unknown, weak signal
        None => 0,  // no data = usage
    }
}
