//! Calibrating a rig from its live previews with reco.

use anyhow::Result;
use bytes::Bytes;
use protocol::rig;
use reco_calibrate::{CalibrationConfig, YuvFrame};
use reco_core::calibration::MatchCalibration;
use reco_core::gpu::GpuContext;
use turbojpeg::{Decompressor, Subsamp, YuvImage};

use crate::live::preview_calibration;

/// Pairs of preview frames to calibrate from.
pub const PAIRS: usize = 20;

pub struct Calibrated {
    pub calibration: MatchCalibration,
    /// reco's confidence in the calibration, in `[0, 1]`.
    pub confidence: f64,
}

/// Finds where the cameras are pointed from pairs of left and right preview
/// JPEGs. The lenses and the viewing settings stay as in `current`.
pub fn calibrate(current: &MatchCalibration, previews: &[[Bytes; 2]]) -> Result<Calibrated> {
    let mut decompressor = Decompressor::new()?;
    let pairs = previews
        .iter()
        .map(|[left, right]| {
            Ok((
                decode(&mut decompressor, left)?,
                decode(&mut decompressor, right)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let lenses = preview_calibration(current);
    let gpu = GpuContext::new_blocking()?;
    let result = reco_calibrate::calibrate(
        &gpu,
        &pairs,
        &lenses.left,
        &lenses.right,
        &CalibrationConfig::default(),
    )?;
    let mut calibration = current.clone();
    calibration.layout = result.calibration.layout;
    Ok(Calibrated {
        calibration,
        confidence: result.confidence,
    })
}

fn decode(decompressor: &mut Decompressor, jpeg: &[u8]) -> Result<YuvFrame> {
    let (width, height) = rig::PREVIEW_SIZE;
    let luma = (width * height) as usize;
    let mut pixels = vec![0; luma * 3 / 2];
    decompressor.decompress_to_yuv(
        jpeg,
        YuvImage {
            pixels: &mut pixels[..],
            width: width as usize,
            align: 1,
            height: height as usize,
            subsamp: Subsamp::Sub2x2,
        },
    )?;
    let v = pixels.split_off(luma * 5 / 4);
    let u = pixels.split_off(luma);
    Ok(YuvFrame {
        y: pixels,
        u,
        v,
        width,
        height,
        timestamp_us: 0,
    })
}
