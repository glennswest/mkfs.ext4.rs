//! Feature masks.
//!
//! Mirrors the `EXT*_FEATURE_{COMPAT,INCOMPAT,RO_COMPAT}_*` definitions in
//! `lib/ext2fs/ext2_fs.h`. Names match those `mke2fs -O` accepts, so a feature
//! string round-trips through this module unchanged.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bitflags::bitflags;

bitflags! {
    /// `s_feature_compat` — an implementation may mount the filesystem
    /// read-write even if it does not understand these.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct CompatFeatures: u32 {
        /// `dir_prealloc`
        const DIR_PREALLOC = 0x0001;
        /// `imagic_inodes`
        const IMAGIC_INODES = 0x0002;
        /// `has_journal`
        const HAS_JOURNAL = 0x0004;
        /// `ext_attr`
        const EXT_ATTR = 0x0008;
        /// `resize_inode`
        const RESIZE_INODE = 0x0010;
        /// `dir_index`
        const DIR_INDEX = 0x0020;
        /// `lazy_bg`
        const LAZY_BG = 0x0040;
        /// `exclude_bitmap`
        const EXCLUDE_BITMAP = 0x0100;
        /// `sparse_super2`
        const SPARSE_SUPER2 = 0x0200;
        /// `fast_commit`
        const FAST_COMMIT = 0x0400;
        /// `stable_inodes`
        const STABLE_INODES = 0x0800;
        /// `orphan_file`
        const ORPHAN_FILE = 0x1000;
    }

    /// `s_feature_incompat` — an implementation that does not understand one of
    /// these must refuse to mount at all.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct IncompatFeatures: u32 {
        /// `compression`
        const COMPRESSION = 0x0001;
        /// `filetype` — directory entries carry a file type byte.
        const FILETYPE = 0x0002;
        /// `needs_recovery` — the journal must be replayed.
        const RECOVER = 0x0004;
        /// `journal_dev` — this device *is* an external journal.
        const JOURNAL_DEV = 0x0008;
        /// `meta_bg`
        const META_BG = 0x0010;
        /// `extent`
        const EXTENTS = 0x0040;
        /// `64bit`
        const SIXTY_FOUR_BIT = 0x0080;
        /// `mmp`
        const MMP = 0x0100;
        /// `flex_bg`
        const FLEX_BG = 0x0200;
        /// `ea_inode`
        const EA_INODE = 0x0400;
        /// `dirdata`
        const DIRDATA = 0x1000;
        /// `metadata_csum_seed`
        const CSUM_SEED = 0x2000;
        /// `largedir`
        const LARGEDIR = 0x4000;
        /// `inline_data`
        const INLINE_DATA = 0x8000;
        /// `encrypt`
        const ENCRYPT = 0x10000;
        /// `casefold`
        const CASEFOLD = 0x20000;
    }

    /// `s_feature_ro_compat` — an implementation that does not understand one of
    /// these may mount the filesystem read-only, but not read-write.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct RoCompatFeatures: u32 {
        /// `sparse_super`
        const SPARSE_SUPER = 0x0001;
        /// `large_file`
        const LARGE_FILE = 0x0002;
        /// `huge_file`
        const HUGE_FILE = 0x0008;
        /// `uninit_bg` — legacy crc16 group descriptor checksums.
        const GDT_CSUM = 0x0010;
        /// `dir_nlink`
        const DIR_NLINK = 0x0020;
        /// `extra_isize`
        const EXTRA_ISIZE = 0x0040;
        /// `snapshot`
        const HAS_SNAPSHOT = 0x0080;
        /// `quota`
        const QUOTA = 0x0100;
        /// `bigalloc`
        const BIGALLOC = 0x0200;
        /// `metadata_csum` — crc32c checksums on all metadata. Implies the
        /// group descriptor checksum semantics of `GDT_CSUM`, and the two must
        /// never both be set.
        const METADATA_CSUM = 0x0400;
        /// `replica`
        const REPLICA = 0x0800;
        /// `read-only`
        const READONLY = 0x1000;
        /// `project`
        const PROJECT = 0x2000;
        /// `shared_blocks`
        const SHARED_BLOCKS = 0x4000;
        /// `verity`
        const VERITY = 0x8000;
        /// `orphan_present`
        const ORPHAN_PRESENT = 0x10000;
    }
}

