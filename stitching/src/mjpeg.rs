//! The cameras' recorded video: each camera's frames as JPEGs one after
//! another (Motion JPEG), read and decoded here.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result, ensure};
use turbojpeg::{Decompressor, Subsamp, YuvImage};

use crate::gpu::YuvPlanes;

const MARKER: u8 = 0xFF;
const START_OF_IMAGE: u8 = 0xD8;
const END_OF_IMAGE: u8 = 0xD9;
const START_OF_SCAN: u8 = 0xDA;
/// Markers without a segment after them: restarts and TEM.
fn is_standalone(marker: u8) -> bool {
    matches!(marker, 0xD0..=0xD7 | 0x01)
}

/// Reads the JPEGs of a Motion JPEG stream one by one, finding where each
/// ends by walking its markers.
pub struct JpegReader<R> {
    reader: BufReader<R>,
}

impl<R: Read> JpegReader<R> {
    pub fn new(reader: R) -> Self {
        JpegReader {
            reader: BufReader::with_capacity(1 << 20, reader),
        }
    }

    /// The next JPEG; `None` at the end of the stream.
    pub fn next_jpeg(&mut self) -> Result<Option<Vec<u8>>> {
        if self.reader.fill_buf()?.is_empty() {
            return Ok(None);
        }
        ensure!(self.read_marker()? == START_OF_IMAGE, "not a JPEG");
        let mut jpeg = vec![MARKER, START_OF_IMAGE];
        let mut pending = None;
        loop {
            let marker = match pending.take() {
                Some(marker) => marker,
                None => self.read_marker()?,
            };
            jpeg.extend([MARKER, marker]);
            if marker == END_OF_IMAGE {
                return Ok(Some(jpeg));
            }
            if is_standalone(marker) {
                continue;
            }
            let mut length = [0; 2];
            self.reader
                .read_exact(&mut length)
                .context("a JPEG ends early")?;
            jpeg.extend(length);
            let length = u16::from_be_bytes(length) as usize;
            ensure!(length >= 2, "a JPEG segment is too short");
            let start = jpeg.len();
            jpeg.resize(start + length - 2, 0);
            self.reader
                .read_exact(&mut jpeg[start..])
                .context("a JPEG ends early")?;
            if marker == START_OF_SCAN {
                pending = Some(self.copy_entropy_coded(&mut jpeg)?);
            }
        }
    }

    fn read_byte(&mut self) -> Result<u8> {
        let mut byte = [0];
        self.reader
            .read_exact(&mut byte)
            .context("a JPEG ends early")?;
        Ok(byte[0])
    }

    /// Reads a marker, skipping any fill bytes before it.
    fn read_marker(&mut self) -> Result<u8> {
        ensure!(self.read_byte()? == MARKER, "expected a JPEG marker");
        let mut marker = self.read_byte()?;
        while marker == MARKER {
            marker = self.read_byte()?;
        }
        Ok(marker)
    }

    /// Copies a scan's entropy-coded data into `jpeg` and returns the marker
    /// after it.
    fn copy_entropy_coded(&mut self, jpeg: &mut Vec<u8>) -> Result<u8> {
        loop {
            let buffer = self.reader.fill_buf()?;
            ensure!(!buffer.is_empty(), "a JPEG ends early");
            let Some(position) = buffer.iter().position(|&byte| byte == MARKER) else {
                jpeg.extend_from_slice(buffer);
                let length = buffer.len();
                self.reader.consume(length);
                continue;
            };
            jpeg.extend_from_slice(&buffer[..position]);
            self.reader.consume(position + 1);
            let mut code = self.read_byte()?;
            while code == MARKER {
                code = self.read_byte()?;
            }
            // A zero after the marker byte stuffs a literal 0xFF.
            if code == 0 || is_standalone(code) {
                jpeg.extend([MARKER, code]);
            } else {
                return Ok(code);
            }
        }
    }
}

/// A frame in planar YUV 4:2:0.
pub struct YuvFrame {
    size: (u32, u32),
    data: Vec<u8>,
}

