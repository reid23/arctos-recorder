//! Finding how the rig's cameras are turned from what they both see. Both
//! cameras' previews are reprojected into a pinhole view of where they
//! overlap, so the same things look alike in both; SIFT features are
//! matched between the two, and a rotation is fitted to the matches with
//! RANSAC. Lenses fitted to a chessboard one at a time rarely agree exactly
//! where the views overlap, which a rotation alone can't make up for, so
//! the fit also scales the two lenses' focal lengths apart. Matches only fix
//! how the cameras are turned relative to each other (and their lenses'
//! relative scale), so the pair of orientations closest to the nominal
//! mount is chosen.

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use nalgebra::{DMatrix, DVector, Matrix4, Rotation3, Vector2, Vector3, Vector4};
use opencv::core::{DMatch, KeyPoint, Mat, Point, Scalar, Vec3b, Vector};
use opencv::features2d::{BFMatcher, KeyPointsFilter, SIFT};
use opencv::prelude::*;
use opencv::{core, imgproc};
use serde::{Deserialize, Serialize};
use stitching::geometry::{Calibration, Lens, Orientation};

use crate::rotation::{self, RayPair};
use crate::{Check, picture};

/// Features found in each view at most.
const FEATURES: i32 = 4000;
/// A match is kept only if it is this much closer than the next best
/// (Lowe's ratio test), both ways round.
const RATIO: f32 = 0.75;
/// How far (in view pixels) a match may be from where the fitted rotation
/// puts it and still agree.
const INLIER_PIXELS: f64 = 3.0;
/// View pixels kept clear of each camera's picture edge, so the edge itself
/// doesn't make features.
const EDGE_MARGIN: f64 = 8.0;
/// Times the previews are matched, each time reprojected with the last
/// alignment found so more features line up.
const PASSES: usize = 2;
/// [`INLIER_PIXELS`] for matching the whole previews, where lens errors
/// add up toward the edges.
const WHOLE_INLIER_PIXELS: f64 = 6.0;
/// Agreeing matches between the whole previews needed to trust them over
/// the orientations aligned from.
const TRUSTED_WHOLE_INLIERS: usize = 20;
/// Pixels between the points sampled to find where the cameras overlap.
const OVERLAP_SAMPLE_STEP: usize = 8;
/// Degrees of margin around the overlap in the view.
const VIEW_MARGIN: f64 = 6.0;
/// Bounds on half the view's field of view in each direction, in degrees.
const VIEW_HALF_FOV: (f64, f64) = (12.0, 50.0);
/// Half the view's field of view (across, down) in degrees when the
/// cameras don't seem to overlap at all.
const BLIND_HALF_FOV: (f64, f64) = (40.0, 30.0);
/// Most match lines drawn.
const DRAWN_MATCHES: usize = 300;
/// Times the rotation and focal lengths are refitted to the matches that
/// agree with the last fit.
const REFITS: usize = 10;
/// The most steps each refit takes.
const REFIT_STEPS: usize = 50;
/// Share of the agreeing matches left out at the top and at the bottom when
/// measuring how much of the overlap they span.
const SPAN_TRIM: f64 = 0.05;

pub const ENOUGH_INLIERS: usize = 40;
pub const GOOD_INLIER_SHARE: f64 = 0.3;
/// The most a good alignment's matches may disagree (root mean square, in
/// view pixels, about the size of preview pixels).
pub const GOOD_ERROR: f64 = 1.5;
/// The least share of the overlap's height a good alignment's matches span,
/// so that how the lenses' scales compare is pinned down.
pub const GOOD_SPAN: f64 = 0.4;

/// The cameras' orientations found from their previews.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alignment {
    /// The calibration aligned from, with the orientations found and the
    /// lenses' focal lengths scaled by [`Alignment::focal_scales`].
    pub calibration: Calibration,
    /// What each lens's focal lengths were scaled by for the two to agree
    /// where the views overlap.
    pub focal_scales: [f64; 2],
    pub matches: usize,
    /// Matches that agree with the orientations found.
    pub inliers: usize,
    /// How far the agreeing matches are from where the orientations put
    /// them (root mean square, in view pixels).
    pub error: f64,
    /// Share of the height where the views overlap that the agreeing
    /// matches span.
    pub span: f64,
    /// Matches in each pair of previews.
    pub pairs: Vec<PairMatches>,
    #[serde(skip)]
    pub pictures: AlignmentPictures,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairMatches {
    pub matches: usize,
    pub inliers: usize,
}

/// What the alignment saw, in the view of where the cameras overlap, for
/// the pair of previews with the most agreeing matches. The overlays show
/// the left camera in red and the right in cyan, so the views line up where
/// it's gray.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlignmentPictures {
    /// Each camera's view with its features: agreeing matches in green,
    /// other matches in red, the rest gray.
    pub features: [Bytes; 2],
    /// The views side by side with lines between matches.
    pub matches: Bytes,
    pub before: Bytes,
    pub after: Bytes,
}

