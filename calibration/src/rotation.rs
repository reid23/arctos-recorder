//! Rotations fitted to matched rays.

use nalgebra::{Matrix3, Rotation3, Vector3};

/// Attempts at most when fitting robustly.
const MAX_ATTEMPTS: usize = 2000;
/// Chance of having drawn at least one pair of inliers when fitting stops.
const CONFIDENCE: f64 = 0.999;
/// Times the fit is redone with the inliers of the last one.
const REFINEMENTS: usize = 5;

/// A pair of unit rays seeing the same thing: `to` is where `from` should
/// end up.
pub type RayPair = (Vector3<f64>, Vector3<f64>);

/// The rotation \(R\) maximizing \(\mathrm{tr}(R^T M)\) for a `matrix`
/// \(M\): the rotation nearest to it.
pub fn nearest_rotation(matrix: &Matrix3<f64>) -> Rotation3<f64> {
    let svd = matrix.svd(true, true);
    let u = svd.u.expect("asked for U");
    let v_transpose = svd.v_t.expect("asked for V^T");
    let reflection = (u * v_transpose).determinant().signum();
    let correction = Matrix3::from_diagonal(&Vector3::new(1.0, 1.0, reflection));
    Rotation3::from_matrix_unchecked(u * correction * v_transpose)
}

/// The rotation best taking each pair's `from` to its `to` (Kabsch's
/// method): maximizes \(\sum_i b_i \cdot R a_i\).
pub fn fit_rotation<'a>(pairs: impl IntoIterator<Item = &'a RayPair>) -> Rotation3<f64> {
    let covariance = pairs
        .into_iter()
        .fold(Matrix3::zeros(), |sum, (from, to)| sum + to * from.transpose());
    nearest_rotation(&covariance)
}

/// A rotation fitted despite wrong pairs.
#[derive(Clone, Debug)]
pub struct RobustFit {
    pub rotation: Rotation3<f64>,
    /// Whether each pair agrees with the rotation.
    pub inliers: Vec<bool>,
}

impl RobustFit {
    pub fn inlier_count(&self) -> usize {
        self.inliers.iter().filter(|&&inlier| inlier).count()
    }
}

/// Fits a rotation to `pairs`, some of which may be wrong (RANSAC): a pair
/// agrees with a rotation if it takes `from` within `threshold` radians of
/// `to`. `None` with fewer than two pairs.
pub fn fit_rotation_robustly(pairs: &[RayPair], threshold: f64) -> Option<RobustFit> {
    if pairs.len() < 2 {
        return None;
    }
    let least_cosine = threshold.cos();
    let agreeing = |rotation: &Rotation3<f64>| -> Vec<bool> {
        pairs
            .iter()
            .map(|(from, to)| (rotation * from).dot(to) >= least_cosine)
            .collect()
    };
    let count = |inliers: &[bool]| inliers.iter().filter(|&&inlier| inlier).count();

    let mut random = SplitMix64(0x5EED);
    let mut best: Option<(usize, Rotation3<f64>)> = None;
    let mut attempts_needed = MAX_ATTEMPTS;
    let mut attempt = 0;
    while attempt < attempts_needed {
        attempt += 1;
        let first = random.below(pairs.len());
        let second = random.below(pairs.len());
        if pairs[first].0.cross(&pairs[second].0).norm() < 1e-3 {
            continue;
        }
        let rotation = fit_rotation([&pairs[first], &pairs[second]]);
        let agree = count(&agreeing(&rotation));
        if best.is_none_or(|(most, _)| agree > most) {
            best = Some((agree, rotation));
            let share = agree as f64 / pairs.len() as f64;
            let miss = 1.0 - share * share;
            if miss <= 0.0 {
                break;
            }
            if miss < 1.0 {
                let needed = (1.0 - CONFIDENCE).ln() / miss.ln();
                attempts_needed = (needed.ceil() as usize).clamp(1, MAX_ATTEMPTS);
            }
        }
    }
    let (_, mut rotation) = best?;
    let mut inliers = agreeing(&rotation);
    for _ in 0..REFINEMENTS {
        if count(&inliers) < 2 {
            break;
        }
        rotation = fit_rotation(
            pairs
                .iter()
                .zip(&inliers)
                .filter(|&(_, &inlier)| inlier)
                .map(|(pair, _)| pair),
        );
        let refined = agreeing(&rotation);
        if refined == inliers {
            break;
        }
        inliers = refined;
    }
    Some(RobustFit { rotation, inliers })
}

