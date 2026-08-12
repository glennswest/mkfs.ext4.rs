//! Formatting parameters, and the defaults `mke2fs` applies.
//!
//! The defaults come from `misc/mke2fs.conf.in` and the size classification in
//! `parse_fs_type()` in `misc/mke2fs.c`. Reproducing them exactly is the point:
//! a 64 MiB ext4 filesystem gets 1 KiB blocks and one inode per 4 KiB, not the
//! 4 KiB blocks a reading of the defaults section alone would suggest.

use crate::features::{CompatFeatures, FeatureMasks, IncompatFeatures, RoCompatFeatures};

/// Which filesystem to write.
///
/// `mke2fs` is one program that writes all three; the profile only decides
/// which features are turned on over the common base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// No journal, no extents.
    Ext2,
    /// A journal, but no extents.
    Ext3,
    /// Journal, extents, checksums, 64-bit.
    #[default]
    Ext4,
}

impl Profile {
    /// The name `mke2fs -t` uses.
    pub fn name(&self) -> &'static str {
        match self {
            Profile::Ext2 => "ext2",
            Profile::Ext3 => "ext3",
            Profile::Ext4 => "ext4",
        }
    }

    /// Features this profile adds over `base_features`.
    ///
    /// From the `[fs_types]` section of `mke2fs.conf`.
    pub fn features(&self) -> FeatureMasks {
        let mut m = base_features();
        match self {
            Profile::Ext2 => {}
            Profile::Ext3 => {
                m.compat |= CompatFeatures::HAS_JOURNAL;
            }
            Profile::Ext4 => {
                m.compat |= CompatFeatures::HAS_JOURNAL;
                m.incompat |= IncompatFeatures::EXTENTS
                    | IncompatFeatures::FLEX_BG
                    | IncompatFeatures::SIXTY_FOUR_BIT
                    | IncompatFeatures::CSUM_SEED;
                m.ro_compat |= RoCompatFeatures::HUGE_FILE
                    | RoCompatFeatures::METADATA_CSUM
                    | RoCompatFeatures::DIR_NLINK
                    | RoCompatFeatures::EXTRA_ISIZE;
            }
        }
        m
    }
}

impl std::str::FromStr for Profile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ext2" => Ok(Profile::Ext2),
            "ext3" => Ok(Profile::Ext3),
            "ext4" => Ok(Profile::Ext4),
            other => Err(format!("unknown filesystem type '{other}'")),
        }
    }
}

/// `base_features` from `mke2fs.conf`: the features every profile starts with.
pub fn base_features() -> FeatureMasks {
    FeatureMasks {
        compat: CompatFeatures::RESIZE_INODE
            | CompatFeatures::DIR_INDEX
            | CompatFeatures::EXT_ATTR,
        incompat: IncompatFeatures::FILETYPE,
        ro_compat: RoCompatFeatures::SPARSE_SUPER | RoCompatFeatures::LARGE_FILE,
    }
}

/// The size class `mke2fs` puts a filesystem in, which chooses the block size
/// and inode ratio. `parse_fs_type()` in `misc/mke2fs.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeType {
    /// Under 3 MiB.
    Floppy,
    /// Under 512 MiB.
    Small,
    /// Under 4 TiB.
    Default,
    /// Under 16 TiB.
    Big,
    /// 16 TiB and up.
    Huge,
}

impl SizeType {
    /// Classify by total size in bytes.
    ///
    /// `mke2fs` compares block counts against a block count for one megabyte,
    /// which is the same comparison in bytes and so does not depend on the
    /// block size it has not chosen yet.
    pub fn of(size_bytes: u64) -> Self {
        const MIB: u64 = 1024 * 1024;
        match size_bytes {
            s if s < 3 * MIB => SizeType::Floppy,
            s if s < 512 * MIB => SizeType::Small,
            s if s < 4 * 1024 * 1024 * MIB => SizeType::Default,
            s if s < 16 * 1024 * 1024 * MIB => SizeType::Big,
            _ => SizeType::Huge,
        }
    }

    /// Default block size for this class.
    pub fn block_size(&self) -> u32 {
        match self {
            SizeType::Floppy | SizeType::Small => 1024,
            _ => 4096,
        }
    }

    /// Default bytes-per-inode for this class.
    pub fn inode_ratio(&self) -> u32 {
        match self {
            SizeType::Floppy => 8192,
            SizeType::Small => 4096,
            SizeType::Default => 16384,
            SizeType::Big => 32768,
            SizeType::Huge => 65536,
        }
    }
}

/// `mke2fs` default for `default_mntopts`: `acl,user_xattr`.
pub const DEFAULT_MNTOPTS: u32 =
    crate::structs::superblock::defm::ACL | crate::structs::superblock::defm::XATTR_USER;

/// How large a journal to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JournalSize {
    /// The size `mke2fs` picks for the filesystem's block count.
    #[default]
    Default,
    /// An explicit number of blocks.
    Blocks(u32),
    /// No journal, whatever the profile says.
    None,
}

