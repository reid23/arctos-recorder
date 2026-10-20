//! Fitting a lens to pictures of a chessboard held in front of it.

use anyhow::{Result, ensure};
use bytes::Bytes;
use nalgebra::{DMatrix, DVector, Rotation3, Vector2, Vector3};
use opencv::core::{
    Mat, Point, Point2d, Point2f, Point3d, Scalar, Size, TermCriteria, TermCriteria_Type, Vector,
};
use opencv::prelude::*;
use opencv::{calib3d, imgproc};
use serde::{Deserialize, Serialize};
use stitching::geometry::Lens;

use crate::{Check, picture};

/// Pictures needed to fit a lens at all.
pub const FEWEST_PICTURES: usize = 4;
/// Pictures needed before a fit can be good enough.
pub const ENOUGH_PICTURES: usize = 12;
/// The most a good fit may be off where it puts the corners (root mean
/// square, in pixels).
pub const GOOD_ERROR: f64 = 0.75;
/// The newest pictures left out to check that the fit has settled.
pub const HOLDOUT: usize = 3;
/// The most a settled fit may move anywhere the board has been when the
/// newest pictures are left out, in pixels.
pub const GOOD_INSTABILITY: f64 = 1.0;
/// The picture is divided into this many areas across and down, all of
/// which the board must have reached.
pub const COVERAGE_AREAS: usize = 3;
/// Points across and down the picture where fits are compared.
const COMPARISON_GRID: (u32, u32) = (17, 9);

/// A printed chessboard, counted in inner corners: the points where four
/// squares meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    pub columns: u32,
    pub rows: u32,
}

impl Board {
    /// The corners on the board, row by row, a square apart.
    fn points(&self) -> Vec<Vector3<f64>> {
        (0..self.rows)
            .flat_map(|row| (0..self.columns).map(move |column| Vector3::new(column as f64, row as f64, 0.0)))
            .collect()
    }

    /// Lines of corners: the rows, then the columns.
    fn lines<'a>(&self, corners: &'a [Vector2<f64>]) -> Vec<Vec<&'a Vector2<f64>>> {
        let (columns, rows) = (self.columns as usize, self.rows as usize);
        let across = (0..rows).map(|row| corners[row * columns..(row + 1) * columns].iter().collect());
        let down = (0..columns).map(|column| (0..rows).map(|row| &corners[row * columns + column]).collect());
        across.chain(down).collect()
    }
}

/// Where a board's corners were seen in a picture, row by row, in pixels.
pub type Corners = Vec<Vector2<f64>>;

/// The board's corners in a grayscale picture; `None` unless the whole
/// board is in view. Fast enough to run on every preview.
pub fn find_corners(picture: &Mat, board: Board) -> Result<Option<Corners>> {
    let mut found = Vector::<Point2f>::new();
    let flags = calib3d::CALIB_CB_ADAPTIVE_THRESH
        | calib3d::CALIB_CB_NORMALIZE_IMAGE
        | calib3d::CALIB_CB_FAST_CHECK;
    let size = Size::new(board.columns as i32, board.rows as i32);
    if !calib3d::find_chessboard_corners(picture, size, &mut found, flags)? {
        return Ok(None);
    }
    let corners: Corners = found
        .iter()
        .map(|corner| Vector2::new(corner.x as f64, corner.y as f64))
        .collect();
    // Half the distance to the nearest corner: wide enough to settle on the
    // corner, narrow enough to leave its neighbors out.
    let spacing = board
        .lines(&corners)
        .iter()
        .flat_map(|line| line.windows(2).map(|pair| (pair[1] - pair[0]).norm()))
        .fold(f64::INFINITY, f64::min);
    let radius = (spacing / 2.0).ceil().max(1.0) as i32;
    imgproc::corner_sub_pix(
        picture,
        &mut found,
        Size::new(radius, radius),
        Size::new(-1, -1),
        TermCriteria::new(
            TermCriteria_Type::COUNT as i32 | TermCriteria_Type::EPS as i32,
            30,
            0.01,
        )?,
    )?;
    Ok(Some(
        found
            .iter()
            .map(|corner| Vector2::new(corner.x as f64, corner.y as f64))
            .collect(),
    ))
}

