//! The rig's geometry: each camera's lens and where it points, and the
//! cylindrical panorama they are stitched into.
//!
//! Directions use the cameras' convention: x right, y down, z forward. The
//! rig's frame is the same with z pointing the way the rig faces, so a camera
//! with a zero [`Orientation`] looks straight ahead. Pixel coordinates follow
//! OpenCV: the center of the top left pixel is \((0, 0)\).

use std::f64::consts::PI;

use anyhow::{Result, ensure};
use nalgebra::{Rotation3, Vector2, Vector3};
use protocol::{Side, rig};
use serde::{Deserialize, Serialize};

/// Where libcamera held the rig's lenses before csw focused them, which
/// lenses measured then were measured at (see [`Lens::focus`]).
pub const DEFAULT_FOCUS: f64 = 1.0;
/// How much longer the rig's lenses' focal lengths get per unit of lens
/// position, from where they focus at infinity: focusing closer moves a
/// lens away from the sensor, magnifying the image. Measured on the Camera
/// Module 3 Wide.
const MAGNIFICATION_PER_LENS_POSITION: f64 = 0.0018;

/// [`DEFAULT_FOCUS`], for what serde fills in.
pub fn default_focus() -> f64 {
    DEFAULT_FOCUS
}

/// How many times bigger the rig's lenses make things look focused at lens
/// position `to` than at `from`.
pub fn magnification(from: f64, to: f64) -> f64 {
    (1.0 + MAGNIFICATION_PER_LENS_POSITION * to) / (1.0 + MAGNIFICATION_PER_LENS_POSITION * from)
}

/// A fisheye lens in the Kannala–Brandt model (OpenCV's `cv::fisheye`): a
/// ray \(\theta\) off the optical axis lands
/// \(\theta_d = \theta (1 + k_1 \theta^2 + k_2 \theta^4 + k_3 \theta^6 + k_4 \theta^8)\)
/// focal lengths from the principal point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lens {
    /// Size of the images the other values are measured in, in pixels.
    pub width: u32,
    pub height: u32,
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    /// \(k_1\) to \(k_4\).
    pub distortion: [f64; 4],
    /// The lens position (libcamera's `LensPosition`) the other values were
    /// measured at.
    #[serde(default = "default_focus")]
    pub focus: f64,
}

impl Lens {
    /// The rig's lens as first calibrated: about 104 degrees across and 65
    /// degrees high.
    pub fn nominal() -> Lens {
        let (width, height) = rig::RECORD_SIZE;
        Lens {
            width,
            height,
            fx: 982.0,
            fy: 982.0,
            cx: (width as f64 - 1.0) / 2.0,
            cy: (height as f64 - 1.0) / 2.0,
            distortion: [0.6435, -0.3434, 0.0, 0.0],
            focus: DEFAULT_FOCUS,
        }
    }

    /// The same lens measured in `width` by `height` images of the same
    /// field of view.
    pub fn scaled_to(&self, width: u32, height: u32) -> Lens {
        let scale_x = width as f64 / self.width as f64;
        let scale_y = height as f64 / self.height as f64;
        Lens {
            width,
            height,
            fx: self.fx * scale_x,
            fy: self.fy * scale_y,
            cx: (self.cx + 0.5) * scale_x - 0.5,
            cy: (self.cy + 0.5) * scale_y - 0.5,
            distortion: self.distortion,
            focus: self.focus,
        }
    }

    /// The same lens focused at lens position `focus`, which magnifies the
    /// image about the principal point.
    pub fn refocused(&self, focus: f64) -> Lens {
        let scale = magnification(self.focus, focus);
        Lens {
            fx: self.fx * scale,
            fy: self.fy * scale,
            focus,
            ..self.clone()
        }
    }

