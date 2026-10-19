//! The scoreboard overlay, composited onto video after it is stitched.
//!
//! The overlay is an image of Arctos's scoreboard page rendered by Chromium
//! (what OBS browser sources use), laid out [`PAGE_SIZE`] CSS pixels and
//! scaled to [`Overlay::width`] percent of the video. Recordings use
//! Arctos's renderings of each scoreboard state of the match; live previews
//! use screenshots of the live scoreboard page (see [`crate::browser`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use arctos::Arctos;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::browser;

/// Size the scoreboard page is laid out at, in CSS pixels. The width matches
/// the layout of Arctos's renderer.
pub const PAGE_SIZE: (u32, u32) = (800, 200);
const SCREENSHOT_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Placement of the scoreboard overlay on the video.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub enabled: bool,
    pub horizontal: Align,
    pub vertical: Align,
    /// Offsets from the anchor, in percent of the video's width/height,
    /// towards the center.
    pub x: f64,
    pub y: f64,
    /// In percent of the video's width.
    pub width: f64,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            enabled: true,
            horizontal: Align::Start,
            vertical: Align::Start,
            x: 2.0,
            y: 3.0,
            width: 35.0,
        }
    }
}

/// Left/top, center, or right/bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

impl Align {
    pub const ALL: [Align; 3] = [Align::Start, Align::Center, Align::End];
}

impl Overlay {
    /// Size in pixels of the overlay image on a `video` sized frame.
    pub fn image_size(&self, video: (u32, u32)) -> (u32, u32) {
        let width = (video.0 as f64 * self.width / 100.0).round().max(1.0);
        let height = (width * PAGE_SIZE.1 as f64 / PAGE_SIZE.0 as f64).round();
        (width as u32, height as u32)
    }

    /// Scale from CSS pixels to video pixels.
    pub fn scale(&self, video: (u32, u32)) -> f64 {
        self.image_size(video).0 as f64 / PAGE_SIZE.0 as f64
    }

    /// Top left corner of the overlay image on a `video` sized frame.
    pub fn position(&self, video: (u32, u32)) -> (i64, i64) {
        let image = self.image_size(video);
        let place = |align: Align, offset: f64, video: u32, image: u32| {
            let offset = video as f64 * offset / 100.0;
            let position = match align {
                Align::Start => offset,
                Align::Center => (video as f64 - image as f64) / 2.0 + offset,
                Align::End => video as f64 - image as f64 - offset,
            };
            position.round() as i64
        };
        (
            place(self.horizontal, self.x, video.0, image.0),
            place(self.vertical, self.y, video.1, image.1),
        )
    }
}

/// An RGBA image with straight (not premultiplied) alpha.
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    pub fn decode_png(png: &[u8]) -> Result<Self> {
        let mut decoder = png::Decoder::new(png);
        decoder.set_transformations(
            png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
        );
        let mut reader = decoder.read_info()?;
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels)?;
        if info.color_type != png::ColorType::Rgba {
            bail!("unexpected PNG color type {:?}", info.color_type);
        }
        pixels.truncate(info.buffer_size());
        Ok(RgbaImage {
            width: info.width,
            height: info.height,
            pixels,
        })
    }

    /// Draws this image over an RGBA `frame` with its top left corner at
    /// `position`, clipping whatever falls outside the frame.
    pub fn draw_onto(&self, frame: &mut [u8], frame_size: (u32, u32), position: (i64, i64)) {
        let clip = |position: i64, size: u32, frame: u32| {
            let start = position.clamp(0, frame as i64);
            let end = (position + size as i64).clamp(0, frame as i64);
            (start as usize, end as usize, (start - position) as usize)
        };
        let (x_start, x_end, image_x) = clip(position.0, self.width, frame_size.0);
        let (y_start, y_end, image_y) = clip(position.1, self.height, frame_size.1);
        let row_bytes = (x_end - x_start) * 4;
        for row in 0..y_end.saturating_sub(y_start) {
            let frame_start = ((y_start + row) * frame_size.0 as usize + x_start) * 4;
            let image_start = ((image_y + row) * self.width as usize + image_x) * 4;
            let destination = &mut frame[frame_start..frame_start + row_bytes];
            let source = &self.pixels[image_start..image_start + row_bytes];
            for (destination, source) in destination.chunks_exact_mut(4).zip(source.chunks_exact(4))
            {
                let alpha = source[3] as u32;
                for channel in 0..3 {
                    let blended = source[channel] as u32 * alpha
                        + destination[channel] as u32 * (255 - alpha);
                    destination[channel] = ((blended + 127) / 255) as u8;
                }
            }
        }
    }
}