/// How far apart two sightings of a board are: the mean distance between
/// their corners, taking the board either way round (it looks the same
/// turned half a turn).
pub fn separation(a: &[Vector2<f64>], b: &[Vector2<f64>]) -> f64 {
    let mean = |distances: Box<dyn Iterator<Item = f64> + '_>| distances.sum::<f64>() / a.len() as f64;
    let forward = mean(Box::new(a.iter().zip(b).map(|(a, b)| (a - b).norm())));
    let reversed = mean(Box::new(a.iter().zip(b.iter().rev()).map(|(a, b)| (a - b).norm())));
    forward.min(reversed)
}

/// How big the board looks: the distance between its first and last
/// corners.
pub fn apparent_size(corners: &[Vector2<f64>]) -> f64 {
    match (corners.first(), corners.last()) {
        (Some(first), Some(last)) => (last - first).norm(),
        _ => 0.0,
    }
}

/// The farthest any corner is from the straight line through the ends of
/// its row or column, in pixels.
pub fn bend(board: Board, corners: &[Vector2<f64>]) -> f64 {
    board
        .lines(corners)
        .iter()
        .filter_map(|line| {
            let (first, last) = (line.first()?, line.last()?);
            let along = (*last - *first).try_normalize(1e-9)?;
            let normal = Vector2::new(-along.y, along.x);
            line.iter()
                .map(|corner| (*corner - *first).dot(&normal).abs())
                .reduce(f64::max)
        })
        .fold(0.0, f64::max)
}

/// A lens fitted to pictures of a board.
#[derive(Clone, Debug, PartialEq)]
pub struct LensFit {
    /// Measured in the pictures' size.
    pub lens: Lens,
    /// How far off the fit puts the corners (root mean square, in pixels).
    pub error: f64,
    /// The same for each picture.
    pub picture_errors: Vec<f64>,
    /// The farthest the fit moves anywhere the board has been when the
    /// newest [`HOLDOUT`] pictures are left out, in pixels; `None` with too
    /// few pictures to tell.
    pub instability: Option<f64>,
    /// Which areas of the picture the board has reached, by row.
    pub coverage: [[bool; COVERAGE_AREAS]; COVERAGE_AREAS],
}

impl LensFit {
    pub fn checks(&self) -> Vec<Check> {
        let pictures = self.picture_errors.len();
        let covered = self.coverage.iter().flatten().filter(|&&covered| covered).count();
        let areas = COVERAGE_AREAS * COVERAGE_AREAS;
        vec![
            Check::new(
                format!("{pictures} pictures (needs {ENOUGH_PICTURES})"),
                pictures >= ENOUGH_PICTURES,
            ),
            Check::new(
                format!("The board has reached {covered} of the picture's {areas} areas (needs all)"),
                covered == areas,
            ),
            Check::new(
                format!("Corners fit within {:.2} px (needs under {GOOD_ERROR})", self.error),
                self.error < GOOD_ERROR,
            ),
            match self.instability {
                Some(instability) => Check::new(
                    format!(
                        "Leaving out the newest {HOLDOUT} pictures moves the fit {instability:.2} px \
                         where the board has been (needs under {GOOD_INSTABILITY})"
                    ),
                    instability < GOOD_INSTABILITY,
                ),
                None => Check::new(
                    format!(
                        "Settled (checked from {} pictures)",
                        FEWEST_PICTURES + HOLDOUT
                    ),
                    false,
                ),
            },
        ]
    }
}

/// Fits a lens to sightings of `board`, starting from `guess` (measured in
/// the pictures' size).
pub fn fit_lens(board: Board, sightings: &[Corners], guess: &Lens) -> Result<LensFit> {
    ensure!(
        sightings.len() >= FEWEST_PICTURES,
        "needs at least {FEWEST_PICTURES} pictures"
    );
    let (lens, poses) = calibrate(board, sightings, guess)?;
    let max_angle = lens.max_angle();
    let points = board.points();
    let picture_errors: Vec<f64> = sightings
        .iter()
        .zip(&poses)
        .map(|(corners, (rotation, translation))| {
            let squared: f64 = points
                .iter()
                .zip(corners)
                .map(|(point, corner)| {
                    lens.project_within(&(rotation * point + translation), max_angle)
                        .map_or(f64::INFINITY, |seen| (seen - corner).norm_squared())
                })
                .sum();
            (squared / corners.len() as f64).sqrt()
        })
        .collect();
    let error = (picture_errors.iter().map(|error| error * error).sum::<f64>()
        / picture_errors.len() as f64)
        .sqrt();
    let instability = if sightings.len() >= FEWEST_PICTURES + HOLDOUT {
        let (without_newest, _) = calibrate(board, &sightings[..sightings.len() - HOLDOUT], guess)?;
        Some(largest_difference(&lens, &without_newest, sightings)?)
    } else {
        None
    };
    Ok(LensFit {
        coverage: coverage(sightings, (lens.width, lens.height)),
        lens,
        error,
        picture_errors,
        instability,
    })
}

