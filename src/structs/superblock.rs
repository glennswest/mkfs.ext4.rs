//! The ext2/3/4 superblock.
//!
//! Mirrors `struct ext2_super_block` from `lib/ext2fs/ext2_fs.h`. Field offsets
//! are the offsets in that struct and are asserted in the tests at the bottom
//! of this file; they are not to be inferred from field order here.

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::features::{CompatFeatures, IncompatFeatures, RoCompatFeatures};

/// Byte offset of the primary superblock on any ext2/3/4 filesystem.
///
/// The first 1024 bytes are reserved for a boot sector, so the superblock
/// always starts here — including on 1 KiB-block filesystems, where it means
/// the superblock is block 1 rather than block 0.
pub const SUPERBLOCK_OFFSET: u64 = 1024;

/// On-disk size of the superblock.
pub const SUPERBLOCK_LEN: usize = 1024;

/// `EXT2_SUPER_MAGIC`
pub const EXT2_SUPER_MAGIC: u16 = 0xef53;

/// `EXT2_GOOD_OLD_INODE_SIZE` — the inode size of a revision-0 filesystem.
pub const GOOD_OLD_INODE_SIZE: u16 = 128;

/// `EXT2_GOOD_OLD_FIRST_INO` — first non-reserved inode on a revision-0 fs.
pub const GOOD_OLD_FIRST_INO: u32 = 11;

/// `EXT2_DYNAMIC_REV`
pub const DYNAMIC_REV: u32 = 1;

/// `EXT2_MIN_DESC_SIZE`
pub const MIN_DESC_SIZE: u16 = 32;

/// `EXT2_MIN_DESC_SIZE_64BIT`
pub const MIN_DESC_SIZE_64BIT: u16 = 64;

/// Reserved inode numbers (`ext2_fs.h`).
pub mod ino {
    /// Bad blocks inode.
    pub const BAD: u32 = 1;
    /// Root directory.
    pub const ROOT: u32 = 2;
    /// User quota.
    pub const USR_QUOTA: u32 = 3;
    /// Group quota.
    pub const GRP_QUOTA: u32 = 4;
    /// Boot loader.
    pub const BOOT_LOADER: u32 = 5;
    /// Undelete directory.
    pub const UNDEL_DIR: u32 = 6;
    /// Reserved group descriptors (resize) inode.
    pub const RESIZE: u32 = 7;
    /// Journal inode.
    pub const JOURNAL: u32 = 8;
    /// Snapshot exclude inode.
    pub const EXCLUDE: u32 = 9;
    /// Non-upstream replica inode.
    pub const REPLICA: u32 = 10;
}

/// Filesystem state bits (`s_state`).
pub mod state {
    /// `EXT2_VALID_FS` — unmounted cleanly.
    pub const VALID_FS: u16 = 0x0001;
    /// `EXT2_ERROR_FS` — errors detected.
    pub const ERROR_FS: u16 = 0x0002;
    /// `EXT3_ORPHAN_FS` — orphans being recovered.
    pub const ORPHAN_FS: u16 = 0x0004;
    /// `EXT4_FC_REPLAY` — fast commit replay ongoing.
    pub const FC_REPLAY: u16 = 0x0020;
}

/// Behaviour on error (`s_errors`).
pub mod errors {
    /// Continue execution.
    pub const CONTINUE: u16 = 1;
    /// Remount read-only.
    pub const REMOUNT_RO: u16 = 2;
    /// Panic.
    pub const PANIC: u16 = 3;
}

/// Default mount option bits (`s_default_mount_opts`).
pub mod defm {
    /// `EXT2_DEFM_DEBUG`
    pub const DEBUG: u32 = 0x0001;
    /// `EXT2_DEFM_BSDGROUPS`
    pub const BSDGROUPS: u32 = 0x0002;
    /// `EXT2_DEFM_XATTR_USER`
    pub const XATTR_USER: u32 = 0x0004;
    /// `EXT2_DEFM_ACL`
    pub const ACL: u32 = 0x0008;
    /// `EXT2_DEFM_UID16`
    pub const UID16: u32 = 0x0010;
    /// `EXT3_DEFM_JMODE_DATA`
    pub const JMODE_DATA: u32 = 0x0020;
    /// `EXT3_DEFM_JMODE_ORDERED`
    pub const JMODE_ORDERED: u32 = 0x0040;
    /// `EXT3_DEFM_JMODE_WBACK`
    pub const JMODE_WBACK: u32 = 0x0060;
    /// `EXT4_DEFM_NOBARRIER`
    pub const NOBARRIER: u32 = 0x0100;
    /// `EXT4_DEFM_BLOCK_VALIDITY`
    pub const BLOCK_VALIDITY: u32 = 0x0200;
    /// `EXT4_DEFM_DISCARD`
    pub const DISCARD: u32 = 0x0400;
    /// `EXT4_DEFM_NODELALLOC`
    pub const NODELALLOC: u32 = 0x0800;
}

/// Misc flags (`s_flags`).
pub mod flags {
    /// `EXT2_FLAGS_SIGNED_HASH`
    pub const SIGNED_HASH: u32 = 0x0001;
    /// `EXT2_FLAGS_UNSIGNED_HASH`
    pub const UNSIGNED_HASH: u32 = 0x0002;
    /// `EXT2_FLAGS_TEST_FILESYS`
    pub const TEST_FILESYS: u32 = 0x0004;
}

/// Directory hash algorithms (`s_def_hash_version`).
pub mod hash {
    /// `EXT2_HASH_LEGACY`
    pub const LEGACY: u8 = 0;
    /// `EXT2_HASH_HALF_MD4`
    pub const HALF_MD4: u8 = 1;
    /// `EXT2_HASH_TEA`
    pub const TEA: u8 = 2;
    /// `EXT2_HASH_SIPHASH`
    pub const SIPHASH: u8 = 6;
}

/// `EXT2_CRC32C_CHKSUM` — the only metadata checksum algorithm defined.
pub const CRC32C_CHKSUM: u8 = 1;

/// `EXT3_JNL_BACKUP_BLOCKS` — `s_jnl_blocks` holds the journal inode's block
/// map, which is what `dumpe2fs` reports as "Journal backup: inode blocks".
pub const JNL_BACKUP_BLOCKS: u8 = 1;