impl Alignment {
    pub fn checks(&self) -> Vec<Check> {
        let share = self.inliers as f64 / self.matches.max(1) as f64;
        vec![
            Check::new(
                format!("{} matches agree (needs {ENOUGH_INLIERS})", self.inliers),
                self.inliers >= ENOUGH_INLIERS,
            ),
            Check::new(
                format!(
                    "{:.0}% of the matches agree (needs {:.0}%)",
                    share * 100.0,
                    GOOD_INLIER_SHARE * 100.0
                ),
                share >= GOOD_INLIER_SHARE,
            ),
            Check::new(
                format!("They agree within {:.2} px (needs under {GOOD_ERROR})", self.error),
                self.error < GOOD_ERROR,
            ),
            Check::new(
                format!(
                    "They span {:.0}% of the overlap's height (needs {:.0}%)",
                    self.span * 100.0,
                    GOOD_SPAN * 100.0
                ),
                self.span >= GOOD_SPAN,
            ),
        ]
    }
}

/// Finds the cameras' orientations from pairs of previews (JPEGs, indexed
/// by [`protocol::Side::index`]), starting from `calibration` and choosing
/// the orientations closest to `mount`.
pub fn align(previews: &[[Bytes; 2]], calibration: &Calibration, mount: &[Orientation; 2]) -> Result<Alignment> {
    ensure!(!previews.is_empty(), "no previews to align");
    let pictures: Vec<[Mat; 2]> = previews
        .iter()
        .map(|[left, right]| Ok([picture::decode_gray(left)?, picture::decode_gray(right)?]))
        .collect::<Result<_>>()?;
    let size = (pictures[0][0].cols() as u32, pictures[0][0].rows() as u32);
    for pair in &pictures {
        for picture in pair {
            ensure!(
                (picture.cols() as u32, picture.rows() as u32) == size,
                "the previews aren't all the same size"
            );
        }
    }
    let nominal = mount.map(|orientation| orientation.rotation());
    let mut current = calibration.clone();
    let turn = |calibration: &mut Calibration, relative: &Rotation3<f64>| {
        let [left, right] = rotation::closest_pair(relative, [&nominal[0], &nominal[1]]);
        calibration.cameras[0].orientation = Orientation::from_rotation(&left);
        calibration.cameras[1].orientation = Orientation::from_rotation(&right);
    };

    // The whole previews are matched first, which doesn't depend on how the
    // cameras are thought to be turned, to recover from a poor guess.
    let matched = match_previews(&Viewpoint::Whole, &pictures)?;
    let focal = (calibration.cameras[0].lens.fx + calibration.cameras[1].lens.fx) / 2.0
        * (size.0 as f64 / calibration.cameras[0].lens.width as f64);
    let whole_rays = rays(&matched.sources, &preview_lenses(calibration, size));
    if let Some(fit) = rotation::fit_rotation_robustly(&whole_rays, WHOLE_INLIER_PIXELS / focal)
        && fit.inlier_count() >= TRUSTED_WHOLE_INLIERS
    {
        turn(&mut current, &fit.rotation);
    }

    let frame = SeamFrame::around_overlap(&current, size);
    let starting_warp = Warp::new(&frame, calibration, size)?;
    let mut warp = Warp::new(&frame, &current, size)?;
    let mut last = None;
    for _ in 0..PASSES {
        let matched = match_previews(&Viewpoint::Seam(&frame, &warp), &pictures)?;
        let fit = fit_rotation_and_stretch(&matched.sources, &preview_lenses(&current, size), INLIER_PIXELS / frame.focal)
            .context("the cameras' previews have no features in common")?;
        turn(&mut current, &fit.rotation);
        for (camera, factor) in current.cameras.iter_mut().zip(focal_factors(fit.stretch)) {
            camera.lens = scale_focal_lengths(&camera.lens, factor);
        }
        warp = Warp::new(&frame, &current, size)?;
        last = Some((matched, fit));
    }
    let (matched, fit) = last.context("no passes")?;

    let mut pairs = vec![PairMatches { matches: 0, inliers: 0 }; pictures.len()];
    for (found, inlier) in matched.found.iter().zip(&fit.inliers) {
        pairs[found.pair].matches += 1;
        pairs[found.pair].inliers += *inlier as usize;
    }
    let squared_error: f64 = rays(&matched.sources, &preview_lenses(&current, size))
        .iter()
        .zip(&fit.inliers)
        .filter(|(_, inlier)| **inlier)
        .map(|((from, to), _)| ((fit.rotation * from).angle(to) * frame.focal).powi(2))
        .sum();
    let inliers = fit.inliers.iter().filter(|&&inlier| inlier).count();
    let best_pair = (0..pairs.len())
        .max_by_key(|&pair| pairs[pair].inliers)
        .unwrap_or(0);
    let pictures = AlignmentPictures {
        features: draw_features(&matched, &fit.inliers, best_pair)?,
        matches: draw_matches(&matched, &fit.inliers, best_pair)?,
        before: overlay(&starting_warp, &pictures[best_pair])?,
        after: overlay(&warp, &pictures[best_pair])?,
    };
    Ok(Alignment {
        focal_scales: [0, 1].map(|side| current.cameras[side].lens.fx / calibration.cameras[side].lens.fx),
        span: span(&frame, &warp, &matched, &fit.inliers),
        calibration: current,
        matches: matched.found.len(),
        inliers,
        error: (squared_error / inliers.max(1) as f64).sqrt(),
        pairs,
        pictures,
    })
}