    /// \(\theta_d\) of a ray \(\theta\) off the optical axis.
    pub fn distort(&self, theta: f64) -> f64 {
        let [k1, k2, k3, k4] = self.distortion;
        let theta2 = theta * theta;
        theta * (1.0 + theta2 * (k1 + theta2 * (k2 + theta2 * (k3 + theta2 * k4))))
    }

    /// The widest angle off the optical axis the model describes: past it
    /// \(\theta_d\) stops growing, so pixels would map back to two rays.
    pub fn max_angle(&self) -> f64 {
        let [k1, k2, k3, k4] = self.distortion;
        let slope = |theta: f64| {
            let theta2 = theta * theta;
            1.0 + theta2
                * (3.0 * k1 + theta2 * (5.0 * k2 + theta2 * (7.0 * k3 + theta2 * 9.0 * k4)))
        };
        const STEPS: usize = 180;
        let step = PI / STEPS as f64;
        let Some(first) = (1..=STEPS)
            .map(|index| index as f64 * step)
            .find(|&theta| slope(theta) <= 0.0)
        else {
            return PI;
        };
        let (mut low, mut high) = (first - step, first);
        for _ in 0..40 {
            let middle = (low + high) / 2.0;
            if slope(middle) <= 0.0 {
                high = middle;
            } else {
                low = middle;
            }
        }
        low
    }

    /// Where a ray (in the camera's frame) lands, wherever that is relative
    /// to the image; `None` past [`Lens::max_angle`].
    pub fn project(&self, direction: &Vector3<f64>) -> Option<Vector2<f64>> {
        self.project_within(direction, self.max_angle())
    }

    /// [`Lens::project`] given the lens's [max angle](Lens::max_angle), for
    /// projecting many rays.
    pub fn project_within(&self, direction: &Vector3<f64>, max_angle: f64) -> Option<Vector2<f64>> {
        let radius = direction.xy().norm();
        let theta = radius.atan2(direction.z);
        if theta >= max_angle {
            return None;
        }
        let toward = if radius > 0.0 {
            direction.xy() / radius
        } else {
            Vector2::zeros()
        };
        let distorted = self.distort(theta);
        Some(Vector2::new(
            self.fx * distorted * toward.x + self.cx,
            self.fy * distorted * toward.y + self.cy,
        ))
    }

    /// The (unit) ray that lands on `pixel`.
    pub fn unproject(&self, pixel: &Vector2<f64>) -> Vector3<f64> {
        self.unproject_within(pixel, self.max_angle())
    }

    /// [`Lens::unproject`] given the lens's [max angle](Lens::max_angle),
    /// for unprojecting many pixels.
    pub fn unproject_within(&self, pixel: &Vector2<f64>, max_angle: f64) -> Vector3<f64> {
        let normalized = Vector2::new((pixel.x - self.cx) / self.fx, (pixel.y - self.cy) / self.fy);
        let distorted = normalized.norm();
        if distorted == 0.0 {
            return Vector3::z();
        }
        let theta = self.undistort(distorted, max_angle);
        let toward = normalized / distorted;
        Vector3::new(
            theta.sin() * toward.x,
            theta.sin() * toward.y,
            theta.cos(),
        )
    }

    /// The \(\theta\) for which [`Lens::distort`] gives `distorted` (or the
    /// max angle, past it), found with Newton's method kept inside a
    /// shrinking bracket.
    fn undistort(&self, distorted: f64, max_angle: f64) -> f64 {
        let [k1, k2, k3, k4] = self.distortion;
        let (mut low, mut high) = (0.0, max_angle);
        if self.distort(high) <= distorted {
            return high;
        }
        let mut theta = distorted.min(high / 2.0);
        for _ in 0..50 {
            let error = self.distort(theta) - distorted;
            if error > 0.0 {
                high = theta;
            } else {
                low = theta;
            }
            let theta2 = theta * theta;
            let slope = 1.0
                + theta2
                    * (3.0 * k1 + theta2 * (5.0 * k2 + theta2 * (7.0 * k3 + theta2 * 9.0 * k4)));
            let newton = theta - error / slope;
            let next = if newton > low && newton < high {
                newton
            } else {
                (low + high) / 2.0
            };
            if (next - theta).abs() < 1e-13 {
                return next;
            }
            theta = next;
        }
        theta
    }

