//! Stitching a camera's live previews into a panorama preview.

use anyhow::Result;
use bytes::Bytes;
use protocol::{Side, rig};
use reco_core::calibration::MatchCalibration;
use reco_core::core::StitchCore;
use reco_core::core::types::{RenderOutcome, StitchCoreConfig};
use reco_core::gpu::GpuContext;
use reco_core::render::pipeline::YuvPlanes;
use reco_core::render::renderer::InputFormat;
use reco_core::render::viewport::ViewportConfig;
use turbojpeg::{Compressor, Decompressor, Image, PixelFormat, Subsamp, YuvImage};

use crate::overlay::PlacedImage;

/// Size of the stitched preview.
pub const OUTPUT_SIZE: (u32, u32) = (1280, 720);
/// Frame rate of the previews.
pub const FPS: u32 = rig::FPS / rig::PREVIEW_DECIMATION;
const JPEG_QUALITY: i32 = 70;

pub struct LiveStitcher {
    core: StitchCore,
    compressor: Compressor,
    /// The stitched frame with the overlay drawn on it.
    composited: Vec<u8>,
}

impl LiveStitcher {
    pub fn new(calibration: &MatchCalibration) -> Result<Self> {
        let (width, height) = rig::PREVIEW_SIZE;
        let mut config = StitchCoreConfig::new(
            preview_calibration(calibration),
            width,
            height,
            InputFormat::Yuv420p,
        );
        config.viewport = ViewportConfig {
            width: OUTPUT_SIZE.0,
            height: OUTPUT_SIZE.1,
            blend_width: calibration.blend_width,
            rig_tilt: calibration.rig_tilt as f32,
            rig_roll: calibration.rig_roll as f32,
            lens_correction_amount: calibration.lens_correction_amount,
            ..ViewportConfig::default()
        };
        let mut core = StitchCore::new(GpuContext::new_blocking()?, config)?;
        // Previews are decoded from JPEGs, which are full range.
        core.pipeline_mut().set_full_range(true);
        // Where the cameras' views don't meet, the panorama has black gaps;
        // constraining the view to avoid them can zoom it in to nothing.
        core.set_constrained_look(false);
        let mut compressor = Compressor::new()?;
        compressor.set_quality(JPEG_QUALITY)?;
        Ok(LiveStitcher {
            core,
            compressor,
            composited: Vec::new(),
        })
    }

    pub fn set_calibration(&mut self, calibration: &MatchCalibration) {
        self.core
            .update_calibration(preview_calibration(calibration));
    }

    /// Stitches a pair of previews into a JPEG, with `overlay` drawn on
    /// top; `None` while the GPU is still busy with earlier frames.
    pub fn stitch(
        &mut self,
        frame: &StereoYuv,
        overlay: Option<&PlacedImage>,
    ) -> Result<Option<Bytes>> {
        let [left, right] = frame.planes();
        let RenderOutcome::Rgba(rgba) = self.core.submit_frame_yuv(&left, &right)? else {
            return Ok(None);
        };
        let pixels = match overlay {
            Some(overlay) => {
                self.composited.clear();
                self.composited.extend_from_slice(rgba);
                overlay
                    .image
                    .draw_onto(&mut self.composited, OUTPUT_SIZE, overlay.position);
                &self.composited[..]
            }
            None => rgba,
        };
        let (width, height) = (OUTPUT_SIZE.0 as usize, OUTPUT_SIZE.1 as usize);
        let jpeg = self.compressor.compress_to_vec(Image {
            pixels,
            width,
            pitch: width * 4,
            height,
            format: PixelFormat::RGBA,
        })?;
        Ok(Some(jpeg.into()))
    }
}

/// The calibration is for the recording resolution; previews have the same
/// field of view at a lower resolution.
pub(crate) fn preview_calibration(calibration: &MatchCalibration) -> MatchCalibration {
    let scale = rig::PREVIEW_SIZE.0 as f64 / rig::RECORD_SIZE.0 as f64;
    let mut scaled = calibration.clone();
    for camera in [&mut scaled.left, &mut scaled.right] {
        camera.width = rig::PREVIEW_SIZE.0;
        camera.height = rig::PREVIEW_SIZE.1;
        camera.fx *= scale;
        camera.fy *= scale;
        camera.cx *= scale;
        camera.cy *= scale;
    }
    scaled
}

/// A pair of full-range YUV420P previews, the left one stacked on top of the
/// right one. That makes it one ordinary [`StereoYuv::SIZE`] YUV420P frame
/// for ffmpeg, while each camera's planes are still contiguous.
pub struct StereoYuv {
    data: Vec<u8>,
    scratch: Vec<u8>,
}

impl StereoYuv {
    pub const SIZE: (u32, u32) = (rig::PREVIEW_SIZE.0, rig::PREVIEW_SIZE.1 * 2);
    /// Luma bytes of one camera.
    const LUMA: usize = (rig::PREVIEW_SIZE.0 * rig::PREVIEW_SIZE.1) as usize;
    pub const BYTES: usize = Self::LUMA * 3;

    pub fn new() -> Self {
        StereoYuv {
            data: vec![0; Self::BYTES],
            scratch: vec![0; Self::LUMA * 3 / 2],
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Decodes one camera's preview JPEG into its half of the frame.
    pub fn decode(
        &mut self,
        decompressor: &mut Decompressor,
        side: Side,
        jpeg: &[u8],
    ) -> Result<()> {
        decompressor.decompress_to_yuv(
            jpeg,
            YuvImage {
                pixels: &mut self.scratch[..],
                width: rig::PREVIEW_SIZE.0 as usize,
                align: 1,
                height: rig::PREVIEW_SIZE.1 as usize,
                subsamp: Subsamp::Sub2x2,
            },
        )?;
        let (y, chroma) = self.scratch.split_at(Self::LUMA);
        let (u, v) = chroma.split_at(Self::LUMA / 4);
        for (plane, src) in [y, u, v].into_iter().enumerate() {
            let start = Self::plane_start(plane) + side.index() * src.len();
            self.data[start..start + src.len()].copy_from_slice(src);
        }
        Ok(())
    }

    /// Each camera's planes.
    pub fn planes(&self) -> [YuvPlanes<'_>; 2] {
        Side::BOTH.map(|side| {
            let [y, u, v] = [0, 1, 2].map(|plane| {
                let len = if plane == 0 {
                    Self::LUMA
                } else {
                    Self::LUMA / 4
                };
                let start = Self::plane_start(plane) + side.index() * len;
                &self.data[start..start + len]
            });
            YuvPlanes { y, u, v }
        })
    }

    /// Offset of the stacked frame's Y, U or V plane.
    fn plane_start(plane: usize) -> usize {
        [0, Self::LUMA * 2, Self::LUMA * 5 / 2][plane]
    }
}

impl Default for StereoYuv {
    fn default() -> Self {
        Self::new()
    }
}