/// Everything needed to lay down a filesystem.
///
/// Fields left `None` take the `mke2fs` default for the device size, which is
/// computed in [`crate::layout::Geometry::compute`] rather than here — the
/// defaults depend on the size, and the size is not known until the device is.
#[derive(Debug, Clone)]
pub struct Params {
    /// Which filesystem to write.
    pub profile: Profile,
    /// Block size in bytes. `mke2fs -b`.
    pub block_size: Option<u32>,
    /// Inode size in bytes. `mke2fs -I`.
    pub inode_size: Option<u16>,
    /// Total inodes. `mke2fs -N`.
    pub inodes_count: Option<u32>,
    /// Bytes per inode. `mke2fs -i`.
    pub inode_ratio: Option<u32>,
    /// Percentage of blocks reserved for root. `mke2fs -m`.
    pub reserved_percent: f64,
    /// Volume label. `mke2fs -L`.
    pub label: Option<String>,
    /// Filesystem UUID. `mke2fs -U`. Random when absent.
    pub uuid: Option<[u8; 16]>,
    /// Directory hash seed. `mke2fs -E hash_seed=`. Random when absent.
    pub hash_seed: Option<[u8; 16]>,
    /// Blocks per group. `mke2fs -g`.
    pub blocks_per_group: Option<u32>,
    /// Groups per flex group, as a count not a log. `mke2fs -G`.
    pub flex_bg_size: Option<u32>,
    /// Journal sizing. `mke2fs -J size=`.
    pub journal: JournalSize,
    /// Feature overrides applied after the profile's own. `mke2fs -O`.
    pub feature_spec: Option<String>,
    /// Creation timestamp. Fixed rather than "now" makes output reproducible,
    /// the same role `SOURCE_DATE_EPOCH` plays for `mke2fs`.
    pub mkfs_time: Option<u64>,
    /// Leave inode tables unwritten, marking the groups uninitialised.
    ///
    /// `mke2fs -E lazy_itable_init`. Enormously faster on a large device, and
    /// safe only when the filesystem carries group descriptor checksums.
    pub lazy_itable_init: bool,
    /// Reserved-block owner. `mke2fs -E resuid=`.
    pub resuid: u16,
    /// Reserved-block group. `mke2fs -E resgid=`.
    pub resgid: u16,
    /// Bound on how many block groups are written at once. `None` uses a
    /// default proportional to the available parallelism.
    pub concurrency: Option<usize>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            profile: Profile::Ext4,
            block_size: None,
            inode_size: None,
            inodes_count: None,
            inode_ratio: None,
            reserved_percent: 5.0,
            label: None,
            uuid: None,
            hash_seed: None,
            blocks_per_group: None,
            flex_bg_size: None,
            journal: JournalSize::Default,
            feature_spec: None,
            mkfs_time: None,
            lazy_itable_init: false,
            resuid: 0,
            resgid: 0,
            concurrency: None,
        }
    }
}

impl Params {
    /// Parameters for a profile, with every `mke2fs` default in place.
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            ..Default::default()
        }
    }

    /// Set the volume label. Truncated to 16 bytes, as `mke2fs` does.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the filesystem UUID.
    pub fn uuid(mut self, uuid: [u8; 16]) -> Self {
        self.uuid = Some(uuid);
        self
    }

    /// Set the block size.
    pub fn block_size(mut self, size: u32) -> Self {
        self.block_size = Some(size);
        self
    }

    /// Set the inode size.
    pub fn inode_size(mut self, size: u16) -> Self {
        self.inode_size = Some(size);
        self
    }

    /// Set the total inode count.
    pub fn inodes_count(mut self, count: u32) -> Self {
        self.inodes_count = Some(count);
        self
    }

    /// Set bytes per inode.
    pub fn inode_ratio(mut self, ratio: u32) -> Self {
        self.inode_ratio = Some(ratio);
        self
    }

    /// Set the percentage of blocks reserved for root.
    pub fn reserved_percent(mut self, percent: f64) -> Self {
        self.reserved_percent = percent;
        self
    }

    /// Turn the journal off, whatever the profile would do.
    ///
    /// The case RouterOS needs: it cannot replay a journal, so one that ever
    /// goes dirty leaves the filesystem read-only for good.
    pub fn no_journal(mut self) -> Self {
        self.journal = JournalSize::None;
        self
    }

    /// Apply a `mke2fs -O`-style feature list over the profile's features.
    pub fn features(mut self, spec: impl Into<String>) -> Self {
        self.feature_spec = Some(spec.into());
        self
    }

    /// Fix the creation timestamp, making the output reproducible.
    pub fn mkfs_time(mut self, secs: u64) -> Self {
        self.mkfs_time = Some(secs);
        self
    }

    /// Leave inode tables unwritten.
    pub fn lazy_itable_init(mut self, lazy: bool) -> Self {
        self.lazy_itable_init = lazy;
        self
    }

    /// Bound how many block groups are written concurrently.
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = Some(n);
        self
    }

    /// Resolve the feature masks: the profile's, then `-O` overrides, then the
    /// adjustments the format itself requires.
    pub fn resolve_features(&self) -> Result<FeatureMasks, String> {
        let mut m = self.profile.features();
        if let Some(spec) = &self.feature_spec {
            m.apply(spec)?;
        }
        if matches!(self.journal, JournalSize::None) {
            m.compat.remove(CompatFeatures::HAS_JOURNAL);
        }
        normalise(&mut m)?;
        Ok(m)
    }
}

