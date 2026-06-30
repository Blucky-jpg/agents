/// Elo rating math for hypothesis tournament.
///
/// Standard Elo with configurable K-factor. Used by the ranking agent
/// to update hypothesis ratings after pairwise comparisons.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    A,
    B,
    Draw,
}

/// Expected score for player A given both ratings.
pub fn expected_score(rating_a: f64, rating_b: f64) -> f64 {
    1.0 / (1.0 + 10f64.powf((rating_b - rating_a) / 400.0))
}

/// Update Elo ratings after a match.
///
/// Returns `(new_rating_a, new_rating_b)`.
pub fn update_elo(rating_a: f64, rating_b: f64, winner: Winner, k: f64) -> (f64, f64) {
    let ea = expected_score(rating_a, rating_b);
    let eb = 1.0 - ea;
    let (sa, sb) = match winner {
        Winner::A => (1.0, 0.0),
        Winner::B => (0.0, 1.0),
        Winner::Draw => (0.5, 0.5),
    };
    let new_a = rating_a + k * (sa - ea);
    let new_b = rating_b + k * (sb - eb);
    (new_a, new_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_ratings_draw_gives_no_change() {
        let (a, b) = update_elo(1200.0, 1200.0, Winner::Draw, 32.0);
        assert!((a - 1200.0).abs() < 0.01);
        assert!((b - 1200.0).abs() < 0.01);
    }

    #[test]
    fn underdog_wins_gets_more_points() {
        let (a, b) = update_elo(1000.0, 1400.0, Winner::A, 32.0);
        // Underdog A wins: gains ~29 points, B loses ~29
        assert!(a > 1028.0);
        assert!(b < 1372.0);
    }

    #[test]
    fn favorite_wins_gets_fewer_points() {
        let (a, b) = update_elo(1400.0, 1000.0, Winner::A, 32.0);
        // Favorite A wins: gains ~3 points
        assert!(a < 1404.0);
        assert!(b > 996.0);
    }

    #[test]
    fn expected_score_symmetry() {
        let e = expected_score(1200.0, 1200.0);
        assert!((e - 0.5).abs() < 0.001);
    }

    #[test]
    fn expected_score_higher_rated_favored() {
        let e = expected_score(1600.0, 1200.0);
        assert!(e > 0.9);
    }

    #[test]
    fn k_factor_scales_changes() {
        let (a1, _) = update_elo(1200.0, 1200.0, Winner::A, 16.0);
        let (a2, _) = update_elo(1200.0, 1200.0, Winner::A, 32.0);
        assert!((a2 - 1200.0) > (a1 - 1200.0));
    }
}
