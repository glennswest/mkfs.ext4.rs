//! Which block size a device's sector size and capacity produce.
//!
//! Real drives report one of two logical sector sizes: 512 or 4096. That
//! number is a floor, not a suggestion — a filesystem whose blocks are smaller
//! than a sector cannot be written a block at a time, because the drive has no
//! way to write less than a sector. The kernel hides this behind a
//! read-modify-write; a smaller implementation such as lwext4 does not, and
//! simply refuses every write.
//!
//! That is not hypothetical. It is the most likely cause of stormblock#39: a
//! 256 MiB volume on 4 KiB-sector storage, formatted with 1 KiB blocks because
//! its *size class* called for them, mounting cleanly and rejecting every
//! write.
//!
//! Every expected value below was measured from `mke2fs` 1.47.3 on a real loop
//! device of that sector size — not derived from this implementation.

use mkfs_ext4::layout::Geometry;
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The block size we choose for a device of this capacity and sector size.
fn block_size_for(size: u64, sector_size: u32) -> u32 {
    let params = Params::new(Profile::Ext4).sector_size(sector_size);
    Geometry::compute(size, &params)
        .expect("geometry should be computable")
        .block_size
}

#[test]
fn block_size_matches_mke2fs_on_both_real_sector_sizes() {
    // Measured on Fedora 43, e2fsprogs 1.47.3, against loop devices created
    // with `losetup -b <sector>`:
    //
    //     sectors  16M   64M   512M  1G    8G    64G
    //     512      1024  1024  4096  4096  4096  4096
    //     4096     4096  4096  4096  4096  4096  4096
    let cases: &[(u64, u32, u32)] = &[
        (16 * MIB, 512, 1024),
        (64 * MIB, 512, 1024),
        (512 * MIB, 512, 4096),
        (GIB, 512, 4096),
        (8 * GIB, 512, 4096),
        (64 * GIB, 512, 4096),
        // The same capacities on 4 KiB sectors. The two small ones are the
        // interesting rows: their size class asks for 1024 and the sector size
        // overrules it.
        (16 * MIB, 4096, 4096),
        (64 * MIB, 4096, 4096),
        (512 * MIB, 4096, 4096),
        (GIB, 4096, 4096),
        (8 * GIB, 4096, 4096),
        (64 * GIB, 4096, 4096),
    ];

    for &(size, sector, expected) in cases {
        let got = block_size_for(size, sector);
        assert_eq!(
            got, expected,
            "{} MiB on {sector}-byte sectors: chose {got}, mke2fs chooses {expected}",
            size / MIB
        );
    }
}

#[test]
fn a_four_kilobyte_sector_lifts_a_small_volume_off_its_size_class() {
    // The pair that matters, stated on its own: identical capacity, different
    // sector size, different answer. Getting this wrong produces a filesystem
    // that mounts and cannot be written to.
    assert_eq!(block_size_for(64 * MIB, 512), 1024);
    assert_eq!(block_size_for(64 * MIB, 4096), 4096);
}

#[test]
fn a_block_can_never_be_smaller_than_a_sector() {
    // Across every capacity and both real sector sizes, and then some absurd
    // ones, in case storage ever reports them.
    for sector in [512u32, 1024, 2048, 4096, 8192] {
        for size in [16 * MIB, 64 * MIB, 512 * MIB, GIB, 64 * GIB] {
            let block = block_size_for(size, sector);
            assert!(
                block >= sector,
                "{} MiB on {sector}-byte sectors chose a {block}-byte block, \
                 which the device cannot write",
                size / MIB
            );
            assert_eq!(block % sector, 0, "a block must be whole sectors");
        }
    }
}

#[test]
fn an_explicit_block_size_still_cannot_go_below_the_sector() {
    // Asking for 1024 on 4 KiB sectors is asking for the stormblock#39 bug.
    // It is refused rather than honoured.
    let params = Params::new(Profile::Ext4).sector_size(4096).block_size(1024);
    let result = Geometry::compute(64 * MIB, &params);
    let message = match result {
        Ok(geometry) => panic!(
            "a 1024-byte block on a 4096-byte sector device should be refused, \
             got {}",
            geometry.block_size
        ),
        Err(error) => error.to_string(),
    };
    // And refused for the stated reason — an assertion that only checks for
    // an error would pass just as happily if the geometry failed for some
    // unrelated reason.
    assert!(
        message.contains("512") || message.contains("4096") || message.contains("sector"),
        "refused, but not because of the sector size: {message}"
    );
}