/// Which mask a feature name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSet {
    /// `s_feature_compat`
    Compat,
    /// `s_feature_incompat`
    Incompat,
    /// `s_feature_ro_compat`
    RoCompat,
}

/// A single feature bit, resolved from its `mke2fs` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// Which mask it lives in.
    pub set: FeatureSet,
    /// The bit within that mask.
    pub bit: u32,
    /// The canonical name.
    pub name: &'static str,
}

/// Every feature name `mke2fs -O` accepts, with the mask and bit it sets.
///
/// Order matters only for display; lookups are by name. Aliases (`extent` vs
/// `extents`, `uninit_bg` vs `uninit_groups`) are listed separately and resolve
/// to the same bit, as they do in `e2p/feature.c`.
pub const FEATURES: &[Feature] = &[
    // compat
    f(FeatureSet::Compat, 0x0001, "dir_prealloc"),
    f(FeatureSet::Compat, 0x0002, "imagic_inodes"),
    f(FeatureSet::Compat, 0x0004, "has_journal"),
    f(FeatureSet::Compat, 0x0008, "ext_attr"),
    f(FeatureSet::Compat, 0x0010, "resize_inode"),
    f(FeatureSet::Compat, 0x0020, "dir_index"),
    f(FeatureSet::Compat, 0x0040, "lazy_bg"),
    f(FeatureSet::Compat, 0x0100, "exclude_bitmap"),
    f(FeatureSet::Compat, 0x0200, "sparse_super2"),
    f(FeatureSet::Compat, 0x0400, "fast_commit"),
    f(FeatureSet::Compat, 0x0800, "stable_inodes"),
    f(FeatureSet::Compat, 0x1000, "orphan_file"),
    // incompat
    f(FeatureSet::Incompat, 0x0001, "compression"),
    f(FeatureSet::Incompat, 0x0002, "filetype"),
    f(FeatureSet::Incompat, 0x0004, "needs_recovery"),
    f(FeatureSet::Incompat, 0x0008, "journal_dev"),
    f(FeatureSet::Incompat, 0x0010, "meta_bg"),
    f(FeatureSet::Incompat, 0x0040, "extent"),
    f(FeatureSet::Incompat, 0x0040, "extents"),
    f(FeatureSet::Incompat, 0x0080, "64bit"),
    f(FeatureSet::Incompat, 0x0100, "mmp"),
    f(FeatureSet::Incompat, 0x0200, "flex_bg"),
    f(FeatureSet::Incompat, 0x0400, "ea_inode"),
    f(FeatureSet::Incompat, 0x1000, "dirdata"),
    f(FeatureSet::Incompat, 0x2000, "metadata_csum_seed"),
    f(FeatureSet::Incompat, 0x4000, "largedir"),
    f(FeatureSet::Incompat, 0x8000, "inline_data"),
    f(FeatureSet::Incompat, 0x10000, "encrypt"),
    f(FeatureSet::Incompat, 0x20000, "casefold"),
    // ro_compat
    f(FeatureSet::RoCompat, 0x0001, "sparse_super"),
    f(FeatureSet::RoCompat, 0x0002, "large_file"),
    f(FeatureSet::RoCompat, 0x0008, "huge_file"),
    f(FeatureSet::RoCompat, 0x0010, "uninit_bg"),
    f(FeatureSet::RoCompat, 0x0010, "uninit_groups"),
    f(FeatureSet::RoCompat, 0x0020, "dir_nlink"),
    f(FeatureSet::RoCompat, 0x0040, "extra_isize"),
    f(FeatureSet::RoCompat, 0x0080, "snapshot"),
    f(FeatureSet::RoCompat, 0x0100, "quota"),
    f(FeatureSet::RoCompat, 0x0200, "bigalloc"),
    f(FeatureSet::RoCompat, 0x0400, "metadata_csum"),
    f(FeatureSet::RoCompat, 0x0800, "replica"),
    f(FeatureSet::RoCompat, 0x1000, "read-only"),
    f(FeatureSet::RoCompat, 0x2000, "project"),
    f(FeatureSet::RoCompat, 0x4000, "shared_blocks"),
    f(FeatureSet::RoCompat, 0x8000, "verity"),
    f(FeatureSet::RoCompat, 0x10000, "orphan_present"),
];

