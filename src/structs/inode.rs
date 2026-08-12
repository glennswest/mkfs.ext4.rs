//! Inodes.
//!
//! Mirrors `struct ext2_inode_large` from `lib/ext2fs/ext2_fs.h`. The first 128
//! bytes are the original ext2 inode; anything beyond that is the "extra" area,
//! whose extent is declared by `i_extra_isize` and which carries nanosecond
//! timestamps, the creation time, the high half of the checksum and the project
//! id.

use crate::bytes::*;
use crate::csum;
use crate::error::{Error, Result};

/// `EXT2_GOOD_OLD_INODE_SIZE` — the original ext2 inode, and the base of every
/// larger one.
pub const GOOD_OLD_INODE_SIZE: usize = 128;

/// Bytes of `i_block`, which hold either 15 block pointers or an inline extent
/// tree.
pub const I_BLOCK_LEN: usize = 60;

/// `EXT2_N_BLOCKS` — direct, indirect, double and triple indirect pointers.
pub const N_BLOCKS: usize = 15;

/// `EXT2_NDIR_BLOCKS` — direct block pointers.
pub const NDIR_BLOCKS: usize = 12;

/// File mode bits.
pub mod mode {
    /// FIFO.
    pub const IFIFO: u16 = 0x1000;
    /// Character device.
    pub const IFCHR: u16 = 0x2000;
    /// Directory.
    pub const IFDIR: u16 = 0x4000;
    /// Block device.
    pub const IFBLK: u16 = 0x6000;
    /// Regular file.
    pub const IFREG: u16 = 0x8000;
    /// Symbolic link.
    pub const IFLNK: u16 = 0xa000;
    /// Socket.
    pub const IFSOCK: u16 = 0xc000;
    /// Mask selecting the format bits above.
    pub const IFMT: u16 = 0xf000;
}

/// Inode flags (`i_flags`).
pub mod iflags {
    /// `EXT2_INDEX_FL` — hash-indexed directory.
    pub const INDEX: u32 = 0x0000_1000;
    /// `EXT3_JOURNAL_DATA_FL`
    pub const JOURNAL_DATA: u32 = 0x0000_4000;
    /// `EXT4_HUGE_FILE_FL` — `i_blocks` counts filesystem blocks, not sectors.
    pub const HUGE_FILE: u32 = 0x0004_0000;
    /// `EXT4_EXTENTS_FL` — `i_block` holds an extent tree.
    pub const EXTENTS: u32 = 0x0008_0000;
    /// `EXT4_INLINE_DATA_FL`
    pub const INLINE_DATA: u32 = 0x1000_0000;
}

/// Field offsets within an inode.
#[allow(missing_docs)]
pub mod off {
    pub const I_MODE: usize = 0x00;
    pub const I_UID: usize = 0x02;
    pub const I_SIZE: usize = 0x04;
    pub const I_ATIME: usize = 0x08;
    pub const I_CTIME: usize = 0x0c;
    pub const I_MTIME: usize = 0x10;
    pub const I_DTIME: usize = 0x14;
    pub const I_GID: usize = 0x18;
    pub const I_LINKS_COUNT: usize = 0x1a;
    pub const I_BLOCKS: usize = 0x1c;
    pub const I_FLAGS: usize = 0x20;
    pub const I_VERSION: usize = 0x24;
    pub const I_BLOCK: usize = 0x28;
    pub const I_GENERATION: usize = 0x64;
    pub const I_FILE_ACL: usize = 0x68;
    pub const I_SIZE_HIGH: usize = 0x6c;
    pub const I_FADDR: usize = 0x70;
    pub const L_I_BLOCKS_HI: usize = 0x74;
    pub const L_I_FILE_ACL_HIGH: usize = 0x76;
    pub const L_I_UID_HIGH: usize = 0x78;
    pub const L_I_GID_HIGH: usize = 0x7a;
    pub const L_I_CHECKSUM_LO: usize = 0x7c;
    pub const L_I_RESERVED: usize = 0x7e;
    pub const I_EXTRA_ISIZE: usize = 0x80;
    pub const I_CHECKSUM_HI: usize = 0x82;
    pub const I_CTIME_EXTRA: usize = 0x84;
    pub const I_MTIME_EXTRA: usize = 0x88;
    pub const I_ATIME_EXTRA: usize = 0x8c;
    pub const I_CRTIME: usize = 0x90;
    pub const I_CRTIME_EXTRA: usize = 0x94;
    pub const I_VERSION_HI: usize = 0x98;
    pub const I_PROJID: usize = 0x9c;
}

