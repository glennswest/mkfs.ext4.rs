//! The JBD2 journal.
//!
//! Mirrors `lib/ext2fs/mkjournal.c` and the on-disk superblock in
//! `lib/ext2fs/jfs_user.h`. Creating a journal at format time is only writing
//! its superblock and reserving its blocks: the journal is empty, so there is
//! nothing to replay.
//!
//! A journal is not always wanted. RouterOS cannot replay one, so a journal
//! that ever goes dirty there leaves the filesystem read-only permanently —
//! hence [`crate::params::Params::no_journal`].

#[cfg(not(feature = "std"))]
use alloc::{vec::Vec};

use crate::bytes::*;
use crate::csum;

/// `JBD2_MAGIC_NUMBER`
pub const JBD2_MAGIC: u32 = 0xc03b_3998;

/// `JBD2_SUPERBLOCK_V2`
pub const JBD2_SUPERBLOCK_V2: u32 = 4;

/// On-disk length of the journal superblock.
pub const JOURNAL_SB_LEN: usize = 1024;

/// JBD2 incompatible features.
pub mod jbd2_incompat {
    /// `JBD2_FEATURE_INCOMPAT_REVOKE`
    pub const REVOKE: u32 = 0x0000_0001;
    /// `JBD2_FEATURE_INCOMPAT_64BIT`
    pub const SIXTY_FOUR_BIT: u32 = 0x0000_0002;
    /// `JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT`
    pub const ASYNC_COMMIT: u32 = 0x0000_0004;
    /// `JBD2_FEATURE_INCOMPAT_CSUM_V2`
    pub const CSUM_V2: u32 = 0x0000_0008;
    /// `JBD2_FEATURE_INCOMPAT_CSUM_V3`
    pub const CSUM_V3: u32 = 0x0000_0010;
    /// `JBD2_FEATURE_INCOMPAT_FAST_COMMIT`
    pub const FAST_COMMIT: u32 = 0x0000_0020;
}

/// Field offsets within the journal superblock.
#[allow(missing_docs)]
pub mod off {
    pub const H_MAGIC: usize = 0x00;
    pub const H_BLOCKTYPE: usize = 0x04;
    pub const H_SEQUENCE: usize = 0x08;
    pub const S_BLOCKSIZE: usize = 0x0c;
    pub const S_MAXLEN: usize = 0x10;
    pub const S_FIRST: usize = 0x14;
    pub const S_SEQUENCE: usize = 0x18;
    pub const S_START: usize = 0x1c;
    pub const S_ERRNO: usize = 0x20;
    pub const S_FEATURE_COMPAT: usize = 0x24;
    pub const S_FEATURE_INCOMPAT: usize = 0x28;
    pub const S_FEATURE_RO_COMPAT: usize = 0x2c;
    pub const S_UUID: usize = 0x30;
    pub const S_NR_USERS: usize = 0x40;
    pub const S_DYNSUPER: usize = 0x44;
    pub const S_MAX_TRANSACTION: usize = 0x48;
    pub const S_MAX_TRANS_DATA: usize = 0x4c;
    pub const S_CHECKSUM_TYPE: usize = 0x50;
    pub const S_NUM_FC_BLKS: usize = 0x54;
    pub const S_HEAD: usize = 0x58;
    pub const S_CHECKSUM: usize = 0xfc;
    pub const S_USERS: usize = 0x100;
}

/// `ext2fs_default_journal_size()` — journal blocks for a filesystem of
/// `num_blocks` blocks, or `None` when it is too small to carry one.
pub fn default_journal_blocks(num_blocks: u64) -> Option<u32> {
    Some(match num_blocks {
        n if n < 2048 => return None,
        n if n < 32768 => 1024,             // < 128 MB -> 4 MB
        n if n < 256 * 1024 => 4096,        // < 1 GB -> 16 MB
        n if n < 512 * 1024 => 8192,        // < 2 GB -> 32 MB
        n if n < 4096 * 1024 => 16384,      // < 16 GB -> 64 MB
        n if n < 8192 * 1024 => 32768,      // < 32 GB -> 128 MB
        n if n < 16384 * 1024 => 65536,     // < 64 GB -> 256 MB
        n if n < 32768 * 1024 => 131_072,   // < 128 GB -> 512 MB
        _ => 262_144,                       // 1 GB
    })
}

