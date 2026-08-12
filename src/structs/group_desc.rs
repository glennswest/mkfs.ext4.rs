//! Block group descriptors.
//!
//! Mirrors `struct ext4_group_desc` from `lib/ext2fs/ext2_fs.h`. A descriptor is
//! 32 bytes without the `64bit` feature and 64 bytes with it; the upper half
//! carries the high 32 bits of each block pointer and the high halves of the
//! counters and bitmap checksums.

use crate::bytes::*;
use crate::csum::{self, GroupDescCsum};

/// `EXT2_MIN_DESC_SIZE`
pub const MIN_DESC_SIZE: usize = 32;

/// `EXT2_MIN_DESC_SIZE_64BIT`
pub const DESC_SIZE_64BIT: usize = 64;

/// Group flags (`bg_flags`).
pub mod bg_flags {
    /// `EXT2_BG_INODE_UNINIT` — the inode table and bitmap were never written.
    pub const INODE_UNINIT: u16 = 0x0001;
    /// `EXT2_BG_BLOCK_UNINIT` — the block bitmap was never written.
    pub const BLOCK_UNINIT: u16 = 0x0002;
    /// `EXT2_BG_INODE_ZEROED` — the inode table on disk is known to be zeroed.
    pub const INODE_ZEROED: u16 = 0x0004;
}

/// Field offsets within a group descriptor.
#[allow(missing_docs)]
pub mod off {
    pub const BG_BLOCK_BITMAP: usize = 0x00;
    pub const BG_INODE_BITMAP: usize = 0x04;
    pub const BG_INODE_TABLE: usize = 0x08;
    pub const BG_FREE_BLOCKS_COUNT: usize = 0x0c;
    pub const BG_FREE_INODES_COUNT: usize = 0x0e;
    pub const BG_USED_DIRS_COUNT: usize = 0x10;
    pub const BG_FLAGS: usize = 0x12;
    pub const BG_EXCLUDE_BITMAP_LO: usize = 0x14;
    pub const BG_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
    pub const BG_INODE_BITMAP_CSUM_LO: usize = 0x1a;
    pub const BG_ITABLE_UNUSED: usize = 0x1c;
    pub const BG_CHECKSUM: usize = 0x1e;
    pub const BG_BLOCK_BITMAP_HI: usize = 0x20;
    pub const BG_INODE_BITMAP_HI: usize = 0x24;
    pub const BG_INODE_TABLE_HI: usize = 0x28;
    pub const BG_FREE_BLOCKS_COUNT_HI: usize = 0x2c;
    pub const BG_FREE_INODES_COUNT_HI: usize = 0x2e;
    pub const BG_USED_DIRS_COUNT_HI: usize = 0x30;
    pub const BG_ITABLE_UNUSED_HI: usize = 0x32;
    pub const BG_EXCLUDE_BITMAP_HI: usize = 0x34;
    pub const BG_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
    pub const BG_INODE_BITMAP_CSUM_HI: usize = 0x3a;
    pub const BG_RESERVED: usize = 0x3c;
}

/// A block group descriptor, with the 32-bit and 64-bit halves merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupDesc {
    /// Block holding this group's block bitmap.
    pub block_bitmap: u64,
    /// Block holding this group's inode bitmap.
    pub inode_bitmap: u64,
    /// First block of this group's inode table.
    pub inode_table: u64,
    /// Free blocks in this group.
    pub free_blocks_count: u32,
    /// Free inodes in this group.
    pub free_inodes_count: u32,
    /// Directories in this group.
    pub used_dirs_count: u32,
    /// `bg_flags`
    pub flags: u16,
    /// Snapshot exclude bitmap block.
    pub exclude_bitmap: u64,
    /// crc32c of the block bitmap.
    pub block_bitmap_csum: u32,
    /// crc32c of the inode bitmap.
    pub inode_bitmap_csum: u32,
    /// Inodes at the end of the table never yet used, so `e2fsck` can skip
    /// them. Also what makes a lazily-initialised inode table safe.
    pub itable_unused: u32,
    /// `bg_checksum` as read. Recomputed on encode.
    pub checksum: u16,
}