/// A decoded inode.
///
/// `Default` is written out rather than derived: `i_block` is 60 bytes, past
/// the length for which the standard library implements `Default` on arrays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    /// `i_mode`
    pub mode: u16,
    /// `i_uid` combined with `l_i_uid_high`.
    pub uid: u32,
    /// `i_size` combined with `i_size_high`.
    pub size: u64,
    /// `i_atime`
    pub atime: u32,
    /// `i_ctime`
    pub ctime: u32,
    /// `i_mtime`
    pub mtime: u32,
    /// `i_dtime`
    pub dtime: u32,
    /// `i_gid` combined with `l_i_gid_high`.
    pub gid: u32,
    /// `i_links_count`
    pub links_count: u16,
    /// `i_blocks` combined with `l_i_blocks_hi`, in 512-byte sectors unless
    /// [`iflags::HUGE_FILE`] is set.
    pub blocks: u64,
    /// `i_flags`
    pub flags: u32,
    /// `osd1.linux1.l_i_version`
    pub version: u32,
    /// `i_block` — 15 block pointers, or an inline extent tree.
    pub block: [u8; I_BLOCK_LEN],
    /// `i_generation`
    pub generation: u32,
    /// `i_file_acl` combined with `l_i_file_acl_high`.
    pub file_acl: u64,
    /// `i_faddr`
    pub faddr: u32,
    /// `i_extra_isize` — zero on a 128-byte inode.
    pub extra_isize: u16,
    /// `i_ctime_extra`
    pub ctime_extra: u32,
    /// `i_mtime_extra`
    pub mtime_extra: u32,
    /// `i_atime_extra`
    pub atime_extra: u32,
    /// `i_crtime`
    pub crtime: u32,
    /// `i_crtime_extra`
    pub crtime_extra: u32,
    /// `i_version_hi`
    pub version_hi: u32,
    /// `i_projid`
    pub projid: u32,
    /// Bytes past the fields above, kept verbatim so a round trip preserves
    /// inline extended attributes.
    pub tail: Vec<u8>,
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            mode: 0,
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: 0,
            blocks: 0,
            flags: 0,
            version: 0,
            block: [0u8; I_BLOCK_LEN],
            generation: 0,
            file_acl: 0,
            faddr: 0,
            extra_isize: 0,
            ctime_extra: 0,
            mtime_extra: 0,
            atime_extra: 0,
            crtime: 0,
            crtime_extra: 0,
            version_hi: 0,
            projid: 0,
            tail: Vec::new(),
        }
    }
}

impl Inode {
    /// A fresh inode of `inode_size` bytes with the extra area declared.
    ///
    /// `mke2fs` sets `i_extra_isize` to `s_want_extra_isize`, which is 32 for
    /// the 256-byte inodes it creates by default.
    pub fn new(inode_size: usize, extra_isize: u16) -> Self {
        Self {
            extra_isize: if inode_size > GOOD_OLD_INODE_SIZE {
                extra_isize
            } else {
                0
            },
            ..Default::default()
        }
    }

    /// The file type bits of `i_mode`.
    pub fn file_type(&self) -> u16 {
        self.mode & mode::IFMT
    }

