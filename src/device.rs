//! The device seam.
//!
//! Everything this crate writes goes through [`BlockDevice`]. The trait takes
//! `&self` rather than `&mut self` for reads *and* writes, which is the whole
//! reason a format can fan out: block groups are disjoint byte ranges, so
//! writing them concurrently needs no exclusion, only positional I/O.
//!
//! A consumer with its own storage — stormblock's thin volumes, an in-memory
//! image, a network-backed device — implements this trait and never materialises
//! a file or a `/dev` node.

#[cfg(not(feature = "std"))]
use alloc::{vec::Vec};

use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::error::{Error, Result};

/// A device this crate can lay a filesystem onto.
///
/// Implementations must be safe to call concurrently from many tasks. Writes to
/// disjoint ranges must not interfere; the formatter relies on it.
#[async_trait::async_trait]
pub trait BlockDevice: Send + Sync {
    /// Total addressable size in bytes.
    fn size(&self) -> u64;

    /// The device's logical sector size — the smallest I/O it will accept.
    ///
    /// This is not cosmetic. A filesystem whose block size is smaller than the
    /// device's sector cannot be written to a block at a time, and an
    /// implementation that tries is entitled to refuse. `mke2fs` raises its
    /// default block size to the sector size for exactly this reason, so a
    /// 256 MiB filesystem gets 1 KiB blocks on a 512-byte-sector device and
    /// 4 KiB blocks on a 4 KiB-sector one.
    ///
    /// Defaults to 512. A device that knows better — a network-backed volume
    /// exporting 4 KiB sectors, say — should say so.
    fn logical_sector_size(&self) -> u32 {
        512
    }

    /// Read exactly `buf.len()` bytes starting at `offset`.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Write all of `buf` starting at `offset`.
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()>;

    /// Flush any buffered writes to stable storage.
    async fn flush(&self) -> Result<()>;

    /// Write `len` zero bytes at `offset`.
    ///
    /// The default implementation writes zeroes in chunks. Devices with a
    /// discard or write-zeroes primitive should override it — a format zeroes a
    /// great deal, and on a thin volume the difference is allocation, not just
    /// speed.
    async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        const CHUNK: usize = 1 << 20;
        let zeroes = vec![0u8; CHUNK.min(len as usize).max(1)];
        let mut written = 0u64;
        while written < len {
            let n = ((len - written) as usize).min(zeroes.len());
            self.write_at(offset + written, &zeroes[..n]).await?;
            written += n as u64;
        }
        Ok(())
    }
}

/// Bounds-check a request against a device size.
pub(crate) fn check_bounds(offset: u64, len: u64, size: u64) -> Result<()> {
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(Error::OutOfBounds { offset, len, size }),
    }
}

/// A device backed by a file or a raw block device.
///
/// Uses positional I/O so concurrent writes to disjoint ranges do not contend
/// on a file cursor.
pub struct FileDevice {
    file: std::fs::File,
    size: u64,
    sector_size: u32,
}

impl FileDevice {
    /// Open an existing file or block device for formatting.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().read(true).write(true).open(path)
        })
        .await
        .map_err(|e| Error::io(0, io::Error::other(e)))?
        .map_err(|e| Error::io(0, e))?;

        let size = device_size(&file)?;
        let sector_size = detect_sector_size(&file);
        Ok(Self {
            file,
            size,
            sector_size,
        })
    }

    /// Create (or truncate) a file of exactly `size` bytes to format into.
    pub async fn create(path: impl AsRef<Path>, size: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?;
            file.set_len(size)?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(|e| Error::io(0, io::Error::other(e)))?
        .map_err(|e| Error::io(0, e))?;

        Ok(Self {
            file,
            size,
            sector_size: 512,
        })
    }

    /// Declare the device's logical sector size.
    ///
    /// A plain file has no sectors of its own, so a caller building an image
    /// for a device with 4 KiB sectors has to say so — otherwise the
    /// filesystem is laid out for the file, not for where it is going.
    pub fn with_sector_size(mut self, sector_size: u32) -> Self {
        self.sector_size = sector_size;
        self
    }
}

/// The device's logical sector size, asked of the kernel.
///
/// A filesystem's block size can never be smaller than this, so guessing is not
/// good enough: on a 4 KiB-sector device, assuming 512 produces a 1 KiB-block
/// filesystem that cannot be written a block at a time. `mke2fs` queries the
/// device and so does this.
///
/// A regular file has no sectors of its own and reports 512, which is what
/// `mke2fs` falls back to as well.
fn detect_sector_size(file: &std::fs::File) -> u32 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        if file
            .metadata()
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false)
        {
            if let Ok(size) = rustix::fs::ioctl_blksszget(file) {
                if size > 0 {
                    return size as u32;
                }
            }
        }
    }
    let _ = file;
    512
}