/// `calibration`'s lenses, scaled to previews `size` big.
fn preview_lenses(calibration: &Calibration, size: (u32, u32)) -> [Lens; 2] {
    calibration.cameras.each_ref().map(|camera| camera.lens.scaled_to(size.0, size.1))
}

/// The rays (in the cameras' frames) where each match was seen in the
/// cameras' previews, through `lenses`.
fn rays(sources: &[[Vector2<f64>; 2]], lenses: &[Lens; 2]) -> Vec<RayPair> {
    let max_angles = lenses.each_ref().map(Lens::max_angle);
    sources
        .iter()
        .map(|[left, right]| {
            (
                lenses[0].unproject_within(left, max_angles[0]),
                lenses[1].unproject_within(right, max_angles[1]),
            )
        })
        .collect()
}

/// What a stretch \(s\) scales each lens's focal lengths by: the left's by
/// \(e^{s/2}\) and the right's by \(e^{-s/2}\), keeping their product.
fn focal_factors(stretch: f64) -> [f64; 2] {
    [(stretch / 2.0).exp(), (-stretch / 2.0).exp()]
}

fn scale_focal_lengths(lens: &Lens, factor: f64) -> Lens {
    Lens {
        fx: lens.fx * factor,
        fy: lens.fy * factor,
        ..lens.clone()
    }
}

/// The rotation between the cameras (taking rays in the left's frame to the
/// right's) and the stretch of their focal lengths, fitted to the matches
/// that agree with them.
struct Fit {
    rotation: Rotation3<f64>,
    stretch: f64,
    /// Whether each match agrees with the fit.
    inliers: Vec<bool>,
}

/// Fits a rotation with RANSAC to where matches were seen in the previews
/// (`sources`, through `lenses`), then refits it together with a stretch of
/// the lenses' focal lengths to the matches agreeing within `threshold`
/// radians, until those stay the same. `None` with fewer than two matches.
fn fit_rotation_and_stretch(sources: &[[Vector2<f64>; 2]], lenses: &[Lens; 2], threshold: f64) -> Option<Fit> {
    let robust = rotation::fit_rotation_robustly(&rays(sources, lenses), threshold)?;
    let mut fit = Fit {
        rotation: robust.rotation,
        stretch: 0.0,
        inliers: robust.inliers,
    };
    let least_cosine = threshold.cos();
    for _ in 0..REFITS {
        let agreeing: Vec<[Vector2<f64>; 2]> = sources
            .iter()
            .zip(&fit.inliers)
            .filter(|(_, inlier)| **inlier)
            .map(|(source, _)| *source)
            .collect();
        if agreeing.len() < 2 {
            break;
        }
        (fit.rotation, fit.stretch) = refit(&agreeing, lenses, fit.rotation, fit.stretch);
        let [left, right] = focal_factors(fit.stretch);
        let stretched = [scale_focal_lengths(&lenses[0], left), scale_focal_lengths(&lenses[1], right)];
        let inliers: Vec<bool> = rays(sources, &stretched)
            .iter()
            .map(|(from, to)| (fit.rotation * from).dot(to) >= least_cosine)
            .collect();
        if inliers == fit.inliers {
            break;
        }
        fit.inliers = inliers;
    }
    Some(fit)
}