/// Apply the rules the format imposes on a feature set, and reject the
/// combinations it forbids.
pub fn normalise(m: &mut FeatureMasks) -> Result<(), String> {
    // metadata_csum and the legacy uninit_bg are two answers to the same
    // question. e2fsprogs refuses the pair rather than guessing.
    if m.ro_compat.contains(RoCompatFeatures::METADATA_CSUM)
        && m.ro_compat.contains(RoCompatFeatures::GDT_CSUM)
    {
        return Err("metadata_csum and uninit_bg cannot both be set".into());
    }
    // A checksum seed only means anything alongside the checksums it seeds.
    if m.incompat.contains(IncompatFeatures::CSUM_SEED)
        && !m.ro_compat.contains(RoCompatFeatures::METADATA_CSUM)
    {
        m.incompat.remove(IncompatFeatures::CSUM_SEED);
    }
    // 64-bit block numbers are only reachable through extents.
    if m.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT)
        && !m.incompat.contains(IncompatFeatures::EXTENTS)
    {
        return Err("the 64bit feature requires extents".into());
    }
    // A resize inode records reserved GDT blocks, which meta_bg does away with.
    if m.incompat.contains(IncompatFeatures::META_BG) {
        m.compat.remove(CompatFeatures::RESIZE_INODE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_classes_match_mke2fs_thresholds() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(SizeType::of(2 * MIB), SizeType::Floppy);
        assert_eq!(SizeType::of(3 * MIB), SizeType::Small);
        assert_eq!(SizeType::of(64 * MIB), SizeType::Small);
        assert_eq!(SizeType::of(511 * MIB), SizeType::Small);
        assert_eq!(SizeType::of(512 * MIB), SizeType::Default);
        assert_eq!(SizeType::of(1024 * 1024 * MIB), SizeType::Default);
        assert_eq!(SizeType::of(4 * 1024 * 1024 * MIB), SizeType::Big);
        assert_eq!(SizeType::of(16 * 1024 * 1024 * MIB), SizeType::Huge);
    }

    #[test]
    fn a_64mib_filesystem_gets_1k_blocks() {
        // The golden reference from mke2fs 1.47.3 has 1 KiB blocks and 16384
        // inodes for 64 MiB, which is the "small" class rather than the
        // headline defaults.
        let class = SizeType::of(64 * 1024 * 1024);
        assert_eq!(class.block_size(), 1024);
        assert_eq!(class.inode_ratio(), 4096);
        assert_eq!(64 * 1024 * 1024 / class.inode_ratio() as u64, 16384);
    }

    #[test]
    fn ext4_profile_matches_the_golden_feature_set() {
        let m = Profile::Ext4.features();
        assert!(m.compat.contains(CompatFeatures::HAS_JOURNAL));
        assert!(m.compat.contains(CompatFeatures::RESIZE_INODE));
        assert!(m.compat.contains(CompatFeatures::DIR_INDEX));
        assert!(m.compat.contains(CompatFeatures::EXT_ATTR));
        assert!(m.incompat.contains(IncompatFeatures::EXTENTS));
        assert!(m.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT));
        assert!(m.incompat.contains(IncompatFeatures::FLEX_BG));
        assert!(m.incompat.contains(IncompatFeatures::CSUM_SEED));
        assert!(m.ro_compat.contains(RoCompatFeatures::METADATA_CSUM));
        assert!(m.ro_compat.contains(RoCompatFeatures::EXTRA_ISIZE));
    }

    #[test]
    fn ext2_has_no_journal_and_no_extents() {
        let m = Profile::Ext2.features();
        assert!(!m.compat.contains(CompatFeatures::HAS_JOURNAL));
        assert!(!m.incompat.contains(IncompatFeatures::EXTENTS));
        assert!(m.ro_compat.contains(RoCompatFeatures::SPARSE_SUPER));
    }

    #[test]
    fn no_journal_clears_the_feature_the_profile_set() {
        let p = Params::new(Profile::Ext4).no_journal();
        let m = p.resolve_features().unwrap();
        assert!(!m.compat.contains(CompatFeatures::HAS_JOURNAL));
        // Everything else the profile chose survives.
        assert!(m.incompat.contains(IncompatFeatures::EXTENTS));
    }

    #[test]
    fn csum_seed_is_dropped_without_metadata_csum() {
        let p = Params::new(Profile::Ext4).features("^metadata_csum");
        let m = p.resolve_features().unwrap();
        assert!(!m.incompat.contains(IncompatFeatures::CSUM_SEED));
    }

    #[test]
    fn sixtyfour_bit_without_extents_is_refused() {
        let p = Params::new(Profile::Ext4).features("^extent");
        assert!(p.resolve_features().is_err());
    }

    #[test]
    fn both_checksum_schemes_at_once_is_refused() {
        let p = Params::new(Profile::Ext4).features("uninit_bg");
        assert!(p.resolve_features().is_err());
    }
}