/// Field offsets within the superblock, named as in `ext2_fs.h`.
#[allow(missing_docs)]
pub mod off {
    pub const S_INODES_COUNT: usize = 0x000;
    pub const S_BLOCKS_COUNT: usize = 0x004;
    pub const S_R_BLOCKS_COUNT: usize = 0x008;
    pub const S_FREE_BLOCKS_COUNT: usize = 0x00c;
    pub const S_FREE_INODES_COUNT: usize = 0x010;
    pub const S_FIRST_DATA_BLOCK: usize = 0x014;
    pub const S_LOG_BLOCK_SIZE: usize = 0x018;
    pub const S_LOG_CLUSTER_SIZE: usize = 0x01c;
    pub const S_BLOCKS_PER_GROUP: usize = 0x020;
    pub const S_CLUSTERS_PER_GROUP: usize = 0x024;
    pub const S_INODES_PER_GROUP: usize = 0x028;
    pub const S_MTIME: usize = 0x02c;
    pub const S_WTIME: usize = 0x030;
    pub const S_MNT_COUNT: usize = 0x034;
    pub const S_MAX_MNT_COUNT: usize = 0x036;
    pub const S_MAGIC: usize = 0x038;
    pub const S_STATE: usize = 0x03a;
    pub const S_ERRORS: usize = 0x03c;
    pub const S_MINOR_REV_LEVEL: usize = 0x03e;
    pub const S_LASTCHECK: usize = 0x040;
    pub const S_CHECKINTERVAL: usize = 0x044;
    pub const S_CREATOR_OS: usize = 0x048;
    pub const S_REV_LEVEL: usize = 0x04c;
    pub const S_DEF_RESUID: usize = 0x050;
    pub const S_DEF_RESGID: usize = 0x052;
    pub const S_FIRST_INO: usize = 0x054;
    pub const S_INODE_SIZE: usize = 0x058;
    pub const S_BLOCK_GROUP_NR: usize = 0x05a;
    pub const S_FEATURE_COMPAT: usize = 0x05c;
    pub const S_FEATURE_INCOMPAT: usize = 0x060;
    pub const S_FEATURE_RO_COMPAT: usize = 0x064;
    pub const S_UUID: usize = 0x068;
    pub const S_VOLUME_NAME: usize = 0x078;
    pub const S_LAST_MOUNTED: usize = 0x088;
    pub const S_ALGORITHM_USAGE_BITMAP: usize = 0x0c8;
    pub const S_PREALLOC_BLOCKS: usize = 0x0cc;
    pub const S_PREALLOC_DIR_BLOCKS: usize = 0x0cd;
    pub const S_RESERVED_GDT_BLOCKS: usize = 0x0ce;
    pub const S_JOURNAL_UUID: usize = 0x0d0;
    pub const S_JOURNAL_INUM: usize = 0x0e0;
    pub const S_JOURNAL_DEV: usize = 0x0e4;
    pub const S_LAST_ORPHAN: usize = 0x0e8;
    pub const S_HASH_SEED: usize = 0x0ec;
    pub const S_DEF_HASH_VERSION: usize = 0x0fc;
    pub const S_JNL_BACKUP_TYPE: usize = 0x0fd;
    pub const S_DESC_SIZE: usize = 0x0fe;
    pub const S_DEFAULT_MOUNT_OPTS: usize = 0x100;
    pub const S_FIRST_META_BG: usize = 0x104;
    pub const S_MKFS_TIME: usize = 0x108;
    pub const S_JNL_BLOCKS: usize = 0x10c;
    pub const S_BLOCKS_COUNT_HI: usize = 0x150;
    pub const S_R_BLOCKS_COUNT_HI: usize = 0x154;
    pub const S_FREE_BLOCKS_HI: usize = 0x158;
    pub const S_MIN_EXTRA_ISIZE: usize = 0x15c;
    pub const S_WANT_EXTRA_ISIZE: usize = 0x15e;
    pub const S_FLAGS: usize = 0x160;
    pub const S_RAID_STRIDE: usize = 0x164;
    pub const S_MMP_UPDATE_INTERVAL: usize = 0x166;
    pub const S_MMP_BLOCK: usize = 0x168;
    pub const S_RAID_STRIPE_WIDTH: usize = 0x170;
    pub const S_LOG_GROUPS_PER_FLEX: usize = 0x174;
    pub const S_CHECKSUM_TYPE: usize = 0x175;
    pub const S_ENCRYPTION_LEVEL: usize = 0x176;
    pub const S_RESERVED_PAD: usize = 0x177;
    pub const S_KBYTES_WRITTEN: usize = 0x178;
    pub const S_SNAPSHOT_INUM: usize = 0x180;
    pub const S_SNAPSHOT_ID: usize = 0x184;
    pub const S_SNAPSHOT_R_BLOCKS_COUNT: usize = 0x188;
    pub const S_SNAPSHOT_LIST: usize = 0x190;
    pub const S_ERROR_COUNT: usize = 0x194;
    pub const S_FIRST_ERROR_TIME: usize = 0x198;
    pub const S_FIRST_ERROR_INO: usize = 0x19c;
    pub const S_FIRST_ERROR_BLOCK: usize = 0x1a0;
    pub const S_FIRST_ERROR_FUNC: usize = 0x1a8;
    pub const S_FIRST_ERROR_LINE: usize = 0x1c8;
    pub const S_LAST_ERROR_TIME: usize = 0x1cc;
    pub const S_LAST_ERROR_INO: usize = 0x1d0;
    pub const S_LAST_ERROR_LINE: usize = 0x1d4;
    pub const S_LAST_ERROR_BLOCK: usize = 0x1d8;
    pub const S_LAST_ERROR_FUNC: usize = 0x1e0;
    pub const S_MOUNT_OPTS: usize = 0x200;
    pub const S_USR_QUOTA_INUM: usize = 0x240;
    pub const S_GRP_QUOTA_INUM: usize = 0x244;
    pub const S_OVERHEAD_CLUSTERS: usize = 0x248;
    pub const S_BACKUP_BGS: usize = 0x24c;
    pub const S_ENCRYPT_ALGOS: usize = 0x254;
    pub const S_ENCRYPT_PW_SALT: usize = 0x258;
    pub const S_LPF_INO: usize = 0x268;
    pub const S_PRJ_QUOTA_INUM: usize = 0x26c;
    pub const S_CHECKSUM_SEED: usize = 0x270;
    pub const S_WTIME_HI: usize = 0x274;
    pub const S_MTIME_HI: usize = 0x275;
    pub const S_MKFS_TIME_HI: usize = 0x276;
    pub const S_LASTCHECK_HI: usize = 0x277;
    pub const S_FIRST_ERROR_TIME_HI: usize = 0x278;
    pub const S_LAST_ERROR_TIME_HI: usize = 0x279;
    pub const S_FIRST_ERROR_ERRCODE: usize = 0x27a;
    pub const S_LAST_ERROR_ERRCODE: usize = 0x27b;
    pub const S_ENCODING: usize = 0x27c;
    pub const S_ENCODING_FLAGS: usize = 0x27e;
    pub const S_ORPHAN_FILE_INUM: usize = 0x280;
    pub const S_CHECKSUM: usize = 0x3fc;
}