    /// Whether this inode is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == mode::IFDIR
    }

    /// Whether this inode is a regular file.
    pub fn is_reg(&self) -> bool {
        self.file_type() == mode::IFREG
    }

    /// Whether this inode is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.file_type() == mode::IFLNK
    }

    /// Whether `i_block` holds an extent tree rather than block pointers.
    pub fn uses_extents(&self) -> bool {
        self.flags & iflags::EXTENTS != 0
    }

    /// Whether this inode's `i_block` is a block map at all.
    ///
    /// It usually is — but not for a device, FIFO or socket, where `i_block`
    /// holds the device number, and not for a fast symlink, where it holds the
    /// target path. Walking those as if they were block pointers reads a
    /// device's major and minor number as a physical block, which is how a
    /// checker ends up reporting a filesystem full of impossible blocks.
    pub fn has_block_map(&self) -> bool {
        match self.file_type() {
            mode::IFCHR | mode::IFBLK | mode::IFIFO | mode::IFSOCK => false,
            // A symlink short enough to sit inside the inode has no blocks.
            mode::IFLNK => self.size >= I_BLOCK_LEN as u64,
            _ => true,
        }
    }

    /// The device number a special file refers to, as (major, minor).
    ///
    /// Small numbers live in `i_block[0]` the classic way; anything larger uses
    /// the wider encoding in `i_block[1]`.
    pub fn device_numbers(&self) -> (u32, u32) {
        let first = get_u32(&self.block, 0);
        if first != 0 {
            ((first >> 8) & 0xff, first & 0xff)
        } else {
            let second = get_u32(&self.block, 4);
            (
                (second & 0x000f_ff00) >> 8,
                (second & 0xff) | ((second >> 12) & 0x000f_ff00),
            )
        }
    }

    /// Point this inode at a device.
    pub fn set_device_numbers(&mut self, major: u32, minor: u32) {
        self.block = [0u8; I_BLOCK_LEN];
        if major < 256 && minor < 256 {
            put_u32(&mut self.block, 0, (major << 8) | minor);
        } else {
            let encoded = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
            put_u32(&mut self.block, 4, encoded);
        }
    }

    /// Whether the inode is in use — an unused inode has no links and no
    /// deletion time.
    pub fn in_use(&self) -> bool {
        self.links_count > 0 || self.dtime != 0
    }

    /// Read `i_block` as 15 block pointers.
    pub fn block_pointers(&self) -> [u32; N_BLOCKS] {
        let mut out = [0u32; N_BLOCKS];
        for (i, p) in out.iter_mut().enumerate() {
            *p = get_u32(&self.block, i * 4);
        }
        out
    }

    /// Write 15 block pointers into `i_block`.
    pub fn set_block_pointers(&mut self, pointers: &[u32; N_BLOCKS]) {
        for (i, p) in pointers.iter().enumerate() {
            put_u32(&mut self.block, i * 4, *p);
        }
    }

    /// Whether this inode carries the high half of its checksum.
    ///
    /// Matches the `has_hi` test in `ext2fs_inode_csum`.
    pub fn has_checksum_hi(&self, inode_size: usize) -> bool {
        inode_size > GOOD_OLD_INODE_SIZE && self.extra_isize >= csum::INODE_CSUM_HI_EXTRA_END
    }

    /// Decode from `inode_size` bytes.
    pub fn decode(buf: &[u8], inode_size: usize) -> Result<Self> {
        if buf.len() < inode_size || inode_size < GOOD_OLD_INODE_SIZE {
            return Err(Error::corrupt(
                "inode",
                format!("need {inode_size} bytes, got {}", buf.len()),
            ));
        }

        let large = inode_size > GOOD_OLD_INODE_SIZE;
        let extra_isize = if large {
            get_u16(buf, off::I_EXTRA_ISIZE)
        } else {
            0
        };
        // A field in the extra area is only present if i_extra_isize declares
        // it; reading past that is reading whatever the extended attributes
        // put there.
        let extra_end = GOOD_OLD_INODE_SIZE + extra_isize as usize;
        let has = |field_off: usize, len: usize| -> bool {
            large && field_off + len <= extra_end.min(inode_size)
        };
        let extra_u32 = |field_off: usize| -> u32 {
            if has(field_off, 4) {
                get_u32(buf, field_off)
            } else {
                0
            }
        };

        let tail_start = extra_end.min(inode_size);
        Ok(Self {
            mode: get_u16(buf, off::I_MODE),
            uid: get_u16(buf, off::I_UID) as u32 | ((get_u16(buf, off::L_I_UID_HIGH) as u32) << 16),
            size: get_u32(buf, off::I_SIZE) as u64
                | ((get_u32(buf, off::I_SIZE_HIGH) as u64) << 32),
            atime: get_u32(buf, off::I_ATIME),
            ctime: get_u32(buf, off::I_CTIME),
            mtime: get_u32(buf, off::I_MTIME),
            dtime: get_u32(buf, off::I_DTIME),
            gid: get_u16(buf, off::I_GID) as u32 | ((get_u16(buf, off::L_I_GID_HIGH) as u32) << 16),
            links_count: get_u16(buf, off::I_LINKS_COUNT),
            blocks: get_u32(buf, off::I_BLOCKS) as u64
                | ((get_u16(buf, off::L_I_BLOCKS_HI) as u64) << 32),
            flags: get_u32(buf, off::I_FLAGS),
            version: get_u32(buf, off::I_VERSION),
            block: get_array(buf, off::I_BLOCK),
            generation: get_u32(buf, off::I_GENERATION),
            file_acl: get_u32(buf, off::I_FILE_ACL) as u64
                | ((get_u16(buf, off::L_I_FILE_ACL_HIGH) as u64) << 32),
            faddr: get_u32(buf, off::I_FADDR),
            extra_isize,
            ctime_extra: extra_u32(off::I_CTIME_EXTRA),
            mtime_extra: extra_u32(off::I_MTIME_EXTRA),
            atime_extra: extra_u32(off::I_ATIME_EXTRA),
            crtime: extra_u32(off::I_CRTIME),
            crtime_extra: extra_u32(off::I_CRTIME_EXTRA),
            version_hi: extra_u32(off::I_VERSION_HI),
            projid: extra_u32(off::I_PROJID),
            tail: {
                // Trailing zeroes are dropped: `encode_into` zero-fills the
                // rest of the inode anyway, so this stays byte-lossless while
                // making a decode/encode/decode cycle settle on one value.
                let raw = if tail_start < inode_size {
                    &buf[tail_start..inode_size]
                } else {
                    &[][..]
                };
                let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
                raw[..end].to_vec()
            },
        })
    }

    /// Encode into `inode_size` bytes, leaving both checksum fields zero.
    pub fn encode(&self, inode_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; inode_size];
        self.encode_into(&mut buf, inode_size);
        buf
    }

    /// Encode into `buf`, leaving both checksum fields zero.
    pub fn encode_into(&self, buf: &mut [u8], inode_size: usize) {
        put_u16(buf, off::I_MODE, self.mode);
        put_u16(buf, off::I_UID, self.uid as u16);
        put_u32(buf, off::I_SIZE, self.size as u32);
        put_u32(buf, off::I_ATIME, self.atime);
        put_u32(buf, off::I_CTIME, self.ctime);
        put_u32(buf, off::I_MTIME, self.mtime);
        put_u32(buf, off::I_DTIME, self.dtime);
        put_u16(buf, off::I_GID, self.gid as u16);
        put_u16(buf, off::I_LINKS_COUNT, self.links_count);
        put_u32(buf, off::I_BLOCKS, self.blocks as u32);
        put_u32(buf, off::I_FLAGS, self.flags);
        put_u32(buf, off::I_VERSION, self.version);
        put_bytes(buf, off::I_BLOCK, I_BLOCK_LEN, &self.block);
        put_u32(buf, off::I_GENERATION, self.generation);
        put_u32(buf, off::I_FILE_ACL, self.file_acl as u32);
        put_u32(buf, off::I_SIZE_HIGH, (self.size >> 32) as u32);
        put_u32(buf, off::I_FADDR, self.faddr);
        put_u16(buf, off::L_I_BLOCKS_HI, (self.blocks >> 32) as u16);
        put_u16(buf, off::L_I_FILE_ACL_HIGH, (self.file_acl >> 32) as u16);
        put_u16(buf, off::L_I_UID_HIGH, (self.uid >> 16) as u16);
        put_u16(buf, off::L_I_GID_HIGH, (self.gid >> 16) as u16);
        put_u16(buf, off::L_I_CHECKSUM_LO, 0);
        put_u16(buf, off::L_I_RESERVED, 0);

        if inode_size <= GOOD_OLD_INODE_SIZE {
            return;
        }

        put_u16(buf, off::I_EXTRA_ISIZE, self.extra_isize);
        put_u16(buf, off::I_CHECKSUM_HI, 0);

        let extra_end = (GOOD_OLD_INODE_SIZE + self.extra_isize as usize).min(inode_size);
        let mut put_extra = |field_off: usize, v: u32| {
            if field_off + 4 <= extra_end {
                put_u32(buf, field_off, v);
            }
        };
        put_extra(off::I_CTIME_EXTRA, self.ctime_extra);
        put_extra(off::I_MTIME_EXTRA, self.mtime_extra);
        put_extra(off::I_ATIME_EXTRA, self.atime_extra);
        put_extra(off::I_CRTIME, self.crtime);
        put_extra(off::I_CRTIME_EXTRA, self.crtime_extra);
        put_extra(off::I_VERSION_HI, self.version_hi);
        put_extra(off::I_PROJID, self.projid);

        if !self.tail.is_empty() && extra_end < inode_size {
            let n = self.tail.len().min(inode_size - extra_end);
            buf[extra_end..extra_end + n].copy_from_slice(&self.tail[..n]);
        }
    }

    /// Encode and stamp the checksum, when the filesystem carries one.
    pub fn encode_with_csum(
        &self,
        inode_size: usize,
        metadata_csum: bool,
        seed: u32,
        inum: u32,
    ) -> Vec<u8> {
        let mut buf = self.encode(inode_size);
        if metadata_csum {
            let crc = csum::inode_csum(seed, inum, self.generation, &buf);
            put_u16(&mut buf, off::L_I_CHECKSUM_LO, crc as u16);
            if self.has_checksum_hi(inode_size) {
                put_u16(&mut buf, off::I_CHECKSUM_HI, (crc >> 16) as u16);
            }
        }
        buf
    }

    /// Verify an inode's checksum against the bytes it was read from.
    pub fn verify_checksum(
        buf: &[u8],
        inode_size: usize,
        metadata_csum: bool,
        seed: u32,
        inum: u32,
    ) -> Result<bool> {
        if !metadata_csum {
            return Ok(true);
        }
        let inode = Self::decode(buf, inode_size)?;
        let has_hi = inode.has_checksum_hi(inode_size);

        let mut provided = get_u16(buf, off::L_I_CHECKSUM_LO) as u32;
        if has_hi {
            provided |= (get_u16(buf, off::I_CHECKSUM_HI) as u32) << 16;
        }

        // The checksum is computed over the inode with its checksum fields
        // zeroed, so zero them in a copy rather than in place.
        let mut zeroed = buf[..inode_size].to_vec();
        put_u16(&mut zeroed, off::L_I_CHECKSUM_LO, 0);
        if has_hi {
            put_u16(&mut zeroed, off::I_CHECKSUM_HI, 0);
        }
        let mut expect = csum::inode_csum(seed, inum, inode.generation, &zeroed);
        if !has_hi {
            expect &= 0xffff;
        }

        if provided == expect {
            return Ok(true);
        }
        // e2fsprogs treats an all-zero inode as valid regardless of checksum;
        // an inode that was never written has no checksum to match.
        Ok(buf[..GOOD_OLD_INODE_SIZE].iter().all(|&b| b == 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_match_ext2_fs_h() {
        assert_eq!(off::I_BLOCK, 0x28);
        assert_eq!(off::I_GENERATION, 0x64);
        assert_eq!(off::L_I_CHECKSUM_LO, 0x7c);
        assert_eq!(off::I_EXTRA_ISIZE, 0x80);
        assert_eq!(off::I_CHECKSUM_HI, 0x82);
        assert_eq!(off::I_PROJID, 0x9c);
        assert_eq!(off::I_BLOCK + I_BLOCK_LEN, off::I_GENERATION);
    }

    #[test]
    fn round_trips_a_large_inode() {
        let mut ino = Inode::new(256, 32);
        ino.mode = mode::IFDIR | 0o755;
        ino.uid = 0x1234_5678;
        ino.gid = 0x8765_4321;
        ino.size = 0x1_0000_1000;
        ino.links_count = 3;
        ino.blocks = 8;
        ino.flags = iflags::EXTENTS;
        ino.generation = 0xabcd_1234;
        ino.crtime = 1_700_000_000;
        ino.atime = 1_700_000_001;
        ino.block[0] = 0x0a;

        let buf = ino.encode(256);
        let back = Inode::decode(&buf, 256).unwrap();
        assert_eq!(ino, back);
        assert!(back.is_dir());
        assert!(back.uses_extents());
    }

    #[test]
    fn a_128_byte_inode_has_no_extra_area() {
        let mut ino = Inode::new(128, 32);
        ino.mode = mode::IFREG | 0o644;
        ino.links_count = 1;
        assert_eq!(ino.extra_isize, 0);

        let buf = ino.encode(128);
        assert_eq!(buf.len(), 128);
        let back = Inode::decode(&buf, 128).unwrap();
        assert_eq!(ino, back);
        assert!(!back.has_checksum_hi(128));
    }

    #[test]
    fn crtime_is_dropped_when_extra_isize_does_not_reach_it() {
        // i_crtime sits at 0x90; an extra_isize of 4 covers only up to 0x84.
        let mut ino = Inode::new(256, 4);
        ino.crtime = 12345;
        let buf = ino.encode(256);
        let back = Inode::decode(&buf, 256).unwrap();
        assert_eq!(back.crtime, 0);
        assert_eq!(back.extra_isize, 4);
    }

    #[test]
    fn checksum_round_trips() {
        let mut ino = Inode::new(256, 32);
        ino.mode = mode::IFREG | 0o644;
        ino.links_count = 1;
        ino.generation = 42;

        let seed = csum::seed_from_uuid(b"0123456789abcdef");
        let buf = ino.encode_with_csum(256, true, seed, 12);
        assert!(Inode::verify_checksum(&buf, 256, true, seed, 12).unwrap());
        // The inode number is part of the checksum, so the same bytes at a
        // different number must not verify.
        assert!(!Inode::verify_checksum(&buf, 256, true, seed, 13).unwrap());
    }

    #[test]
    fn an_all_zero_inode_verifies() {
        let buf = vec![0u8; 256];
        let seed = csum::seed_from_uuid(&[0u8; 16]);
        assert!(Inode::verify_checksum(&buf, 256, true, seed, 99).unwrap());
    }

    #[test]
    fn block_pointers_round_trip() {
        let mut ino = Inode::new(256, 32);
        let mut ptrs = [0u32; N_BLOCKS];
        for (i, p) in ptrs.iter_mut().enumerate() {
            *p = 1000 + i as u32;
        }
        ino.set_block_pointers(&ptrs);
        assert_eq!(ino.block_pointers(), ptrs);
    }
}
