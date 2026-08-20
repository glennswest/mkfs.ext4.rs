//! Synchronous, `no_std` read path.
//!
//! The rest of this crate is async, because formatting a volume wants to fan
//! out across block groups. Reading one file does not, and the consumers that
//! need it most cannot have a runtime at all: **a UEFI driver reads a kernel
//! out of a filesystem before the kernel exists** — no allocator beyond
//! `alloc`, no `async`, no `std`.
//!
//! So this module is the same on-disk structures ([`crate::structs`]) driven by
//! a synchronous seam. There is one definition of the format and two ways to
//! reach it, rather than a second reader that has to stay bit-compatible with
//! this one forever.
//!
//! It is deliberately **read-only and narrow**: mount, resolve a path, read a
//! file. No writing, no allocation policy, no journal replay. Firmware runs
//! before anything can be debugged with a shell, so what it links should be
//! small enough to read in one sitting.
//!
//! ```no_run
//! # use mkfs_ext4::read::{BlockReader, Ext4};
//! # struct MyDev;
//! # impl BlockReader for MyDev {
//! #     fn read_at(&self, _o: u64, _b: &mut [u8]) -> Result<(), ()> { Ok(()) }
//! # }
//! let fs = Ext4::open(&MyDev)?;
//! let kernel = fs.read_file(&MyDev, "/vmlinuz")?;
//! # Ok::<(), mkfs_ext4::error::Error>(())
//! ```


use alloc::vec;
use alloc::vec::Vec;
#[cfg(not(feature = "std"))]
use alloc::string::ToString;

use crate::error::{Error, Result};
use crate::structs::extent::{Extent, ExtentHeader, ExtentIdx};
use crate::structs::inode::Inode;
use crate::structs::superblock::{Superblock, SUPERBLOCK_OFFSET};
use crate::structs::GroupDesc;

/// The root directory is always inode 2 in ext2/3/4.
pub const ROOT_INO: u32 = 2;

/// A synchronous byte-range reader.
///
/// The caller owns the device. A UEFI consumer wraps `BlockIO`, a host test
/// wraps a `Vec<u8>`, and neither needs a runtime.
pub trait BlockReader {
    /// Fill `buf` from `offset`. Any failure is a failure; there is nothing
    /// useful to report from firmware beyond that.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> core::result::Result<(), ()>;
}

fn read_exact(dev: &impl BlockReader, offset: u64, buf: &mut [u8]) -> Result<()> {
    dev.read_at(offset, buf).map_err(|_| Error::DeviceRead { offset })
}

/// A mounted filesystem: the superblock, plus enough geometry to find inodes.
#[derive(Debug, Clone)]
pub struct Ext4 {
    /// The superblock as read from disk.
    pub sb: Superblock,
    block_size: u64,
    inode_size: u64,
    inodes_per_group: u32,
    desc_size: u64,
    /// Where the group descriptor table starts, in bytes.
    gdt_offset: u64,
}

impl Ext4 {
    /// Read the superblock and derive the geometry needed to resolve inodes.
    pub fn open(dev: &impl BlockReader) -> Result<Ext4> {
        let mut buf = [0u8; 1024];
        read_exact(dev, SUPERBLOCK_OFFSET as u64, &mut buf)?;
        let sb = Superblock::decode(&buf)?;

        let block_size = 1024u64 << sb.log_block_size;
        let inode_size = sb.inode_size as u64;
        let desc_size = if sb.desc_size == 0 { 32 } else { sb.desc_size as u64 };

        // The GDT follows the superblock's block. With a 1 KiB block the
        // superblock lives in block 1, so the table starts at block 2.
        let sb_block = if block_size == 1024 { 1 } else { 0 };
        let gdt_offset = (sb_block + 1) * block_size;

        if block_size == 0 || inode_size == 0 || sb.inodes_per_group == 0 {
            return Err(Error::corrupt("ext4 read", "superblock geometry is zero"));
        }

        let inodes_per_group = sb.inodes_per_group;
        Ok(Ext4 {
            sb,
            block_size,
            inode_size,
            inodes_per_group,
            desc_size,
            gdt_offset,
        })
    }