/// A decoded superblock.
///
/// Every field of `struct ext2_super_block` is represented, so a decode/encode
/// round trip is byte-identical — which is what lets the compare module diff a
/// reference filesystem against ours without losing anything it does not
/// understand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    // Sizing.
    /// `s_inodes_count`
    pub inodes_count: u32,
    /// `s_blocks_count` combined with `s_blocks_count_hi`.
    pub blocks_count: u64,
    /// `s_r_blocks_count` combined with `s_r_blocks_count_hi`.
    pub r_blocks_count: u64,
    /// `s_free_blocks_count` combined with `s_free_blocks_hi`.
    pub free_blocks_count: u64,
    /// `s_free_inodes_count`
    pub free_inodes_count: u32,
    /// `s_first_data_block` — 1 for 1 KiB blocks, 0 otherwise.
    pub first_data_block: u32,
    /// `s_log_block_size` — block size is `1024 << this`.
    pub log_block_size: u32,
    /// `s_log_cluster_size`
    pub log_cluster_size: u32,
    /// `s_blocks_per_group`
    pub blocks_per_group: u32,
    /// `s_clusters_per_group`
    pub clusters_per_group: u32,
    /// `s_inodes_per_group`
    pub inodes_per_group: u32,

    // Times and mount bookkeeping.
    /// `s_mtime`
    pub mtime: u64,
    /// `s_wtime`
    pub wtime: u64,
    /// `s_mnt_count`
    pub mnt_count: u16,
    /// `s_max_mnt_count`
    pub max_mnt_count: i16,
    /// `s_state`
    pub state: u16,
    /// `s_errors`
    pub errors: u16,
    /// `s_minor_rev_level`
    pub minor_rev_level: u16,
    /// `s_lastcheck`
    pub lastcheck: u64,
    /// `s_checkinterval`
    pub checkinterval: u32,
    /// `s_creator_os`
    pub creator_os: u32,
    /// `s_rev_level`
    pub rev_level: u32,
    /// `s_def_resuid`
    pub def_resuid: u16,
    /// `s_def_resgid`
    pub def_resgid: u16,

    // Dynamic-rev fields.
    /// `s_first_ino`
    pub first_ino: u32,
    /// `s_inode_size`
    pub inode_size: u16,
    /// `s_block_group_nr` — which group this copy of the superblock lives in.
    pub block_group_nr: u16,
    /// `s_feature_compat`
    pub feature_compat: CompatFeatures,
    /// `s_feature_incompat`
    pub feature_incompat: IncompatFeatures,
    /// `s_feature_ro_compat`
    pub feature_ro_compat: RoCompatFeatures,
    /// `s_uuid`
    pub uuid: [u8; 16],
    /// `s_volume_name`
    pub volume_name: [u8; 16],
    /// `s_last_mounted`
    pub last_mounted: [u8; 64],
    /// `s_algorithm_usage_bitmap`
    pub algorithm_usage_bitmap: u32,

    // Performance hints.
    /// `s_prealloc_blocks`
    pub prealloc_blocks: u8,
    /// `s_prealloc_dir_blocks`
    pub prealloc_dir_blocks: u8,
    /// `s_reserved_gdt_blocks`
    pub reserved_gdt_blocks: u16,

    // Journal.
    /// `s_journal_uuid`
    pub journal_uuid: [u8; 16],
    /// `s_journal_inum`
    pub journal_inum: u32,
    /// `s_journal_dev`
    pub journal_dev: u32,
    /// `s_last_orphan`
    pub last_orphan: u32,
    /// `s_hash_seed`
    pub hash_seed: [u32; 4],
    /// `s_def_hash_version`
    pub def_hash_version: u8,
    /// `s_jnl_backup_type`
    pub jnl_backup_type: u8,
    /// `s_desc_size`
    pub desc_size: u16,
    /// `s_default_mount_opts`
    pub default_mount_opts: u32,
    /// `s_first_meta_bg`
    pub first_meta_bg: u32,
    /// `s_mkfs_time`
    pub mkfs_time: u64,
    /// `s_jnl_blocks` — backup of the journal inode's block map.
    pub jnl_blocks: [u32; 17],

    /// `s_min_extra_isize`
    pub min_extra_isize: u16,
    /// `s_want_extra_isize`
    pub want_extra_isize: u16,
    /// `s_flags`
    pub flags: u32,
    /// `s_raid_stride`
    pub raid_stride: u16,
    /// `s_mmp_update_interval`
    pub mmp_update_interval: u16,
    /// `s_mmp_block`
    pub mmp_block: u64,
    /// `s_raid_stripe_width`
    pub raid_stripe_width: u32,
    /// `s_log_groups_per_flex`
    pub log_groups_per_flex: u8,
    /// `s_checksum_type`
    pub checksum_type: u8,
    /// `s_encryption_level`
    pub encryption_level: u8,
    /// `s_kbytes_written`
    pub kbytes_written: u64,

    /// `s_snapshot_inum`
    pub snapshot_inum: u32,
    /// `s_snapshot_id`
    pub snapshot_id: u32,
    /// `s_snapshot_r_blocks_count`
    pub snapshot_r_blocks_count: u64,
    /// `s_snapshot_list`
    pub snapshot_list: u32,

    /// `s_error_count`
    pub error_count: u32,
    /// `s_first_error_time`
    pub first_error_time: u64,
    /// `s_first_error_ino`
    pub first_error_ino: u32,
    /// `s_first_error_block`
    pub first_error_block: u64,
    /// `s_first_error_func`
    pub first_error_func: [u8; 32],
    /// `s_first_error_line`
    pub first_error_line: u32,
    /// `s_last_error_time`
    pub last_error_time: u64,
    /// `s_last_error_ino`
    pub last_error_ino: u32,
    /// `s_last_error_line`
    pub last_error_line: u32,
    /// `s_last_error_block`
    pub last_error_block: u64,
    /// `s_last_error_func`
    pub last_error_func: [u8; 32],
    /// `s_first_error_errcode`
    pub first_error_errcode: u8,
    /// `s_last_error_errcode`
    pub last_error_errcode: u8,

    /// `s_mount_opts`
    pub mount_opts: [u8; 64],
    /// `s_usr_quota_inum`
    pub usr_quota_inum: u32,
    /// `s_grp_quota_inum`
    pub grp_quota_inum: u32,
    /// `s_overhead_clusters`
    pub overhead_clusters: u32,
    /// `s_backup_bgs`
    pub backup_bgs: [u32; 2],
    /// `s_encrypt_algos`
    pub encrypt_algos: [u8; 4],
    /// `s_encrypt_pw_salt`
    pub encrypt_pw_salt: [u8; 16],
    /// `s_lpf_ino` — the `lost+found` inode.
    pub lpf_ino: u32,
    /// `s_prj_quota_inum`
    pub prj_quota_inum: u32,
    /// `s_checksum_seed` — `crc32c(orig_uuid)` when `csum_seed` is set.
    pub checksum_seed: u32,
    /// `s_encoding`
    pub encoding: u16,
    /// `s_encoding_flags`
    pub encoding_flags: u16,
    /// `s_orphan_file_inum`
    pub orphan_file_inum: u32,
    /// `s_checksum` as read from disk. Recomputed on encode.
    pub checksum: u32,
}