/// Size of a file, or of the block device it refers to.
fn device_size(file: &std::fs::File) -> Result<u64> {
    let meta = file.metadata().map_err(|e| Error::io(0, e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_block_device() {
            // A block device reports zero length in metadata; seek to the end
            // to learn its real size.
            use std::io::{Seek, SeekFrom};
            let mut f = file.try_clone().map_err(|e| Error::io(0, e))?;
            let size = f.seek(SeekFrom::End(0)).map_err(|e| Error::io(0, e))?;
            f.seek(SeekFrom::Start(0)).map_err(|e| Error::io(0, e))?;
            return Ok(size);
        }
    }

    Ok(meta.len())
}

#[async_trait::async_trait]
impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.size)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file
                .read_exact_at(buf, offset)
                .map_err(|e| Error::io(offset, e))
        }
        #[cfg(not(unix))]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                let n = self
                    .file
                    .seek_read(&mut buf[done..], offset + done as u64)
                    .map_err(|e| Error::io(offset, e))?;
                if n == 0 {
                    return Err(Error::io(
                        offset,
                        io::Error::from(io::ErrorKind::UnexpectedEof),
                    ));
                }
                done += n;
            }
            Ok(())
        }
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.size)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file
                .write_all_at(buf, offset)
                .map_err(|e| Error::io(offset, e))
        }
        #[cfg(not(unix))]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                let n = self
                    .file
                    .seek_write(&buf[done..], offset + done as u64)
                    .map_err(|e| Error::io(offset, e))?;
                done += n;
            }
            Ok(())
        }
    }

    async fn flush(&self) -> Result<()> {
        self.file.sync_data().map_err(|e| Error::io(0, e))
    }
}

/// A device held entirely in memory.
///
/// For tests, and for building an image to ship elsewhere without touching a
/// filesystem.
pub struct MemDevice {
    data: Mutex<Vec<u8>>,
    size: u64,
    sector_size: u32,
}

impl MemDevice {
    /// A zeroed device of `size` bytes.
    pub fn new(size: u64) -> Self {
        Self {
            data: Mutex::new(vec![0u8; size as usize]),
            size,
            sector_size: 512,
        }
    }

    /// A device that reports the given logical sector size.
    pub fn with_sector_size(size: u64, sector_size: u32) -> Self {
        Self {
            sector_size,
            ..Self::new(size)
        }
    }

    /// Take a copy of the whole image.
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.lock().expect("mem device poisoned").clone()
    }
}

#[async_trait::async_trait]
impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.size)?;
        let data = self.data.lock().expect("mem device poisoned");
        let start = offset as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.size)?;
        let mut data = self.data.lock().expect("mem device poisoned");
        let start = offset as usize;
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Blanket implementation so `&D` is itself a device.
///
/// Lets a caller check or format a device it only borrowed, and run several
/// operations over one device in sequence without giving up ownership.
#[async_trait::async_trait]
impl<D: BlockDevice + ?Sized> BlockDevice for &D {
    fn size(&self) -> u64 {
        (**self).size()
    }

    fn logical_sector_size(&self) -> u32 {
        (**self).logical_sector_size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf).await
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf).await
    }

    async fn flush(&self) -> Result<()> {
        (**self).flush().await
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        (**self).write_zeroes(offset, len).await
    }
}

/// Blanket implementation so `Arc<D>` is itself a device — the formatter hands
/// clones to each concurrent task.
#[async_trait::async_trait]
impl<D: BlockDevice + ?Sized> BlockDevice for std::sync::Arc<D> {
    fn size(&self) -> u64 {
        (**self).size()
    }

    fn logical_sector_size(&self) -> u32 {
        (**self).logical_sector_size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf).await
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf).await
    }

    async fn flush(&self) -> Result<()> {
        (**self).flush().await
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        (**self).write_zeroes(offset, len).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_device_round_trips() {
        let dev = MemDevice::new(4096);
        dev.write_at(100, b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        dev.read_at(100, &mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn out_of_bounds_is_refused() {
        let dev = MemDevice::new(512);
        let err = dev.write_at(500, &[0u8; 32]).await.unwrap_err();
        assert!(matches!(err, Error::OutOfBounds { .. }));
    }

    #[tokio::test]
    async fn write_zeroes_clears_a_range() {
        let dev = MemDevice::new(4096);
        dev.write_at(0, &[0xffu8; 4096]).await.unwrap();
        dev.write_zeroes(1024, 2048).await.unwrap();
        let image = dev.to_vec();
        assert!(image[..1024].iter().all(|&b| b == 0xff));
        assert!(image[1024..3072].iter().all(|&b| b == 0));
        assert!(image[3072..].iter().all(|&b| b == 0xff));
    }
}
