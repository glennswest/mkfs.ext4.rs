//! A filesystem must not advertise a journal it does not have.
//!
//! `has_journal` comes from the profile; the journal's *size* comes from a
//! size class that declines to make one below 2048 blocks. Those two decisions
//! were taken independently, so on a small filesystem the feature bit survived
//! the journal and the result was a superblock with `has_journal` set, no
//! journal blocks and `s_journal_inum` zero.
//!
//! `mke2fs` never emits that. A kernel asked to mount it looks the journal
//! inode up, finds nothing and refuses — so these filesystems were
//! unmountable, and because every `fsck` pass here passed them, nothing caught
//! it before a real mount would have. Issue #3.
//!
//! The sizes below are where it lands: config, secret and log volumes of a few
//! MiB, which is exactly where it is least likely to be noticed.

use mkfs_ext4::features::CompatFeatures;
use mkfs_ext4::layout::Geometry;
use mkfs_ext4::params::{JournalSize, Params, Profile};

const MIB: u64 = 1024 * 1024;

/// 4 KiB sectors, which is what the storage this runs on reports.
fn geometry(size: u64) -> Geometry {
    let params = Params::new(Profile::Ext4).sector_size(4096);
    Geometry::compute(size, &params).expect("geometry should be computable")
}

#[test]
fn a_filesystem_too_small_for_a_journal_does_not_claim_one() {
    for mib in 1..=7u64 {
        let geom = geometry(mib * MIB);
        assert!(
            geom.blocks_count < 2048,
            "{mib} MiB should be under the journal floor, got {} blocks",
            geom.blocks_count
        );
        assert!(
            !geom.features.compat.contains(CompatFeatures::HAS_JOURNAL),
            "{mib} MiB claims has_journal with no journal behind it"
        );
    }
}

/// The orphan file tracks inodes awaiting deletion across a crash, which is a
/// journal's job to replay. Dropping `has_journal` has to take it too, or the
/// filesystem carries a file nothing will ever read — 128 KiB of it, which on
/// a 1 MiB filesystem is an eighth of the whole thing.
#[test]
fn dropping_the_journal_drops_the_orphan_file_with_it() {
    let geom = geometry(MIB);
    assert!(!geom.features.compat.contains(CompatFeatures::HAS_JOURNAL));
    assert!(
        !geom.features.compat.contains(CompatFeatures::ORPHAN_FILE),
        "an orphan file with no journal to replay it"
    );
}

/// Above the floor nothing changes: the journal is wanted there, and a
/// consumer with no clean unmount needs it.
#[test]
fn a_filesystem_large_enough_still_gets_its_journal() {
    for mib in [8u64, 16, 64, 256] {
        let geom = geometry(mib * MIB);
        assert!(
            geom.features.compat.contains(CompatFeatures::HAS_JOURNAL),
            "{mib} MiB lost its journal"
        );
        assert!(
            geom.features.compat.contains(CompatFeatures::ORPHAN_FILE),
            "{mib} MiB lost its orphan file"
        );
    }
}

/// Asking for no journal has always worked, and must keep working — this is
/// the path the correction reuses, not a new one.
#[test]
fn asking_for_no_journal_is_unchanged() {
    let params = Params::new(Profile::Ext4)
        .sector_size(4096)
        .no_journal();
    let geom = Geometry::compute(64 * MIB, &params).unwrap();
    assert!(!geom.features.compat.contains(CompatFeatures::HAS_JOURNAL));
    assert!(!geom.features.compat.contains(CompatFeatures::ORPHAN_FILE));
}

/// An explicit block count is the caller's decision and is not second-guessed
/// by the size class — only `JournalSize::Default` defers to it.
#[test]
fn an_explicitly_sized_journal_survives_a_small_filesystem() {
    let mut params = Params::new(Profile::Ext4).sector_size(4096);
    params.journal = JournalSize::Blocks(64);
    let geom = Geometry::compute(4 * MIB, &params).unwrap();
    assert!(
        geom.features.compat.contains(CompatFeatures::HAS_JOURNAL),
        "an explicitly sized journal was dropped by the size class"
    );
}

/// ext2 has no journal to begin with, so nothing here can change that.
#[test]
fn ext2_is_untouched() {
    let params = Params::new(Profile::Ext2).sector_size(4096);
    for size in [MIB, 64 * MIB] {
        let geom = Geometry::compute(size, &params).unwrap();
        assert!(!geom.features.compat.contains(CompatFeatures::HAS_JOURNAL));
    }
}