const fn f(set: FeatureSet, bit: u32, name: &'static str) -> Feature {
    Feature { set, bit, name }
}

/// Resolve a `mke2fs` feature name.
pub fn lookup(name: &str) -> Option<Feature> {
    FEATURES.iter().copied().find(|feat| feat.name == name)
}

/// The three feature masks together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeatureMasks {
    /// `s_feature_compat`
    pub compat: CompatFeatures,
    /// `s_feature_incompat`
    pub incompat: IncompatFeatures,
    /// `s_feature_ro_compat`
    pub ro_compat: RoCompatFeatures,
}

impl FeatureMasks {
    /// Apply a `mke2fs -O`-style list: `feature`, `^feature` to clear, `none`
    /// to clear everything.
    ///
    /// Unknown names are an error rather than a silent no-op — a typo in a
    /// feature list otherwise produces a filesystem that silently lacks it.
    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if token.eq_ignore_ascii_case("none") {
                *self = Self::default();
                continue;
            }
            let (clear, name) = match token.strip_prefix('^') {
                Some(rest) => (true, rest),
                None => (false, token),
            };
            let feat = lookup(name).ok_or_else(|| format!("unknown feature '{name}'"))?;
            self.set_bit(feat, !clear);
        }
        Ok(())
    }

    fn set_bit(&mut self, feat: Feature, on: bool) {
        match feat.set {
            FeatureSet::Compat => {
                let bits = CompatFeatures::from_bits_retain(feat.bit);
                self.compat.set(bits, on);
            }
            FeatureSet::Incompat => {
                let bits = IncompatFeatures::from_bits_retain(feat.bit);
                self.incompat.set(bits, on);
            }
            FeatureSet::RoCompat => {
                let bits = RoCompatFeatures::from_bits_retain(feat.bit);
                self.ro_compat.set(bits, on);
            }
        }
    }

    /// Render the set features as a `mke2fs`-style comma-separated list.
    ///
    /// Where a bit has aliases, the first name in [`FEATURES`] is the canonical
    /// one and the only one emitted.
    pub fn to_spec(&self) -> String {
        let mut out: Vec<&str> = Vec::new();
        let mut seen: Vec<(FeatureSet, u32)> = Vec::new();
        for feat in FEATURES {
            let mask = match feat.set {
                FeatureSet::Compat => self.compat.bits(),
                FeatureSet::Incompat => self.incompat.bits(),
                FeatureSet::RoCompat => self.ro_compat.bits(),
            };
            if mask & feat.bit == 0 {
                continue;
            }
            let id = (feat.set, feat.bit);
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            out.push(feat.name);
        }
        out.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_and_clears_features() {
        let mut m = FeatureMasks::default();
        m.apply("has_journal,extent,64bit,metadata_csum").unwrap();
        assert!(m.compat.contains(CompatFeatures::HAS_JOURNAL));
        assert!(m.incompat.contains(IncompatFeatures::EXTENTS));
        assert!(m.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT));
        assert!(m.ro_compat.contains(RoCompatFeatures::METADATA_CSUM));

        m.apply("^64bit,^metadata_csum").unwrap();
        assert!(!m.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT));
        assert!(!m.ro_compat.contains(RoCompatFeatures::METADATA_CSUM));
        assert!(m.incompat.contains(IncompatFeatures::EXTENTS));
    }

    #[test]
    fn none_clears_everything() {
        let mut m = FeatureMasks::default();
        m.apply("has_journal,extent").unwrap();
        m.apply("none").unwrap();
        assert_eq!(m, FeatureMasks::default());
    }

    #[test]
    fn unknown_feature_is_an_error() {
        let mut m = FeatureMasks::default();
        let err = m.apply("has_journal,not_a_feature").unwrap_err();
        assert!(err.contains("not_a_feature"));
    }

    #[test]
    fn extent_and_extents_are_the_same_bit() {
        assert_eq!(
            lookup("extent").unwrap().bit,
            lookup("extents").unwrap().bit
        );
    }
}