impl GroupDesc {
    /// Decode from `desc_size` bytes.
    pub fn decode(buf: &[u8], desc_size: usize) -> Self {
        let wide = desc_size >= DESC_SIZE_64BIT;
        let hi32 = |off: usize| -> u64 {
            if wide {
                (get_u32(buf, off) as u64) << 32
            } else {
                0
            }
        };
        let hi16 = |off: usize| -> u32 {
            if wide {
                (get_u16(buf, off) as u32) << 16
            } else {
                0
            }
        };

        Self {
            block_bitmap: get_u32(buf, off::BG_BLOCK_BITMAP) as u64 | hi32(off::BG_BLOCK_BITMAP_HI),
            inode_bitmap: get_u32(buf, off::BG_INODE_BITMAP) as u64 | hi32(off::BG_INODE_BITMAP_HI),
            inode_table: get_u32(buf, off::BG_INODE_TABLE) as u64 | hi32(off::BG_INODE_TABLE_HI),
            free_blocks_count: get_u16(buf, off::BG_FREE_BLOCKS_COUNT) as u32
                | hi16(off::BG_FREE_BLOCKS_COUNT_HI),
            free_inodes_count: get_u16(buf, off::BG_FREE_INODES_COUNT) as u32
                | hi16(off::BG_FREE_INODES_COUNT_HI),
            used_dirs_count: get_u16(buf, off::BG_USED_DIRS_COUNT) as u32
                | hi16(off::BG_USED_DIRS_COUNT_HI),
            flags: get_u16(buf, off::BG_FLAGS),
            exclude_bitmap: get_u32(buf, off::BG_EXCLUDE_BITMAP_LO) as u64
                | hi32(off::BG_EXCLUDE_BITMAP_HI),
            block_bitmap_csum: get_u16(buf, off::BG_BLOCK_BITMAP_CSUM_LO) as u32
                | if desc_size >= csum::BG_BLOCK_BITMAP_CSUM_HI_LOCATION {
                    (get_u16(buf, off::BG_BLOCK_BITMAP_CSUM_HI) as u32) << 16
                } else {
                    0
                },
            inode_bitmap_csum: get_u16(buf, off::BG_INODE_BITMAP_CSUM_LO) as u32
                | if desc_size >= csum::BG_BLOCK_BITMAP_CSUM_HI_LOCATION + 2 {
                    (get_u16(buf, off::BG_INODE_BITMAP_CSUM_HI) as u32) << 16
                } else {
                    0
                },
            itable_unused: get_u16(buf, off::BG_ITABLE_UNUSED) as u32
                | hi16(off::BG_ITABLE_UNUSED_HI),
            checksum: get_u16(buf, off::BG_CHECKSUM),
        }
    }

    /// Encode into `buf`, which must be `desc_size` bytes.
    ///
    /// The checksum field is left as [`Self::checksum`]; call
    /// [`Self::encode_with_csum`] to have it computed.
    pub fn encode_into(&self, buf: &mut [u8], desc_size: usize) {
        for b in buf[..desc_size].iter_mut() {
            *b = 0;
        }
        let wide = desc_size >= DESC_SIZE_64BIT;

        put_u32(buf, off::BG_BLOCK_BITMAP, self.block_bitmap as u32);
        put_u32(buf, off::BG_INODE_BITMAP, self.inode_bitmap as u32);
        put_u32(buf, off::BG_INODE_TABLE, self.inode_table as u32);
        put_u16(
            buf,
            off::BG_FREE_BLOCKS_COUNT,
            self.free_blocks_count as u16,
        );
        put_u16(
            buf,
            off::BG_FREE_INODES_COUNT,
            self.free_inodes_count as u16,
        );
        put_u16(buf, off::BG_USED_DIRS_COUNT, self.used_dirs_count as u16);
        put_u16(buf, off::BG_FLAGS, self.flags);
        put_u32(buf, off::BG_EXCLUDE_BITMAP_LO, self.exclude_bitmap as u32);
        put_u16(
            buf,
            off::BG_BLOCK_BITMAP_CSUM_LO,
            self.block_bitmap_csum as u16,
        );
        put_u16(
            buf,
            off::BG_INODE_BITMAP_CSUM_LO,
            self.inode_bitmap_csum as u16,
        );
        put_u16(buf, off::BG_ITABLE_UNUSED, self.itable_unused as u16);
        put_u16(buf, off::BG_CHECKSUM, self.checksum);

        if wide {
            put_u32(
                buf,
                off::BG_BLOCK_BITMAP_HI,
                (self.block_bitmap >> 32) as u32,
            );
            put_u32(
                buf,
                off::BG_INODE_BITMAP_HI,
                (self.inode_bitmap >> 32) as u32,
            );
            put_u32(buf, off::BG_INODE_TABLE_HI, (self.inode_table >> 32) as u32);
            put_u16(
                buf,
                off::BG_FREE_BLOCKS_COUNT_HI,
                (self.free_blocks_count >> 16) as u16,
            );
            put_u16(
                buf,
                off::BG_FREE_INODES_COUNT_HI,
                (self.free_inodes_count >> 16) as u16,
            );
            put_u16(
                buf,
                off::BG_USED_DIRS_COUNT_HI,
                (self.used_dirs_count >> 16) as u16,
            );
            put_u16(
                buf,
                off::BG_ITABLE_UNUSED_HI,
                (self.itable_unused >> 16) as u16,
            );
            put_u32(
                buf,
                off::BG_EXCLUDE_BITMAP_HI,
                (self.exclude_bitmap >> 32) as u32,
            );
            if desc_size >= csum::BG_BLOCK_BITMAP_CSUM_HI_LOCATION {
                put_u16(
                    buf,
                    off::BG_BLOCK_BITMAP_CSUM_HI,
                    (self.block_bitmap_csum >> 16) as u16,
                );
            }
            if desc_size >= csum::BG_BLOCK_BITMAP_CSUM_HI_LOCATION + 2 {
                put_u16(
                    buf,
                    off::BG_INODE_BITMAP_CSUM_HI,
                    (self.inode_bitmap_csum >> 16) as u16,
                );
            }
        }
    }

