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

/// Skipping the writes must not change the filesystem.
///
/// `zeroed_medium` says the device already reads back as zeros, so the inode
/// tables and the journal body need not be written. If that is true, the image
/// it produces must be *identical* to the one produced by writing them — and
/// if it is not, the option is not an optimisation, it is a second format.
#[tokio::test]
async fn skipping_writes_on_a_zeroed_medium_changes_nothing() {
    use mkfs_ext4::device::MemDevice;
    use mkfs_ext4::format::format;

    for profile in [Profile::Ext2, Profile::Ext3, Profile::Ext4] {
        for size in [16 * MIB, 256 * MIB] {
            // The hash seed is random per format unless pinned, so without
            // this the two images differ by sixteen bytes for a reason that
            // has nothing to do with what is being tested.
            let pinned = || {
                let mut p = Params::new(profile)
                    .uuid(*b"0123456789abcdef")
                    .mkfs_time(1_700_000_000);
                p.hash_seed = Some(*b"fedcba9876543210");
                p
            };

            let written = MemDevice::new(size);
            format(&written, &pinned()).await.unwrap();

            // MemDevice starts zeroed, which is exactly the precondition.
            let skipped = MemDevice::new(size);
            format(&skipped, &pinned().zeroed_medium(true)).await.unwrap();

            let (a, b) = (written.to_vec(), skipped.to_vec());
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(
                differing,
                0,
                "{profile:?} at {} MiB: {differing} bytes differ when the zeroing is skipped",
                size / MIB
            );
        }
    }
}

/// A journal too large to describe with the four extents that fit in an inode.
///
/// Above about 56 GiB the journal needs more extents than the inode holds, so
/// they move to an extent tree block of their own. That block is a tree node
/// like any other: with `metadata_csum` it carries a tail holding its checksum,
/// the tail costs an entry's worth of room in the header's `max`, and it sits
/// at `EXT4_EXTENT_TAIL_OFFSET` — after the last entry `max` counts, not at
/// the end of the block.
///
/// Written without either, the filesystem is fine everywhere else and the
/// journal cannot be read at all — `e2fsck` reports "Superblock has an invalid
/// journal (inode 8)" and refuses to check the filesystem. Nothing smaller
/// than 64 GiB reaches this path, which is why every fixture missed it.
///
/// Run at three block sizes on purpose. End-of-block and tail offset coincide
/// only when the room after the header divides into entries with exactly four
/// bytes spare, which 1 KiB and 4 KiB do and 2 KiB does not — so a 4 KiB-only
/// case cannot see a checksum written in the wrong place.
async fn journal_extent_block_is_checksummed_at(block_size: u32) {
    use mkfs_ext4::csum;
    use mkfs_ext4::device::{BlockDevice, FileDevice};
    use mkfs_ext4::format::format;
    use mkfs_ext4::fs::Filesystem;
    use mkfs_ext4::structs::extent::{self, ExtentHeader};
    use mkfs_ext4::structs::superblock::ino;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.img");
    // Sparse, and `zeroed_medium` keeps the writes down to tens of megabytes.
    std::fs::File::create(&path)
        .unwrap()
        .set_len(64 * GIB)
        .unwrap();

    let dev = FileDevice::open(&path).await.unwrap();
    format(
        &dev,
        &Params::new(Profile::Ext4)
            .block_size(block_size)
            .zeroed_medium(true),
    )
    .await
    .unwrap();

    let fs = Filesystem::open(&dev).await.unwrap();
    assert!(fs.has_metadata_csum(), "this test is about the checksum");
    assert_eq!(fs.block_size(), block_size, "the block size was not honoured");

    let journal = fs.read_inode(ino::JOURNAL).await.unwrap();
    let header = ExtentHeader::decode(&journal.block).unwrap();
    assert_eq!(
        header.depth, 1,
        "a 64 GiB journal should not have fitted inline; this test proves nothing"
    );

    // Follow the index to the leaf and check the tail the way e2fsck does.
    let leaf = mkfs_ext4::structs::extent::ExtentIdx::decode(
        &journal.block[extent::HEADER_LEN..extent::HEADER_LEN + extent::ENTRY_LEN],
    )
    .leaf;
    let block = fs.read_block(leaf).await.unwrap();

    let leaf_header = ExtentHeader::decode(&block).unwrap();
    assert_eq!(leaf_header.depth, 0, "the leaf should hold extents");
    assert!(leaf_header.entries > 0, "the leaf should describe the journal");
    assert_eq!(
        leaf_header.max,
        ExtentHeader::max_entries(fs.block_size() as usize, true),
        "max must leave room for the checksum tail"
    );

    // `EXT4_EXTENT_TAIL_OFFSET`: where the kernel and e2fsck look, and the
    // limit of what they checksum.
    let at = extent::tail_offset(leaf_header.max);
    let want = csum::extent_block_csum(fs.csum_seed(), ino::JOURNAL, 0, &block[..at]);
    let got = mkfs_ext4::bytes::get_u32(&block, at);
    assert_eq!(got, want, "the journal's extent block carries no valid checksum");

    let _ = BlockDevice::flush(&dev).await;
}

#[tokio::test]
async fn a_journal_needing_an_external_extent_block_is_checksummed() {
    journal_extent_block_is_checksummed_at(4096).await;
}

/// The case the end-of-block offset gets wrong: 2 KiB blocks leave four bytes
/// between the tail and the end of the block.
#[tokio::test]
async fn a_journal_extent_block_is_checksummed_at_2k_blocks() {
    journal_extent_block_is_checksummed_at(2048).await;
}

#[tokio::test]
async fn a_journal_extent_block_is_checksummed_at_1k_blocks() {
    journal_extent_block_is_checksummed_at(1024).await;
}