impl YuvFrame {
    pub fn new(size: (u32, u32)) -> YuvFrame {
        YuvFrame {
            size,
            data: vec![0; (size.0 * size.1 * 3 / 2) as usize],
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Decodes `jpeg`, which must be 4:2:0 and this frame's size.
    pub fn decode(&mut self, decompressor: &mut Decompressor, jpeg: &[u8]) -> Result<()> {
        let header = decompressor.read_header(jpeg)?;
        let (width, height) = self.size;
        ensure!(
            (header.width, header.height) == (width as usize, height as usize),
            "a frame is {}x{} instead of {width}x{height}",
            header.width,
            header.height
        );
        decompressor.decompress_to_yuv(
            jpeg,
            YuvImage {
                pixels: &mut self.data[..],
                width: width as usize,
                align: 1,
                height: height as usize,
                subsamp: Subsamp::Sub2x2,
            },
        )?;
        Ok(())
    }

    pub fn planes(&self) -> YuvPlanes<'_> {
        let luma = (self.size.0 * self.size.1) as usize;
        let (y, chroma) = self.data.split_at(luma);
        let (u, v) = chroma.split_at(luma / 4);
        YuvPlanes { y, u, v }
    }
}

/// One stretch of video: each camera's Motion JPEG file and how many frames
/// to take from them.
pub struct PairedFiles {
    pub files: [PathBuf; 2],
    pub frames: u32,
}

type DecodedPair = Result<[YuvFrame; 2]>;

/// Decodes the frames of pairs of Motion JPEG files in parallel and returns
/// them in order. A stretch whose files run short repeats its last frames
/// to keep the video in time.
pub struct PairDecoder {
    results: mpsc::Receiver<(u64, DecodedPair)>,
    reordered: BTreeMap<u64, DecodedPair>,
    next: u64,
    frames: u64,
    pool: Arc<Mutex<Vec<YuvFrame>>>,
}

impl PairDecoder {
    pub fn start(stretches: Vec<PairedFiles>, size: (u32, u32)) -> PairDecoder {
        let workers = std::thread::available_parallelism().map_or(4, |count| count.get());
        let frames = stretches.iter().map(|stretch| stretch.frames as u64).sum();
        let (jobs, jobs_rx) = mpsc::sync_channel(workers * 2);
        let (results, results_rx) = mpsc::sync_channel(workers * 2);
        std::thread::Builder::new()
            .name("mjpeg-reader".into())
            .spawn(move || read_pairs(stretches, &jobs))
            .expect("spawn the JPEG reader");
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        let pool = Arc::new(Mutex::new(Vec::new()));
        for index in 0..workers {
            let (jobs, results, pool) = (jobs_rx.clone(), results.clone(), pool.clone());
            std::thread::Builder::new()
                .name(format!("mjpeg-decoder-{index}"))
                .spawn(move || decode_pairs(&jobs, &results, &pool, size))
                .expect("spawn a JPEG decoder");
        }
        PairDecoder {
            results: results_rx,
            reordered: BTreeMap::new(),
            next: 0,
            frames,
            pool,
        }
    }

    /// Frames in all the stretches.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The next pair of frames (indexed by [`protocol::Side::index`]);
    /// `None` after the last.
    pub fn next_pair(&mut self) -> Option<DecodedPair> {
        if self.next == self.frames {
            return None;
        }
        let pair = loop {
            if let Some(pair) = self.reordered.remove(&self.next) {
                break pair;
            }
            match self.results.recv() {
                Ok((index, pair)) => {
                    self.reordered.insert(index, pair);
                }
                Err(_) => break Err(anyhow::anyhow!("the JPEG decoders stopped")),
            }
        };
        self.next += 1;
        Some(pair)
    }

    /// Hands a pair's frames back to be decoded into again.
    pub fn recycle(&self, pair: [YuvFrame; 2]) {
        if let Ok(mut pool) = self.pool.lock() {
            pool.extend(pair);
        }
    }
}

type Job = (u64, Result<Arc<[Vec<u8>; 2]>>);

fn read_pairs(stretches: Vec<PairedFiles>, jobs: &mpsc::SyncSender<Job>) {
    let mut index = 0;
    let open = |path: &PathBuf| {
        File::open(path)
            .with_context(|| format!("opening {}", path.display()))
            .map(JpegReader::new)
    };
    for stretch in stretches {
        let [left, right] = &stretch.files;
        let mut readers = match open(left).and_then(|left| Ok([left, open(right)?])) {
            Ok(readers) => readers,
            Err(error) => {
                let _ = jobs.send((index, Err(error)));
                return;
            }
        };
        let mut last: Option<Arc<[Vec<u8>; 2]>> = None;
        let mut short = false;
        for _ in 0..stretch.frames {
            let pair = if short {
                None
            } else {
                let [left, right] = &mut readers;
                match left.next_jpeg().and_then(|left| Ok(left.zip(right.next_jpeg()?))) {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(
                            file = %stretch.files[0].display(),
                            "reading a frame: {error:#}"
                        );
                        None
                    }
                }
            };
            let pair = match (pair, &last) {
                (Some((left, right)), _) => Arc::new([left, right]),
                (None, Some(last)) => {
                    if !short {
                        tracing::warn!(
                            file = %stretch.files[0].display(),
                            "the video runs short; repeating its last frame"
                        );
                        short = true;
                    }
                    last.clone()
                }
                (None, None) => {
                    let _ = jobs.send((
                        index,
                        Err(anyhow::anyhow!(
                            "{} has no frames",
                            stretch.files[0].display()
                        )),
                    ));
                    return;
                }
            };
            last = Some(pair.clone());
            if jobs.send((index, Ok(pair))).is_err() {
                return;
            }
            index += 1;
        }
    }
}

