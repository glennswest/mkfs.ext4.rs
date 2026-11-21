//! `mkfs-ext4 DEVICE BLOCKS` formats `BLOCKS`, not the device (#46, finding
//! `9bfee0c81803`).
//!
//! The count used to reach the `-n` preview only; the real format planned from
//! the whole device. Now one size serves both, read as `mke2fs` reads it
//! (blocks of `-b`, 1 KiB without it, or a size suffix), and the format sees
//! only that much of the device, so the bytes past the filesystem are left as
//! they were.

#![cfg(feature = "cli")]

use std::path::Path;
use std::process::{Command, Output};

const MIB: u64 = 1024 * 1024;
const DEVICE: u64 = 64 * MIB;
const FILL: u8 = 0xa5;

/// A 64 MiB image that is all `FILL`, so anything written shows.
fn image(dir: &Path) -> String {
    let path = dir.join("fs.img");
    std::fs::write(&path, vec![FILL; DEVICE as usize]).unwrap();
    path.to_str().unwrap().to_string()
}

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin).args(args).output().unwrap()
}

fn mkfs(args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_mkfs-ext4"), args)
}

fn text(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `s_blocks_count_lo` and `s_log_block_size` of the primary superblock.
fn superblock_size(img: &[u8]) -> u64 {
    let sb = &img[1024..2048];
    assert_eq!(u16::from_le_bytes([sb[0x38], sb[0x39]]), 0xef53, "no superblock");
    let blocks = u32::from_le_bytes(sb[0x04..0x08].try_into().unwrap());
    let log = u32::from_le_bytes(sb[0x18..0x1c].try_into().unwrap());
    u64::from(blocks) * (1024u64 << log)
}

/// Format `args` onto a fresh image; the filesystem is `expect` bytes, every
/// byte after it is untouched, and `fsck-ext4 -fn` finds it clean.
fn formats_exactly(args: &[&str], expect: u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path());
    let mut full = args.to_vec();
    full.insert(0, "-q");
    let at = full.iter().position(|a| *a == "IMG").unwrap();
    full[at] = path.as_str();
    let out = mkfs(&full);
    assert!(out.status.success(), "{args:?}\n{}", text(&out));

    let img = std::fs::read(&path).unwrap();
    assert_eq!(img.len() as u64, DEVICE);
    assert_eq!(superblock_size(&img), expect, "{args:?}");
    let past = img[expect as usize..].iter().position(|&b| b != FILL);
    assert_eq!(past, None, "{args:?}: wrote past the filesystem");

    let out = run(env!("CARGO_BIN_EXE_fsck-ext4"), &["-fn", path.as_str()]);
    assert_eq!(out.status.code(), Some(0), "{args:?}\n{}", text(&out));
}

#[test]
fn a_block_count_bounds_the_format() {
    // The finding's own example: 16384 blocks of 4 KiB in a larger image.
    formats_exactly(&["-b", "4096", "IMG", "4096"], 16 * MIB);
    // Without -b the count is 1 KiB blocks, as mke2fs reads it.
    formats_exactly(&["IMG", "16384"], 16 * MIB);
    formats_exactly(&["-t", "ext2", "-b", "1024", "IMG", "20000"], 20000 * 1024);
    // Size suffixes, as mke2fs's parse_num_blocks2 takes them.
    formats_exactly(&["IMG", "24M"], 24 * MIB);
    formats_exactly(&["-b", "4096", "IMG", "65536s"], 32 * MIB);
    // No count: the whole device.
    formats_exactly(&["IMG"], DEVICE);
}

#[test]
fn the_preview_and_the_format_agree() {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path());
    let path = path.as_str();
    for args in [
        &["-b", "4096", path, "4096"][..],
        &[path, "16384"][..],
        &[path][..],
    ] {
        let mut preview = vec!["-n"];
        preview.extend_from_slice(args);
        let out = mkfs(&preview);
        assert!(out.status.success(), "{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let blocks: u64 = stdout
            .lines()
            .find_map(|l| l.strip_prefix("Blocks:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bsize: u64 = stdout
            .lines()
            .find_map(|l| l.strip_prefix("Block size:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // -n wrote nothing.
        assert!(std::fs::read(path).unwrap().iter().all(|&b| b == FILL));

        let mut real = vec!["-q"];
        real.extend_from_slice(args);
        let out = mkfs(&real);
        assert!(out.status.success(), "{}", text(&out));
        let img = std::fs::read(path).unwrap();
        assert_eq!(superblock_size(&img), blocks * bsize, "{args:?}");
        std::fs::write(path, vec![FILL; DEVICE as usize]).unwrap();
    }
}

#[test]
fn a_count_past_the_device_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = image(dir.path());
    let path = path.as_str();
    for args in [
        &["-b", "4096", path, "16385"][..],
        &[path, "65537"][..],
        &[path, "1G"][..],
        &["-n", path, "1G"][..],
        &[path, "0"][..],
        &[path, "12x"][..],
        &[path, "M"][..],
        &[path, "99999999999999999999"][..],
    ] {
        let out = mkfs(args);
        assert!(!out.status.success(), "{args:?} was accepted\n{}", text(&out));
        assert!(
            std::fs::read(path).unwrap().iter().all(|&b| b == FILL),
            "{args:?} wrote to the device"
        );
    }
}
