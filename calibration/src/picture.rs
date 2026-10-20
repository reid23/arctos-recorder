//! Decoding previews into OpenCV images, and encoding results to show.

use anyhow::{Result, bail};
use bytes::Bytes;
use opencv::core::{Mat, Size, Vec3b};
use opencv::imgproc;
use opencv::prelude::*;
use turbojpeg::{Image, PixelFormat, Subsamp};

/// Quality of the JPEGs made to show results.
const JPEG_QUALITY: i32 = 85;

pub fn decode_gray(jpeg: &[u8]) -> Result<Mat> {
    let image = turbojpeg::decompress(jpeg, PixelFormat::GRAY)?;
    let borrowed =
        Mat::new_rows_cols_with_data::<u8>(image.height as i32, image.width as i32, &image.pixels)?;
    Ok(borrowed.try_clone()?)
}

pub fn decode_rgb(jpeg: &[u8]) -> Result<Mat> {
    let image = turbojpeg::decompress(jpeg, PixelFormat::RGB)?;
    let borrowed = Mat::new_rows_cols_with_bytes::<Vec3b>(
        image.height as i32,
        image.width as i32,
        &image.pixels,
    )?;
    Ok(borrowed.try_clone()?)
}

/// Encodes a grayscale or RGB image.
pub fn encode(image: &Mat) -> Result<Bytes> {
    let image = if image.is_continuous() {
        image.clone()
    } else {
        image.try_clone()?
    };
    let (format, subsampling) = match image.channels() {
        1 => (PixelFormat::GRAY, Subsamp::Gray),
        3 => (PixelFormat::RGB, Subsamp::Sub2x2),
        channels => bail!("can't encode an image with {channels} channels"),
    };
    let width = image.cols() as usize;
    let jpeg = turbojpeg::compress(
        Image {
            pixels: image.data_bytes()?,
            width,
            pitch: width * format.size(),
            height: image.rows() as usize,
            format,
        },
        JPEG_QUALITY,
        subsampling,
    )?;
    Ok(Bytes::copy_from_slice(&jpeg))
}

/// A copy of a JPEG `width` pixels wide.
pub fn thumbnail(jpeg: &[u8], width: u32) -> Result<Bytes> {
    let picture = decode_rgb(jpeg)?;
    let height = (picture.rows() as f64 * width as f64 / picture.cols() as f64).round() as i32;
    let mut small = Mat::default();
    imgproc::resize(
        &picture,
        &mut small,
        Size::new(width as i32, height.max(1)),
        0.0,
        0.0,
        imgproc::INTER_AREA,
    )?;
    encode(&small)
}
