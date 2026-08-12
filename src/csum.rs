//! Metadata checksums.
//!
//! Mirrors `lib/ext2fs/csum.c`. Two schemes exist and they are mutually
//! exclusive:
//!
//! - `metadata_csum` (crc32c) covers superblock, group descriptors, bitmaps,
//!   inodes, extent blocks, directory blocks and the journal.
//! - `uninit_bg` / `GDT_CSUM` (crc16) covers only group descriptors, and is the
//!   older scheme. Setting both is invalid.
//!
//! Every crc32c here is seeded with the filesystem's checksum seed — either
//! `s_checksum_seed` when `metadata_csum_seed` is set, or `crc32c(~0, uuid)`.
//! That indirection is what allows a filesystem's UUID to be restamped without
//! rewriting every checksum on it.

/// CRC-16 as e2fsprogs computes it: reflected, polynomial 0x8005 (0xA001
/// reflected), no final xor. `lib/ext2fs/crc16.c`.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ CRC16_TABLE[((crc ^ b as u16) & 0xff) as usize];
    }
    crc
}

/// crc32c, seeded. Equivalent to `ext2fs_crc32c_le`.
#[inline]
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    crc32c::crc32c_append(seed, data)
}

/// Offset of `bg_checksum` within a group descriptor.
pub const BG_CHECKSUM_OFFSET: usize = 0x1e;

/// `EXT4_BG_BLOCK_BITMAP_CSUM_HI_LOCATION` — a descriptor at least this large
/// carries the high half of the bitmap checksums.
pub const BG_BLOCK_BITMAP_CSUM_HI_LOCATION: usize = 0x3a;

/// `EXT4_INODE_CSUM_HI_EXTRA_END` — the `i_extra_isize` needed before an inode
/// carries the high half of its checksum.
pub const INODE_CSUM_HI_EXTRA_END: u16 = 4;

/// How a filesystem checksums its group descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDescCsum {
    /// No group descriptor checksums.
    None,
    /// Legacy crc16 (`uninit_bg`).
    Crc16,
    /// crc32c truncated to 16 bits (`metadata_csum`).
    Crc32c,
}

/// Group descriptor checksum.
///
/// `desc` is the descriptor exactly as it appears on disk, `desc_size` bytes
/// long. The `bg_checksum` field is treated as zero for the computation
/// regardless of what it currently holds, so a caller may pass a descriptor
/// with a stale checksum in place.
pub fn group_desc_csum(
    scheme: GroupDescCsum,
    seed: u32,
    uuid: &[u8; 16],
    group: u32,
    desc: &[u8],
) -> u16 {
    let size = desc.len();
    match scheme {
        GroupDescCsum::None => 0,
        GroupDescCsum::Crc32c => {
            // crc32c(seed, group) then the whole descriptor with the checksum
            // field zeroed.
            let mut zeroed = desc.to_vec();
            if size > BG_CHECKSUM_OFFSET + 1 {
                zeroed[BG_CHECKSUM_OFFSET] = 0;
                zeroed[BG_CHECKSUM_OFFSET + 1] = 0;
            }
            let mut crc = crc32c(seed, &group.to_le_bytes());
            crc = crc32c(crc, &zeroed);
            (crc & 0xffff) as u16
        }
        GroupDescCsum::Crc16 => {
            // The legacy scheme skips the checksum field rather than zeroing
            // it, and is seeded from the UUID directly rather than from the
            // filesystem checksum seed.
            let mut crc = crc16(!0, uuid);
            crc = crc16(crc, &group.to_le_bytes());
            crc = crc16(crc, &desc[..BG_CHECKSUM_OFFSET.min(size)]);
            let after = BG_CHECKSUM_OFFSET + 2;
            if after < size {
                crc = crc16(crc, &desc[after..size]);
            }
            crc
        }
    }
}

/// Bitmap checksum — the same computation for block and inode bitmaps.
///
/// Returns the full 32 bits; the caller stores the low half always and the high
/// half only when the descriptor is large enough.
pub fn bitmap_csum(seed: u32, bitmap: &[u8]) -> u32 {
    crc32c(seed, bitmap)
}

/// Inode checksum.
///
/// `inode` is the full inode as it appears on disk (`inode_size` bytes) with
/// its checksum fields already zeroed by the caller. `generation` is the
/// inode's `i_generation`.
pub fn inode_csum(seed: u32, inum: u32, generation: u32, inode: &[u8]) -> u32 {
    let mut crc = crc32c(seed, &inum.to_le_bytes());
    crc = crc32c(crc, &generation.to_le_bytes());
    crc32c(crc, inode)
}

