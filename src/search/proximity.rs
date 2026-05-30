/// Line-number proximity bonus.
/// Computes bonus when 2+ query terms appear within N lines of each other.

pub fn proximity_bonus(line_sets: &[&[usize]], max_gap: usize) -> f64 {
    if line_sets.len() < 2 { return 0.0; }
    let mut total = 0.0f64;
    let mut pairs = 0u32;

    for i in 0..line_sets.len() {
        for j in (i + 1)..line_sets.len() {
            let mut min_dist = usize::MAX;
            for &a in line_sets[i] {
                for &b in line_sets[j] {
                    let dist = if a > b { a - b } else { b - a };
                    if dist < min_dist { min_dist = dist; }
                }
            }
            if min_dist <= max_gap {
                let bonus = (max_gap - min_dist + 1) as f64 / max_gap as f64;
                total += bonus;
            }
            pairs += 1;
        }
    }

    if pairs == 0 { 0.0 } else { total / pairs as f64 * 0.5 }
}
