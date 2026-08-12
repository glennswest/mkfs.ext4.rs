//! Our filesystems, diffed against real `mke2fs` output.
//!
//! The golden fixtures are complete filesystem images produced by e2fsprogs
//! 1.47.3, not just their `dumpe2fs` text. Formatting the same geometry with
//! the same UUID, hash seed and timestamp and then diffing the two is the
//! strongest form of the claim this crate makes — and it is the method
//! stormblock#39 asked for in as many words: *"the diff is the specification"*.
//!
//! Differences of [`Significance::Structural`] are failures. Identity and
//! incidental differences are not, and are printed when the test fails so the
//! reader can see what was ignored.

use std::io::Read;

use mkfs_ext4::compare::{compare, CompareOptions, Significance};
use mkfs_ext4::device::MemDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

/// The UUID, hash seed and timestamp the golden images were pinned to.
const GOLDEN_UUID: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
];
const GOLDEN_HASH_SEED: [u8; 16] = [
    0x87, 0x65, 0x43, 0x21, 0x43, 0x21, 0x87, 0x65, 0xcb, 0xa9, 0x98, 0x76, 0x54, 0x32, 0x1c, 0xba,
];
const GOLDEN_TIME: u64 = 1_700_000_000;

/// Load a golden image into memory.
fn golden(name: &str) -> MemDevice {
    let path = format!(
        "{}/tests/golden/{name}.img.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("opening {path}: {e}"));
    let mut decoder = flate2::read::GzDecoder::new(file);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .unwrap_or_else(|e| panic!("decompressing {path}: {e}"));

    let dev = MemDevice::new(raw.len() as u64);
    // MemDevice starts zeroed; write the image into it.
    futures_write(&dev, &raw);
    dev
}

/// Write a whole image into a memory device, synchronously.
fn futures_write(dev: &MemDevice, raw: &[u8]) {
    // MemDevice's writes never actually await, so this drives the future to
    // completion in one poll rather than needing a runtime.
    let fut = mkfs_ext4::device::BlockDevice::write_at(dev, 0, raw);
    futures_lite_block_on(fut).expect("writing a fixture into memory cannot fail");
}

/// Minimal block-on, so loading a fixture needs no runtime of its own.
fn futures_lite_block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => continue,
        }
    }
}

struct Case {
    name: &'static str,
    size: u64,
    params: fn() -> Params,
}

fn pinned(profile: Profile) -> Params {
    let mut p = Params::new(profile).uuid(GOLDEN_UUID).mkfs_time(GOLDEN_TIME);
    p.hash_seed = Some(GOLDEN_HASH_SEED);
    p
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "ext4-64m-default",
            size: 64 * MIB,
            params: || pinned(Profile::Ext4),
        },
        Case {
            name: "ext4-64m-nojournal",
            size: 64 * MIB,
            params: || pinned(Profile::Ext4).no_journal(),
        },
        Case {
            name: "ext2-64m",
            size: 64 * MIB,
            params: || pinned(Profile::Ext2),
        },
        Case {
            name: "ext3-64m",
            size: 64 * MIB,
            params: || pinned(Profile::Ext3),
        },
        Case {
            name: "ext4-16m-1k",
            size: 16 * MIB,
            params: || pinned(Profile::Ext4).no_journal().block_size(1024),
        },
        Case {
            name: "ext4-256m",
            size: 256 * MIB,
            params: || pinned(Profile::Ext4),
        },
    ]
}

/// Nothing structural may differ from what real `mke2fs` wrote.
#[tokio::test]
async fn structurally_matches_every_golden_reference() {
    let mut failures = Vec::new();

    for case in cases() {
        let reference = golden(case.name);

        let ours = MemDevice::new(case.size);
        format(&ours, &(case.params)())
            .await
            .unwrap_or_else(|e| panic!("{}: formatting: {e}", case.name));

        let a = Filesystem::open(&reference).await.unwrap();
        let b = Filesystem::open(&ours).await.unwrap();

        let report = compare(&a, &b, &CompareOptions::structural()).await.unwrap();
        for diff in report.at_least(Significance::Structural) {
            failures.push(format!("{}: {diff}", case.name));
        }
    }

    assert!(
        failures.is_empty(),
        "structural differences from real mke2fs (mke2fs first, ours second):\n  {}",
        failures.join("\n  ")
    );
}

/// With identity pinned, even the UUID and checksum seed should agree — which
/// is a much stronger statement than "the layout matches".
#[tokio::test]
async fn identity_matches_when_it_is_pinned() {
    let case = &cases()[1]; // ext4-64m-nojournal
    let reference = golden(case.name);
    let ours = MemDevice::new(case.size);
    format(&ours, &(case.params)()).await.unwrap();

    let a = Filesystem::open(&reference).await.unwrap();
    let b = Filesystem::open(&ours).await.unwrap();

    assert_eq!(a.superblock().uuid, b.superblock().uuid);
    assert_eq!(a.superblock().csum_seed(), b.superblock().csum_seed());
    assert_eq!(a.superblock().hash_seed, b.superblock().hash_seed);
    assert_eq!(a.superblock().mkfs_time, b.superblock().mkfs_time);
}

/// The comparison is only meaningful if it can also tell things apart.
#[tokio::test]
async fn the_comparison_notices_a_real_difference() {
    let reference = golden("ext4-64m-nojournal");

    // Same size, deliberately different block size.
    let ours = MemDevice::new(64 * MIB);
    let params = pinned(Profile::Ext4).no_journal().block_size(4096);
    format(&ours, &params).await.unwrap();

    let a = Filesystem::open(&reference).await.unwrap();
    let b = Filesystem::open(&ours).await.unwrap();
    let report = compare(&a, &b, &CompareOptions::structural()).await.unwrap();

    assert!(
        !report.structurally_identical(),
        "a 1 KiB and a 4 KiB filesystem must not compare equal"
    );
}