/// Which areas of `size` pictures the board has reached, by row.
pub fn coverage(sightings: &[Corners], size: (u32, u32)) -> [[bool; COVERAGE_AREAS]; COVERAGE_AREAS] {
    let area = |value: f64, size: u32| {
        ((value + 0.5) / size as f64 * COVERAGE_AREAS as f64).clamp(0.0, COVERAGE_AREAS as f64 - 1.0) as usize
    };
    let mut coverage = [[false; COVERAGE_AREAS]; COVERAGE_AREAS];
    for corner in sightings.iter().flatten() {
        coverage[area(corner.y, size.1)][area(corner.x, size.0)] = true;
    }
    coverage
}

/// The values a fit adjusts for the lens (fx, fy, cx, cy, then the
/// distortion), and for the board's pose in each picture (its rotation as a
/// scaled axis, then its translation).
const LENS_VALUES: usize = 8;
const POSE_VALUES: usize = 6;
const PICTURE_VALUES: usize = LENS_VALUES + POSE_VALUES;
/// The most steps a fit takes.
const FIT_STEPS: usize = 100;

/// Fits the lens and the board's pose in each picture (taking board points
/// to the camera's frame) by least squares with the Levenberg-Marquardt
/// method, starting from `guess`.
fn calibrate(
    board: Board,
    sightings: &[Corners],
    guess: &Lens,
) -> Result<(Lens, Vec<(Rotation3<f64>, Vector3<f64>)>)> {
    let points = board.points();
    let mut lens = [
        guess.fx,
        guess.fy,
        guess.cx,
        guess.cy,
        guess.distortion[0],
        guess.distortion[1],
        guess.distortion[2],
        guess.distortion[3],
    ];
    let mut poses = sightings
        .iter()
        .map(|corners| initial_pose(&points, corners, guess))
        .collect::<Result<Vec<_>>>()?;
    let picture_values = |lens: &[f64; LENS_VALUES], pose: &[f64; POSE_VALUES]| {
        let mut values = [0.0; PICTURE_VALUES];
        values[..LENS_VALUES].copy_from_slice(lens);
        values[LENS_VALUES..].copy_from_slice(pose);
        values
    };
    let cost = |lens: &[f64; LENS_VALUES], poses: &[[f64; POSE_VALUES]]| -> f64 {
        poses
            .iter()
            .zip(sightings)
            .map(|(pose, corners)| {
                picture_residuals(&picture_values(lens, pose), guess, &points, corners).norm_squared()
            })
            .sum()
    };
    let mut current = cost(&lens, &poses);
    ensure!(current.is_finite(), "couldn't place the board in every picture");
    let unknowns = LENS_VALUES + POSE_VALUES * poses.len();
    let mut damping = 1e-3;
    'steps: for _ in 0..FIT_STEPS {
        let mut normal = DMatrix::<f64>::zeros(unknowns, unknowns);
        let mut gradient = DVector::<f64>::zeros(unknowns);
        for (picture, (pose, corners)) in poses.iter().zip(sightings).enumerate() {
            let (jacobian, residuals) =
                picture_jacobian(&picture_values(&lens, pose), guess, &points, corners);
            let block = jacobian.transpose() * &jacobian;
            let slope = jacobian.transpose() * residuals;
            let position = |value: usize| {
                if value < LENS_VALUES {
                    value
                } else {
                    value + POSE_VALUES * picture
                }
            };
            for row in 0..PICTURE_VALUES {
                gradient[position(row)] += slope[row];
                for column in 0..PICTURE_VALUES {
                    normal[(position(row), position(column))] += block[(row, column)];
                }
            }
        }
        loop {
            if damping > 1e12 {
                break 'steps;
            }
            let mut damped = normal.clone();
            for index in 0..unknowns {
                damped[(index, index)] += damping * normal[(index, index)].max(f64::EPSILON);
            }
            if let Some(cholesky) = damped.cholesky() {
                let step = cholesky.solve(&-&gradient);
                let mut next_lens = lens;
                for (value, change) in next_lens.iter_mut().zip(step.iter()) {
                    *value += change;
                }
                let mut next_poses = poses.clone();
                for (pose, changes) in next_poses.iter_mut().zip(step.as_slice()[LENS_VALUES..].chunks(POSE_VALUES)) {
                    for (value, change) in pose.iter_mut().zip(changes) {
                        *value += change;
                    }
                }
                let next = cost(&next_lens, &next_poses);
                if next < current {
                    let settled = current - next <= 1e-12 * current;
                    (lens, poses, current) = (next_lens, next_poses, next);
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
    Ok((
        lens_from(&lens, guess),
        poses.iter().map(|pose| pose_from(pose)).collect(),
    ))
}

/// The lens a fit's `values` describe, measured like `guess`.
fn lens_from(values: &[f64], guess: &Lens) -> Lens {
    Lens {
        fx: values[0],
        fy: values[1],
        cx: values[2],
        cy: values[3],
        distortion: [values[4], values[5], values[6], values[7]],
        ..guess.clone()
    }
}

fn pose_from(values: &[f64]) -> (Rotation3<f64>, Vector3<f64>) {
    (
        Rotation3::new(Vector3::new(values[0], values[1], values[2])),
        Vector3::new(values[3], values[4], values[5]),
    )
}

/// The board's pose in a picture taken through `lens`, as a fit's values.
fn initial_pose(points: &[Vector3<f64>], corners: &[Vector2<f64>], lens: &Lens) -> Result<[f64; POSE_VALUES]> {
    let max_angle = lens.max_angle();
    let board: Vector<Point3d> = points.iter().map(|point| Point3d::new(point.x, point.y, point.z)).collect();
    let mut seen = Vector::<Point2d>::new();
    for corner in corners {
        let ray = lens.unproject_within(corner, max_angle);
        ensure!(ray.z > 0.0, "a corner is too far off the lens's axis to place the board");
        seen.push(Point2d::new(ray.x / ray.z, ray.y / ray.z));
    }
    let (mut rotation, mut translation) = (Mat::default(), Mat::default());
    let placed = calib3d::solve_pnp(
        &board,
        &seen,
        &Mat::eye(3, 3, opencv::core::CV_64F)?.to_mat()?,
        &Mat::default(),
        &mut rotation,
        &mut translation,
        false,
        calib3d::SOLVEPNP_IPPE,
    )?;
    ensure!(placed, "couldn't place the board in a picture");
    let (rotation, translation) = (rotation.data_typed::<f64>()?, translation.data_typed::<f64>()?);
    ensure!(rotation.len() == 3 && translation.len() == 3, "OpenCV gave a pose of the wrong size");
    Ok([rotation[0], rotation[1], rotation[2], translation[0], translation[1], translation[2]])
}

/// How far from where they were seen a picture's corners land through the
/// lens and pose in `values`, across then down for each corner.
fn picture_residuals(
    values: &[f64; PICTURE_VALUES],
    guess: &Lens,
    points: &[Vector3<f64>],
    corners: &[Vector2<f64>],
) -> DVector<f64> {
    let lens = lens_from(values, guess);
    let (rotation, translation) = pose_from(&values[LENS_VALUES..]);
    DVector::from_iterator(
        2 * corners.len(),
        points.iter().zip(corners).flat_map(|(point, corner)| {
            let off = lens
                .project_within(&(rotation * point + translation), f64::INFINITY)
                .map_or(Vector2::repeat(f64::NAN), |landed| landed - corner);
            [off.x, off.y]
        }),
    )
}

/// [`picture_residuals`] and their derivatives by each of the `values`,
/// found numerically.
fn picture_jacobian(
    values: &[f64; PICTURE_VALUES],
    guess: &Lens,
    points: &[Vector3<f64>],
    corners: &[Vector2<f64>],
) -> (DMatrix<f64>, DVector<f64>) {
    let mut jacobian = DMatrix::zeros(2 * corners.len(), PICTURE_VALUES);
    for column in 0..PICTURE_VALUES {
        let step = 1e-6 * values[column].abs().max(1.0);
        let nudged = |change: f64| {
            let mut nudged = *values;
            nudged[column] += change;
            picture_residuals(&nudged, guess, points, corners)
        };
        jacobian.set_column(column, &((nudged(step) - nudged(-step)) / (2.0 * step)));
    }
    (jacobian, picture_residuals(values, guess, points, corners))
}

/// The farthest apart two lenses (of the same size) put the same rays,
/// over a grid across the part of the picture the sightings reach. Beyond
/// that both lenses are guesses.
fn largest_difference(a: &Lens, b: &Lens, sightings: &[Corners]) -> Result<f64> {
    let corners: Vector<Point2f> = sightings
        .iter()
        .flatten()
        .map(|corner| Point2f::new(corner.x as f32, corner.y as f32))
        .collect();
    let mut reached = Vector::<Point2f>::new();
    imgproc::convex_hull_def(&corners, &mut reached)?;
    let (a_max_angle, b_max_angle) = (a.max_angle(), b.max_angle());
    let (across, down) = COMPARISON_GRID;
    let mut largest: f64 = 0.0;
    for row in 0..down {
        for column in 0..across {
            let pixel = Vector2::new(
                column as f64 / (across - 1) as f64 * (a.width as f64 - 1.0),
                row as f64 / (down - 1) as f64 * (a.height as f64 - 1.0),
            );
            let point = Point2f::new(pixel.x as f32, pixel.y as f32);
            if imgproc::point_polygon_test(&reached, point, false)? < 0.0 {
                continue;
            }
            let ray = a.unproject_within(&pixel, a_max_angle);
            let distance = b
                .project_within(&ray, b_max_angle)
                .map_or(f64::INFINITY, |other| (other - pixel).norm());
            largest = largest.max(distance);
        }
    }
    Ok(largest)
}

/// Reprojects pictures to look as if taken by a pinhole camera in the same
/// place, so straight lines look straight.
pub struct Straightener {
    lens: Lens,
    max_angle: f64,
    focal: f64,
    size: (u32, u32),
    map_x: Mat,
    map_y: Mat,
}

/// A straightened picture, with the board's rows and columns drawn on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Straightened {
    pub picture: Bytes,
    /// [`bend`] of the board in the picture and in the straightened
    /// picture (in the picture's pixels); `None` without a board.
    pub bend: Option<(f64, f64)>,
}

impl Straightener {
    /// For pictures through `lens` (in their size), straightened to `size`
    /// with `horizontal_fov` degrees across.
    pub fn new(lens: &Lens, size: (u32, u32), horizontal_fov: f64) -> Result<Straightener> {
        let max_angle = lens.max_angle();
        let focal = size.0 as f64 / 2.0 / (horizontal_fov.to_radians() / 2.0).tan();
        let pixel_count = (size.0 * size.1) as usize;
        let (mut map_x, mut map_y) = (Vec::with_capacity(pixel_count), Vec::with_capacity(pixel_count));
        for row in 0..size.1 {
            for column in 0..size.0 {
                let ray = Vector3::new(
                    column as f64 + 0.5 - size.0 as f64 / 2.0,
                    row as f64 + 0.5 - size.1 as f64 / 2.0,
                    focal,
                );
                let source = lens
                    .project_within(&ray, max_angle)
                    .unwrap_or(Vector2::new(-1.0, -1.0));
                map_x.push(source.x as f32);
                map_y.push(source.y as f32);
            }
        }
        let map = |values: &[f32]| -> Result<Mat> {
            Ok(Mat::new_rows_cols_with_data::<f32>(size.1 as i32, size.0 as i32, values)?.try_clone()?)
        };
        Ok(Straightener {
            lens: lens.clone(),
            max_angle,
            focal,
            size,
            map_x: map(&map_x)?,
            map_y: map(&map_y)?,
        })
    }

    /// Where a picture's `pixel` ends up in the straightened picture.
    fn straightened(&self, pixel: &Vector2<f64>) -> Option<Vector2<f64>> {
        let ray = self.lens.unproject_within(pixel, self.max_angle);
        (ray.z > 0.0).then(|| {
            Vector2::new(
                ray.x / ray.z * self.focal + self.size.0 as f64 / 2.0 - 0.5,
                ray.y / ray.z * self.focal + self.size.1 as f64 / 2.0 - 0.5,
            )
        })
    }

    /// Straightens an RGB `picture`, drawing the board's rows and columns
    /// if `corners` are given.
    pub fn straighten(&self, picture: &Mat, board: Board, corners: Option<&Corners>) -> Result<Straightened> {
        let mut straightened = Mat::default();
        imgproc::remap(
            picture,
            &mut straightened,
            &self.map_x,
            &self.map_y,
            imgproc::INTER_LINEAR,
            opencv::core::BORDER_CONSTANT,
            Scalar::all(0.0),
        )?;
        let mut bends = None;
        if let Some(corners) = corners {
            let moved: Option<Corners> = corners.iter().map(|corner| self.straightened(corner)).collect();
            if let Some(moved) = moved {
                draw_grid(&mut straightened, board, &moved)?;
                let scale = self.lens.width as f64 / self.size.0 as f64;
                bends = Some((bend(board, corners), bend(board, &moved) * scale));
            }
        }
        Ok(Straightened {
            picture: picture::encode(&straightened)?,
            bend: bends,
        })
    }
}

/// Draws the board's rows and columns as straight lines between their ends,
/// and its corners as dots.
fn draw_grid(image: &mut Mat, board: Board, corners: &[Vector2<f64>]) -> Result<()> {
    const SUBPIXEL_BITS: i32 = 4;
    let point = |pixel: &Vector2<f64>| {
        let scale = (1 << SUBPIXEL_BITS) as f64;
        Point::new((pixel.x * scale).round() as i32, (pixel.y * scale).round() as i32)
    };
    for line in board.lines(corners) {
        if let (Some(first), Some(last)) = (line.first(), line.last()) {
            imgproc::line(
                image,
                point(first),
                point(last),
                Scalar::new(255.0, 60.0, 60.0, 0.0),
                1,
                imgproc::LINE_AA,
                SUBPIXEL_BITS,
            )?;
        }
    }
    for corner in corners {
        imgproc::circle(
            image,
            point(corner),
            3 << SUBPIXEL_BITS,
            Scalar::new(40.0, 220.0, 40.0, 0.0),
            -1,
            imgproc::LINE_AA,
            SUBPIXEL_BITS,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: Board = Board { columns: 8, rows: 6 };
    const SIZE: (u32, u32) = (640, 360);

    /// A picture of the board (squares a unit across, posed by `rotation`
    /// and `translation` in the camera's frame) through `lens`, in front of a
    /// gray wall. Each pixel averages the light falling across it, as a
    /// camera's do.
    fn photograph(lens: &Lens, rotation: &Rotation3<f64>, translation: &Vector3<f64>) -> Mat {
        const SAMPLES_ACROSS_A_PIXEL: u32 = 3;
        let fine = lens.scaled_to(lens.width * SAMPLES_ACROSS_A_PIXEL, lens.height * SAMPLES_ACROSS_A_PIXEL);
        let max_angle = fine.max_angle();
        let to_board = rotation.inverse();
        let origin = to_board * -translation;
        let mut pixels = Vec::with_capacity((fine.width * fine.height) as usize);
        for row in 0..fine.height {
            for column in 0..fine.width {
                let ray = to_board * fine.unproject_within(&Vector2::new(column as f64, row as f64), max_angle);
                let distance = -origin.z / ray.z;
                let shade = if distance > 0.0 {
                    let (x, y) = (origin.x + distance * ray.x, origin.y + distance * ray.y);
                    let on_board = |value: f64, corners: u32| value >= -2.0 && value <= corners as f64 + 1.0;
                    let in_squares = |value: f64, corners: u32| value >= -1.0 && value <= corners as f64;
                    if !on_board(x, BOARD.columns) || !on_board(y, BOARD.rows) {
                        110
                    } else if in_squares(x, BOARD.columns) && in_squares(y, BOARD.rows)
                        && (x.floor() as i64 + y.floor() as i64).rem_euclid(2) == 0
                    {
                        20
                    } else {
                        235
                    }
                } else {
                    110
                };
                pixels.push(shade);
            }
        }
        let rendered = Mat::new_rows_cols_with_data::<u8>(fine.height as i32, fine.width as i32, &pixels).unwrap();
        let mut picture = Mat::default();
        imgproc::resize(
            &rendered,
            &mut picture,
            Size::new(lens.width as i32, lens.height as i32),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )
        .unwrap();
        picture
    }

    /// Board poses spread over the picture: the board's middle `distance`
    /// squares away toward `yaw` and `pitch` degrees, turned by `tilt`.
    fn pose(yaw: f64, pitch: f64, distance: f64, tilt: (f64, f64, f64)) -> (Rotation3<f64>, Vector3<f64>) {
        let toward = Rotation3::from_euler_angles(pitch.to_radians(), yaw.to_radians(), 0.0) * Vector3::z();
        let rotation = Rotation3::from_euler_angles(tilt.0.to_radians(), tilt.1.to_radians(), tilt.2.to_radians());
        let middle = Vector3::new((BOARD.columns - 1) as f64 / 2.0, (BOARD.rows - 1) as f64 / 2.0, 0.0);
        (rotation, toward * distance - rotation * middle)
    }

    fn true_lens() -> Lens {
        Lens {
            fx: 990.0,
            fy: 985.0,
            cx: 1160.0,
            cy: 640.0,
            distortion: [0.62, -0.30, 0.05, -0.02],
            ..Lens::nominal()
        }
        .scaled_to(SIZE.0, SIZE.1)
    }

    #[test]
    fn finds_the_corners_of_a_board_in_order() {
        let lens = true_lens();
        let (rotation, translation) = pose(0.0, 0.0, 9.0, (10.0, -15.0, 5.0));
        let corners = find_corners(&photograph(&lens, &rotation, &translation), BOARD)
            .unwrap()
            .expect("board found");
        let expected: Vec<Vector2<f64>> = BOARD
            .points()
            .iter()
            .map(|point| lens.project(&(rotation * point + translation)).unwrap())
            .collect();
        assert!(separation(&corners, &expected) < 0.3, "{}", separation(&corners, &expected));
    }

    #[test]
    fn finds_no_corners_without_a_board() {
        let blank = Mat::new_rows_cols_with_default(360, 640, opencv::core::CV_8UC1, Scalar::all(128.0)).unwrap();
        assert_eq!(find_corners(&blank, BOARD).unwrap(), None);
    }

    #[test]
    fn separation_takes_the_board_either_way_round() {
        let corners: Corners = BOARD.points().iter().map(|point| point.xy() * 20.0).collect();
        let turned: Corners = corners.iter().rev().cloned().collect();
        assert_eq!(separation(&corners, &turned), 0.0);
        let moved: Corners = corners.iter().map(|corner| corner + Vector2::new(3.0, 4.0)).collect();
        assert!((separation(&corners, &moved) - 5.0).abs() < 1e-12);
    }

    #[test]
    fn fits_the_lens_that_took_the_pictures() {
        let lens = true_lens();
        let poses = [
            pose(0.0, 0.0, 9.0, (0.0, 0.0, 0.0)),
            pose(-24.0, 0.0, 10.0, (0.0, 25.0, 0.0)),
            pose(24.0, 0.0, 10.0, (0.0, -25.0, 5.0)),
            pose(0.0, -10.0, 10.0, (-20.0, 0.0, 0.0)),
            pose(0.0, 10.0, 10.0, (20.0, 0.0, -5.0)),
            pose(-22.0, -9.0, 11.0, (-15.0, 20.0, 0.0)),
            pose(22.0, -9.0, 11.0, (-15.0, -20.0, 10.0)),
            pose(-22.0, 9.0, 11.0, (15.0, 20.0, 0.0)),
            pose(22.0, 9.0, 11.0, (15.0, -20.0, -10.0)),
            pose(-12.0, 0.0, 8.5, (25.0, 10.0, 15.0)),
            pose(12.0, 0.0, 8.5, (-25.0, -10.0, -15.0)),
            pose(-28.0, 0.0, 12.0, (0.0, 35.0, 0.0)),
            pose(28.0, 0.0, 12.0, (0.0, -35.0, 0.0)),
            pose(0.0, 0.0, 9.0, (20.0, 25.0, 15.0)),
            pose(-8.0, 4.0, 13.0, (-30.0, 0.0, 0.0)),
            pose(8.0, -4.0, 13.0, (30.0, 0.0, 0.0)),
        ];
        let sightings: Vec<Corners> = std::thread::scope(|scope| {
            let searches: Vec<_> = poses
                .iter()
                .map(|(rotation, translation)| {
                    let lens = &lens;
                    scope.spawn(move || find_corners(&photograph(lens, rotation, translation), BOARD).unwrap())
                })
                .collect();
            searches.into_iter().filter_map(|search| search.join().unwrap()).collect()
        });
        assert!(sightings.len() >= 12, "found the board in {} pictures", sightings.len());
        let fit = fit_lens(BOARD, &sightings, &Lens::nominal().scaled_to(SIZE.0, SIZE.1)).unwrap();
        assert!(fit.error < 0.3, "error {}", fit.error);
        assert!((fit.lens.fx / lens.fx - 1.0).abs() < 0.01, "fx {} vs {}", fit.lens.fx, lens.fx);
        let worst = sightings
            .iter()
            .flatten()
            .map(|corner| (fit.lens.project(&lens.unproject(corner)).unwrap() - corner).norm())
            .fold(0.0, f64::max);
        assert!(worst < 0.5, "the fit is off by {worst} px where the board was");
        let instability = fit.instability.unwrap();
        assert!(instability < GOOD_INSTABILITY, "instability {instability}");
    }

    #[test]
    fn fits_lenses_from_boards_held_all_over_the_picture() {
        let lens = true_lens();
        let guess = Lens::nominal().scaled_to(SIZE.0, SIZE.1);
        let mut seed: u64 = 7;
        let mut random = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..4 {
            let mut sightings = Vec::new();
            while sightings.len() < ENOUGH_PICTURES {
                let toward = lens.unproject(&Vector2::new(random() * SIZE.0 as f64, random() * SIZE.1 as f64));
                let mut turn = || (random() - 0.5) * 1.2;
                let rotation = Rotation3::from_euler_angles(turn(), turn(), turn());
                let middle = Vector3::new((BOARD.columns - 1) as f64 / 2.0, (BOARD.rows - 1) as f64 / 2.0, 0.0);
                let translation = toward * (7.0 + random() * 10.0) - rotation * middle;
                let corners: Option<Corners> = BOARD
                    .points()
                    .iter()
                    .map(|point| lens.project(&(rotation * point + translation)).filter(|corner| lens.contains(corner)))
                    .collect();
                if let Some(corners) = corners {
                    let mut jitter = || (random() - 0.5) * 0.2;
                    sightings.push(corners.iter().map(|corner| corner + Vector2::new(jitter(), jitter())).collect());
                }
            }
            let fit = fit_lens(BOARD, &sightings, &guess).unwrap();
            assert!(fit.error < 0.1, "error {}", fit.error);
            let instability = fit.instability.unwrap();
            assert!(instability < GOOD_INSTABILITY, "instability {instability}");
            let off = largest_difference(&lens, &fit.lens, &sightings).unwrap();
            assert!(off < GOOD_INSTABILITY, "the fit is off by {off} px where the board was");
        }
    }

    #[test]
    fn straightening_straightens_the_board() {
        let lens = true_lens();
        let (rotation, translation) = pose(20.0, 0.0, 9.0, (0.0, -15.0, 0.0));
        let gray = photograph(&lens, &rotation, &translation);
        let corners: Corners = BOARD
            .points()
            .iter()
            .map(|point| lens.project(&(rotation * point + translation)).unwrap())
            .collect();
        let mut picture = Mat::default();
        imgproc::cvt_color_def(&gray, &mut picture, imgproc::COLOR_GRAY2RGB).unwrap();
        let straightener = Straightener::new(&lens, (480, 270), 90.0).unwrap();
        let (before, after) = straightener.straighten(&picture, BOARD, Some(&corners)).unwrap().bend.unwrap();
        assert!(before > 0.5 && after < 0.01, "bent {before} px, then {after} px");
    }
}