/// The live scoreboard of a field, to show over its live preview.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveOverlay {
    pub tournament: String,
    pub field: String,
    pub placement: Overlay,
}

/// An overlay image and where to draw it.
pub struct PlacedImage {
    pub image: RgbaImage,
    pub position: (i64, i64),
}

/// Keeps `images` showing the live scoreboard described by `config`, placed
/// on a `video` sized frame. Returns once `config` closes.
pub async fn follow_live(
    arctos: &Arctos,
    mut config: watch::Receiver<Option<LiveOverlay>>,
    images: &watch::Sender<Option<Arc<PlacedImage>>>,
    video: (u32, u32),
) {
    loop {
        let current = config.borrow_and_update().clone();
        images.send_replace(None);
        let changed = match current {
            Some(current) => tokio::select! {
                never = show_live(arctos, &current, images, video) => never,
                changed = config.changed() => changed,
            },
            None => config.changed().await,
        };
        if changed.is_err() {
            return;
        }
    }
}

async fn show_live(
    arctos: &Arctos,
    overlay: &LiveOverlay,
    images: &watch::Sender<Option<Arc<PlacedImage>>>,
    video: (u32, u32),
) -> ! {
    let url = format!(
        "{}{}",
        arctos.url(),
        arctos::scoreboard_path(&overlay.tournament, &overlay.field)
    );
    let headers: Vec<(String, String)> = arctos
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_owned())))
        .collect();
    let position = overlay.placement.position(video);
    let show = async || -> Result<()> {
        let mut page =
            browser::Page::open(&url, &headers, PAGE_SIZE, overlay.placement.scale(video)).await?;
        let mut interval = tokio::time::interval(SCREENSHOT_INTERVAL);
        loop {
            interval.tick().await;
            let image = RgbaImage::decode_png(&page.screenshot().await?)?;
            images.send_replace(Some(Arc::new(PlacedImage { image, position })));
        }
    };
    loop {
        if let Err(e) = show().await {
            tracing::warn!("showing the live scoreboard of {}: {e:#}", overlay.field);
        }
        images.send_replace(None);
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_match_the_preview_layout() {
        let video = (1000, 500);
        let overlay = |horizontal, vertical| Overlay {
            enabled: true,
            horizontal,
            vertical,
            x: 10.0,
            y: 10.0,
            width: 40.0,
        };
        assert_eq!(
            overlay(Align::Start, Align::Start).image_size(video),
            (400, 100)
        );
        assert_eq!(
            overlay(Align::Start, Align::Start).position(video),
            (100, 50)
        );
        assert_eq!(
            overlay(Align::Center, Align::Center).position(video),
            (400, 250)
        );
        assert_eq!(overlay(Align::End, Align::End).position(video), (500, 350));
    }

    #[test]
    fn draws_with_alpha_and_clips_to_the_frame() {
        let image = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 255, 255, 255, 0],
        };
        let mut frame = vec![0, 0, 0, 255, 10, 10, 10, 255];
        image.draw_onto(&mut frame, (2, 1), (1, 0));
        assert_eq!(frame, vec![0, 0, 0, 255, 255, 0, 0, 255]);
        let mut frame = vec![9; 8];
        image.draw_onto(&mut frame, (2, 1), (-1, 0));
        assert_eq!(frame, vec![9; 8]);
        image.draw_onto(&mut frame, (2, 1), (5, 5));
        assert_eq!(frame, vec![9; 8]);
    }
}