/// The rotation and stretch best taking the left rays of `sources` to the
/// right ones, by least squares with the Levenberg-Marquardt method,
/// starting from `rotation` and `stretch`.
fn refit(
    sources: &[[Vector2<f64>; 2]],
    lenses: &[Lens; 2],
    rotation: Rotation3<f64>,
    stretch: f64,
) -> (Rotation3<f64>, f64) {
    // The values fitted: a turn (as a scaled axis) applied after `rotation`,
    // then the stretch.
    let fitted = |values: &Vector4<f64>| (Rotation3::new(values.xyz()) * rotation, values.w);
    let residuals = |values: &Vector4<f64>| -> DVector<f64> {
        let (rotation, stretch) = fitted(values);
        let [left, right] = focal_factors(stretch);
        let stretched = [scale_focal_lengths(&lenses[0], left), scale_focal_lengths(&lenses[1], right)];
        let pairs = rays(sources, &stretched);
        DVector::from_iterator(
            3 * pairs.len(),
            pairs.iter().flat_map(|(from, to)| {
                let difference = rotation * from - to;
                [difference.x, difference.y, difference.z]
            }),
        )
    };
    let mut values = Vector4::new(0.0, 0.0, 0.0, stretch);
    let mut current = residuals(&values);
    let mut cost = current.norm_squared();
    let mut damping = 1e-3;
    'steps: for _ in 0..REFIT_STEPS {
        let mut jacobian = DMatrix::zeros(current.len(), 4);
        for column in 0..4 {
            let step = 1e-6;
            let nudged = |change: f64| {
                let mut nudged = values;
                nudged[column] += change;
                residuals(&nudged)
            };
            jacobian.set_column(column, &((nudged(step) - nudged(-step)) / (2.0 * step)));
        }
        let normal: Matrix4<f64> = (jacobian.transpose() * &jacobian).fixed_view::<4, 4>(0, 0).into();
        let gradient: Vector4<f64> = (jacobian.transpose() * &current).fixed_rows::<4>(0).into();
        loop {
            if damping > 1e12 {
                break 'steps;
            }
            let mut damped = normal;
            for index in 0..4 {
                damped[(index, index)] += damping * normal[(index, index)].max(f64::EPSILON);
            }
            if let Some(cholesky) = damped.cholesky() {
                let next_values = values + cholesky.solve(&-gradient);
                let next = residuals(&next_values);
                let next_cost = next.norm_squared();
                if next_cost < cost {
                    let settled = cost - next_cost <= 1e-12 * cost;
                    (values, current, cost) = (next_values, next, next_cost);
                    damping = (damping / 10.0).max(1e-12);
                    if settled {
                        break 'steps;
                    }
                    continue 'steps;
                }
            }
            damping *= 10.0;
        }
    }
    fitted(&values)
}

/// Share of the height where both cameras see that the agreeing matches
/// span, leaving out the [`SPAN_TRIM`] highest and lowest.
fn span(frame: &SeamFrame, warp: &Warp, matched: &Matched, inliers: &[bool]) -> f64 {
    let width = frame.size.0 as usize;
    let shared_rows: Vec<usize> = (0..frame.size.1 as usize)
        .filter(|row| (row * width..(row + 1) * width).any(|index| warp.seen[0][index] && warp.seen[1][index]))
        .collect();
    let (Some(first), Some(last)) = (shared_rows.first(), shared_rows.last()) else {
        return 0.0;
    };
    let mut rows: Vec<f64> = matched
        .found
        .iter()
        .zip(inliers)
        .filter(|(_, inlier)| **inlier)
        .map(|(found, _)| found.pixels[0].y)
        .collect();
    if rows.is_empty() {
        return 0.0;
    }
    rows.sort_by(f64::total_cmp);
    let trimmed = (rows.len() as f64 * SPAN_TRIM) as usize;
    (rows[rows.len() - 1 - trimmed] - rows[trimmed]) / (last - first + 1) as f64
}

/// A pinhole view of the rig's surroundings where the cameras overlap,
/// level with the rig.
struct SeamFrame {
    /// Takes directions in the view's frame to the rig's.
    rotation: Rotation3<f64>,
    focal: f64,
    size: (u32, u32),
}

impl SeamFrame {
    /// Around the directions both cameras of `calibration` see, for
    /// previews `size` big.
    fn around_overlap(calibration: &Calibration, size: (u32, u32)) -> SeamFrame {
        let lenses = calibration.cameras.each_ref().map(|camera| camera.lens.scaled_to(size.0, size.1));
        let max_angles = lenses.each_ref().map(Lens::max_angle);
        let to_rig = calibration.cameras.each_ref().map(|camera| camera.orientation.rotation());
        let mut shared = Vec::new();
        for row in (0..size.1).step_by(OVERLAP_SAMPLE_STEP) {
            for column in (0..size.0).step_by(OVERLAP_SAMPLE_STEP) {
                let pixel = Vector2::new(column as f64, row as f64);
                let direction = to_rig[0] * lenses[0].unproject_within(&pixel, max_angles[0]);
                let seen = lenses[1].project_within(&(to_rig[1].inverse() * direction), max_angles[1]);
                if seen.is_some_and(|seen| lenses[1].contains(&seen)) {
                    shared.push(direction);
                }
            }
        }
        let forward = if shared.is_empty() {
            to_rig[0] * Vector3::z() + to_rig[1] * Vector3::z()
        } else {
            shared.iter().sum()
        }
        .normalize();
        let right = Vector3::y().cross(&forward).normalize();
        let down = forward.cross(&right);
        let rotation = Rotation3::from_basis_unchecked(&[right, down, forward]);

        let half_fov = if shared.is_empty() {
            BLIND_HALF_FOV
        } else {
            let to_view = rotation.inverse();
            let (mut across, mut up_down): (f64, f64) = (0.0, 0.0);
            for direction in &shared {
                let direction = to_view * direction;
                across = across.max(direction.x.atan2(direction.z).abs());
                up_down = up_down.max(direction.y.atan2(direction.z).abs());
            }
            let bounded = |angle: f64| (angle.to_degrees() + VIEW_MARGIN).clamp(VIEW_HALF_FOV.0, VIEW_HALF_FOV.1);
            (bounded(across), bounded(up_down))
        };
        let focal = (lenses[0].fx + lenses[1].fx) / 2.0;
        let extent = |half: f64| (2.0 * focal * half.to_radians().tan()).round() as u32;
        SeamFrame {
            rotation,
            focal,
            size: (extent(half_fov.0), extent(half_fov.1)),
        }
    }