    /// Degrees seen across and down the image, through the principal point.
    pub fn field_of_view(&self) -> (f64, f64) {
        let max_angle = self.max_angle();
        let angle = |x: f64, y: f64| {
            self.unproject_within(&Vector2::new(x, y), max_angle)
                .z
                .acos()
                .to_degrees()
        };
        let (right, bottom) = (self.width as f64 - 0.5, self.height as f64 - 0.5);
        (
            angle(-0.5, self.cy) + angle(right, self.cy),
            angle(self.cx, -0.5) + angle(self.cx, bottom),
        )
    }

    /// Whether `pixel` is inside the image.
    pub fn contains(&self, pixel: &Vector2<f64>) -> bool {
        let inside = |value: f64, size: u32| value >= -0.5 && value <= size as f64 - 0.5;
        inside(pixel.x, self.width) && inside(pixel.y, self.height)
    }
}

/// Where a camera points relative to the rig, in degrees: turned `yaw` to
/// the right about the rig's vertical axis, then tilted `pitch` up, then
/// rolled `roll` clockwise as seen from behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Orientation {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

impl Orientation {
    /// The rotation taking directions in the camera's (or view's) frame to
    /// the rig's frame.
    pub fn rotation(&self) -> Rotation3<f64> {
        Rotation3::from_axis_angle(&Vector3::y_axis(), self.yaw.to_radians())
            * Rotation3::from_axis_angle(&Vector3::x_axis(), self.pitch.to_radians())
            * Rotation3::from_axis_angle(&Vector3::z_axis(), self.roll.to_radians())
    }

    /// The orientation whose [`Orientation::rotation`] is `rotation`.
    pub fn from_rotation(rotation: &Rotation3<f64>) -> Orientation {
        let matrix = rotation.matrix();
        Orientation {
            yaw: matrix[(0, 2)].atan2(matrix[(2, 2)]).to_degrees(),
            pitch: (-matrix[(1, 2)]).clamp(-1.0, 1.0).asin().to_degrees(),
            roll: matrix[(1, 0)].atan2(matrix[(1, 1)]).to_degrees(),
        }
    }
}

/// One camera of the rig.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    pub lens: Lens,
    pub orientation: Orientation,
}

/// The rig's cameras, indexed by [`Side::index`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub cameras: [Camera; 2],
}

impl Calibration {
    /// Both cameras with the [nominal lens](Lens::nominal), pointed as
    /// `orientations`.
    pub fn nominal(orientations: [Orientation; 2]) -> Calibration {
        Calibration {
            cameras: orientations.map(|orientation| Camera {
                lens: Lens::nominal(),
                orientation,
            }),
        }
    }

    pub fn camera(&self, side: Side) -> &Camera {
        &self.cameras[side.index()]
    }

    pub fn validate(&self) -> Result<()> {
        for camera in &self.cameras {
            let lens = &camera.lens;
            ensure!(lens.width > 0 && lens.height > 0, "a lens has no image size");
            ensure!(
                lens.fx > 0.0 && lens.fy > 0.0,
                "a lens's focal length isn't positive"
            );
            ensure!(
                lens.distortion.iter().chain([&lens.cx, &lens.cy, &lens.focus]).all(|v| v.is_finite()),
                "a lens has a value that isn't a number"
            );
        }
        Ok(())
    }
}