/// The journal superblock for a freshly created, empty journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalSuperblock {
    /// Block size of the filesystem carrying the journal.
    pub blocksize: u32,
    /// Total blocks in the journal, including this superblock.
    pub maxlen: u32,
    /// First block available for log records — 1, just past this superblock.
    pub first: u32,
    /// Sequence number of the first commit that will be written.
    pub sequence: u32,
    /// Block where the log starts. Zero on an empty journal.
    pub start: u32,
    /// `s_feature_incompat`
    pub feature_incompat: u32,
    /// UUID of the journal, matching the filesystem's.
    pub uuid: [u8; 16],
    /// Filesystems sharing this journal. One, unless it is external.
    pub nr_users: u32,
    /// Checksum algorithm, `EXT2_CRC32C_CHKSUM` when checksums are on.
    pub checksum_type: u8,
}

impl JournalSuperblock {
    /// The journal superblock `mke2fs` writes for a fresh filesystem.
    ///
    /// **No features, and no checksum** — even on a `metadata_csum`
    /// filesystem. `ext2fs_create_journal_superblock2()` sets the magic,
    /// geometry, sequence and UUID and nothing else, and `dumpe2fs` on a
    /// stock ext4 image agrees: "Journal features: (none)". The kernel turns
    /// on `csum_v3` the first time it actually uses the journal.
    ///
    /// Setting `csum_v3` here looks like the consistent thing to do and is
    /// wrong: e2fsck then validates a checksum against a journal the rest of
    /// the format never checksums, and rejects the filesystem outright with
    /// "Journal superblock is corrupt".
    pub fn new(blocksize: u32, blocks: u32, uuid: [u8; 16]) -> Self {
        Self {
            blocksize,
            maxlen: blocks,
            first: 1,
            sequence: 1,
            start: 0,
            feature_incompat: 0,
            uuid,
            nr_users: 1,
            checksum_type: 0,
        }
    }

    /// Encode into a full block, checksum included.
    ///
    /// The buffer is the journal's first block; anything past the 1024-byte
    /// superblock is left zero.
    pub fn encode(&self, block_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; block_size];

        put_u32_be(&mut buf, off::H_MAGIC, JBD2_MAGIC);
        put_u32_be(&mut buf, off::H_BLOCKTYPE, JBD2_SUPERBLOCK_V2);
        put_u32_be(&mut buf, off::H_SEQUENCE, 0);
        put_u32_be(&mut buf, off::S_BLOCKSIZE, self.blocksize);
        put_u32_be(&mut buf, off::S_MAXLEN, self.maxlen);
        put_u32_be(&mut buf, off::S_FIRST, self.first);
        put_u32_be(&mut buf, off::S_SEQUENCE, self.sequence);
        put_u32_be(&mut buf, off::S_START, self.start);
        put_u32_be(&mut buf, off::S_ERRNO, 0);
        put_u32_be(&mut buf, off::S_FEATURE_COMPAT, 0);
        put_u32_be(&mut buf, off::S_FEATURE_INCOMPAT, self.feature_incompat);
        put_u32_be(&mut buf, off::S_FEATURE_RO_COMPAT, 0);
        put_bytes(&mut buf, off::S_UUID, 16, &self.uuid);
        put_u32_be(&mut buf, off::S_NR_USERS, self.nr_users);
        put_u32_be(&mut buf, off::S_DYNSUPER, 0);
        put_u32_be(&mut buf, off::S_MAX_TRANSACTION, 0);
        put_u32_be(&mut buf, off::S_MAX_TRANS_DATA, 0);
        put_u8(&mut buf, off::S_CHECKSUM_TYPE, self.checksum_type);
        put_u32_be(&mut buf, off::S_NUM_FC_BLKS, 0);
        put_u32_be(&mut buf, off::S_HEAD, 0);