    /// The direction (in the rig's frame) shown at `pixel`.
    fn direction_at(&self, pixel: &Vector2<f64>) -> Vector3<f64> {
        self.rotation
            * Vector3::new(
                pixel.x + 0.5 - self.size.0 as f64 / 2.0,
                pixel.y + 0.5 - self.size.1 as f64 / 2.0,
                self.focal,
            )
    }
}

/// Where in each camera's preview each pixel of a [`SeamFrame`] comes
/// from, given a calibration.
struct Warp {
    maps: [(Mat, Mat); 2],
    /// Whether each camera sees each pixel, row by row.
    seen: [Vec<bool>; 2],
    masks: [Mat; 2],
    /// Takes directions in the rig's frame to each camera's.
    to_camera: [Rotation3<f64>; 2],
    /// Each camera's lens (scaled to the previews) and its max angle.
    lenses: [(Lens, f64); 2],
}

impl Warp {
    fn new(frame: &SeamFrame, calibration: &Calibration, preview_size: (u32, u32)) -> Result<Warp> {
        let (width, height) = frame.size;
        type Built = ((Mat, Mat), Vec<bool>, Mat, Rotation3<f64>, (Lens, f64));
        let build = |side: usize| -> Result<Built> {
            let camera = &calibration.cameras[side];
            let lens = camera.lens.scaled_to(preview_size.0, preview_size.1);
            let max_angle = lens.max_angle();
            let to_camera = camera.orientation.rotation().inverse();
            let inside = |value: f64, size: u32| value >= EDGE_MARGIN && value <= size as f64 - 1.0 - EDGE_MARGIN;
            let pixel_count = (width * height) as usize;
            let (mut map_x, mut map_y) = (Vec::with_capacity(pixel_count), Vec::with_capacity(pixel_count));
            let mut seen = Vec::with_capacity(pixel_count);
            for row in 0..height {
                for column in 0..width {
                    let direction = to_camera * frame.direction_at(&Vector2::new(column as f64, row as f64));
                    let source = lens
                        .project_within(&direction, max_angle)
                        .filter(|source| inside(source.x, lens.width) && inside(source.y, lens.height));
                    let source = source.unwrap_or(Vector2::new(-1.0, -1.0));
                    map_x.push(source.x as f32);
                    map_y.push(source.y as f32);
                    seen.push(source.x >= 0.0);
                }
            }
            let mat = |values: &[f32]| -> Result<Mat> {
                Ok(Mat::new_rows_cols_with_data::<f32>(height as i32, width as i32, values)?.try_clone()?)
            };
            let mask_values: Vec<u8> = seen.iter().map(|&seen| if seen { 255 } else { 0 }).collect();
            let mask = Mat::new_rows_cols_with_data::<u8>(height as i32, width as i32, &mask_values)?.try_clone()?;
            Ok(((mat(&map_x)?, mat(&map_y)?), seen, mask, to_camera, (lens, max_angle)))
        };
        let (left, right) = (build(0)?, build(1)?);
        Ok(Warp {
            maps: [left.0, right.0],
            seen: [left.1, right.1],
            masks: [left.2, right.2],
            to_camera: [left.3, right.3],
            lenses: [left.4, right.4],
        })
    }

    fn reproject(&self, side: usize, picture: &Mat) -> Result<Mat> {
        let mut view = Mat::default();
        let (map_x, map_y) = &self.maps[side];
        imgproc::remap(
            picture,
            &mut view,
            map_x,
            map_y,
            imgproc::INTER_LINEAR,
            core::BORDER_CONSTANT,
            Scalar::all(0.0),
        )?;
        Ok(view)
    }
}

/// A match between the views of one pair of previews.
struct Found {
    pair: usize,
    /// Where it is in each view.
    pixels: [Vector2<f64>; 2],
}

/// The matches found in every pair of previews.
struct Matched {
    /// The views of each pair, as matched.
    views: Vec<[Mat; 2]>,
    keypoints: Vec<[Vec<Vector2<f64>>; 2]>,
    found: Vec<Found>,
    /// Where each match is in the two cameras' previews.
    sources: Vec<[Vector2<f64>; 2]>,
}