impl Default for Superblock {
    fn default() -> Self {
        Self {
            inodes_count: 0,
            blocks_count: 0,
            r_blocks_count: 0,
            free_blocks_count: 0,
            free_inodes_count: 0,
            first_data_block: 0,
            log_block_size: 2,
            log_cluster_size: 2,
            blocks_per_group: 0,
            clusters_per_group: 0,
            inodes_per_group: 0,
            mtime: 0,
            wtime: 0,
            mnt_count: 0,
            max_mnt_count: -1,
            state: state::VALID_FS,
            errors: errors::CONTINUE,
            minor_rev_level: 0,
            lastcheck: 0,
            checkinterval: 0,
            creator_os: 0,
            rev_level: DYNAMIC_REV,
            def_resuid: 0,
            def_resgid: 0,
            first_ino: GOOD_OLD_FIRST_INO,
            inode_size: 256,
            block_group_nr: 0,
            feature_compat: CompatFeatures::empty(),
            feature_incompat: IncompatFeatures::empty(),
            feature_ro_compat: RoCompatFeatures::empty(),
            uuid: [0; 16],
            volume_name: [0; 16],
            last_mounted: [0; 64],
            algorithm_usage_bitmap: 0,
            prealloc_blocks: 0,
            prealloc_dir_blocks: 0,
            reserved_gdt_blocks: 0,
            journal_uuid: [0; 16],
            journal_inum: 0,
            journal_dev: 0,
            last_orphan: 0,
            hash_seed: [0; 4],
            def_hash_version: hash::HALF_MD4,
            jnl_backup_type: 0,
            desc_size: 0,
            default_mount_opts: 0,
            first_meta_bg: 0,
            mkfs_time: 0,
            jnl_blocks: [0; 17],
            min_extra_isize: 0,
            want_extra_isize: 0,
            flags: 0,
            raid_stride: 0,
            mmp_update_interval: 0,
            mmp_block: 0,
            raid_stripe_width: 0,
            log_groups_per_flex: 0,
            checksum_type: 0,
            encryption_level: 0,
            kbytes_written: 0,
            snapshot_inum: 0,
            snapshot_id: 0,
            snapshot_r_blocks_count: 0,
            snapshot_list: 0,
            error_count: 0,
            first_error_time: 0,
            first_error_ino: 0,
            first_error_block: 0,
            first_error_func: [0; 32],
            first_error_line: 0,
            last_error_time: 0,
            last_error_ino: 0,
            last_error_line: 0,
            last_error_block: 0,
            last_error_func: [0; 32],
            first_error_errcode: 0,
            last_error_errcode: 0,
            mount_opts: [0; 64],
            usr_quota_inum: 0,
            grp_quota_inum: 0,
            overhead_clusters: 0,
            backup_bgs: [0; 2],
            encrypt_algos: [0; 4],
            encrypt_pw_salt: [0; 16],
            lpf_ino: 0,
            prj_quota_inum: 0,
            checksum_seed: 0,
            encoding: 0,
            encoding_flags: 0,
            orphan_file_inum: 0,
            checksum: 0,
        }
    }
}

impl Superblock {
    /// Block size in bytes.
    pub fn block_size(&self) -> u32 {
        1024u32 << self.log_block_size
    }

    /// Cluster size in bytes (equals block size unless `bigalloc` is set).
    pub fn cluster_size(&self) -> u32 {
        1024u32 << self.log_cluster_size
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u32 {
        if self.blocks_per_group == 0 {
            return 0;
        }
        let data_blocks = self.blocks_count - self.first_data_block as u64;
        data_blocks.div_ceil(self.blocks_per_group as u64) as u32
    }

    /// Size of one group descriptor, honouring `64bit`.
    pub fn desc_size(&self) -> u16 {
        if self.feature_incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT) {
            self.desc_size.max(MIN_DESC_SIZE_64BIT)
        } else {
            MIN_DESC_SIZE
        }
    }

    /// Group descriptors that fit in one block.
    pub fn desc_per_block(&self) -> u32 {
        self.block_size() / self.desc_size() as u32
    }