/// Extent block checksum — `crc32c(seed, inum, generation, block[..len-4])`,
/// where the trailing four bytes are the `ext3_extent_tail` itself.
pub fn extent_block_csum(seed: u32, inum: u32, generation: u32, block: &[u8]) -> u32 {
    let mut crc = crc32c(seed, &inum.to_le_bytes());
    crc = crc32c(crc, &generation.to_le_bytes());
    crc32c(crc, block)
}

/// Directory leaf block checksum, stored in the fake `ext2_dir_entry_tail` at
/// the end of the block.
pub fn dirent_csum(seed: u32, inum: u32, generation: u32, block: &[u8]) -> u32 {
    let mut crc = crc32c(seed, &inum.to_le_bytes());
    crc = crc32c(crc, &generation.to_le_bytes());
    crc32c(crc, block)
}

/// The filesystem checksum seed for a given UUID: `crc32c(~0, uuid)`.
///
/// With `metadata_csum_seed` set the superblock stores this explicitly instead,
/// and it stops tracking the UUID.
pub fn seed_from_uuid(uuid: &[u8; 16]) -> u32 {
    crc32c(!0, uuid)
}

/// CRC-16 table, poly 0x8005 reflected (0xA001). `lib/ext2fs/crc16.c`.
const CRC16_TABLE: [u16; 256] = build_crc16_table();

const fn build_crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u16;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_table_matches_e2fsprogs() {
        // Entries taken from the literal table in lib/ext2fs/crc16.c, which is
        // printed eight to a row.
        assert_eq!(CRC16_TABLE[0], 0x0000);
        assert_eq!(CRC16_TABLE[1], 0xC0C1);
        assert_eq!(CRC16_TABLE[2], 0xC181);
        assert_eq!(CRC16_TABLE[3], 0x0140);
        assert_eq!(CRC16_TABLE[8], 0xC601);
        assert_eq!(CRC16_TABLE[16], 0xCC01);
        assert_eq!(CRC16_TABLE[32], 0xD801);
        assert_eq!(CRC16_TABLE[64], 0xF001);
        assert_eq!(CRC16_TABLE[128], 0xA001);
        assert_eq!(CRC16_TABLE[255], 0x4040);
    }

    #[test]
    fn crc16_of_known_input() {
        // CRC-16/ARC of "123456789" with init 0 is 0xBB3D — the standard check
        // value for this polynomial and reflection.
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    #[test]
    fn seed_tracks_the_uuid() {
        let a = seed_from_uuid(b"0123456789abcdef");
        let b = seed_from_uuid(b"fedcba9876543210");
        assert_ne!(a, b);
    }

    #[test]
    fn group_desc_csum_ignores_the_stale_checksum_field() {
        let uuid = *b"0123456789abcdef";
        let seed = seed_from_uuid(&uuid);
        let mut desc = vec![0u8; 64];
        desc[0..4].copy_from_slice(&100u32.to_le_bytes());

        let clean = group_desc_csum(GroupDescCsum::Crc32c, seed, &uuid, 3, &desc);
        // Scribble a stale checksum into the field; the result must not move.
        desc[BG_CHECKSUM_OFFSET] = 0xaa;
        desc[BG_CHECKSUM_OFFSET + 1] = 0xbb;
        let stale = group_desc_csum(GroupDescCsum::Crc32c, seed, &uuid, 3, &desc);
        assert_eq!(clean, stale);
    }

    #[test]
    fn group_desc_csum_depends_on_group_number() {
        let uuid = *b"0123456789abcdef";
        let seed = seed_from_uuid(&uuid);
        let desc = vec![0u8; 64];
        assert_ne!(
            group_desc_csum(GroupDescCsum::Crc32c, seed, &uuid, 0, &desc),
            group_desc_csum(GroupDescCsum::Crc32c, seed, &uuid, 1, &desc),
        );
        assert_ne!(
            group_desc_csum(GroupDescCsum::Crc16, seed, &uuid, 0, &desc),
            group_desc_csum(GroupDescCsum::Crc16, seed, &uuid, 1, &desc),
        );
    }

    #[test]
    fn no_scheme_means_zero() {
        let uuid = [0u8; 16];
        assert_eq!(
            group_desc_csum(GroupDescCsum::None, 0, &uuid, 7, &[0u8; 32]),
            0
        );
    }
}
