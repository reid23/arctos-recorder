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

use crate::browser::{self, Chromium};

/// Size the scoreboard page is laid out at, in CSS pixels. The width matches
/// the layout of Arctos's renderer.
pub const PAGE_SIZE: (u32, u32) = (800, 200);
/// Overlay images are a multiple of this wide, so the page scaled to their
/// width is a whole number of pixels high: Chromium fills a partly covered
/// last row of a screenshot with the page's top row.
const WIDTH_STEP: u32 = PAGE_SIZE.0 / PAGE_SIZE.1;
const SCREENSHOT_INTERVAL: Duration = Duration::from_secs(1);
/// Screenshots that may fail in a row, e.g. while the page reloads, before
/// it is opened again.
const SCREENSHOT_FAILURES: u32 = 5;
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
        let steps = (video.0 as f64 * self.width / 100.0 / WIDTH_STEP as f64).round().max(1.0);
        let width = steps as u32 * WIDTH_STEP;
        (width, width * PAGE_SIZE.1 / PAGE_SIZE.0)
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
/// on a frame of the size given with it. The scoreboard page stays open
/// between changes: a new position redraws the last screenshot, a new width
/// rescales the page and takes one immediately, and only a different
/// tournament or field loads another page. Returns once `config` closes.
pub async fn follow_live(
    arctos: &Arctos,
    chromium: &Chromium,
    mut config: watch::Receiver<Option<(LiveOverlay, (u32, u32))>>,
    images: &watch::Sender<Option<Arc<PlacedImage>>>,
) {
    let mut page: Option<browser::Page> = None;
    let mut shown: Option<(String, String, f64)> = None;
    let mut image: Option<Arc<RgbaImage>> = None;
    let mut failures = 0;
    let mut interval = tokio::time::interval(SCREENSHOT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    loop {
        let current = config.borrow_and_update().clone();
        let Some((overlay, video)) = current else {
            images.send_replace(None);
            page = None;
            shown = None;
            image = None;
            failures = 0;
            if config.changed().await.is_err() {
                return;
            }
            continue;
        };

        let scale = overlay.placement.scale(video);
        let different_page = page.is_none()
            || shown.as_ref().is_none_or(|(tournament, field, _)| {
                tournament != &overlay.tournament || field != &overlay.field
            });
        let mut take_screenshot = false;
        if different_page {
            match open_page(arctos, chromium, &overlay, scale).await {
                Ok(opened) => {
                    page = Some(opened);
                    shown = Some((overlay.tournament.clone(), overlay.field.clone(), scale));
                    failures = 0;
                    take_screenshot = true;
                }
                Err(error) => {
                    tracing::warn!("showing the live scoreboard of {}: {error:#}", overlay.field);
                    page = None;
                    shown = None;
                    tokio::select! {
                        changed = config.changed() => {
                            if changed.is_err() {
                                return;
                            }
                        }
                        () = tokio::time::sleep(RETRY_INTERVAL) => {}
                    }
                    continue;
                }
            }
        } else if shown.as_ref().is_some_and(|(_, _, shown_scale)| *shown_scale != scale) {
            let rescaled = match page.as_mut() {
                Some(page) => page.set_scale(PAGE_SIZE, scale).await,
                None => Ok(()),
            };
            if let Err(error) = rescaled {
                tracing::warn!("rescaling the live scoreboard of {}: {error:#}", overlay.field);
                page = None;
                shown = None;
                continue;
            }
            shown = Some((overlay.tournament.clone(), overlay.field.clone(), scale));
            take_screenshot = true;
        }

        if let Some(image) = &image {
            publish(images, image, &overlay, video);
        }
        if take_screenshot {
            if !capture(&mut page, &mut image, &mut failures, images, &overlay, video).await {
                page = None;
                shown = None;
            }
            interval.reset();
        }

        tokio::select! {
            changed = config.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = interval.tick() => {
                if !capture(&mut page, &mut image, &mut failures, images, &overlay, video).await {
                    page = None;
                    shown = None;
                }
            }
        }
    }
}

async fn open_page(
    arctos: &Arctos,
    chromium: &Chromium,
    overlay: &LiveOverlay,
    scale: f64,
) -> Result<browser::Page> {
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
    browser::Page::open(chromium, &url, &headers, PAGE_SIZE, scale).await
}

/// Replaces `image` with a screenshot. `false` once that has failed
/// [`SCREENSHOT_FAILURES`] times in a row and the page should be opened again.
async fn capture(
    page: &mut Option<browser::Page>,
    image: &mut Option<Arc<RgbaImage>>,
    failures: &mut u32,
    images: &watch::Sender<Option<Arc<PlacedImage>>>,
    overlay: &LiveOverlay,
    video: (u32, u32),
) -> bool {
    let Some(page) = page.as_mut() else {
        return false;
    };
    match page.screenshot().await.and_then(|png| RgbaImage::decode_png(&png)) {
        Ok(shot) => {
            *failures = 0;
            let shot = Arc::new(shot);
            publish(images, &shot, overlay, video);
            *image = Some(shot);
            true
        }
        Err(error) => {
            *failures += 1;
            if *failures == SCREENSHOT_FAILURES {
                tracing::warn!("showing the live scoreboard of {}: {error:#}", overlay.field);
                false
            } else {
                true
            }
        }
    }
}

fn publish(
    images: &watch::Sender<Option<Arc<PlacedImage>>>,
    image: &RgbaImage,
    overlay: &LiveOverlay,
    video: (u32, u32),
) {
    images.send_replace(Some(Arc::new(PlacedImage {
        image: RgbaImage {
            width: image.width,
            height: image.height,
            pixels: image.pixels.clone(),
        },
        position: overlay.placement.position(video),
    })));
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

    fn overlay_of_width(width: f64) -> Overlay {
        Overlay {
            width,
            ..Overlay::default()
        }
    }

    #[test]
    fn images_are_the_page_scaled_to_whole_pixels() {
        for video_width in [1280, 1920, 2970, 3000, 3840] {
            for percent in (10..=1000).map(|tenths| tenths as f64 / 10.0) {
                let overlay = overlay_of_width(percent);
                let (width, height) = overlay.image_size((video_width, 1000));
                let scale = overlay.scale((video_width, 1000));
                assert_eq!(width * PAGE_SIZE.1, height * PAGE_SIZE.0);
                assert!((f64::from(PAGE_SIZE.1) * scale - f64::from(height)).abs() < 1e-9);
                assert!((f64::from(width) - f64::from(video_width) * percent / 100.0).abs() <= 2.0);
            }
        }
    }

    #[tokio::test]
    #[ignore = "needs Chromium"]
    async fn nothing_is_drawn_below_the_page() {
        let page = "data:text/html,<body style='margin:0'>\
            <div style='height:50px;background:white'></div>";
        let video = (3000, 1000);
        let overlay = overlay_of_width(34.5);
        let mut page = browser::Page::open(&Chromium::installed(), page, &[], PAGE_SIZE, overlay.scale(video))
            .await
            .unwrap();
        let image = RgbaImage::decode_png(&page.screenshot().await.unwrap()).unwrap();
        assert_eq!((image.width, image.height), overlay.image_size(video));
        let row = |y: u32| &image.pixels[(y * image.width * 4) as usize..((y + 1) * image.width * 4) as usize];
        assert!(row(0).chunks(4).all(|pixel| pixel[3] == 255));
        assert!(row(image.height - 1).chunks(4).all(|pixel| pixel[3] == 0));
    }
}
