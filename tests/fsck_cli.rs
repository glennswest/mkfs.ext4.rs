//! `fsck-ext4`'s command line against `e2fsck`'s (#8).
//!
//! The owner's rule on #6: scripts written for `e2fsck` must work unchanged.
//! So `-p`/`-a`, `-C fd` and `-t` work; `e2fsck`'s flags this checker cannot
//! honour are refused with its usage exit, 16; an option conflict is its
//! `fatal_error`, 8; and no usage error ever exits 2, which to an `e2fsck`
//! caller means "errors corrected, reboot".

#![cfg(feature = "cli")]

use std::path::Path;
use std::process::{Command, Output};

use mkfs_ext4::device::FileDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::params::{Params, Profile};
use mkfs_ext4::structs::superblock::ino;

const MIB: u64 = 1024 * 1024;

async fn image(dir: &Path) -> String {
    let path = dir.join("fs.img");
    let dev = FileDevice::create(&path, 16 * MIB).await.unwrap();
    format(&dev, &Params::new(Profile::Ext4)).await.unwrap();
    path.to_str().unwrap().to_string()
}

fn fsck(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsck-ext4"))
        .args(args)
        .output()
        .unwrap()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn usage_errors_exit_16_never_2() {
    for args in [&["-Q", "x"][..], &[][..], &["--no-such-flag", "x"][..], &["-C"][..]] {
        let out = fsck(args);
        assert_eq!(code(&out), 16, "{args:?}\n{}", text(&out));
    }
}

#[test]
fn help_and_version_exit_0() {
    for args in [&["--help"][..], &["-h"][..], &["-V"][..], &["--version"][..]] {
        let out = fsck(args);
        assert_eq!(code(&out), 0, "{args:?}\n{}", text(&out));
    }
}

#[tokio::test]
async fn unsupported_e2fsck_flags_are_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(dir.path()).await;
    for (args, flag) in [
        (&["-b", "8193"][..], "-b"),
        (&["-B", "4096"][..], "-B"),
        (&["-c"][..], "-c"),
        (&["-cc"][..], "-c"),
        (&["-fD"][..], "-D"),
        (&["-E", "discard"][..], "-E"),
        (&["-j", "/dev/journal"][..], "-j"),
        (&["-k"][..], "-k"),
        (&["-l", "bad.txt"][..], "-l"),
        (&["-L", "bad.txt"][..], "-L"),
        (&["-z", "undo"][..], "-z"),
    ] {
        let mut all = args.to_vec();
        all.push(&img);
        let out = fsck(&all);
        assert_eq!(code(&out), 16, "{args:?}\n{}", text(&out));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(&format!("{flag} (")), "{args:?}\n{}", text(&out));
        assert!(err.contains("not supported"), "{args:?}\n{}", text(&out));
    }
}

#[tokio::test]
async fn conflicting_modes_exit_8_as_e2fsck_does() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(dir.path()).await;
    for args in [&["-n", "-y"][..], &["-p", "-n"][..], &["-a", "-y"][..], &["-py"][..]] {
        let mut all = args.to_vec();
        all.push(&img);
        let out = fsck(&all);
        assert_eq!(code(&out), 8, "{args:?}\n{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("Only one of the options -p/-a, -n or -y may be specified."),
            "{args:?}\n{}",
            text(&out)
        );
    }
}

/// What boot tooling runs: `fsck -a`, `systemd-fsck`'s `-a -C fd`, and the
/// usual hand-typed forms.
#[tokio::test]
async fn preen_progress_and_timing_work_on_a_clean_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(dir.path()).await;
    for args in [
        &["-p"][..],
        &["-a"][..],
        &["-a", "-C", "0"][..],
        &["-pC0"][..],
        &["-p", "-C", "-1"][..],
        &["-pf"][..],
        &["-fy"][..],
        &["-fn"][..],
        &["-t"][..],
        &["-ptt"][..],
    ] {
        let mut all = args.to_vec();
        all.push(&img);
        let out = fsck(&all);
        assert_eq!(code(&out), 0, "{args:?}\n{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("blocks"), "{args:?}\n{}", text(&out));
        assert_eq!(
            stdout.contains("time:"),
            args.iter().any(|a| a.contains('t')),
            "{args:?}\n{}",
            text(&out)
        );
    }
}

#[tokio::test]
async fn preen_repairs_a_wrong_link_count_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(dir.path()).await;
    {
        let fs = Filesystem::open(FileDevice::open(&img).await.unwrap()).await.unwrap();
        let mut root = fs.read_inode(ino::ROOT).await.unwrap();
        root.links_count = 99;
        fs.write_inode(ino::ROOT, &root).await.unwrap();
    }
    let out = fsck(&["-pf", &img]);
    assert_eq!(code(&out), 1, "{}", text(&out));
    let out = fsck(&["-fn", &img]);
    assert_eq!(code(&out), 0, "after preen:\n{}", text(&out));
}

#[tokio::test]
async fn preen_stops_on_what_needs_a_person_and_exits_4() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(dir.path()).await;
    {
        let mut fs = Filesystem::open(FileDevice::open(&img).await.unwrap()).await.unwrap();
        fs.group_descs_mut()[0].checksum ^= 0xffff;
        // Write the table without re-stamping, which flush would do.
        let sb = fs.superblock().clone();
        let desc_size = sb.desc_size() as usize;
        let mut raw = vec![0u8; sb.gdt_blocks() as usize * sb.block_size() as usize];
        for (g, d) in fs.group_descs().iter().enumerate() {
            d.encode_into(&mut raw[g * desc_size..], desc_size);
        }
        let gdt_block = if sb.block_size() == 1024 { 2 } else { 1 };
        fs.write_block(gdt_block, &raw).await.unwrap();
    }
    let out = fsck(&["-a", "-C", "0", &img]);
    assert_eq!(code(&out), 4, "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!(
            "{img}: UNEXPECTED INCONSISTENCY; RUN fsck MANUALLY.\n\t(i.e., without -a or -p options)"
        )),
        "{}",
        text(&out)
    );
}