    /// Blocks occupied by the group descriptor table.
    pub fn gdt_blocks(&self) -> u32 {
        self.group_count().div_ceil(self.desc_per_block())
    }

    /// Inodes that fit in one block.
    pub fn inodes_per_block(&self) -> u32 {
        self.block_size() / self.inode_size as u32
    }

    /// Blocks occupied by one group's inode table.
    pub fn itable_blocks_per_group(&self) -> u32 {
        self.inodes_per_group.div_ceil(self.inodes_per_block())
    }

    /// Whether metadata checksums are in use.
    pub fn has_metadata_csum(&self) -> bool {
        self.feature_ro_compat
            .contains(RoCompatFeatures::METADATA_CSUM)
    }

    /// The seed for metadata checksums.
    ///
    /// With `csum_seed` set the superblock carries an explicit seed, which lets
    /// the UUID change without invalidating every checksum on the filesystem.
    /// Otherwise the seed is `crc32c(~0, uuid)`.
    pub fn csum_seed(&self) -> u32 {
        if self
            .feature_incompat
            .contains(IncompatFeatures::CSUM_SEED)
        {
            self.checksum_seed
        } else {
            crate::csum::crc32c(!0, &self.uuid)
        }
    }

    /// The volume label.
    pub fn label(&self) -> String {
        field_to_string(&self.volume_name)
    }

    /// The filesystem UUID in its canonical hyphenated form.
    pub fn uuid_string(&self) -> String {
        uuid::Uuid::from_bytes(self.uuid).hyphenated().to_string()
    }

