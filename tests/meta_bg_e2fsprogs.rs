//! A descriptor flush under meta_bg, against real `e2fsck` (#33).
//!
//! `Filesystem::flush_group_descs` is what fio-ext4 calls after it writes.
//! Under meta_bg it used to write the whole descriptor table after every
//! backup superblock, over that group's bitmaps and inode table. Here a
//! meta_bg filesystem is formatted, its descriptors changed and flushed, and
//! `e2fsck -fn` must find it clean.
//!
//! Needs `e2fsck` on the host; without it the test says so and passes.

#![cfg(feature = "std")]

use std::process::Command;

use mkfs_ext4::device::{BlockDevice, FileDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{check, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const GIB: u64 = 1024 * 1024 * 1024;

#[tokio::test]
async fn a_meta_bg_descriptor_flush_checks_clean_with_e2fsck() {
    if Command::new("e2fsck").arg("-V").output().is_err() {
        eprintln!("e2fsck not found: skipping the e2fsprogs differential test");
        return;
    }
    // 16 GiB at 4 KiB blocks: 128 groups, two descriptor blocks; 64 GiB:
    // 512 groups, eight. Sparse files, so they cost what is written.
    for (size, features) in [(16 * GIB, "meta_bg"), (64 * GIB, "meta_bg"), (64 * GIB, "^meta_bg")] {
        let name = format!("{} GiB {features}", size / GIB);
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("fs.img");
        let dev = FileDevice::create(&img, size).await.unwrap();
        let params = Params::new(Profile::Ext4)
            .no_journal()
            .features(features)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();

        // Change every descriptor's checksummed content and flush, then put
        // it back and flush again: both flushes reach every copy.
        let mut fs = Filesystem::open(&dev).await.unwrap();
        assert_eq!(fs.meta_bg(), features == "meta_bg", "{name}");
        let real = fs.group_descs().to_vec();
        for desc in fs.group_descs_mut() {
            desc.flags ^= 0x8000;
        }
        fs.flush_group_descs().await.unwrap();
        let reopened = Filesystem::open(&dev).await.unwrap();
        assert!(
            reopened.group_descs().iter().zip(&real).all(|(a, b)| a.flags == b.flags ^ 0x8000),
            "{name}: the flushed descriptors read back"
        );
        drop(reopened);
        fs.group_descs_mut().copy_from_slice(&real);
        fs.flush_group_descs().await.unwrap();
        drop(fs);
        dev.flush().await.unwrap();

        let report = check(&dev, &FsckOptions::check_only().force(true)).await.unwrap();
        assert!(report.is_clean(), "{name}: ours: {:?}", report.problems);
        drop(dev);

        let out = Command::new("e2fsck").arg("-fn").arg(&img).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{name}: e2fsck -fn:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
