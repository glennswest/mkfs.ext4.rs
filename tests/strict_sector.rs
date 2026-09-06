//! Formatting, checking and writing on a device that refuses anything but
//! whole sectors at a sector boundary.
//!
//! That is how a stormblock thin volume behaves, and how any device that
//! enforces its logical block is entitled to behave: a 1024-byte superblock
//! at byte 1024, a lone 256-byte inode, a descriptor table at its exact
//! length — all `EINVAL` on a 4 KiB-sector device (#5, fio.ext4.rs#4). A loop
//! device hides every one of them behind a kernel read-modify-write, which is
//! why none of the other tests could see it.
//!
//! The rule these tests hold the crate to: every device operation is a whole
//! filesystem block at a block boundary. A block is never smaller than a
//! sector, so alignment follows.

use mkfs_ext4::device::{BlockDevice, MemDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{self, FsckOptions};
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::superblock::ino;

const MIB: u64 = 1024 * 1024;

fn pinned(profile: Profile) -> Params {
    let mut p = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    p.hash_seed = Some(*b"fedcba9876543210");
    p
}

async fn assert_clean(dev: &MemDevice, what: &str) {
    let report = fsck::check(dev, &FsckOptions::check_only()).await.unwrap();
    assert!(
        report.is_clean(),
        "{what}: not clean:\n{}",
        report
            .problems
            .iter()
            .map(|p| format!("  [pass {} {}] {}", p.pass, p.code, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Format on a strict device, and prove the image is the one a permissive
/// device would have received — writing in whole blocks must change nothing
/// about what ends up on the medium.
async fn format_strict(profile: Profile, size: u64, sector: u32) -> MemDevice {
    let strict = MemDevice::strict(size, sector);
    format(&strict, &pinned(profile))
        .await
        .unwrap_or_else(|e| panic!("{profile:?} {} MiB on {sector}-byte sectors: {e}", size / MIB));

    let loose = MemDevice::with_sector_size(size, sector);
    format(&loose, &pinned(profile)).await.unwrap();

    let (a, b) = (strict.to_vec(), loose.to_vec());
    let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert_eq!(
        differing,
        0,
        "{profile:?} {} MiB on {sector}-byte sectors: {differing} bytes differ between \
         a strict and a permissive device",
        size / MIB
    );
    strict
}

#[tokio::test]
async fn formats_and_checks_on_a_device_enforcing_4k_sectors() {
    for profile in [Profile::Ext2, Profile::Ext3, Profile::Ext4] {
        for size in [16 * MIB, 64 * MIB, 256 * MIB] {
            let dev = format_strict(profile, size, 4096).await;
            assert_clean(&dev, &format!("{profile:?} {} MiB", size / MIB)).await;
        }
    }
}

/// The smallest template stormblock provisions, and the first one the issue
/// saw fail — at offset 16384, a 4 KiB-aligned inode-table block written one
/// inode at a time.
#[tokio::test]
async fn formats_the_one_mebibyte_template_on_4k_sectors() {
    let dev = format_strict(Profile::Ext4, MIB, 4096).await;
    assert_clean(&dev, "1 MiB ext4").await;
}

/// A 512-byte sector is the other real one, and 1 KiB blocks on it are the
/// case where an inode (256 bytes) is smaller than a sector while the
/// superblock is not. The superblock never tripped here; the inodes did.
#[tokio::test]
async fn formats_and_checks_on_a_device_enforcing_512_byte_sectors() {
    for profile in [Profile::Ext2, Profile::Ext4] {
        let dev = format_strict(profile, 16 * MIB, 512).await;
        assert_clean(&dev, &format!("{profile:?} 16 MiB on 512")).await;
    }
}

/// The read side (fio.ext4.rs#4): opening reads byte 1024 through the
/// sectors that hold it, an inode through its table block, and writing either
/// back goes through the same block.
#[tokio::test]
async fn opens_reads_and_writes_back_on_a_device_enforcing_4k_sectors() {
    let dev = format_strict(Profile::Ext4, 256 * MIB, 4096).await;

    let mut fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(fs.block_size(), 4096);

    // An inode round trip: read, change, write, and read the neighbour to
    // show the rest of the block came through untouched.
    let lost_found_before = fs.read_inode(ino::LOST_FOUND).await.unwrap();
    let mut root = fs.read_inode(ino::ROOT).await.unwrap();
    root.mtime = 1_800_000_000;
    fs.write_inode(ino::ROOT, &root).await.unwrap();
    assert_eq!(fs.read_inode(ino::ROOT).await.unwrap().mtime, 1_800_000_000);
    assert_eq!(
        fs.read_inode(ino::LOST_FOUND).await.unwrap(),
        lost_found_before,
        "writing inode 2 must not disturb inode 11 in the same block"
    );

    // The superblock round trip, and its checksum re-read by fsck.
    fs.superblock_mut().wtime = 1_800_000_000;
    fs.flush_superblock().await.unwrap();
    fs.flush_group_descs().await.unwrap();
    fs.device().flush().await.unwrap();
    drop(fs);

    let fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(fs.superblock().wtime, 1_800_000_000);
    assert_eq!(fs.read_inode(ino::ROOT).await.unwrap().mtime, 1_800_000_000);

    // A backup superblock, read as the block it occupies.
    let sb = fs.superblock();
    assert!(fs.group_count() > 1, "256 MiB should have a backup to open");
    let backup = sb.first_data_block as u64 + sb.blocks_per_group as u64;
    let from_backup = Filesystem::open_with_backup(&dev, backup).await.unwrap();
    assert_eq!(from_backup.superblock().uuid, sb.uuid);

    assert_clean(&dev, "after inode and superblock writes").await;
}
