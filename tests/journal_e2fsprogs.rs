//! Journal replay and orphan release, against real e2fsprogs (#7).
//!
//! The logs and orphans here are not written by this crate: `debugfs` writes
//! them (`journal_open` / `journal_write` / `journal_close`, the tools
//! e2fsprogs' own `j_*` tests use, and `set_inode_field` /
//! `set_super_value` for orphans). Each image is then copied, one copy
//! repaired by real `e2fsck -fy` and the other by this checker, and the two
//! compared block by block. Afterwards `e2fsck -fn` must find ours clean.
//!
//! Needs `debugfs` and `e2fsck` (e2fsprogs 1.47 or later) on the host, and
//! nothing else — no root, no mount. Without them each test says so and
//! passes, so a machine without e2fsprogs can still run the suite.

#![cfg(feature = "std")]

use std::path::{Path, PathBuf};
use std::process::Command;

use mkfs_ext4::device::{BlockDevice, FileDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{check, FsckOptions};
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::IncompatFeatures;

const MIB: u64 = 1024 * 1024;

fn have_e2fsprogs() -> bool {
    let ok = |tool: &str| Command::new(tool).arg("-V").output().is_ok();
    if ok("debugfs") && ok("e2fsck") {
        return true;
    }
    eprintln!("debugfs / e2fsck not found: skipping the e2fsprogs differential test");
    false
}

async fn image(dir: &Path, profile: Profile, size: u64) -> PathBuf {
    let path = dir.join("fs.img");
    let dev = FileDevice::create(&path, size).await.unwrap();
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params).await.unwrap();
    dev.flush().await.unwrap();
    path
}

fn debugfs(image: &Path, commands: &str) -> String {
    let cmd = image.with_extension("cmd");
    std::fs::write(&cmd, commands).unwrap();
    let out = Command::new("debugfs")
        .arg("-w")
        .arg("-f")
        .arg(&cmd)
        .arg(image)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "debugfs failed:\n{commands}\n{text}");
    text
}

