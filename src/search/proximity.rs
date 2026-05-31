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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sets() {
        let s: Vec<&[usize]> = vec![];
        assert!((proximity_bonus(&s, 10) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn single_set() {
        let s = vec![&[1usize, 5, 10][..]];
        assert!((proximity_bonus(&s, 10) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn exact_same_line() {
        let s = vec![&[5usize][..], &[5usize][..]];
        let b = proximity_bonus(&s, 10);
        let expected = (10.0 - 0.0 + 1.0) / 10.0;
        assert!((b - expected * 0.5).abs() < 1e-10, "got {}", b);
    }

    #[test]
    fn gap_within_bounds() {
        let s = vec![&[5usize][..], &[8usize][..]];
        let b = proximity_bonus(&s, 10);
        assert!(b > 0.0, "within max_gap should get bonus: {}", b);
    }

    #[test]
    fn gap_exceeds_max() {
        let s = vec![&[5usize][..], &[30usize][..]];
        let b = proximity_bonus(&s, 10);
        assert!((b - 0.0).abs() < 1e-10, "exceeds max_gap should get zero: {}", b);
    }

    #[test]
    fn multiple_sets_adjacent() {
        let s = vec![&[1usize][..], &[2usize][..], &[3usize][..]];
        let b = proximity_bonus(&s, 10);
        assert!(b > 0.0, "adjacent sets should get bonus: {}", b);
    }

    #[test]
    fn three_sets_one_far() {
        let s = vec![&[1usize][..], &[2usize][..], &[100usize][..]];
        let b = proximity_bonus(&s, 10);
        assert!(b < 0.3, "far set should reduce average: {}", b);
    }

    #[test]
    fn zero_gap_max() {
        let s = vec![&[5usize][..], &[5usize][..]];
        let b = proximity_bonus(&s, 0);
        // max_gap=0: only exact same line gets bonus
        assert!(b > 0.0, "same line at zero gap should get bonus: {}", b);
    }

    #[test]
    fn multiple_elements_per_set() {
        let s = vec![&[1usize, 20, 50][..], &[3usize, 22, 55][..]];
        let b = proximity_bonus(&s, 10);
        assert!(b > 0.0, "should find min distance (gap=2): {}", b);
    }
}
