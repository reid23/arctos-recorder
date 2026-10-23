//! Stitching a camera's live previews into a panorama preview.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use protocol::{Side, rig};
use turbojpeg::{Compressor, Decompressor, Subsamp, YuvImage};

use crate::geometry::StitchConfig;
use crate::gpu::{Gpu, GpuChoice, Output, OverlayImage, Stitcher, YuvPlanes};
use crate::overlay::PlacedImage;

/// Frame rate of the previews.
pub const FPS: u32 = rig::FPS / rig::PREVIEW_DECIMATION;
/// Of the stitched panoramas, which are compressed again to be restreamed.
const JPEG_QUALITY: i32 = 90;

pub struct LiveStitcher {
    stitcher: Stitcher,
    preview_size: (u32, u32),
    compressor: Compressor,
    /// The overlay drawn now, so it's only uploaded when it changes.
    overlay: Option<Arc<PlacedImage>>,
}

impl LiveStitcher {
    /// Stitches `preview_size` previews into `size` panoramas on `gpu`.
    pub fn new(
        config: &StitchConfig,
        preview_size: (u32, u32),
        size: (u32, u32),
        gpu: GpuChoice,
    ) -> Result<Self> {
        let stitcher = Stitcher::new(&Gpu::shared(gpu)?, preview_size, size, Output::Jpeg, config)?;
        Ok(LiveStitcher {
            stitcher,
            preview_size,
            compressor: panorama_compressor()?,
            overlay: None,
        })
    }

    pub fn configure(&mut self, config: &StitchConfig) -> Result<()> {
        self.stitcher.configure(config)
    }

    pub fn preview_size(&self) -> (u32, u32) {
        self.preview_size
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.stitcher.output_size()
    }

    /// Stitches a pair of previews into a JPEG with `overlay` drawn on top.
    /// Waits for the result, so no pair submitted with [`LiveStitcher::start`]
    /// may still be on the GPU.
    pub fn stitch(&mut self, frame: &StereoYuv, overlay: Option<&Arc<PlacedImage>>) -> Result<Bytes> {
        self.prepare(frame, overlay)?;
        let stitched = self.stitcher.stitch(frame.planes())?;
        panorama_jpeg(&mut self.compressor, stitched, self.output_size())
    }

    /// Submits a pair of previews without waiting, so the next pair can be
    /// uploaded while this one finishes. [`LiveStitcher::take`] reads the
    /// oldest submitted pair back. The overlay is read when a frame is
    /// packed, which may be the one already on the GPU, so a new overlay
    /// can show up one frame early.
    pub fn start(&mut self, frame: &StereoYuv, overlay: Option<&Arc<PlacedImage>>) -> Result<()> {
        self.prepare(frame, overlay)?;
        self.stitcher.start_frame(frame.planes())
    }

    fn prepare(&mut self, frame: &StereoYuv, overlay: Option<&Arc<PlacedImage>>) -> Result<()> {
        ensure!(
            frame.preview_size() == self.preview_size,
            "the previews are {:?}, not the {:?} being stitched",
            frame.preview_size(),
            self.preview_size
        );
        let unchanged = match (overlay, &self.overlay) {
            (Some(new), Some(current)) => Arc::ptr_eq(new, current),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.overlay = overlay.cloned();
            self.stitcher.set_overlay(overlay.map(|overlay| OverlayImage {
                image: &overlay.image,
                position: overlay.position,
            }));
        }
        Ok(())
    }

    /// The oldest pair of previews [submitted](LiveStitcher::start), as
    /// planar YUV 4:2:0.
    pub fn take(&mut self) -> Result<Vec<u8>> {
        Ok(self.stitcher.take_frame()?.to_vec())
    }
}

pub fn panorama_compressor() -> Result<Compressor> {
    let mut compressor = Compressor::new()?;
    compressor.set_quality(JPEG_QUALITY)?;
    compressor.set_subsamp(Subsamp::Sub2x2)?;
    Ok(compressor)
}

/// `yuv` is planar YUV 4:2:0 at `size`, as [`LiveStitcher::take`] returns.
pub fn panorama_jpeg(compressor: &mut Compressor, yuv: &[u8], size: (u32, u32)) -> Result<Bytes> {
    let (width, height) = size;
    let jpeg = compressor
        .compress_yuv_to_vec(YuvImage {
            pixels: yuv,
            width: width as usize,
            align: 1,
            height: height as usize,
            subsamp: Subsamp::Sub2x2,
        })
        .context("compressing the panorama")?;
    Ok(jpeg.into())
}

/// The size of a pair of preview JPEGs; `None` if they differ, as they do
/// for a moment while the cameras change it.
pub fn pair_size(jpegs: [&[u8]; 2]) -> Result<Option<(u32, u32)>> {
    let [left, right] = jpegs.map(|jpeg| {
        turbojpeg::read_header(jpeg)
            .map(|header| (header.width as u32, header.height as u32))
            .context("reading a preview's size")
    });
    let (left, right) = (left?, right?);
    Ok((left == right).then_some(left))
}

/// A pair of full-range YUV420P previews, the left one stacked on top of the
/// right one. That makes it one ordinary [`StereoYuv::stacked_size`]
/// YUV420P frame for ffmpeg, while each camera's planes are still
/// contiguous.
pub struct StereoYuv {
    preview_size: (u32, u32),
    data: Vec<u8>,
    /// Each camera's preview as decoded.
    scratch: [Vec<u8>; 2],
}