        if self.feature_incompat & jbd2_incompat::CSUM_V3 != 0 {
            // Computed over the whole 1024-byte superblock with the checksum
            // field zeroed, and stored big-endian like the rest of JBD2.
            let crc = csum::crc32c(!0, &buf[..JOURNAL_SB_LEN]);
            put_u32_be(&mut buf, off::S_CHECKSUM, crc);
        }

        buf
    }
}

/// Write a big-endian `u32`.
///
/// JBD2 is the one part of ext4 stored big-endian — it predates the filesystem
/// and kept the journalling layer's own convention.
fn put_u32_be(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// Read a big-endian `u32`.
pub fn get_u32_be(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_sizes_match_e2fsprogs() {
        assert_eq!(default_journal_blocks(1024), None);
        assert_eq!(default_journal_blocks(2048), Some(1024));
        assert_eq!(default_journal_blocks(32767), Some(1024));
        assert_eq!(default_journal_blocks(32768), Some(4096));
        assert_eq!(default_journal_blocks(256 * 1024), Some(8192));
        assert_eq!(default_journal_blocks(512 * 1024), Some(16384));
        assert_eq!(default_journal_blocks(4096 * 1024), Some(32768));
        assert_eq!(default_journal_blocks(8192 * 1024), Some(65536));
        assert_eq!(default_journal_blocks(16384 * 1024), Some(131_072));
        assert_eq!(default_journal_blocks(32768 * 1024), Some(262_144));
    }

    /// A 64 MiB filesystem of 1 KiB blocks is 65536 blocks, which lands in the
    /// 4 MiB journal band — 4096 blocks.
    #[test]
    fn a_64mib_filesystem_gets_a_4096_block_journal() {
        assert_eq!(default_journal_blocks(65536), Some(4096));
    }

    #[test]
    fn superblock_is_big_endian_with_the_jbd2_magic() {
        let sb = JournalSuperblock::new(1024, 4096, [0xab; 16]);
        let buf = sb.encode(1024);

        assert_eq!(get_u32_be(&buf, off::H_MAGIC), JBD2_MAGIC);
        assert_eq!(&buf[0..4], &[0xc0, 0x3b, 0x39, 0x98]);
        assert_eq!(get_u32_be(&buf, off::H_BLOCKTYPE), JBD2_SUPERBLOCK_V2);
        assert_eq!(get_u32_be(&buf, off::S_BLOCKSIZE), 1024);
        assert_eq!(get_u32_be(&buf, off::S_MAXLEN), 4096);
        assert_eq!(get_u32_be(&buf, off::S_FIRST), 1);
        assert_eq!(get_u32_be(&buf, off::S_SEQUENCE), 1);
        assert_eq!(get_u32_be(&buf, off::S_START), 0);
        assert_eq!(get_u32_be(&buf, off::S_NR_USERS), 1);
        assert_eq!(&buf[off::S_UUID..off::S_UUID + 16], &[0xab; 16]);
    }

    /// A fresh journal carries no features and no checksum, matching what
    /// `dumpe2fs` reports for a stock ext4 image: "Journal features: (none)".
    #[test]
    fn a_fresh_journal_has_no_features_and_no_checksum() {
        let sb = JournalSuperblock::new(4096, 1024, [0; 16]);
        let buf = sb.encode(4096);
        assert_eq!(get_u32_be(&buf, off::S_CHECKSUM), 0);
        assert_eq!(get_u32_be(&buf, off::S_FEATURE_INCOMPAT), 0);
        assert_eq!(get_u32_be(&buf, off::S_FEATURE_COMPAT), 0);
        assert_eq!(get_u32_be(&buf, off::S_FEATURE_RO_COMPAT), 0);
        assert_eq!(buf[off::S_CHECKSUM_TYPE], 0);
    }

    #[test]
    fn the_superblock_occupies_one_block_and_the_rest_is_zero() {
        let sb = JournalSuperblock::new(4096, 1024, [1; 16]);
        let buf = sb.encode(4096);
        assert_eq!(buf.len(), 4096);
        assert!(buf[JOURNAL_SB_LEN..].iter().all(|&b| b == 0));
    }
}
