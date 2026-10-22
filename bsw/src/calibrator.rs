//! The work behind the calibration window: fitting a camera's lenses to
//! pictures of a chessboard held in front of them, and aligning its cameras
//! from what they both see. The pictures and the last alignment are kept in
//! the camera's calibration directory for the next time the window opens.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use calibration::Check;
use calibration::alignment::{self, Alignment, AlignmentPictures};
use calibration::chessboard::{self, Board, Corners, LensFit, Straightener};
use calibration::picture;
use protocol::{Side, rig};
use serde::{Deserialize, Serialize};
use stitching::geometry::{self, Calibration, Lens, Orientation};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::hub::{CameraLink, Hub};

/// What the board's corners are measured in, whatever size the previews
/// they were found in.
pub const SIGHTING_SIZE: (u32, u32) = rig::DEFAULT_PREVIEW_SIZE;
/// A board anyone can print: 9 by 7 squares.
pub const DEFAULT_BOARD: Board = Board {
    columns: 8,
    rows: 6,
};
/// Width of the copies of the pictures taken that are shown.
const THUMBNAIL_WIDTH: u32 = 256;
/// How fast the board's corners may move (on average, in pixels a second)
/// for it to count as held still, and for how long. Slow enough that it
/// hardly moves while a frame is exposed.
const STILL_SPEED: f64 = 40.0;
const STILL_TIME: Duration = Duration::from_millis(300);
/// How far the board must be from where every picture was taken (on
/// average, as a share of how big it looks) for a new one to be taken.
const NEW_POSITION: f64 = 0.1;
/// Size and horizontal field of view in degrees of the straightened
/// picture.
const STRAIGHTENED_SIZE: (u32, u32) = (768, 432);
const STRAIGHTENED_FOV: f64 = 90.0;
/// Pairs of previews the cameras are aligned from, and the time between
/// them.
pub const ALIGNMENT_PAIRS: usize = 8;
const ALIGNMENT_INTERVAL: Duration = Duration::from_millis(400);
/// In each side's directory, the board and the pictures taken of it, which
/// are kept beside it.
const PICTURES_FILE: &str = "pictures.json";
/// The last alignment, beside its pictures.
const ALIGNMENT_DIRECTORY: &str = "alignment";
const ALIGNMENT_FILE: &str = "alignment.json";

/// A JPEG as a URL an `<img>` can show.
pub fn data_url(jpeg: &[u8]) -> String {
    format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg))
}

/// Changes the calibration `camera` is stitched with, starting from the
/// one it is stitched with now.
pub fn change_calibration(hub: &Hub, camera: &str, change: impl FnOnce(&mut Calibration)) {
    let Some(link) = hub.link(camera) else {
        return;
    };
    let mut calibration = link.stitch.borrow().calibration.clone();
    change(&mut calibration);
    hub.update_settings(|settings| {
        settings.cameras.entry(camera.to_owned()).or_default().setup.calibration = Some(calibration);
    });
}

/// What fitting one camera's lens shows.
#[derive(Clone, Debug)]
pub struct LensSession {
    pub board: Board,
    /// Whether a picture is taken whenever the board is held still
    /// somewhere new.
    pub automatic: bool,
    /// The board in the latest preview.
    pub sighting: Sighting,
    pub pictures: Vec<Picture>,
    pub fit: Option<LensFit>,
    pub checks: Vec<Check>,
    /// The newest picture straightened with the fit.
    pub straightened: Option<StraightenedPicture>,
    /// Why the lens couldn't be fitted.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Sighting {
    #[default]
    NoBoard,
    Moving(Corners),
    /// Held still somewhere new, waiting for a picture to be taken.
    Ready(Corners),
    /// Held still where a picture was already taken.
    AlreadyTaken(Corners),
    JustTaken(Corners),
}