fn e2fsck(image: &Path, flags: &str) -> (i32, String) {
    let out = Command::new("e2fsck").arg(flags).arg(image).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Blocks that differ between two images, less the one holding the primary
/// superblock (check times, mount counts, bytes written).
fn differing_blocks(a: &Path, b: &Path, block_size: usize) -> Vec<u64> {
    let a = std::fs::read(a).unwrap();
    let b = std::fs::read(b).unwrap();
    assert_eq!(a.len(), b.len());
    let sb_block = (1024 / block_size) as u64;
    a.chunks(block_size)
        .zip(b.chunks(block_size))
        .enumerate()
        .filter(|(i, (x, y))| *i as u64 != sb_block && x != y)
        .map(|(i, _)| i as u64)
        .collect()
}

/// Repair one copy with e2fsck and one with this checker; return both paths
/// and what each said.
async fn both(dir: &Path, img: &Path) -> (PathBuf, PathBuf, String, String) {
    let theirs = dir.join("theirs.img");
    let ours = dir.join("ours.img");
    std::fs::copy(img, &theirs).unwrap();
    std::fs::copy(img, &ours).unwrap();
    let (code, their_text) = e2fsck(&theirs, "-fy");
    assert!(code <= 1, "e2fsck -fy exited {code}:\n{their_text}");
    let dev = FileDevice::open(&ours).await.unwrap();
    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    let our_text = report
        .problems
        .iter()
        .map(|p| format!("[{}] {}", p.code, p.message))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(report.exit_code() <= 1, "our repair exited {}:\n{our_text}", report.exit_code());
    drop(dev);
    let (code, text) = e2fsck(&ours, "-fn");
    assert_eq!(code, 0, "e2fsck -fn on our repair:\n{text}\nours:\n{our_text}");
    (theirs, ours, their_text, our_text)
}

/// Spare blocks near the end of the filesystem, with something to log there.
async fn payload(dir: &Path, img: &Path, n: usize, byte: u8) -> (Vec<u64>, PathBuf) {
    let dev = FileDevice::open(img).await.unwrap();
    let fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let last = fs.superblock().blocks_count;
    let blocks = (0..n as u64).map(|i| last - 100 + i * 3).collect();
    let file = dir.join(format!("payload-{byte:02x}"));
    let mut data = Vec::new();
    for i in 0..n {
        let mut block = vec![byte ^ i as u8; bs];
        // Every other block starts with the JBD2 magic, so it is logged escaped.
        if i % 2 == 1 {
            block[..4].copy_from_slice(&0xc03b_3998u32.to_be_bytes());
        }
        data.extend(block);
    }
    std::fs::write(&file, data).unwrap();
    (blocks, file)
}

fn list(blocks: &[u64]) -> String {
    blocks.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

async fn needs_recovery(img: &Path) -> bool {
    let dev = FileDevice::open(img).await.unwrap();
    let fs = Filesystem::open(&dev).await.unwrap();
    fs.superblock().feature_incompat.contains(IncompatFeatures::RECOVER)
}

async fn journal_case(name: &str, profile: Profile, size: u64, script: impl Fn(&[u64], &Path, &[u64], &Path) -> String) {
    if !have_e2fsprogs() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let img = image(dir, profile, size).await;
    let (a, fa) = payload(dir, &img, 4, 0x41).await;
    let (b, fb) = payload(dir, &img, 3, 0x62).await;
    let commands = script(&a, &fa, &b, &fb);
    let out = debugfs(&img, &commands);
    assert!(needs_recovery(&img).await, "{name}: debugfs left no recovery to do:\n{out}");
    let logdump = debugfs(&img, "logdump\n");

    let (theirs, ours, their_text, our_text) = both(dir, &img).await;
    let block_size = {
        let dev = FileDevice::open(&ours).await.unwrap();
        Filesystem::open(&dev).await.unwrap().block_size() as usize
    };
    let diff = differing_blocks(&theirs, &ours, block_size);
    assert!(
        diff.is_empty(),
        "{name}: blocks {diff:?} differ after replay\nlog:\n{logdump}\ne2fsck:\n{their_text}\nours:\n{our_text}"
    );
}

#[tokio::test]
async fn a_plain_journal_replays_as_e2fsck_replays_it() {
    journal_case("plain", Profile::Ext4, 64 * MIB, |a, fa, b, fb| {
        format!(
            "jo\njw -b {} {}\njw -b {} {}\njc\n",
            list(a),
            fa.display(),
            list(b),
            fb.display()
        )
    })
    .await;
}

#[tokio::test]
async fn a_checksummed_journal_replays_as_e2fsck_replays_it() {
    journal_case("csum", Profile::Ext4, 64 * MIB, |a, fa, b, fb| {
        format!(
            "jo -c\njw -b {} {}\njw -b {} {}\njc\n",
            list(a),
            fa.display(),
            list(b),
            fb.display()
        )
    })
    .await;
}

#[tokio::test]
async fn revokes_and_an_uncommitted_tail_replay_as_e2fsck_replays_them() {
    journal_case("revoke", Profile::Ext4, 64 * MIB, |a, fa, b, fb| {
        // a logged, then half of it revoked, then b logged in a transaction
        // that never commits.
        format!(
            "jo -c\njw -b {} {}\njw -r {}\njw -b {} -c {}\njc\n",
            list(a),
            fa.display(),
            list(&a[..2]),
            list(b),
            fb.display()
        )
    })
    .await;
}

#[tokio::test]
async fn an_ext3_journal_replays_as_e2fsck_replays_it() {
    journal_case("ext3", Profile::Ext3, 64 * MIB, |a, fa, b, fb| {
        format!(
            "jo\njw -b {} {}\njw -r {}\njw -b {} {}\njc\n",
            list(a),
            fa.display(),
            list(&a[1..2]),
            list(b),
            fb.display()
        )
    })
    .await;
}

#[tokio::test]
async fn a_4k_journal_replays_as_e2fsck_replays_it() {
    journal_case("4k", Profile::Ext4, 512 * MIB, |a, fa, b, fb| {
        format!(
            "jo -c\njw -b {} {}\njw -b {} {}\njc\n",
            list(a),
            fa.display(),
            list(b),
            fb.display()
        )
    })
    .await;
}

/// Orphans set up by debugfs: an unlinked file on the chain, a linked one to
/// truncate, and both released by e2fsck and by us.
#[tokio::test]
async fn orphans_are_released_as_e2fsck_releases_them() {
    if !have_e2fsprogs() {
        return;
    }
    for (profile, size) in [(Profile::Ext4, 64 * MIB), (Profile::Ext3, 64 * MIB), (Profile::Ext4, 512 * MIB)] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let img = image(dir, profile, size).await;
        let src = dir.join("src");
        std::fs::write(&src, vec![0x5au8; 300 * 1024]).unwrap();
        debugfs(
            &img,
            &format!(
                "write {0} gone\nwrite {0} short\nwrite {0} kept\n",
                src.display()
            ),
        );
        let (gone, short) = {
            let dev = FileDevice::open(&img).await.unwrap();
            let fs = Filesystem::open(&dev).await.unwrap();
            (
                fs.resolve_path("/gone").await.unwrap().unwrap(),
                fs.resolve_path("/short").await.unwrap().unwrap(),
            )
        };
        let out = debugfs(
            &img,
            &format!(
                "unlink gone\nsif <{gone}> links_count 0\nsif <{gone}> dtime {short}\n\
                 sif <{short}> size 10000\nssv last_orphan {gone}\n"
            ),
        );

        let (theirs, ours, their_text, our_text) = both(dir, &img).await;
        let block_size = {
            let dev = FileDevice::open(&ours).await.unwrap();
            Filesystem::open(&dev).await.unwrap().block_size() as usize
        };
        assert!(our_text.contains(&format!("Clearing orphaned inode {gone} ")), "{our_text}");
        assert!(our_text.contains(&format!("Truncating orphaned inode {short} ")), "{our_text}");
        assert!(their_text.contains("orphaned inode"), "{their_text}");

        // Bitmaps, descriptors and counts must agree with e2fsck's. The two
        // inodes themselves may be laid out differently (e2fsck leaves a
        // freed inode's map; how a truncated extent tree is rewritten is
        // a choice), so the inode-table blocks holding them are compared by
        // the check above, not byte for byte.
        // What a free block holds does not matter either.
        let dev = FileDevice::open(&ours).await.unwrap();
        let fs = Filesystem::open(&dev).await.unwrap();
        let mut allowed = Vec::new();
        for inum in [gone, short] {
            let (_, block, _) = fs.inode_location(inum).unwrap();
            allowed.push(block);
        }
        let mut diff = Vec::new();
        for b in differing_blocks(&theirs, &ours, block_size) {
            if allowed.contains(&b) {
                continue;
            }
            let group = fs.group_of_block(b);
            let bitmap = fs.read_block_bitmap(group).await.unwrap();
            if Filesystem::<&FileDevice>::test_bit(&bitmap, b - fs.group_first_block(group)) {
                diff.push(b);
            }
        }
        drop(fs);
        assert!(
            diff.is_empty(),
            "{profile:?} {size}: blocks {diff:?} differ after orphan release\nsetup:\n{out}\ne2fsck:\n{their_text}\nours:\n{our_text}"
        );
    }
}