    /// Encode and stamp `bg_checksum` for `group`.
    pub fn encode_with_csum(
        &self,
        buf: &mut [u8],
        desc_size: usize,
        scheme: GroupDescCsum,
        seed: u32,
        uuid: &[u8; 16],
        group: u32,
    ) {
        self.encode_into(buf, desc_size);
        let csum = csum::group_desc_csum(scheme, seed, uuid, group, &buf[..desc_size]);
        put_u16(buf, off::BG_CHECKSUM, csum);
    }

    /// Whether this group's block bitmap has never been written.
    pub fn block_uninit(&self) -> bool {
        self.flags & bg_flags::BLOCK_UNINIT != 0
    }

    /// Whether this group's inode table and bitmap have never been written.
    pub fn inode_uninit(&self) -> bool {
        self.flags & bg_flags::INODE_UNINIT != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_at_both_sizes() {
        let gd = GroupDesc {
            block_bitmap: 0x1_0000_0021,
            inode_bitmap: 0x1_0000_0022,
            inode_table: 0x1_0000_0023,
            free_blocks_count: 0x1_2345,
            free_inodes_count: 0x2_3456,
            used_dirs_count: 3,
            flags: bg_flags::INODE_UNINIT,
            exclude_bitmap: 0,
            block_bitmap_csum: 0xdead_beef,
            inode_bitmap_csum: 0xfeed_face,
            itable_unused: 0x1_1111,
            checksum: 0,
        };

        let mut buf = [0u8; DESC_SIZE_64BIT];
        gd.encode_into(&mut buf, DESC_SIZE_64BIT);
        assert_eq!(GroupDesc::decode(&buf, DESC_SIZE_64BIT), gd);

        // At 32 bytes only the low halves survive, which is exactly what a
        // non-64bit filesystem stores.
        let mut small = [0u8; MIN_DESC_SIZE];
        gd.encode_into(&mut small, MIN_DESC_SIZE);
        let back = GroupDesc::decode(&small, MIN_DESC_SIZE);
        assert_eq!(back.block_bitmap, 0x21);
        assert_eq!(back.free_blocks_count, 0x2345);
        assert_eq!(back.block_bitmap_csum, 0xbeef);
    }

    #[test]
    fn checksum_is_stamped_into_the_field() {
        let gd = GroupDesc {
            block_bitmap: 10,
            inode_bitmap: 11,
            inode_table: 12,
            ..Default::default()
        };
        let uuid = *b"0123456789abcdef";
        let seed = csum::seed_from_uuid(&uuid);
        let mut buf = [0u8; DESC_SIZE_64BIT];
        gd.encode_with_csum(
            &mut buf,
            DESC_SIZE_64BIT,
            GroupDescCsum::Crc32c,
            seed,
            &uuid,
            0,
        );
        let stamped = get_u16(&buf, off::BG_CHECKSUM);
        assert_ne!(stamped, 0);

        // Re-deriving from the encoded bytes must reproduce the same value.
        let recomputed = csum::group_desc_csum(GroupDescCsum::Crc32c, seed, &uuid, 0, &buf);
        assert_eq!(stamped, recomputed);
    }
}