fn decode_pairs(
    jobs: &Mutex<mpsc::Receiver<Job>>,
    results: &mpsc::SyncSender<(u64, DecodedPair)>,
    pool: &Mutex<Vec<YuvFrame>>,
    size: (u32, u32),
) {
    let mut decompressor = match Decompressor::new() {
        Ok(decompressor) => decompressor,
        Err(error) => {
            tracing::error!("creating a JPEG decoder: {error}");
            return;
        }
    };
    loop {
        let job = match jobs.lock() {
            Ok(jobs) => jobs.recv(),
            Err(_) => return,
        };
        let Ok((index, jpegs)) = job else {
            return;
        };
        let decoded = jpegs.and_then(|jpegs| {
            let mut frames = [(); 2].map(|()| {
                pool.lock()
                    .ok()
                    .and_then(|mut pool| pool.pop())
                    .filter(|frame| frame.size() == size)
                    .unwrap_or_else(|| YuvFrame::new(size))
            });
            for (frame, jpeg) in frames.iter_mut().zip(jpegs.iter()) {
                frame.decode(&mut decompressor, jpeg)?;
            }
            Ok(frames)
        });
        if results.send((index, decoded)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use turbojpeg::{Compressor, YuvImage};

    use super::*;

    fn jpeg(size: (u32, u32), luma: u8) -> Vec<u8> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let mut pixels = vec![luma; width * height];
        pixels.resize(width * height * 3 / 2, 128);
        let mut compressor = Compressor::new().unwrap();
        compressor.set_quality(90).unwrap();
        compressor
            .compress_yuv_to_vec(YuvImage {
                pixels: &pixels[..],
                width,
                align: 1,
                height,
                subsamp: Subsamp::Sub2x2,
            })
            .unwrap()
    }

    #[test]
    fn reader_splits_concatenated_jpegs() {
        let jpegs = [jpeg((64, 32), 10), jpeg((64, 32), 200), jpeg((32, 16), 90)];
        let stream: Vec<u8> = jpegs.concat();
        let mut reader = JpegReader::new(&stream[..]);
        for expected in &jpegs {
            assert_eq!(reader.next_jpeg().unwrap().as_ref(), Some(expected));
        }
        assert!(reader.next_jpeg().unwrap().is_none());
    }

    #[test]
    fn reader_rejects_a_truncated_jpeg() {
        let whole = jpeg((64, 32), 10);
        let mut reader = JpegReader::new(&whole[..whole.len() - 10]);
        assert!(reader.next_jpeg().is_err());
    }

    #[test]
    fn decoder_returns_frames_in_order_and_pads_short_files() {
        let directory = std::env::temp_dir().join(format!("mjpeg-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let size = (64, 32);
        let file = |name: &str, lumas: &[u8]| {
            let path = directory.join(name);
            let stream: Vec<u8> = lumas.iter().flat_map(|&luma| jpeg(size, luma)).collect();
            std::fs::write(&path, stream).unwrap();
            path
        };
        let lumas: Vec<u8> = (0..40).map(|index| index * 5).collect();
        let stretches = vec![
            PairedFiles {
                files: [file("a-left", &lumas), file("a-right", &lumas)],
                frames: 40,
            },
            PairedFiles {
                files: [file("b-left", &[30, 60]), file("b-right", &[30, 60])],
                frames: 4,
            },
        ];
        let mut decoder = PairDecoder::start(stretches, size);
        assert_eq!(decoder.frames(), 44);
        let mut seen = Vec::new();
        while let Some(pair) = decoder.next_pair() {
            let pair = pair.unwrap();
            seen.push(pair[0].planes().y[0]);
            assert_eq!(pair[0].planes().y[0], pair[1].planes().y[0]);
            decoder.recycle(pair);
        }
        let expected: Vec<u8> = lumas.iter().copied().chain([30, 60, 60, 60]).collect();
        assert_eq!(seen.len(), expected.len());
        for (seen, expected) in seen.iter().zip(&expected) {
            assert!(seen.abs_diff(*expected) <= 2, "{seen} instead of {expected}");
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
