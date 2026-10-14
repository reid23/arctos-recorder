//! Camera frame buffers from the Pi's cached video heap.
//!
//! libcamera's own buffers are mapped uncached, which makes the JPEG encoders
//! read them several times slower than normal memory. Cached buffers need
//! explicit cache maintenance around CPU access instead, see [`read`].

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use libcamera::framebuffer::{AsFrameBuffer, FrameBufferPlane, OwnedFrameBuffer};
use libcamera::framebuffer_map::MemoryMappedFrameBuffer;

pub type Buffer = MemoryMappedFrameBuffer<OwnedFrameBuffer>;

const HEAP: &str = "/dev/dma_heap/vidbuf_cached";

/// `DMA_HEAP_IOCTL_ALLOC` from `linux/dma-heap.h`.
const HEAP_ALLOC: libc::c_ulong = 0xC018_4800;
/// `DMA_BUF_IOCTL_SYNC` and its flags from `linux/dma-buf.h`.
const BUF_SYNC: libc::c_ulong = 0x4008_6200;
const SYNC_READ: u64 = 1 << 0;
const SYNC_END: u64 = 1 << 2;

#[repr(C)]
struct HeapAllocation {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

pub struct Heap(File);

impl Heap {
    pub fn open() -> Result<Self> {
        Ok(Heap(
            File::options()
                .read(true)
                .write(true)
                .open(HEAP)
                .context(HEAP)?,
        ))
    }

    /// A buffer for a YUV420 frame with tightly packed planes.
    pub fn yuv420(&self, (width, height): (u32, u32)) -> Result<Buffer> {
        let luma = width * height;
        let mut allocation = HeapAllocation {
            len: u64::from(luma * 3 / 2),
            fd: 0,
            fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            heap_flags: 0,
        };
        if unsafe { libc::ioctl(self.0.as_raw_fd(), HEAP_ALLOC, &mut allocation) } < 0 {
            return Err(io::Error::last_os_error()).context("allocating a frame buffer");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(allocation.fd as RawFd) };
        let planes = [(0, luma), (luma, luma / 4), (luma * 5 / 4, luma / 4)]
            .into_iter()
            .map(|(offset, length)| {
                Ok(FrameBufferPlane {
                    fd: fd.try_clone()?,
                    offset,
                    length,
                })
            })
            .collect::<io::Result<_>>()?;
        Ok(Buffer::new(OwnedFrameBuffer::new(planes, None)?)?)
    }
}

/// Gives `f` the Y, U and V planes of a frame the camera has filled.
pub fn read<T>(buffer: &Buffer, f: impl FnOnce([&[u8]; 3]) -> T) -> Result<T> {
    let fd = buffer.planes().get(0).expect("three planes").fd();
    sync(fd, SYNC_READ)?;
    let planes = buffer.data();
    let result = f([planes[0], planes[1], planes[2]]);
    sync(fd, SYNC_READ | SYNC_END)?;
    Ok(result)
}

fn sync(fd: RawFd, flags: u64) -> Result<()> {
    if unsafe { libc::ioctl(fd, BUF_SYNC, &flags) } < 0 {
        return Err(io::Error::last_os_error()).context("syncing a frame buffer");
    }
    Ok(())
}