/// The rotations of two cameras whose frames are related by `relative`
/// (\(R_2^{-1} R_1\), taking directions in the first camera's frame to the
/// second's) that are closest to `nominal` (in the sum of squared
/// differences of the rotation matrices).
pub fn closest_pair(relative: &Rotation3<f64>, nominal: [&Rotation3<f64>; 2]) -> [Rotation3<f64>; 2] {
    // With \(R_1 = R_2 Q\), the distance is least where
    // \(\mathrm{tr}(R_2^T (N_1 Q^T + N_2))\) is greatest.
    let second = nearest_rotation(
        &(nominal[0].matrix() * relative.matrix().transpose() + nominal[1].matrix()),
    );
    [second * relative, second]
}

/// A small, fast pseudorandom generator (Vigna's SplitMix64), so fits are
/// repeatable.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotation(yaw: f64, pitch: f64, roll: f64) -> Rotation3<f64> {
        Rotation3::from_euler_angles(roll.to_radians(), pitch.to_radians(), yaw.to_radians())
    }

    fn angle_between(a: &Rotation3<f64>, b: &Rotation3<f64>) -> f64 {
        nalgebra::UnitQuaternion::from_rotation_matrix(&a.rotation_to(b)).angle()
    }

    fn rays(count: usize) -> Vec<Vector3<f64>> {
        let mut random = SplitMix64(7);
        let mut unit = || random.next() as f64 / u64::MAX as f64 * 2.0 - 1.0;
        (0..count)
            .map(|_| Vector3::new(unit() * 0.5, unit() * 0.4, 1.0).normalize())
            .collect()
    }

    #[test]
    fn fits_the_rotation_between_exact_pairs() {
        let truth = rotation(80.0, -10.0, 3.0);
        let pairs: Vec<RayPair> = rays(20).into_iter().map(|ray| (ray, truth * ray)).collect();
        assert!(angle_between(&fit_rotation(&pairs), &truth) < 1e-6);
    }

    #[test]
    fn robust_fit_ignores_wrong_pairs() {
        let truth = rotation(-95.0, 4.0, -2.0);
        let sources = rays(100);
        let mut random = SplitMix64(11);
        let pairs: Vec<RayPair> = sources
            .iter()
            .enumerate()
            .map(|(index, ray)| {
                if index % 5 < 2 {
                    (*ray, sources[random.below(sources.len())])
                } else {
                    let nudge = rotation(0.0, 0.01 * (index % 3) as f64, 0.0);
                    (*ray, nudge * truth * ray)
                }
            })
            .collect();
        let fit = fit_rotation_robustly(&pairs, 0.002).unwrap();
        assert!(angle_between(&fit.rotation, &truth) < 1e-3);
        for (index, inlier) in fit.inliers.iter().enumerate() {
            assert_eq!(*inlier, index % 5 >= 2, "pair {index}");
        }
    }

    #[test]
    fn closest_pair_keeps_a_consistent_nominal_pair() {
        let first = rotation(-53.0, -25.0, 0.0);
        let second = rotation(53.0, -25.0, 0.0);
        let relative = second.inverse() * first;
        let [found_first, found_second] = closest_pair(&relative, [&first, &second]);
        assert!(angle_between(&found_first, &first) < 1e-6);
        assert!(angle_between(&found_second, &second) < 1e-6);
    }

    #[test]
    fn closest_pair_splits_the_difference() {
        let first = rotation(-50.0, -25.0, 0.0);
        let second = rotation(50.0, -25.0, 0.0);
        let relative = rotation(53.0, -25.0, 0.0).inverse() * rotation(-53.0, -25.0, 0.0);
        let [found_first, found_second] = closest_pair(&relative, [&first, &second]);
        assert!(angle_between(&(found_second.inverse() * found_first), &relative) < 1e-6);
        let first_change = angle_between(&found_first, &first);
        let second_change = angle_between(&found_second, &second);
        assert!((first_change - second_change).abs() < 1e-6);
        assert!((first_change - 3.0_f64.to_radians()).abs() < 0.2_f64.to_radians());
    }
}