    /// Filesystem block size in bytes.
    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    fn read_group_desc(&self, dev: &impl BlockReader, group: u32) -> Result<GroupDesc> {
        let off = self.gdt_offset + group as u64 * self.desc_size;
        let mut buf = vec![0u8; self.desc_size as usize];
        read_exact(dev, off, &mut buf)?;
        Ok(GroupDesc::decode(&buf, self.desc_size as usize))
    }

    /// Read one inode by number.
    pub fn read_inode(&self, dev: &impl BlockReader, ino: u32) -> Result<Inode> {
        if ino == 0 {
            return Err(Error::corrupt("ext4 read", "inode 0 does not exist"));
        }
        let group = (ino - 1) / self.inodes_per_group;
        let index = ((ino - 1) % self.inodes_per_group) as u64;
        let gd = self.read_group_desc(dev, group)?;
        let table = gd.inode_table * self.block_size;
        let off = table + index * self.inode_size;

        let mut buf = vec![0u8; self.inode_size as usize];
        read_exact(dev, off, &mut buf)?;
        Inode::decode(&buf, self.inode_size as usize)
    }

    /// Walk an inode's extent tree and return its extents in logical order.
    ///
    /// Only the extent form is supported: a filesystem this crate wrote always
    /// uses extents, and firmware reading a legacy indirect-mapped image is not
    /// a case worth carrying.
    fn extents_of(&self, dev: &impl BlockReader, inode: &Inode) -> Result<Vec<Extent>> {
        if !inode.uses_extents() {
            return Err(Error::UnsupportedFeature(
                alloc::string::String::from("inode uses the legacy indirect block map, not extents"),
            ));
        }
        let mut out = Vec::new();
        self.walk_extents(dev, &inode.block, &mut out, 0)?;
        out.sort_by_key(|e| e.block);
        Ok(out)
    }

    fn walk_extents(
        &self,
        dev: &impl BlockReader,
        node: &[u8],
        out: &mut Vec<Extent>,
        depth: u32,
    ) -> Result<()> {
        // A malformed tree must not become unbounded recursion in firmware.
        if depth > 5 {
            return Err(Error::corrupt("ext4 read", "extent tree deeper than 5 levels"));
        }
        // `ExtentHeader::decode` validates the magic itself.
        let hdr = ExtentHeader::decode(node)?;

        let entries = hdr.entries as usize;
        if hdr.depth == 0 {
            for i in 0..entries {
                let at = 12 + i * 12;
                if at + 12 > node.len() {
                    return Err(Error::corrupt("ext4 read", "extent runs past its node"));
                }
                out.push(Extent::decode(&node[at..at + 12]));
            }
            return Ok(());
        }

        for i in 0..entries {
            let at = 12 + i * 12;
            if at + 12 > node.len() {
                return Err(Error::corrupt("ext4 read", "extent index runs past its node"));
            }
            let idx = ExtentIdx::decode(&node[at..at + 12]);
            let mut child = vec![0u8; self.block_size as usize];
            read_exact(dev, idx.leaf * self.block_size, &mut child)?;
            self.walk_extents(dev, &child, out, depth + 1)?;
        }
        Ok(())
    }

    /// Read a whole file by inode.
    pub fn read_inode_data(&self, dev: &impl BlockReader, inode: &Inode) -> Result<Vec<u8>> {
        let size = inode.size;
        let mut out = vec![0u8; size as usize];
        for e in self.extents_of(dev, inode)? {
            let start = e.block as u64 * self.block_size;
            if start >= size {
                continue;
            }
            // An uninitialised extent reads as zeroes rather than as whatever
            // the allocator left behind.
            if e.is_uninit() {
                continue;
            }
            let len = (e.effective_len() as u64 * self.block_size).min(size - start);
            read_exact(
                dev,
                e.start * self.block_size,
                &mut out[start as usize..(start + len) as usize],
            )?;
        }
        Ok(out)
    }