/// How the previews are looked at to match them.
enum Viewpoint<'a> {
    /// As they are.
    Whole,
    /// Reprojected into a view of where the cameras overlap.
    Seam(&'a SeamFrame, &'a Warp),
}

impl Viewpoint<'_> {
    fn view(&self, side: usize, picture: &Mat) -> Result<Mat> {
        match self {
            Viewpoint::Whole => Ok(picture.clone()),
            Viewpoint::Seam(_, warp) => warp.reproject(side, picture),
        }
    }

    /// Where features may be found; empty for anywhere.
    fn mask(&self, side: usize) -> Mat {
        match self {
            Viewpoint::Whole => Mat::default(),
            Viewpoint::Seam(_, warp) => warp.masks[side].clone(),
        }
    }

    /// Where in `side`'s preview `pixel` of its view comes from.
    fn source(&self, side: usize, pixel: &Vector2<f64>) -> Option<Vector2<f64>> {
        match self {
            Viewpoint::Whole => Some(*pixel),
            Viewpoint::Seam(frame, warp) => {
                let (lens, max_angle) = &warp.lenses[side];
                lens.project_within(&(warp.to_camera[side] * frame.direction_at(pixel)), *max_angle)
            }
        }
    }
}

fn match_previews(viewpoint: &Viewpoint, pictures: &[[Mat; 2]]) -> Result<Matched> {
    let per_pair: Vec<Result<([Mat; 2], [Vec<KeyPoint>; 2], Vec<(usize, usize)>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = pictures
            .iter()
            .map(|pair| scope.spawn(move || match_pair(viewpoint, pair)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| Err(anyhow::anyhow!("matching panicked"))))
            .collect()
    });
    let mut matched = Matched {
        views: Vec::new(),
        keypoints: Vec::new(),
        found: Vec::new(),
        sources: Vec::new(),
    };
    for (pair, result) in per_pair.into_iter().enumerate() {
        let (views, keypoints, matches) = result?;
        let points = keypoints.each_ref().map(|keypoints| {
            keypoints
                .iter()
                .map(|keypoint| Vector2::new(keypoint.pt().x as f64, keypoint.pt().y as f64))
                .collect::<Vec<_>>()
        });
        for (left, right) in matches {
            let pixels = [points[0][left], points[1][right]];
            if let (Some(left), Some(right)) = (viewpoint.source(0, &pixels[0]), viewpoint.source(1, &pixels[1])) {
                matched.sources.push([left, right]);
                matched.found.push(Found { pair, pixels });
            }
        }
        matched.views.push(views);
        matched.keypoints.push(points);
    }
    Ok(matched)
}

/// Matches features between the views of a pair of previews.
fn match_pair(viewpoint: &Viewpoint, pair: &[Mat; 2]) -> Result<([Mat; 2], [Vec<KeyPoint>; 2], Vec<(usize, usize)>)> {
    let mut detector = SIFT::create_def()?;
    let mut views = Vec::with_capacity(2);
    let mut keypoints = Vec::with_capacity(2);
    let mut descriptors = Vec::with_capacity(2);
    for side in 0..2 {
        let view = viewpoint.view(side, &pair[side])?;
        let mut found = Vector::<KeyPoint>::new();
        // What SIFT does with `nfeatures`, which can't be passed the same way
        // to every OpenCV 4 release.
        detector.detect(&view, &mut found, &core::no_array())?;
        KeyPointsFilter::retain_best(&mut found, FEATURES)?;
        KeyPointsFilter::run_by_pixels_mask(&mut found, &viewpoint.mask(side))?;
        let mut described = Mat::default();
        detector.compute(&view, &mut found, &mut described)?;
        views.push(view);
        keypoints.push(found.to_vec());
        descriptors.push(described);
    }
    let matches = if keypoints.iter().all(|found| found.len() >= 2) {
        mutual_matches(&descriptors[0], &descriptors[1])?
    } else {
        Vec::new()
    };
    let [left_view, right_view]: [Mat; 2] = views.try_into().map_err(|_| anyhow::anyhow!("two views"))?;
    let [left_keypoints, right_keypoints]: [Vec<KeyPoint>; 2] =
        keypoints.try_into().map_err(|_| anyhow::anyhow!("two sets of keypoints"))?;
    Ok(([left_view, right_view], [left_keypoints, right_keypoints], matches))
}