/// The part of the panorama shown: a cylindrical projection about the
/// rig's vertical axis, so a level rig shows the horizon straight. Each
/// column is a direction around the axis and each row a height on the
/// cylinder; the vertical field of view follows from the output's aspect
/// ratio.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// Direction of the middle column in degrees, to the right of the rig's
    /// forward.
    pub yaw: f64,
    /// Elevation of the middle row in degrees, up from level.
    pub pitch: f64,
    /// Degrees the cylinder's axis is tilted clockwise about the middle
    /// column's direction, to level a rig that isn't.
    pub roll: f64,
    pub horizontal_fov: f64,
}

impl View {
    /// Centered on the cameras and wide enough to show both.
    pub fn fitting(calibration: &Calibration) -> View {
        let [left, right] = &calibration.cameras;
        let mut view = View {
            yaw: (left.orientation.yaw + right.orientation.yaw) / 2.0,
            pitch: (left.orientation.pitch + right.orientation.pitch) / 2.0,
            roll: 0.0,
            horizontal_fov: 0.0,
        };
        let to_cylinder = view.cylinder_rotation().inverse();
        let widest = calibration
            .cameras
            .iter()
            .flat_map(|camera| {
                let lens = &camera.lens;
                let to_rig = camera.orientation.rotation();
                let middle = (lens.height as f64 - 1.0) / 2.0;
                [-0.5, lens.width as f64 - 0.5].map(move |x| {
                    let ray = to_cylinder * to_rig * lens.unproject(&Vector2::new(x, middle));
                    ray.x.atan2(ray.z).abs()
                })
            })
            .fold(0.0, f64::max);
        view.horizontal_fov = (2.0 * widest).to_degrees().clamp(10.0, 350.0);
        view
    }

    /// The rotation taking directions in the cylinder's frame (y along its
    /// axis, z toward the middle column) to the rig's frame.
    pub fn cylinder_rotation(&self) -> Rotation3<f64> {
        Rotation3::from_axis_angle(&Vector3::y_axis(), self.yaw.to_radians())
            * Rotation3::from_axis_angle(&Vector3::z_axis(), self.roll.to_radians())
    }

    /// Output pixels per radian of the cylinder.
    pub fn focal_length(&self, size: (u32, u32)) -> f64 {
        size.0 as f64 / self.horizontal_fov.to_radians()
    }

    /// The cylinder's height (down, per unit of radius) at the middle row.
    pub fn middle_height(&self) -> f64 {
        -self.pitch.to_radians().tan()
    }

    /// The direction (in the cylinder's frame, not unit length) shown at
    /// `pixel` of a `size` output.
    pub fn direction_at(&self, pixel: &Vector2<f64>, size: (u32, u32)) -> Vector3<f64> {
        let focal = self.focal_length(size);
        let azimuth = (pixel.x + 0.5 - size.0 as f64 / 2.0) / focal;
        let height = self.middle_height() + (pixel.y + 0.5 - size.1 as f64 / 2.0) / focal;
        Vector3::new(azimuth.sin(), height, azimuth.cos())
    }

    /// Where a direction (in the cylinder's frame) is shown on a `size`
    /// output; `None` along the cylinder's axis.
    pub fn pixel_of(&self, direction: &Vector3<f64>, size: (u32, u32)) -> Option<Vector2<f64>> {
        let across = direction.xz().norm();
        if across == 0.0 {
            return None;
        }
        let focal = self.focal_length(size);
        let azimuth = direction.x.atan2(direction.z);
        let height = direction.y / across - self.middle_height();
        Some(Vector2::new(
            azimuth * focal + size.0 as f64 / 2.0 - 0.5,
            height * focal + size.1 as f64 / 2.0 - 0.5,
        ))
    }
}