impl StereoYuv {
    pub fn new(preview_size: (u32, u32)) -> Self {
        let luma = (preview_size.0 * preview_size.1) as usize;
        StereoYuv {
            preview_size,
            data: vec![0; luma * 3],
            scratch: [(); 2].map(|()| vec![0; luma * 3 / 2]),
        }
    }

    pub fn preview_size(&self) -> (u32, u32) {
        self.preview_size
    }

    pub fn stacked_size(&self) -> (u32, u32) {
        (self.preview_size.0, self.preview_size.1 * 2)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Luma bytes of one camera.
    fn luma(&self) -> usize {
        (self.preview_size.0 * self.preview_size.1) as usize
    }

    /// Decodes both cameras' preview JPEGs (indexed by [`Side::index`]) into
    /// their halves of the frame, in parallel.
    pub fn decode(&mut self, decompressors: &mut [Decompressor; 2], jpegs: [&[u8]; 2]) -> Result<()> {
        let (width, height) = self.preview_size;
        let decode_preview = |decompressor: &mut Decompressor, jpeg: &[u8], pixels: &mut [u8]| -> Result<()> {
            let header = decompressor.read_header(jpeg).context("reading a preview's size")?;
            ensure!(
                (header.width, header.height) == (width as usize, height as usize),
                "a preview is {} by {}, not {width} by {height}",
                header.width,
                header.height
            );
            decompressor
                .decompress_to_yuv(
                    jpeg,
                    YuvImage {
                        pixels,
                        width: width as usize,
                        align: 1,
                        height: height as usize,
                        subsamp: Subsamp::Sub2x2,
                    },
                )
                .context("decoding a preview")
        };
        let [left_decompressor, right_decompressor] = decompressors;
        let [left_pixels, right_pixels] = &mut self.scratch;
        std::thread::scope(|scope| {
            let right = scope.spawn(|| decode_preview(right_decompressor, jpegs[1], right_pixels));
            decode_preview(left_decompressor, jpegs[0], left_pixels)?;
            right
                .join()
                .map_err(|_| anyhow::anyhow!("decoding a preview panicked"))?
        })?;
        let luma = self.luma();
        for (side, decoded) in Side::BOTH.into_iter().zip(&self.scratch) {
            let (y, chroma) = decoded.split_at(luma);
            let (u, v) = chroma.split_at(luma / 4);
            for (plane, source) in [y, u, v].into_iter().enumerate() {
                let start = plane_start(luma, plane) + side.index() * source.len();
                self.data[start..start + source.len()].copy_from_slice(source);
            }
        }
        Ok(())
    }

    /// Each camera's planes.
    pub fn planes(&self) -> [YuvPlanes<'_>; 2] {
        let luma = self.luma();
        Side::BOTH.map(|side| {
            let [y, u, v] = [0, 1, 2].map(|plane| {
                let length = if plane == 0 { luma } else { luma / 4 };
                let start = plane_start(luma, plane) + side.index() * length;
                &self.data[start..start + length]
            });
            YuvPlanes { y, u, v }
        })
    }
}

/// Offset of the stacked frame's Y, U or V plane, for previews with `luma`
/// bytes of luma each.
fn plane_start(luma: usize, plane: usize) -> usize {
    [0, luma * 2, luma * 5 / 2][plane]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_preview(size: (u32, u32), shade: u8) -> Vec<u8> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let pixels = vec![shade; width * height * 3];
        let image = turbojpeg::Image {
            pixels: pixels.as_slice(),
            width,
            pitch: width * 3,
            height,
            format: turbojpeg::PixelFormat::RGB,
        };
        turbojpeg::compress(image, 95, Subsamp::Sub2x2).unwrap().to_vec()
    }

    fn decompressors() -> [Decompressor; 2] {
        [Decompressor::new().unwrap(), Decompressor::new().unwrap()]
    }

    fn near(values: &[u8], expected: u8) -> bool {
        values.iter().all(|value| value.abs_diff(expected) <= 1)
    }

    #[test]
    fn previews_of_any_size_are_stacked_left_over_right() {
        let size = (64, 36);
        let luma = 64 * 36;
        let [left, right] = [gray_preview(size, 40), gray_preview(size, 200)];
        assert_eq!(pair_size([&left, &right]).unwrap(), Some(size));
        let mut frame = StereoYuv::new(size);
        assert_eq!(frame.stacked_size(), (64, 72));
        frame.decode(&mut decompressors(), [&left, &right]).unwrap();
        assert_eq!(frame.bytes().len(), luma * 3);
        assert!(near(&frame.bytes()[..luma], 40));
        assert!(near(&frame.bytes()[luma..luma * 2], 200));
        for (planes, shade) in frame.planes().into_iter().zip([40, 200]) {
            assert_eq!((planes.y.len(), planes.u.len(), planes.v.len()), (luma, luma / 4, luma / 4));
            assert!(near(planes.y, shade));
            assert!(near(planes.u, 128) && near(planes.v, 128));
        }
    }

    #[test]
    fn previews_of_another_size_are_refused() {
        let [small, big] = [gray_preview((64, 36), 40), gray_preview((128, 72), 40)];
        assert_eq!(pair_size([&small, &big]).unwrap(), None);
        let mut frame = StereoYuv::new((128, 72));
        assert!(frame.decode(&mut decompressors(), [&small, &small]).is_err());
    }
}