impl Sighting {
    pub fn corners(&self) -> Option<&Corners> {
        match self {
            Sighting::NoBoard => None,
            Sighting::Moving(corners)
            | Sighting::Ready(corners)
            | Sighting::AlreadyTaken(corners)
            | Sighting::JustTaken(corners) => Some(corners),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Picture {
    /// In [`SIGHTING_SIZE`] pixels.
    pub corners: Corners,
    /// A small copy, as a data URL.
    pub thumbnail: String,
    jpeg: Bytes,
    /// Name of the JPEG in the side's directory.
    file: String,
    /// The lens position it was taken at.
    focus: f64,
}

/// A side's pictures as saved in [`PICTURES_FILE`].
#[derive(Serialize, Deserialize)]
struct SavedPictures {
    board: Board,
    pictures: Vec<SavedPicture>,
}

#[derive(Serialize, Deserialize)]
struct SavedPicture {
    file: String,
    /// In [`SIGHTING_SIZE`] pixels.
    corners: Corners,
    #[serde(default = "geometry::default_focus")]
    focus: f64,
}

#[derive(Clone, Debug)]
pub struct StraightenedPicture {
    /// As a data URL.
    pub picture: String,
    /// How far the board's corners are from straight lines through its
    /// rows and columns, at most, in pixels: before and after straightening.
    pub bend: Option<(f64, f64)>,
}

pub enum LensCommand {
    /// Starts or stops looking for the board in the previews.
    Watch(bool),
    TakePicture,
    Remove(usize),
    Clear,
    SetBoard(Board),
    SetAutomatic(bool),
}

/// Fitting one camera's lens, for as long as this lives.
pub struct LensCalibration {
    pub state: watch::Sender<LensSession>,
    commands: mpsc::UnboundedSender<LensCommand>,
    task: JoinHandle<()>,
}

impl LensCalibration {
    /// Starts from the pictures last kept in `directory`, the camera's
    /// calibration directory.
    pub fn start(runtime: &Handle, link: Arc<CameraLink>, side: Side, directory: &Path) -> LensCalibration {
        let state = watch::Sender::new(LensSession {
            board: DEFAULT_BOARD,
            automatic: true,
            sighting: Sighting::NoBoard,
            pictures: Vec::new(),
            fit: None,
            checks: Vec::new(),
            straightened: None,
            error: None,
        });
        let (commands, received) = mpsc::unbounded_channel();
        let directory = directory.join(match side {
            Side::Left => "left",
            Side::Right => "right",
        });
        let task = runtime.spawn(calibrate_lens(link, side, directory, state.clone(), received));
        LensCalibration {
            state,
            commands,
            task,
        }
    }

    pub fn command(&self, command: LensCommand) {
        // The task only stops when this is dropped.
        let _ = self.commands.send(command);
    }
}

impl Drop for LensCalibration {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn calibrate_lens(
    link: Arc<CameraLink>,
    side: Side,
    directory: PathBuf,
    state: watch::Sender<LensSession>,
    mut commands: mpsc::UnboundedReceiver<LensCommand>,
) {
    let (width, height) = SIGHTING_SIZE;
    let guess = link.stitch.borrow().calibration.camera(side).lens.scaled_to(width, height);
    let current_focus = || link.stitch.borrow().calibration.camera(side).lens.focus;
    match load_pictures(&directory).await {
        Ok(Some((board, pictures))) => {
            state.send_modify(|session| {
                session.board = board;
                session.pictures = pictures;
            });
            refit(&state, &guess).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("loading the last lens pictures: {e:#}"),
    }
    let mut previews = link.previews[side.index()].subscribe();
    let mut watching = false;
    // The latest preview the board was seen in, and when.
    let mut latest: Option<(Bytes, Corners, Instant)> = None;
    // Since when the board has been moving slower than STILL_SPEED.
    let mut steady_since: Option<Instant> = None;
    loop {
        let preview = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    return;
                };
                match command {
                    LensCommand::Watch(on) => {
                        watching = on;
                        latest = None;
                        steady_since = None;
                        state.send_modify(|session| session.sighting = Sighting::NoBoard);
                    }
                    LensCommand::TakePicture => {
                        if let Some((jpeg, corners, _)) = latest.clone() {
                            take_picture(&state, &directory, jpeg, corners.clone(), current_focus()).await;
                            state.send_modify(|session| session.sighting = Sighting::JustTaken(corners));
                            refit(&state, &guess).await;
                        }
                    }
                    LensCommand::Remove(index) => {
                        let mut removed = None;
                        state.send_modify(|session| {
                            if index < session.pictures.len() {
                                removed = Some(session.pictures.remove(index));
                            }
                        });
                        if let Some(removed) = removed {
                            if let Err(e) = tokio::fs::remove_file(directory.join(&removed.file)).await {
                                tracing::warn!("deleting a lens picture: {e}");
                            }
                            save_pictures(&directory, &state).await;
                        }
                        refit(&state, &guess).await;
                    }
                    LensCommand::Clear => {
                        state.send_modify(|session| session.pictures.clear());
                        delete_pictures(&directory, &state).await;
                        refit(&state, &guess).await;
                    }
                    LensCommand::SetBoard(board) => {
                        if state.borrow().board != board {
                            latest = None;
                            steady_since = None;
                            state.send_modify(|session| {
                                session.board = board;
                                session.pictures.clear();
                                session.sighting = Sighting::NoBoard;
                            });
                            delete_pictures(&directory, &state).await;
                            refit(&state, &guess).await;
                        }
                    }
                    LensCommand::SetAutomatic(on) => {
                        state.send_modify(|session| session.automatic = on);
                    }
                }
                continue;
            }
            changed = previews.changed(), if watching => {
                if changed.is_err() {
                    return;
                }
                previews.borrow_and_update().clone()
            }
        };
        let Some(jpeg) = preview else {
            continue;
        };
        let board = state.borrow().board;
        let found = {
            let jpeg = jpeg.clone();
            tokio::task::spawn_blocking(move || -> Result<Option<Corners>> {
                let corners = chessboard::find_corners(&picture::decode_gray(&jpeg)?, board)?;
                let size = jpeg_size(&jpeg)?;
                Ok(corners.map(|corners| rescaled(&corners, size, SIGHTING_SIZE)))
            })
            .await
        };
        let corners = match found {
            Ok(Ok(corners)) => corners,
            Ok(Err(e)) => {
                tracing::warn!("looking for the chessboard: {e:#}");
                None
            }
            Err(e) => {
                tracing::error!("looking for the chessboard: {e}");
                return;
            }
        };
        let Some(corners) = corners else {
            latest = None;
            steady_since = None;
            state.send_modify(|session| session.sighting = Sighting::NoBoard);
            continue;
        };
        let now = Instant::now();
        let steady = latest.as_ref().is_some_and(|(_, previous, seen)| {
            chessboard::separation(previous, &corners) < STILL_SPEED * (now - *seen).as_secs_f64()
        });
        steady_since = if steady { steady_since.or(Some(now)) } else { None };
        let still = steady_since.is_some_and(|since| now - since >= STILL_TIME);
        latest = Some((jpeg.clone(), corners.clone(), now));
        let (new, automatic) = {
            let session = state.borrow();
            let far_enough = NEW_POSITION * chessboard::apparent_size(&corners);
            let new = session
                .pictures
                .iter()
                .all(|picture| chessboard::separation(&picture.corners, &corners) > far_enough);
            (new, session.automatic)
        };
        let sighting = match (still, new) {
            (false, _) => Sighting::Moving(corners),
            (true, false) => Sighting::AlreadyTaken(corners),
            (true, true) if !automatic => Sighting::Ready(corners),
            (true, true) => {
                take_picture(&state, &directory, jpeg, corners.clone(), current_focus()).await;
                state.send_modify(|session| session.sighting = Sighting::JustTaken(corners));
                refit(&state, &guess).await;
                continue;
            }
        };
        state.send_modify(|session| session.sighting = sighting);
    }
}

async fn take_picture(
    state: &watch::Sender<LensSession>,
    directory: &Path,
    jpeg: Bytes,
    corners: Corners,
    focus: f64,
) {
    let thumbnail = match thumbnail(&jpeg).await {
        Ok(thumbnail) => thumbnail,
        Err(e) => {
            tracing::error!("shrinking a calibration picture: {e}");
            return;
        }
    };
    let file = format!("{}.jpg", chrono::Local::now().format("%Y%m%d-%H%M%S%.6f"));
    let written = async {
        tokio::fs::create_dir_all(directory).await?;
        tokio::fs::write(directory.join(&file), &jpeg).await
    };
    if let Err(e) = written.await {
        tracing::warn!("saving a lens picture: {e}");
    }
    state.send_modify(|session| {
        session.pictures.push(Picture {
            corners,
            thumbnail,
            jpeg,
            file,
            focus,
        })
    });
    save_pictures(directory, state).await;
}

/// A small copy of `jpeg` to show, as a data URL.
async fn thumbnail(jpeg: &Bytes) -> Result<String> {
    let shrunk = {
        let jpeg = jpeg.clone();
        tokio::task::spawn_blocking(move || picture::thumbnail(&jpeg, THUMBNAIL_WIDTH)).await?
    };
    Ok(match shrunk {
        Ok(thumbnail) => data_url(&thumbnail),
        Err(e) => {
            tracing::warn!("shrinking a calibration picture: {e:#}");
            data_url(jpeg)
        }
    })
}

/// Writes the session's board and which pictures it has to `directory`.
async fn save_pictures(directory: &Path, state: &watch::Sender<LensSession>) {
    let saved = {
        let session = state.borrow();
        SavedPictures {
            board: session.board,
            pictures: session
                .pictures
                .iter()
                .map(|picture| SavedPicture {
                    file: picture.file.clone(),
                    corners: picture.corners.clone(),
                    focus: picture.focus,
                })
                .collect(),
        }
    };
    let written = async {
        tokio::fs::create_dir_all(directory).await?;
        tokio::fs::write(directory.join(PICTURES_FILE), serde_json::to_vec_pretty(&saved)?).await?;
        anyhow::Ok(())
    };
    if let Err(e) = written.await {
        tracing::warn!("saving the lens pictures: {e:#}");
    }
}

/// Deletes every picture kept in `directory`, keeping the session's board.
async fn delete_pictures(directory: &Path, state: &watch::Sender<LensSession>) {
    match tokio::fs::remove_dir_all(directory).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("deleting the lens pictures: {e}"),
    }
    save_pictures(directory, state).await;
}

/// The board and pictures kept in `directory`, if any.
async fn load_pictures(directory: &Path) -> Result<Option<(Board, Vec<Picture>)>> {
    let json = match tokio::fs::read(directory.join(PICTURES_FILE)).await {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let saved: SavedPictures = serde_json::from_slice(&json)?;
    let mut pictures = Vec::with_capacity(saved.pictures.len());
    for SavedPicture { file, corners, focus } in saved.pictures {
        let jpeg = Bytes::from(
            tokio::fs::read(directory.join(&file))
                .await
                .with_context(|| format!("reading {file}"))?,
        );
        pictures.push(Picture {
            thumbnail: thumbnail(&jpeg).await?,
            corners,
            jpeg,
            file,
            focus,
        });
    }
    Ok(Some((saved.board, pictures)))
}

/// Fits the lens, focused as for the newest picture, to the pictures taken
/// so far.
async fn refit(state: &watch::Sender<LensSession>, guess: &Lens) {
    let (board, guess, sightings, newest) = {
        let session = state.borrow();
        let guess = guess.refocused(session.pictures.last().map_or(guess.focus, |p| p.focus));
        let sightings: Vec<Corners> = session
            .pictures
            .iter()
            .map(|p| refocused(&p.corners, p.focus, &guess))
            .collect();
        let newest = session.pictures.last().map(|p| (p.jpeg.clone(), p.corners.clone()));
        (session.board, guess, sightings, newest)
    };
    if sightings.len() < chessboard::FEWEST_PICTURES {
        state.send_modify(|session| {
            session.fit = None;
            session.checks.clear();
            session.straightened = None;
            session.error = None;
        });
        return;
    }
    let fitted = tokio::task::spawn_blocking(move || -> Result<(LensFit, Option<StraightenedPicture>)> {
        let fit = chessboard::fit_lens(board, &sightings, &guess)?;
        let straightened = match newest {
            Some((jpeg, corners)) => {
                let size = jpeg_size(&jpeg)?;
                let lens = fit.lens.scaled_to(size.0, size.1);
                let straightener = Straightener::new(&lens, STRAIGHTENED_SIZE, STRAIGHTENED_FOV)?;
                let corners = rescaled(&corners, SIGHTING_SIZE, size);
                let straightened =
                    straightener.straighten(&picture::decode_rgb(&jpeg)?, board, Some(&corners))?;
                Some(StraightenedPicture {
                    picture: data_url(&straightened.picture),
                    bend: straightened.bend,
                })
            }
            None => None,
        };
        Ok((fit, straightened))
    })
    .await
    .context("fitting the lens")
    .and_then(|fitted| fitted);
    state.send_modify(|session| match fitted {
        Ok((fit, straightened)) => {
            session.checks = fit.checks();
            session.fit = Some(fit);
            session.straightened = straightened;
            session.error = None;
        }
        Err(e) => session.error = Some(format!("{e:#}")),
    });
}

fn jpeg_size(jpeg: &[u8]) -> Result<(u32, u32)> {
    let header = turbojpeg::read_header(jpeg)?;
    Ok((header.width as u32, header.height as u32))
}

/// Where `corners` seen with the lens focused at `from` are seen through
/// `lens`, which is measured in the same size pictures.
fn refocused(corners: &Corners, from: f64, lens: &Lens) -> Corners {
    let scale = geometry::magnification(from, lens.focus);
    corners
        .iter()
        .map(|corner| {
            let mut moved = *corner;
            moved.x = lens.cx + (corner.x - lens.cx) * scale;
            moved.y = lens.cy + (corner.y - lens.cy) * scale;
            moved
        })
        .collect()
}

/// Where `corners` seen in a `from` picture are in a `to` picture of the
/// same view.
fn rescaled(corners: &Corners, from: (u32, u32), to: (u32, u32)) -> Corners {
    let scale = |pixel: f64, from: u32, to: u32| (pixel + 0.5) * to as f64 / from as f64 - 0.5;
    corners
        .iter()
        .map(|corner| {
            let mut moved = *corner;
            moved.x = scale(corner.x, from.0, to.0);
            moved.y = scale(corner.y, from.1, to.1);
            moved
        })
        .collect()
}

/// What aligning the cameras shows.
#[derive(Clone, Debug, Default)]
pub enum AlignmentState {
    #[default]
    Idle,
    /// Collecting pairs of previews; this many so far.
    Collecting(usize),
    Matching,
    Done(Arc<ShownAlignment>),
    Failed(String),
}

#[derive(Debug)]
pub struct ShownAlignment {
    pub alignment: Alignment,
    pub checks: Vec<Check>,
    /// The orientations aligned from.
    pub before: [Orientation; 2],
    /// The pictures of [`calibration::alignment::AlignmentPictures`] as
    /// data URLs.
    pub features: [String; 2],
    pub matches: String,
    pub overlay_before: String,
    pub overlay_after: String,
}

impl ShownAlignment {
    fn new(alignment: Alignment, before: [Orientation; 2]) -> ShownAlignment {
        let pictures = &alignment.pictures;
        ShownAlignment {
            checks: alignment.checks(),
            before,
            features: pictures.features.each_ref().map(|jpeg| data_url(jpeg)),
            matches: data_url(&pictures.matches),
            overlay_before: data_url(&pictures.before),
            overlay_after: data_url(&pictures.after),
            alignment,
        }
    }
}

/// An alignment as saved in [`ALIGNMENT_FILE`], without its pictures.
#[derive(Serialize, Deserialize)]
struct SavedAlignment {
    before: [Orientation; 2],
    alignment: Alignment,
}

/// The files an alignment's pictures are saved in, beside
/// [`ALIGNMENT_FILE`].
fn alignment_picture_files(pictures: &mut AlignmentPictures) -> [(&'static str, &mut Bytes); 5] {
    let [left, right] = &mut pictures.features;
    [
        ("features-left.jpg", left),
        ("features-right.jpg", right),
        ("matches.jpg", &mut pictures.matches),
        ("before.jpg", &mut pictures.before),
        ("after.jpg", &mut pictures.after),
    ]
}

async fn save_alignment(directory: &Path, shown: &ShownAlignment) -> Result<()> {
    tokio::fs::create_dir_all(directory).await?;
    let mut pictures = shown.alignment.pictures.clone();
    for (file, jpeg) in alignment_picture_files(&mut pictures) {
        tokio::fs::write(directory.join(file), &*jpeg).await?;
    }
    let saved = SavedAlignment {
        before: shown.before,
        alignment: shown.alignment.clone(),
    };
    tokio::fs::write(directory.join(ALIGNMENT_FILE), serde_json::to_vec_pretty(&saved)?).await?;
    Ok(())
}

/// The alignment kept in `directory`, if any.
async fn load_alignment(directory: &Path) -> Result<Option<ShownAlignment>> {
    let json = match tokio::fs::read(directory.join(ALIGNMENT_FILE)).await {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let SavedAlignment { before, mut alignment } = serde_json::from_slice(&json)?;
    for (file, jpeg) in alignment_picture_files(&mut alignment.pictures) {
        *jpeg = Bytes::from(
            tokio::fs::read(directory.join(file))
                .await
                .with_context(|| format!("reading {file}"))?,
        );
    }
    Ok(Some(ShownAlignment::new(alignment, before)))
}

/// Aligns cameras for the calibration window, stopping when dropped.
pub struct Aligner {
    pub state: watch::Sender<AlignmentState>,
    /// Where the last alignment is kept.
    directory: PathBuf,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Aligner {
    /// Shows the last alignment kept in `directory`, the camera's
    /// calibration directory, if there is one.
    pub fn new(runtime: &Handle, directory: &Path) -> Aligner {
        let state = watch::Sender::default();
        let directory = directory.join(ALIGNMENT_DIRECTORY);
        let task = runtime.spawn({
            let (state, directory) = (state.clone(), directory.clone());
            async move {
                match load_alignment(&directory).await {
                    Ok(Some(shown)) => {
                        state.send_replace(AlignmentState::Done(Arc::new(shown)));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("loading the last alignment: {e:#}"),
                }
            }
        });
        Aligner {
            state,
            directory,
            task: Mutex::new(Some(task)),
        }
    }

    /// Aligns `link`'s cameras from what they see now, starting from the
    /// calibration they are stitched with and choosing the orientations
    /// closest to `mount`.
    pub fn start(&self, runtime: &Handle, link: Arc<CameraLink>, mount: [Orientation; 2]) {
        let (state, directory) = (self.state.clone(), self.directory.clone());
        let task = runtime.spawn(async move {
            let result = align(&link, mount, &state).await;
            state.send_replace(match result {
                Ok(shown) => {
                    if let Err(e) = save_alignment(&directory, &shown).await {
                        tracing::warn!("saving the alignment: {e:#}");
                    }
                    AlignmentState::Done(Arc::new(shown))
                }
                Err(e) => AlignmentState::Failed(format!("{e:#}")),
            });
        });
        if let Some(previous) = self.task.lock().unwrap().replace(task) {
            previous.abort();
        }
    }
}

impl Drop for Aligner {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

async fn align(
    link: &CameraLink,
    mount: [Orientation; 2],
    state: &watch::Sender<AlignmentState>,
) -> Result<ShownAlignment> {
    let calibration = link.stitch.borrow().calibration.clone();
    let mut previews = Vec::with_capacity(ALIGNMENT_PAIRS);
    for collected in 0..ALIGNMENT_PAIRS {
        state.send_replace(AlignmentState::Collecting(collected));
        if collected > 0 {
            tokio::time::sleep(ALIGNMENT_INTERVAL).await;
        }
        let [left, right] = Side::BOTH.map(|side| link.previews[side.index()].borrow().clone());
        let (Some(left), Some(right)) = (left, right) else {
            anyhow::bail!("the camera isn't sending previews");
        };
        previews.push([left, right]);
    }
    state.send_replace(AlignmentState::Matching);
    let before = calibration.cameras.each_ref().map(|camera| camera.orientation);
    let alignment =
        tokio::task::spawn_blocking(move || alignment::align(&previews, &calibration, &mount))
            .await??;
    Ok(ShownAlignment::new(alignment, before))
}

#[cfg(test)]
mod tests {
    use calibration::alignment::PairMatches;
    use nalgebra::{Vector2, Vector3};

    use super::*;

    fn empty_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("bsw-{name}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    fn gray_jpeg(shade: u8) -> Bytes {
        let (width, height) = (64, 36);
        let pixels = vec![shade; width * height];
        let image = turbojpeg::Image {
            pixels: pixels.as_slice(),
            width,
            pitch: width,
            height,
            format: turbojpeg::PixelFormat::GRAY,
        };
        Bytes::copy_from_slice(&turbojpeg::compress(image, 90, turbojpeg::Subsamp::Gray).unwrap())
    }

    fn kept(pictures: &[Picture]) -> Vec<(String, Corners, Bytes, f64)> {
        pictures
            .iter()
            .map(|picture| (picture.file.clone(), picture.corners.clone(), picture.jpeg.clone(), picture.focus))
            .collect()
    }

    #[test]
    fn corners_seen_focused_elsewhere_are_moved_to_where_the_lens_sees_them() {
        let focused_then = Lens::nominal().scaled_to(SIGHTING_SIZE.0, SIGHTING_SIZE.1);
        let focused_now = focused_then.refocused(9.8);
        let directions = [Vector3::new(0.3, -0.2, 1.0), Vector3::new(-1.0, 0.4, 0.5)];
        let seen: Corners = directions.iter().map(|direction| focused_then.project(direction).unwrap()).collect();
        let moved = refocused(&seen, focused_then.focus, &focused_now);
        for (moved, direction) in moved.iter().zip(&directions) {
            let expected = focused_now.project(direction).unwrap();
            assert!((moved - expected).norm() < 1e-9, "{moved} vs {expected}");
        }
    }

    #[test]
    fn corners_seen_at_any_preview_size_fit_the_same_lens() {
        let lens = Lens::nominal().scaled_to(SIGHTING_SIZE.0, SIGHTING_SIZE.1);
        let directions = [Vector3::new(0.3, -0.2, 1.0), Vector3::new(-1.0, 0.4, 0.5)];
        let expected: Corners = directions.iter().map(|direction| lens.project(direction).unwrap()).collect();
        for (width, height) in rig::PREVIEW_SIZES {
            let seen: Corners =
                directions.iter().map(|direction| lens.scaled_to(width, height).project(direction).unwrap()).collect();
            let moved = rescaled(&seen, (width, height), SIGHTING_SIZE);
            for (moved, expected) in moved.iter().zip(&expected) {
                assert!((moved - expected).norm() < 1e-9, "{width} by {height}: {moved} vs {expected}");
            }
            let back = rescaled(&moved, SIGHTING_SIZE, (width, height));
            for (back, seen) in back.iter().zip(&seen) {
                assert!((back - seen).norm() < 1e-9, "{width} by {height}: {back} vs {seen}");
            }
        }
    }

    #[tokio::test]
    async fn lens_pictures_are_kept_until_deleted() {
        let directory = empty_directory("lens-pictures");
        assert!(load_pictures(&directory).await.unwrap().is_none());
        let board = Board { columns: 9, rows: 6 };
        let state = watch::Sender::new(LensSession {
            board,
            automatic: true,
            sighting: Sighting::NoBoard,
            pictures: Vec::new(),
            fit: None,
            checks: Vec::new(),
            straightened: None,
            error: None,
        });
        let corners = |x: f64| vec![Vector2::new(x, 1.0 / 3.0), Vector2::new(x + 2.25, 3.0)];
        take_picture(&state, &directory, gray_jpeg(40), corners(0.1), 1.0).await;
        take_picture(&state, &directory, gray_jpeg(200), corners(10.7), 9.75).await;

        let (loaded_board, loaded) = load_pictures(&directory).await.unwrap().unwrap();
        assert_eq!(loaded_board, board);
        assert_eq!(loaded.len(), 2);
        assert_eq!(kept(&loaded), kept(&state.borrow().pictures));

        state.send_modify(|session| session.pictures.clear());
        delete_pictures(&directory, &state).await;
        let (loaded_board, loaded) = load_pictures(&directory).await.unwrap().unwrap();
        assert_eq!(loaded_board, board);
        assert!(loaded.is_empty());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn alignments_are_kept_with_their_pictures() {
        let directory = empty_directory("alignment");
        assert!(load_alignment(&directory).await.unwrap().is_none());
        let orientation = |yaw: f64, pitch: f64, roll: f64| Orientation { yaw, pitch, roll };
        let before = [orientation(-35.0, -15.0, 0.0), orientation(35.0, -15.0, 0.0)];
        let mut calibration = Calibration::nominal([orientation(-36.217, -9.83, 6.547), orientation(36.386, -10.591, -7.116)]);
        calibration.cameras[0].lens.fx *= 1.0 / 3.0;
        let alignment = Alignment {
            calibration,
            focal_scales: [0.9896, 1.0105],
            matches: 254,
            inliers: 205,
            error: 0.89,
            span: 0.58,
            pairs: vec![PairMatches { matches: 30, inliers: 26 }, PairMatches { matches: 32, inliers: 25 }],
            pictures: AlignmentPictures {
                features: [gray_jpeg(10), gray_jpeg(20)],
                matches: gray_jpeg(30),
                before: gray_jpeg(40),
                after: gray_jpeg(50),
            },
        };
        let shown = ShownAlignment::new(alignment.clone(), before);
        save_alignment(&directory, &shown).await.unwrap();

        let loaded = load_alignment(&directory).await.unwrap().unwrap();
        assert_eq!(loaded.before, before);
        assert_eq!(loaded.alignment.calibration, alignment.calibration);
        assert_eq!(loaded.alignment.focal_scales, alignment.focal_scales);
        assert_eq!(
            (loaded.alignment.matches, loaded.alignment.inliers, loaded.alignment.error, loaded.alignment.span),
            (alignment.matches, alignment.inliers, alignment.error, alignment.span)
        );
        assert_eq!(loaded.alignment.pairs, alignment.pairs);
        assert_eq!(loaded.alignment.pictures, alignment.pictures);
        assert_eq!(loaded.checks, shown.checks);
        assert_eq!(
            [&loaded.matches, &loaded.overlay_before, &loaded.overlay_after],
            [&shown.matches, &shown.overlay_before, &shown.overlay_after]
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