/// How the cameras' images are blended where they meet (Burt and Adelson's
/// multiresolution spline). Each image is split into `layers` bands of
/// detail with Gaussian blurs of \(\sigma_i = \sigma_0 k^i\): band \(i\) is
/// the image blurred by \(\sigma_i\) (not at all for band 0) less the image
/// blurred by \(\sigma_{i+1}\), and the last band is the blurriest image.
/// Band \(i\) is blended across the seam blurred by \(\sigma_i\), so fine
/// detail switches cameras sharply while overall brightness changes
/// gradually.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Blend {
    /// \(\sigma_0\) in pixels of an output [`Blend::REFERENCE_HEIGHT`] tall,
    /// scaled with the output's height.
    pub finest_sigma: f64,
    /// \(k\), the ratio between consecutive bands' blurs.
    pub sigma_ratio: f64,
    pub layers: u32,
}

impl Default for Blend {
    fn default() -> Self {
        Blend {
            finest_sigma: 2.0,
            sigma_ratio: 2.0,
            layers: 6,
        }
    }
}

impl Blend {
    pub const MAX_LAYERS: u32 = 10;
    pub const REFERENCE_HEIGHT: u32 = 1080;

    /// Each band's \(\sigma_i\) in pixels of an output `height` pixels
    /// tall, finest first.
    pub fn sigmas_for(&self, height: u32) -> Vec<f64> {
        let finest = self.finest_sigma.max(0.0) * height as f64 / Self::REFERENCE_HEIGHT as f64;
        let ratio = self.sigma_ratio.max(1.0);
        (0..self.layers.clamp(1, Self::MAX_LAYERS))
            .map(|layer| finest * ratio.powi(layer as i32))
            .collect()
    }
}

/// The stitched video's shape: its width over its height.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AspectRatio(pub f64);

impl Default for AspectRatio {
    fn default() -> Self {
        AspectRatio(16.0 / 9.0)
    }
}

impl AspectRatio {
    pub const MIN: f64 = 1.0;
    pub const MAX: f64 = 4.0;

    /// The size of this shape about `width` pixels wide that the stitcher
    /// can output: its width a multiple of 8 and its height even.
    pub fn size_with_width(self, width: f64) -> (u32, u32) {
        let width = ((width / 8.0).round() as u32).max(1) * 8;
        let height = width as f64 / self.0.clamp(Self::MIN, Self::MAX);
        (width, ((height / 2.0).round() as u32).max(1) * 2)
    }
}

/// Everything that decides how a rig's footage is stitched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StitchConfig {
    pub calibration: Calibration,
    pub view: View,
    pub blend: Blend,
    #[serde(default)]
    pub aspect_ratio: AspectRatio,
}

impl StitchConfig {
    /// `calibration` with the view [fitting](View::fitting) it, the default
    /// blend and the default shape.
    pub fn fitting(calibration: Calibration) -> StitchConfig {
        StitchConfig {
            view: View::fitting(&calibration),
            blend: Blend::default(),
            aspect_ratio: AspectRatio::default(),
            calibration,
        }
    }

    /// The output width at which the panorama has as many pixels per degree
    /// as the cameras' `input` sized pictures have in their middles.
    pub fn native_width(&self, input: (u32, u32)) -> f64 {
        let focal = self
            .calibration
            .cameras
            .iter()
            .map(|camera| {
                let lens = camera.lens.scaled_to(input.0, input.1);
                lens.fx.max(lens.fy)
            })
            .fold(0.0, f64::max);
        focal * self.view.horizontal_fov.to_radians()
    }