/// Matches passing the ratio test both ways round that pick each other.
fn mutual_matches(a: &Mat, b: &Mat) -> Result<Vec<(usize, usize)>> {
    let matcher = BFMatcher::create(core::NORM_L2, false)?;
    let ratio_test = |query: &Mat, train: &Mat| -> Result<Vec<Option<usize>>> {
        let mut nearest = Vector::<Vector<DMatch>>::new();
        matcher.knn_train_match(query, train, &mut nearest, 2, &core::no_array(), false)?;
        let mut chosen = vec![None; query.rows() as usize];
        for candidates in &nearest {
            if candidates.len() == 2 {
                let (best, next) = (candidates.get(0)?, candidates.get(1)?);
                if best.distance < RATIO * next.distance {
                    chosen[best.query_idx as usize] = Some(best.train_idx as usize);
                }
            }
        }
        Ok(chosen)
    };
    let forward = ratio_test(a, b)?;
    let backward = ratio_test(b, a)?;
    Ok(forward
        .iter()
        .enumerate()
        .filter_map(|(from, to)| {
            let to = (*to)?;
            (backward[to] == Some(from)).then_some((from, to))
        })
        .collect())
}

fn to_rgb(gray: &Mat) -> Result<Mat> {
    let mut rgb = Mat::default();
    imgproc::cvt_color_def(gray, &mut rgb, imgproc::COLOR_GRAY2RGB)?;
    Ok(rgb)
}

const GREEN: (f64, f64, f64) = (40.0, 220.0, 40.0);
const RED: (f64, f64, f64) = (240.0, 50.0, 50.0);
const GRAY: (f64, f64, f64) = (150.0, 150.0, 150.0);

fn color((red, green, blue): (f64, f64, f64)) -> Scalar {
    Scalar::new(red, green, blue, 0.0)
}

fn point(pixel: &Vector2<f64>, offset: f64) -> Point {
    Point::new((pixel.x + offset).round() as i32, pixel.y.round() as i32)
}

fn draw_features(matched: &Matched, inliers: &[bool], pair: usize) -> Result<[Bytes; 2]> {
    let mut drawn = Vec::with_capacity(2);
    for side in 0..2 {
        let mut image = to_rgb(&matched.views[pair][side])?;
        for keypoint in &matched.keypoints[pair][side] {
            imgproc::circle(&mut image, point(keypoint, 0.0), 2, color(GRAY), 1, imgproc::LINE_AA, 0)?;
        }
        for (found, inlier) in matched.found.iter().zip(inliers).filter(|(found, _)| found.pair == pair) {
            let shade = if *inlier { GREEN } else { RED };
            imgproc::circle(&mut image, point(&found.pixels[side], 0.0), 4, color(shade), 2, imgproc::LINE_AA, 0)?;
        }
        drawn.push(picture::encode(&image)?);
    }
    let [left, right]: [Bytes; 2] = drawn.try_into().map_err(|_| anyhow::anyhow!("two pictures"))?;
    Ok([left, right])
}

fn draw_matches(matched: &Matched, inliers: &[bool], pair: usize) -> Result<Bytes> {
    let [left, right] = &matched.views[pair];
    let mut side_by_side = Mat::default();
    core::hconcat2(&to_rgb(left)?, &to_rgb(right)?, &mut side_by_side)?;
    let offset = left.cols() as f64;
    let in_pair: Vec<(&Found, bool)> = matched
        .found
        .iter()
        .zip(inliers.iter().copied())
        .filter(|(found, _)| found.pair == pair)
        .collect();
    let step = in_pair.len().div_ceil(DRAWN_MATCHES).max(1);
    for (found, inlier) in in_pair.into_iter().step_by(step) {
        let shade = if inlier { GREEN } else { RED };
        imgproc::line(
            &mut side_by_side,
            point(&found.pixels[0], 0.0),
            point(&found.pixels[1], offset),
            color(shade),
            1,
            imgproc::LINE_AA,
            0,
        )?;
    }
    picture::encode(&side_by_side)
}