    /// List one directory's entries as `(name, inode)`.
    pub fn read_dir(&self, dev: &impl BlockReader, dir: &Inode) -> Result<Vec<(Vec<u8>, u32)>> {
        let data = self.read_inode_data(dev, dir)?;
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let ino = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
            let rec_len = u16::from_le_bytes([data[at + 4], data[at + 5]]) as usize;
            let name_len = data[at + 6] as usize;
            if rec_len < 8 || at + rec_len > data.len() {
                break;
            }
            if ino != 0 && name_len > 0 && at + 8 + name_len <= data.len() {
                out.push((data[at + 8..at + 8 + name_len].to_vec(), ino));
            }
            at += rec_len;
        }
        Ok(out)
    }

    /// Resolve an absolute path to its inode number.
    ///
    /// Plain component-by-component descent: no symlink following, no `..`
    /// handling. Firmware reads a known layout, and quietly resolving a symlink
    /// out of an image it is about to boot is not a favour.
    pub fn lookup(&self, dev: &impl BlockReader, path: &str) -> Result<u32> {
        let mut ino = ROOT_INO;
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            let dir = self.read_inode(dev, ino)?;
            let entries = self.read_dir(dev, &dir)?;
            let found = entries
                .iter()
                .find(|(name, _)| name.as_slice() == part.as_bytes())
                .map(|(_, i)| *i);
            match found {
                Some(next) => ino = next,
                None => return Err(Error::corrupt("ext4 read", "path component not found")),
            }
        }
        Ok(ino)
    }

    /// Read a file by absolute path — the call firmware actually makes.
    pub fn read_file(&self, dev: &impl BlockReader, path: &str) -> Result<Vec<u8>> {
        let ino = self.lookup(dev, path)?;
        let inode = self.read_inode(dev, ino)?;
        self.read_inode_data(dev, &inode)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::device::{BlockDevice, MemDevice};
    use crate::params::Params;

    /// Adapts the async device to the synchronous seam, so the reader is
    /// exercised against a filesystem *this crate wrote*. If the formatter and
    /// the reader ever disagree about the on-disk layout, this is where it
    /// shows — which is the whole reason the two share `structs`.
    struct SyncOver<'a>(&'a MemDevice);

    impl BlockReader for SyncOver<'_> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
            futures::executor::block_on(self.0.read_at(offset, buf)).map_err(|_| ())
        }
    }

    async fn formatted(size: u64, block_size: Option<u32>) -> MemDevice {
        let dev = MemDevice::new(size);
        let mut params = Params::default();
        params.block_size = block_size;
        crate::format::format(&dev, &params).await.unwrap();
        dev
    }

    #[tokio::test]
    async fn reads_back_the_superblock_this_crate_wrote() {
        for (size, bs) in [(64 << 20, Some(4096)), (16 << 20, Some(1024))] {
            let dev = formatted(size, bs).await;
            let fs = Ext4::open(&SyncOver(&dev)).expect("superblock decodes");
            assert_eq!(
                fs.block_size(),
                bs.unwrap() as u64,
                "block size disagrees for a {size}-byte device"
            );
            assert!(fs.sb.inodes_per_group > 0);
        }
    }

    #[tokio::test]
    async fn walks_the_root_directory() {
        let dev = formatted(64 << 20, Some(4096)).await;
        let d = SyncOver(&dev);
        let fs = Ext4::open(&d).unwrap();

        let root = fs.read_inode(&d, ROOT_INO).expect("root inode");
        assert!(root.is_dir(), "inode 2 must be a directory");

        let entries = fs.read_dir(&d, &root).expect("root dirents");
        let names: Vec<&[u8]> = entries.iter().map(|(n, _)| n.as_slice()).collect();
        assert!(names.contains(&b".".as_slice()), "root is missing '.'");
        assert!(names.contains(&b"..".as_slice()), "root is missing '..'");

        // `.` points back at the root, which is the cheapest check that the
        // dirent decode is aligned rather than merely plausible.
        let dot = entries.iter().find(|(n, _)| n == b".").unwrap().1;
        assert_eq!(dot, ROOT_INO);
    }

    #[tokio::test]
    async fn lookup_resolves_the_root_and_refuses_what_is_not_there() {
        let dev = formatted(64 << 20, Some(4096)).await;
        let d = SyncOver(&dev);
        let fs = Ext4::open(&d).unwrap();

        assert_eq!(fs.lookup(&d, "/").unwrap(), ROOT_INO);
        assert_eq!(fs.lookup(&d, "").unwrap(), ROOT_INO);
        assert!(fs.lookup(&d, "/nothing-here").is_err());
    }

    #[tokio::test]
    async fn refuses_a_device_that_is_not_ext4() {
        let dev = MemDevice::new(16 << 20);
        assert!(
            Ext4::open(&SyncOver(&dev)).is_err(),
            "a zeroed device must not decode as a filesystem"
        );
    }
}