    /// Where `side`'s camera saw what `pixel` of a `size` output shows,
    /// measured in `input` sized images, and how far off its optical axis
    /// that is in radians; `None` if past the lens's [max angle](Lens::max_angle).
    pub fn source_of(
        &self,
        side: Side,
        pixel: &Vector2<f64>,
        size: (u32, u32),
        input: (u32, u32),
    ) -> Option<(Vector2<f64>, f64)> {
        let camera = self.calibration.camera(side);
        let lens = camera.lens.scaled_to(input.0, input.1);
        let to_camera = camera.orientation.rotation().inverse() * self.view.cylinder_rotation();
        let direction = to_camera * self.view.direction_at(pixel, size);
        let angle = direction.xy().norm().atan2(direction.z);
        Some((lens.project(&direction)?, angle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64, tolerance: f64) {
        assert!((a - b).abs() <= tolerance, "{a} is not within {tolerance} of {b}");
    }

    #[test]
    fn lens_unprojects_what_it_projects() {
        let lens = Lens::nominal();
        for pixel in [
            Vector2::new(0.0, 0.0),
            Vector2::new(1151.5, 647.5),
            Vector2::new(2303.0, 100.0),
            Vector2::new(300.0, 1295.0),
        ] {
            let back = lens.project(&lens.unproject(&pixel)).unwrap();
            assert_close(back.x, pixel.x, 1e-6);
            assert_close(back.y, pixel.y, 1e-6);
        }
    }

    #[test]
    fn nominal_lens_has_its_field_of_view() {
        let (horizontal, vertical) = Lens::nominal().field_of_view();
        assert_close(horizontal, 103.7, 0.1);
        assert_close(vertical, 64.7, 0.1);
    }

    #[test]
    fn scaling_a_lens_scales_where_rays_land() {
        let lens = Lens::nominal();
        let small = lens.scaled_to(1536, 864);
        let ray = Vector3::new(0.3, -0.2, 1.0);
        let full = lens.project(&ray).unwrap();
        let scaled = small.project(&ray).unwrap();
        assert_close((scaled.x + 0.5) * 1.5 - 0.5, full.x, 1e-9);
        assert_close((scaled.y + 0.5) * 1.5 - 0.5, full.y, 1e-9);
    }

    #[test]
    fn refocusing_magnifies_about_the_principal_point() {
        let lens = Lens::nominal();
        let refocused = lens.refocused(9.8);
        let scale = magnification(DEFAULT_FOCUS, 9.8);
        assert_close(scale, 1.0158, 1e-4);
        let center = Vector2::new(lens.cx, lens.cy);
        for ray in [Vector3::new(0.3, -0.2, 1.0), Vector3::new(-1.0, 0.4, 0.5)] {
            let before = lens.project(&ray).unwrap();
            let after = refocused.project(&ray).unwrap();
            assert!((after - center - (before - center) * scale).norm() < 1e-9);
        }
        let back = refocused.refocused(DEFAULT_FOCUS);
        assert_close(back.fx, lens.fx, 1e-9);
        assert_close(back.fy, lens.fy, 1e-9);
        let twice = lens.refocused(4.0).refocused(9.8);
        assert_close(twice.fx, refocused.fx, 1e-9);
    }

    #[test]
    fn lenses_from_before_their_focus_was_given_were_at_the_default() {
        let mut json = serde_json::to_value(Lens::nominal().refocused(9.8)).unwrap();
        json.as_object_mut().unwrap().remove("focus");
        let lens: Lens = serde_json::from_value(json).unwrap();
        assert_eq!(lens.focus, DEFAULT_FOCUS);
    }

    #[test]
    fn orientation_angles_turn_the_way_they_say() {
        let forward = |orientation: Orientation| orientation.rotation() * Vector3::z();
        let right = forward(Orientation {
            yaw: 30.0,
            ..Default::default()
        });
        assert!(right.x > 0.0);
        let up = forward(Orientation {
            pitch: 30.0,
            ..Default::default()
        });
        assert!(up.y < 0.0);
        let clockwise = Orientation {
            roll: 30.0,
            ..Default::default()
        }
        .rotation()
            * Vector3::x();
        assert!(clockwise.y > 0.0);
    }

    #[test]
    fn orientation_survives_a_round_trip_through_a_rotation() {
        let orientation = Orientation {
            yaw: -53.0,
            pitch: -25.0,
            roll: 4.0,
        };
        let back = Orientation::from_rotation(&orientation.rotation());
        assert_close(back.yaw, orientation.yaw, 1e-9);
        assert_close(back.pitch, orientation.pitch, 1e-9);
        assert_close(back.roll, orientation.roll, 1e-9);
    }

    #[test]
    fn view_finds_the_pixel_showing_a_direction() {
        let view = View {
            yaw: 10.0,
            pitch: -20.0,
            roll: 2.0,
            horizontal_fov: 180.0,
        };
        let size = (1920, 1080);
        let pixel = Vector2::new(400.0, 700.0);
        let back = view.pixel_of(&view.direction_at(&pixel, size), size).unwrap();
        assert_close(back.x, pixel.x, 1e-9);
        assert_close(back.y, pixel.y, 1e-9);
    }

    #[test]
    fn view_keeps_the_horizon_on_one_row() {
        let view = View {
            yaw: 30.0,
            pitch: -25.0,
            roll: 0.0,
            horizontal_fov: 200.0,
        };
        let size = (1920, 1080);
        let to_cylinder = view.cylinder_rotation().inverse();
        let rows: Vec<f64> = [-80.0_f64, -20.0, 45.0, 120.0]
            .iter()
            .map(|azimuth| {
                let level = Vector3::new(azimuth.to_radians().sin(), 0.0, azimuth.to_radians().cos());
                view.pixel_of(&(to_cylinder * level), size).unwrap().y
            })
            .collect();
        for row in &rows {
            assert_close(*row, rows[0], 1e-9);
        }
        assert!(rows[0] < 539.5, "looking down puts the horizon above the middle");
    }

    #[test]
    fn fitting_view_spans_both_cameras() {
        let calibration = Calibration::nominal([
            Orientation {
                yaw: -45.0,
                ..Default::default()
            },
            Orientation {
                yaw: 45.0,
                ..Default::default()
            },
        ]);
        let view = View::fitting(&calibration);
        assert_close(view.yaw, 0.0, 1e-9);
        assert_close(view.horizontal_fov, 90.0 + 102.0, 2.0);
    }

    #[test]
    fn aspect_ratio_gives_sizes_the_stitcher_takes() {
        assert_eq!(AspectRatio::default().size_with_width(1280.0), (1280, 720));
        assert_eq!(AspectRatio(2.0).size_with_width(1283.0), (1280, 640));
        assert_eq!(AspectRatio(2.35).size_with_width(1286.0), (1288, 548));
    }

    #[test]
    fn native_width_has_the_cameras_pixels_per_degree() {
        let calibration = Calibration::nominal([Orientation::default(); 2]);
        let config = StitchConfig {
            view: View {
                horizontal_fov: 180.0,
                ..View::fitting(&calibration)
            },
            ..StitchConfig::fitting(calibration)
        };
        assert_close(config.native_width(rig::RECORD_SIZE), 982.0 * PI, 1e-9);
        assert_close(config.native_width(rig::DEFAULT_PREVIEW_SIZE), 982.0 * PI * 2.0 / 3.0, 1e-9);
    }

    #[test]
    fn stitch_configs_from_before_their_shape_was_given_take_the_default() {
        let calibration = Calibration::nominal([Orientation::default(); 2]);
        let mut json = serde_json::to_value(StitchConfig::fitting(calibration)).unwrap();
        json.as_object_mut().unwrap().remove("aspect_ratio");
        let config: StitchConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.aspect_ratio, AspectRatio::default());
    }

    #[test]
    fn blend_scales_with_the_output() {
        let blend = Blend {
            finest_sigma: 3.0,
            sigma_ratio: 1.5,
            layers: 3,
        };
        assert_eq!(blend.sigmas_for(1080), [3.0, 4.5, 6.75]);
        assert_eq!(blend.sigmas_for(720), [2.0, 3.0, 4.5]);
    }

    #[test]
    fn blend_settings_from_before_its_blurs_were_given_take_the_defaults() {
        let blend: Blend = serde_json::from_str(r#"{"levels": 6, "feather": 4.0}"#).unwrap();
        assert_eq!(blend, Blend::default());
    }
}