/// The two cameras' views laid over each other: the left camera in red and
/// the right in cyan, either alone in gray.
fn overlay(warp: &Warp, pair: &[Mat; 2]) -> Result<Bytes> {
    let views = [warp.reproject(0, &pair[0])?, warp.reproject(1, &pair[1])?];
    let (width, height) = (views[0].cols(), views[0].rows());
    let shades = [views[0].data_bytes()?, views[1].data_bytes()?];
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for index in 0..(width * height) as usize {
        let (left, right) = (shades[0][index], shades[1][index]);
        pixels.push(match (warp.seen[0][index], warp.seen[1][index]) {
            (true, true) => Vec3b::from([left, right, right]),
            (true, false) => Vec3b::from([left; 3]),
            (false, true) => Vec3b::from([right; 3]),
            (false, false) => Vec3b::from([0; 3]),
        });
    }
    let image = Mat::new_rows_cols_with_data::<Vec3b>(height, width, &pixels)?;
    picture::encode(&image.try_clone()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u32, u32) = (768, 432);

    /// A random value in \([0, 1)\) for each point of an integer lattice.
    fn lattice(x: i64, y: i64, z: i64) -> f64 {
        let mut hash = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ (z as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        hash ^= hash >> 32;
        (hash >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Smoothly interpolated lattice values.
    fn value_noise(point: &Vector3<f64>) -> f64 {
        let base = point.map(f64::floor);
        let fraction = (point - base).map(|t| t * t * (3.0 - 2.0 * t));
        let mut value = 0.0;
        for corner in 0..8 {
            let offset = Vector3::new((corner & 1) as f64, ((corner >> 1) & 1) as f64, ((corner >> 2) & 1) as f64);
            let weight: f64 = (0..3)
                .map(|axis| if offset[axis] > 0.0 { fraction[axis] } else { 1.0 - fraction[axis] })
                .product();
            let lattice_point = base + offset;
            value += weight * lattice(lattice_point.x as i64, lattice_point.y as i64, lattice_point.z as i64);
        }
        value
    }

    /// A detailed scene all around the rig: the brightness seen in a
    /// direction, made of blobs of many sizes.
    fn scene(direction: &Vector3<f64>) -> f64 {
        let direction = direction.normalize();
        let mut shade = 0.0;
        let mut weight = 1.0;
        let mut frequency = 10.0;
        for _ in 0..5 {
            shade += weight * (value_noise(&(direction * frequency)) - 0.5);
            weight *= 0.65;
            frequency *= 2.0;
        }
        (128.0 + 220.0 * shade).clamp(0.0, 255.0)
    }

    fn photograph(lens: &Lens, orientation: &Orientation) -> Bytes {
        let max_angle = lens.max_angle();
        let to_rig = orientation.rotation();
        let mut pixels = Vec::with_capacity((lens.width * lens.height) as usize);
        for row in 0..lens.height {
            for column in 0..lens.width {
                let ray = lens.unproject_within(&Vector2::new(column as f64, row as f64), max_angle);
                pixels.push(scene(&(to_rig * ray)) as u8);
            }
        }
        let image = Mat::new_rows_cols_with_data::<u8>(lens.height as i32, lens.width as i32, &pixels).unwrap();
        picture::encode(&image.try_clone().unwrap()).unwrap()
    }

    fn rig(left: Orientation, right: Orientation) -> Calibration {
        let mut calibration = Calibration::nominal([left, right]);
        for camera in &mut calibration.cameras {
            camera.lens = camera.lens.scaled_to(SIZE.0, SIZE.1);
        }
        calibration
    }

    /// A rig turned a little off from [`MOUNT`].
    fn truth() -> Calibration {
        rig(
            Orientation { yaw: -41.0, pitch: -24.0, roll: 1.5 },
            Orientation { yaw: 42.5, pitch: -26.0, roll: -1.0 },
        )
    }

    const MOUNT: [Orientation; 2] = [
        Orientation { yaw: -40.0, pitch: -25.0, roll: 0.0 },
        Orientation { yaw: 40.0, pitch: -25.0, roll: 0.0 },
    ];

    /// Aligns, from the nominal rig as mounted, the previews `truth` takes.
    fn align_previews_of(truth: &Calibration) -> Alignment {
        let previews = [truth.cameras.each_ref().map(|camera| photograph(&camera.lens, &camera.orientation))];
        let alignment = align(&previews, &rig(MOUNT[0], MOUNT[1]), &MOUNT).unwrap();
        assert!(crate::all_passed(&alignment.checks()), "{:?}", alignment.checks());
        assert!(alignment.error < 1.0, "error {}", alignment.error);
        alignment
    }

    /// Asserts that `alignment` found how `truth`'s cameras are turned
    /// relative to each other, and its lenses scaled from the nominal one by
    /// `focal_scales`.
    fn assert_found(alignment: &Alignment, truth: &Calibration, focal_scales: [f64; 2]) {
        let relative = |calibration: &Calibration| {
            let [left, right] = calibration.cameras.each_ref().map(|camera| camera.orientation.rotation());
            right.inverse() * left
        };
        let off = relative(&alignment.calibration).rotation_to(&relative(truth)).angle().to_degrees();
        assert!(off < 0.05, "relative orientation off by {off} degrees");
        for (found, expected) in alignment.focal_scales.iter().zip(focal_scales) {
            assert!(
                (found / expected - 1.0).abs() < 0.001,
                "focal lengths scaled by {:?}, not {focal_scales:?}",
                alignment.focal_scales
            );
        }
    }

    #[test]
    fn aligns_cameras_from_what_they_both_see() {
        let truth = truth();
        assert_found(&align_previews_of(&truth), &truth, [1.0; 2]);
    }

    #[test]
    fn evens_out_lenses_that_disagree() {
        let mut truth = truth();
        let focal_scales = focal_factors(0.015);
        for (camera, factor) in truth.cameras.iter_mut().zip(focal_scales) {
            camera.lens = scale_focal_lengths(&camera.lens, factor);
        }
        assert_found(&align_previews_of(&truth), &truth, focal_scales);
    }
}
