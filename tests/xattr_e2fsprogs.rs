//! Extended-attribute blocks, against real e2fsprogs (#25).
//!
//! `debugfs ea_set` writes attributes too large for the inode, which e2fsprogs
//! spills into a block of their own (`i_file_acl`). Real `e2fsck -fn` must
//! find the image clean, and so must this checker: the block counts toward
//! each inode's `i_blocks` and is in use in the bitmap.
//!
//! Needs `debugfs` and `e2fsck` (e2fsprogs 1.47 or later) on the host, and
//! nothing else — no root, no mount. Without them the test says so and
//! passes.

#![cfg(feature = "std")]

use std::path::Path;
use std::process::Command;

use mkfs_ext4::device::{BlockDevice, FileDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{check, FsckOptions};
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

fn have_e2fsprogs() -> bool {
    let ok = |tool: &str| Command::new(tool).arg("-V").output().is_ok();
    if ok("debugfs") && ok("e2fsck") {
        return true;
    }
    eprintln!("debugfs / e2fsck not found: skipping the e2fsprogs differential test");
    false
}

fn run(tool: &str, args: &[&str], image: &Path) -> (i32, String) {
    let out = Command::new(tool).args(args).arg(image).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

async fn case(name: &str, params: Params, size: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let img = dir.join("fs.img");
    let dev = FileDevice::create(&img, size).await.unwrap();
    format(&dev, &params).await.unwrap();
    dev.flush().await.unwrap();
    drop(dev);

    // Values too big for any inode, so each goes to a block.
    let label = dir.join("label");
    std::fs::write(&label, vec![b'x'; 600]).unwrap();
    let acl = dir.join("acl");
    std::fs::write(&acl, vec![b'a'; 300]).unwrap();
    let data = dir.join("data");
    std::fs::write(&data, b"hello\n").unwrap();

    let (l, a, d) = (label.display(), acl.display(), data.display());
    let commands = format!(
        "mkdir /d\n\
         write {d} /f0\n\
         write {d} /f1\n\
         write {d} /d/f2\n\
         ea_set -f {l} /f0 security.selinux\n\
         ea_set -f {a} /f0 user.acl\n\
         ea_set -f {l} /f1 user.big\n\
         ea_set -f {l} /d security.selinux\n\
         ea_set -f {l} / security.selinux\n\
         ea_set -f {a} /d/f2 user.other\n"
    );
    let cmd = dir.join("cmd");
    std::fs::write(&cmd, &commands).unwrap();
    let (code, text) = run("debugfs", &["-w", "-f", cmd.to_str().unwrap()], &img);
    assert_eq!(code, 0, "{name}: debugfs failed:\n{text}");

    let (code, text) = run("e2fsck", &["-fn"], &img);
    assert_eq!(code, 0, "{name}: e2fsck -fn:\n{text}");

    let dev = FileDevice::open(&img).await.unwrap();
    let fs = Filesystem::open(&dev).await.unwrap();
    let mut spilled = 0;
    for path in ["/", "/d", "/f0", "/f1", "/d/f2"] {
        let inum = fs.resolve_path(path).await.unwrap().unwrap();
        if fs.read_inode(inum).await.unwrap().file_acl != 0 {
            spilled += 1;
        }
    }
    assert_eq!(spilled, 5, "{name}: debugfs left an attribute set in the inode");
    drop(fs);

    let report = check(&dev, &FsckOptions::check_only().force(true)).await.unwrap();
    let ours = report
        .problems
        .iter()
        .map(|p| format!("[{}] {}", p.code, p.message))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(report.is_clean(), "{name}: e2fsck clean, ours:\n{ours}");
}

#[tokio::test]
async fn xattr_blocks_written_by_debugfs_check_clean() {
    if !have_e2fsprogs() {
        return;
    }
    let base = || {
        Params::new(Profile::Ext4)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000)
    };
    case("ext4 1k", base(), 32 * MIB).await;
    case("ext4 4k", base().block_size(4096), 64 * MIB).await;
    case("ext4 4k no journal", base().block_size(4096).no_journal(), 64 * MIB).await;
    case(
        "ext2",
        Params::new(Profile::Ext2)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000),
        32 * MIB,
    )
    .await;
}