    /// Decode a superblock from a 1024-byte buffer.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < SUPERBLOCK_LEN {
            return Err(Error::corrupt(
                "superblock",
                format!("need {SUPERBLOCK_LEN} bytes, got {}", buf.len()),
            ));
        }

        let magic = get_u16(buf, off::S_MAGIC);
        if magic != EXT2_SUPER_MAGIC {
            return Err(Error::NotExtFilesystem { found: magic });
        }

        let rev_level = get_u32(buf, off::S_REV_LEVEL);
        // A revision-0 filesystem has no dynamic fields; the sizes are fixed.
        let (first_ino, inode_size) = if rev_level == 0 {
            (GOOD_OLD_FIRST_INO, GOOD_OLD_INODE_SIZE)
        } else {
            (
                get_u32(buf, off::S_FIRST_INO),
                get_u16(buf, off::S_INODE_SIZE),
            )
        };

        let mut hash_seed = [0u32; 4];
        for (i, seed) in hash_seed.iter_mut().enumerate() {
            *seed = get_u32(buf, off::S_HASH_SEED + i * 4);
        }
        let mut jnl_blocks = [0u32; 17];
        for (i, blk) in jnl_blocks.iter_mut().enumerate() {
            *blk = get_u32(buf, off::S_JNL_BLOCKS + i * 4);
        }
        let mut backup_bgs = [0u32; 2];
        for (i, bg) in backup_bgs.iter_mut().enumerate() {
            *bg = get_u32(buf, off::S_BACKUP_BGS + i * 4);
        }

        // The `_hi` bytes extend 32-bit timestamps past 2038. They are only
        // meaningful when the inode is large enough to carry the extra bits;
        // for superblock times e2fsprogs always reads them.
        let time64 = |lo: usize, hi: usize| -> u64 {
            get_u32(buf, lo) as u64 | ((get_u8(buf, hi) as u64) << 32)
        };

        Ok(Self {
            inodes_count: get_u32(buf, off::S_INODES_COUNT),
            blocks_count: get_u32(buf, off::S_BLOCKS_COUNT) as u64
                | ((get_u32(buf, off::S_BLOCKS_COUNT_HI) as u64) << 32),
            r_blocks_count: get_u32(buf, off::S_R_BLOCKS_COUNT) as u64
                | ((get_u32(buf, off::S_R_BLOCKS_COUNT_HI) as u64) << 32),
            free_blocks_count: get_u32(buf, off::S_FREE_BLOCKS_COUNT) as u64
                | ((get_u32(buf, off::S_FREE_BLOCKS_HI) as u64) << 32),
            free_inodes_count: get_u32(buf, off::S_FREE_INODES_COUNT),
            first_data_block: get_u32(buf, off::S_FIRST_DATA_BLOCK),
            log_block_size: get_u32(buf, off::S_LOG_BLOCK_SIZE),
            log_cluster_size: get_u32(buf, off::S_LOG_CLUSTER_SIZE),
            blocks_per_group: get_u32(buf, off::S_BLOCKS_PER_GROUP),
            clusters_per_group: get_u32(buf, off::S_CLUSTERS_PER_GROUP),
            inodes_per_group: get_u32(buf, off::S_INODES_PER_GROUP),
            mtime: time64(off::S_MTIME, off::S_MTIME_HI),
            wtime: time64(off::S_WTIME, off::S_WTIME_HI),
            mnt_count: get_u16(buf, off::S_MNT_COUNT),
            max_mnt_count: get_u16(buf, off::S_MAX_MNT_COUNT) as i16,
            state: get_u16(buf, off::S_STATE),
            errors: get_u16(buf, off::S_ERRORS),
            minor_rev_level: get_u16(buf, off::S_MINOR_REV_LEVEL),
            lastcheck: time64(off::S_LASTCHECK, off::S_LASTCHECK_HI),
            checkinterval: get_u32(buf, off::S_CHECKINTERVAL),
            creator_os: get_u32(buf, off::S_CREATOR_OS),
            rev_level,
            def_resuid: get_u16(buf, off::S_DEF_RESUID),
            def_resgid: get_u16(buf, off::S_DEF_RESGID),
            first_ino,
            inode_size,
            block_group_nr: get_u16(buf, off::S_BLOCK_GROUP_NR),
            feature_compat: CompatFeatures::from_bits_retain(get_u32(buf, off::S_FEATURE_COMPAT)),
            feature_incompat: IncompatFeatures::from_bits_retain(get_u32(
                buf,
                off::S_FEATURE_INCOMPAT,
            )),
            feature_ro_compat: RoCompatFeatures::from_bits_retain(get_u32(
                buf,
                off::S_FEATURE_RO_COMPAT,
            )),
            uuid: get_array(buf, off::S_UUID),
            volume_name: get_array(buf, off::S_VOLUME_NAME),
            last_mounted: get_array(buf, off::S_LAST_MOUNTED),
            algorithm_usage_bitmap: get_u32(buf, off::S_ALGORITHM_USAGE_BITMAP),
            prealloc_blocks: get_u8(buf, off::S_PREALLOC_BLOCKS),
            prealloc_dir_blocks: get_u8(buf, off::S_PREALLOC_DIR_BLOCKS),
            reserved_gdt_blocks: get_u16(buf, off::S_RESERVED_GDT_BLOCKS),
            journal_uuid: get_array(buf, off::S_JOURNAL_UUID),
            journal_inum: get_u32(buf, off::S_JOURNAL_INUM),
            journal_dev: get_u32(buf, off::S_JOURNAL_DEV),
            last_orphan: get_u32(buf, off::S_LAST_ORPHAN),
            hash_seed,
            def_hash_version: get_u8(buf, off::S_DEF_HASH_VERSION),
            jnl_backup_type: get_u8(buf, off::S_JNL_BACKUP_TYPE),
            desc_size: get_u16(buf, off::S_DESC_SIZE),
            default_mount_opts: get_u32(buf, off::S_DEFAULT_MOUNT_OPTS),
            first_meta_bg: get_u32(buf, off::S_FIRST_META_BG),
            mkfs_time: time64(off::S_MKFS_TIME, off::S_MKFS_TIME_HI),
            jnl_blocks,
            min_extra_isize: get_u16(buf, off::S_MIN_EXTRA_ISIZE),
            want_extra_isize: get_u16(buf, off::S_WANT_EXTRA_ISIZE),
            flags: get_u32(buf, off::S_FLAGS),
            raid_stride: get_u16(buf, off::S_RAID_STRIDE),
            mmp_update_interval: get_u16(buf, off::S_MMP_UPDATE_INTERVAL),
            mmp_block: get_u64(buf, off::S_MMP_BLOCK),
            raid_stripe_width: get_u32(buf, off::S_RAID_STRIPE_WIDTH),
            log_groups_per_flex: get_u8(buf, off::S_LOG_GROUPS_PER_FLEX),
            checksum_type: get_u8(buf, off::S_CHECKSUM_TYPE),
            encryption_level: get_u8(buf, off::S_ENCRYPTION_LEVEL),
            kbytes_written: get_u64(buf, off::S_KBYTES_WRITTEN),
            snapshot_inum: get_u32(buf, off::S_SNAPSHOT_INUM),
            snapshot_id: get_u32(buf, off::S_SNAPSHOT_ID),
            snapshot_r_blocks_count: get_u64(buf, off::S_SNAPSHOT_R_BLOCKS_COUNT),
            snapshot_list: get_u32(buf, off::S_SNAPSHOT_LIST),
            error_count: get_u32(buf, off::S_ERROR_COUNT),
            first_error_time: time64(off::S_FIRST_ERROR_TIME, off::S_FIRST_ERROR_TIME_HI),
            first_error_ino: get_u32(buf, off::S_FIRST_ERROR_INO),
            first_error_block: get_u64(buf, off::S_FIRST_ERROR_BLOCK),
            first_error_func: get_array(buf, off::S_FIRST_ERROR_FUNC),
            first_error_line: get_u32(buf, off::S_FIRST_ERROR_LINE),
            last_error_time: time64(off::S_LAST_ERROR_TIME, off::S_LAST_ERROR_TIME_HI),
            last_error_ino: get_u32(buf, off::S_LAST_ERROR_INO),
            last_error_line: get_u32(buf, off::S_LAST_ERROR_LINE),
            last_error_block: get_u64(buf, off::S_LAST_ERROR_BLOCK),
            last_error_func: get_array(buf, off::S_LAST_ERROR_FUNC),
            first_error_errcode: get_u8(buf, off::S_FIRST_ERROR_ERRCODE),
            last_error_errcode: get_u8(buf, off::S_LAST_ERROR_ERRCODE),
            mount_opts: get_array(buf, off::S_MOUNT_OPTS),
            usr_quota_inum: get_u32(buf, off::S_USR_QUOTA_INUM),
            grp_quota_inum: get_u32(buf, off::S_GRP_QUOTA_INUM),
            overhead_clusters: get_u32(buf, off::S_OVERHEAD_CLUSTERS),
            backup_bgs,
            encrypt_algos: get_array(buf, off::S_ENCRYPT_ALGOS),
            encrypt_pw_salt: get_array(buf, off::S_ENCRYPT_PW_SALT),
            lpf_ino: get_u32(buf, off::S_LPF_INO),
            prj_quota_inum: get_u32(buf, off::S_PRJ_QUOTA_INUM),
            checksum_seed: get_u32(buf, off::S_CHECKSUM_SEED),
            encoding: get_u16(buf, off::S_ENCODING),
            encoding_flags: get_u16(buf, off::S_ENCODING_FLAGS),
            orphan_file_inum: get_u32(buf, off::S_ORPHAN_FILE_INUM),
            checksum: get_u32(buf, off::S_CHECKSUM),
        })
    }

    /// Encode into a fresh 1024-byte buffer, setting `s_checksum` if the
    /// filesystem carries metadata checksums.
    pub fn encode(&self) -> [u8; SUPERBLOCK_LEN] {
        let mut buf = [0u8; SUPERBLOCK_LEN];
        self.encode_into(&mut buf);
        buf
    }

    /// Encode into `buf`, which must be at least [`SUPERBLOCK_LEN`] bytes.
    pub fn encode_into(&self, buf: &mut [u8]) {
        put_u32(buf, off::S_INODES_COUNT, self.inodes_count);
        put_u32(buf, off::S_BLOCKS_COUNT, self.blocks_count as u32);
        put_u32(buf, off::S_R_BLOCKS_COUNT, self.r_blocks_count as u32);
        put_u32(buf, off::S_FREE_BLOCKS_COUNT, self.free_blocks_count as u32);
        put_u32(buf, off::S_FREE_INODES_COUNT, self.free_inodes_count);
        put_u32(buf, off::S_FIRST_DATA_BLOCK, self.first_data_block);
        put_u32(buf, off::S_LOG_BLOCK_SIZE, self.log_block_size);
        put_u32(buf, off::S_LOG_CLUSTER_SIZE, self.log_cluster_size);
        put_u32(buf, off::S_BLOCKS_PER_GROUP, self.blocks_per_group);
        put_u32(buf, off::S_CLUSTERS_PER_GROUP, self.clusters_per_group);
        put_u32(buf, off::S_INODES_PER_GROUP, self.inodes_per_group);
        put_u32(buf, off::S_MTIME, self.mtime as u32);
        put_u32(buf, off::S_WTIME, self.wtime as u32);
        put_u16(buf, off::S_MNT_COUNT, self.mnt_count);
        put_u16(buf, off::S_MAX_MNT_COUNT, self.max_mnt_count as u16);
        put_u16(buf, off::S_MAGIC, EXT2_SUPER_MAGIC);
        put_u16(buf, off::S_STATE, self.state);
        put_u16(buf, off::S_ERRORS, self.errors);
        put_u16(buf, off::S_MINOR_REV_LEVEL, self.minor_rev_level);
        put_u32(buf, off::S_LASTCHECK, self.lastcheck as u32);
        put_u32(buf, off::S_CHECKINTERVAL, self.checkinterval);
        put_u32(buf, off::S_CREATOR_OS, self.creator_os);
        put_u32(buf, off::S_REV_LEVEL, self.rev_level);
        put_u16(buf, off::S_DEF_RESUID, self.def_resuid);
        put_u16(buf, off::S_DEF_RESGID, self.def_resgid);
        put_u32(buf, off::S_FIRST_INO, self.first_ino);
        put_u16(buf, off::S_INODE_SIZE, self.inode_size);
        put_u16(buf, off::S_BLOCK_GROUP_NR, self.block_group_nr);
        put_u32(buf, off::S_FEATURE_COMPAT, self.feature_compat.bits());
        put_u32(buf, off::S_FEATURE_INCOMPAT, self.feature_incompat.bits());
        put_u32(buf, off::S_FEATURE_RO_COMPAT, self.feature_ro_compat.bits());
        put_bytes(buf, off::S_UUID, 16, &self.uuid);
        put_bytes(buf, off::S_VOLUME_NAME, 16, &self.volume_name);
        put_bytes(buf, off::S_LAST_MOUNTED, 64, &self.last_mounted);
        put_u32(
            buf,
            off::S_ALGORITHM_USAGE_BITMAP,
            self.algorithm_usage_bitmap,
        );
        put_u8(buf, off::S_PREALLOC_BLOCKS, self.prealloc_blocks);
        put_u8(buf, off::S_PREALLOC_DIR_BLOCKS, self.prealloc_dir_blocks);
        put_u16(buf, off::S_RESERVED_GDT_BLOCKS, self.reserved_gdt_blocks);
        put_bytes(buf, off::S_JOURNAL_UUID, 16, &self.journal_uuid);
        put_u32(buf, off::S_JOURNAL_INUM, self.journal_inum);
        put_u32(buf, off::S_JOURNAL_DEV, self.journal_dev);
        put_u32(buf, off::S_LAST_ORPHAN, self.last_orphan);
        for (i, seed) in self.hash_seed.iter().enumerate() {
            put_u32(buf, off::S_HASH_SEED + i * 4, *seed);
        }
        put_u8(buf, off::S_DEF_HASH_VERSION, self.def_hash_version);
        put_u8(buf, off::S_JNL_BACKUP_TYPE, self.jnl_backup_type);
        put_u16(buf, off::S_DESC_SIZE, self.desc_size);
        put_u32(buf, off::S_DEFAULT_MOUNT_OPTS, self.default_mount_opts);
        put_u32(buf, off::S_FIRST_META_BG, self.first_meta_bg);
        put_u32(buf, off::S_MKFS_TIME, self.mkfs_time as u32);
        for (i, blk) in self.jnl_blocks.iter().enumerate() {
            put_u32(buf, off::S_JNL_BLOCKS + i * 4, *blk);
        }
        put_u32(buf, off::S_BLOCKS_COUNT_HI, (self.blocks_count >> 32) as u32);
        put_u32(
            buf,
            off::S_R_BLOCKS_COUNT_HI,
            (self.r_blocks_count >> 32) as u32,
        );
        put_u32(
            buf,
            off::S_FREE_BLOCKS_HI,
            (self.free_blocks_count >> 32) as u32,
        );
        put_u16(buf, off::S_MIN_EXTRA_ISIZE, self.min_extra_isize);
        put_u16(buf, off::S_WANT_EXTRA_ISIZE, self.want_extra_isize);
        put_u32(buf, off::S_FLAGS, self.flags);
        put_u16(buf, off::S_RAID_STRIDE, self.raid_stride);
        put_u16(buf, off::S_MMP_UPDATE_INTERVAL, self.mmp_update_interval);
        put_u64(buf, off::S_MMP_BLOCK, self.mmp_block);
        put_u32(buf, off::S_RAID_STRIPE_WIDTH, self.raid_stripe_width);
        put_u8(buf, off::S_LOG_GROUPS_PER_FLEX, self.log_groups_per_flex);
        put_u8(buf, off::S_CHECKSUM_TYPE, self.checksum_type);
        put_u8(buf, off::S_ENCRYPTION_LEVEL, self.encryption_level);
        put_u8(buf, off::S_RESERVED_PAD, 0);
        put_u64(buf, off::S_KBYTES_WRITTEN, self.kbytes_written);
        put_u32(buf, off::S_SNAPSHOT_INUM, self.snapshot_inum);
        put_u32(buf, off::S_SNAPSHOT_ID, self.snapshot_id);
        put_u64(
            buf,
            off::S_SNAPSHOT_R_BLOCKS_COUNT,
            self.snapshot_r_blocks_count,
        );
        put_u32(buf, off::S_SNAPSHOT_LIST, self.snapshot_list);
        put_u32(buf, off::S_ERROR_COUNT, self.error_count);
        put_u32(buf, off::S_FIRST_ERROR_TIME, self.first_error_time as u32);
        put_u32(buf, off::S_FIRST_ERROR_INO, self.first_error_ino);
        put_u64(buf, off::S_FIRST_ERROR_BLOCK, self.first_error_block);
        put_bytes(buf, off::S_FIRST_ERROR_FUNC, 32, &self.first_error_func);
        put_u32(buf, off::S_FIRST_ERROR_LINE, self.first_error_line);
        put_u32(buf, off::S_LAST_ERROR_TIME, self.last_error_time as u32);
        put_u32(buf, off::S_LAST_ERROR_INO, self.last_error_ino);
        put_u32(buf, off::S_LAST_ERROR_LINE, self.last_error_line);
        put_u64(buf, off::S_LAST_ERROR_BLOCK, self.last_error_block);
        put_bytes(buf, off::S_LAST_ERROR_FUNC, 32, &self.last_error_func);
        put_bytes(buf, off::S_MOUNT_OPTS, 64, &self.mount_opts);
        put_u32(buf, off::S_USR_QUOTA_INUM, self.usr_quota_inum);
        put_u32(buf, off::S_GRP_QUOTA_INUM, self.grp_quota_inum);
        put_u32(buf, off::S_OVERHEAD_CLUSTERS, self.overhead_clusters);
        for (i, bg) in self.backup_bgs.iter().enumerate() {
            put_u32(buf, off::S_BACKUP_BGS + i * 4, *bg);
        }
        put_bytes(buf, off::S_ENCRYPT_ALGOS, 4, &self.encrypt_algos);
        put_bytes(buf, off::S_ENCRYPT_PW_SALT, 16, &self.encrypt_pw_salt);
        put_u32(buf, off::S_LPF_INO, self.lpf_ino);
        put_u32(buf, off::S_PRJ_QUOTA_INUM, self.prj_quota_inum);
        put_u32(buf, off::S_CHECKSUM_SEED, self.checksum_seed);
        put_u8(buf, off::S_WTIME_HI, (self.wtime >> 32) as u8);
        put_u8(buf, off::S_MTIME_HI, (self.mtime >> 32) as u8);
        put_u8(buf, off::S_MKFS_TIME_HI, (self.mkfs_time >> 32) as u8);
        put_u8(buf, off::S_LASTCHECK_HI, (self.lastcheck >> 32) as u8);
        put_u8(
            buf,
            off::S_FIRST_ERROR_TIME_HI,
            (self.first_error_time >> 32) as u8,
        );
        put_u8(
            buf,
            off::S_LAST_ERROR_TIME_HI,
            (self.last_error_time >> 32) as u8,
        );
        put_u8(buf, off::S_FIRST_ERROR_ERRCODE, self.first_error_errcode);
        put_u8(buf, off::S_LAST_ERROR_ERRCODE, self.last_error_errcode);
        put_u16(buf, off::S_ENCODING, self.encoding);
        put_u16(buf, off::S_ENCODING_FLAGS, self.encoding_flags);
        put_u32(buf, off::S_ORPHAN_FILE_INUM, self.orphan_file_inum);

        // The superblock checksum covers everything before the field itself.
        // Written last, and only when the feature is on — a filesystem without
        // metadata_csum must leave these four bytes zero.
        let csum = if self.has_metadata_csum() {
            crate::csum::crc32c(!0, &buf[..off::S_CHECKSUM])
        } else {
            0
        };
        put_u32(buf, off::S_CHECKSUM, csum);
    }

    /// Verify `s_checksum` against the buffer it was decoded from.
    pub fn verify_checksum(&self, buf: &[u8]) -> bool {
        if !self.has_metadata_csum() {
            return true;
        }
        let expect = crate::csum::crc32c(!0, &buf[..off::S_CHECKSUM]);
        expect == get_u32(buf, off::S_CHECKSUM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_match_ext2_fs_h() {
        // Spot-checks against the documented offsets in struct ext2_super_block.
        assert_eq!(off::S_MAGIC, 0x38);
        assert_eq!(off::S_UUID, 0x68);
        assert_eq!(off::S_VOLUME_NAME, 0x78);
        assert_eq!(off::S_LAST_MOUNTED, 0x88);
        assert_eq!(off::S_JNL_BLOCKS, 0x10c);
        assert_eq!(off::S_BLOCKS_COUNT_HI, 0x150);
        assert_eq!(off::S_MOUNT_OPTS, 0x200);
        assert_eq!(off::S_CHECKSUM_SEED, 0x270);
        assert_eq!(off::S_CHECKSUM, 0x3fc);
        // s_reserved[94] runs from 0x284 to the checksum with nothing between.
        assert_eq!(off::S_ORPHAN_FILE_INUM + 4 + 94 * 4, off::S_CHECKSUM);
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let mut sb = Superblock {
            inodes_count: 16384,
            blocks_count: 65536,
            r_blocks_count: 3276,
            free_blocks_count: 60000,
            free_inodes_count: 16373,
            first_data_block: 0,
            log_block_size: 2,
            log_cluster_size: 2,
            blocks_per_group: 32768,
            clusters_per_group: 32768,
            inodes_per_group: 8192,
            inode_size: 256,
            uuid: *b"0123456789abcdef",
            ..Default::default()
        };
        sb.volume_name[..4].copy_from_slice(b"data");
        sb.mkfs_time = 1_700_000_000;

        let buf = sb.encode();
        let back = Superblock::decode(&buf).unwrap();
        assert_eq!(sb, back);
        assert_eq!(back.block_size(), 4096);
        assert_eq!(back.label(), "data");
    }

    #[test]
    fn checksum_is_written_only_with_the_feature() {
        let plain = Superblock {
            blocks_count: 1024,
            blocks_per_group: 8192,
            ..Default::default()
        };
        let buf = plain.encode();
        assert_eq!(get_u32(&buf, off::S_CHECKSUM), 0);

        let csummed = Superblock {
            feature_ro_compat: RoCompatFeatures::METADATA_CSUM,
            ..plain
        };
        let buf = csummed.encode();
        assert_ne!(get_u32(&buf, off::S_CHECKSUM), 0);
        let back = Superblock::decode(&buf).unwrap();
        assert!(back.verify_checksum(&buf));
    }

    #[test]
    fn rejects_a_buffer_with_no_magic() {
        let buf = [0u8; SUPERBLOCK_LEN];
        assert!(matches!(
            Superblock::decode(&buf),
            Err(Error::NotExtFilesystem { found: 0 })
        ));
    }
}
