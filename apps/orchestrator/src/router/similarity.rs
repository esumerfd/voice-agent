//! Pure cosine similarity (ROUT-01) -- a plain dot-product-over-norms loop,
//! no crate, per STACK.md (microseconds at "tens of workflows" scale).
//! Never panics or asserts on data that arrived over the network: returning
//! an `Option` (rather than 09-RESEARCH.md's sketched `debug_assert_eq!`)
//! is deliberate -- both embeddings originate from an Ollama HTTP response,
//! and this crate never asserts on external input.

/// Cosine similarity between two embeddings. Returns `None` -- never a
/// panic, never a number a caller could mistake for a valid (if low) score
/// -- when the slice lengths differ, either norm is zero, or the computed
/// value is not finite (ROUT-02 precision probe).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return None;
    }
    let score = dot / (norm_a * norm_b);
    if score.is_finite() {
        Some(score)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_unit_vectors_score_one() {
        let a = [1.0_f32, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &a), Some(1.0));
    }

    #[test]
    fn orthogonal_vectors_score_zero() {
        let a = [1.0_f32, 0.0];
        let b = [0.0_f32, 1.0];
        let score = cosine_similarity(&a, &b).expect("orthogonal, non-zero vectors must score");
        assert!(score.abs() < 1e-6, "expected ~0.0, got {score}");
    }

    #[test]
    fn opposite_vectors_score_negative_one() {
        let a = [1.0_f32, 0.0];
        let b = [-1.0_f32, 0.0];
        let score = cosine_similarity(&a, &b).expect("opposite vectors must score");
        assert!((score + 1.0).abs() < 1e-6, "expected ~-1.0, got {score}");
    }

    #[test]
    fn mismatched_dimensions_return_none() {
        let a = [1.0_f32, 0.0, 0.0];
        let b = [1.0_f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);
    }

    #[test]
    fn a_zero_vector_returns_none_rather_than_dividing_by_zero() {
        let a = [0.0_f32, 0.0];
        let b = [1.0_f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);
        assert_eq!(cosine_similarity(&b, &a), None);
    }

    #[test]
    fn non_finite_input_never_produces_a_comparable_score() {
        let a = [f32::NAN, 0.0];
        let b = [1.0_f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);

        let a = [f32::INFINITY, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);
    }
}
