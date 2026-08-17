//! Diff a directory of raw images against the golden `mke2fs` references.
//!
//! Replays what the compare tool reported the first time it was run, against
//! the formatter as it stood before those differences were fixed.
use std::io::Read;

use mkfs_ext4::compare::{compare, CompareOptions, Significance};
use mkfs_ext4::device::{BlockDevice, MemDevice};
use mkfs_ext4::fs::Filesystem;

async fn load(raw: Vec<u8>) -> MemDevice {
    let dev = MemDevice::new(raw.len() as u64);
    dev.write_at(0, &raw).await.unwrap();
    dev
}

async fn golden(name: &str) -> MemDevice {
    let path = format!("{}/tests/golden/{name}.img.gz", env!("CARGO_MANIFEST_DIR"));
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(file).read_to_end(&mut raw).unwrap();
    load(raw).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).expect("usage: replay <dir>");
    let names = [
        "ext4-64m-default", "ext4-64m-nojournal", "ext2-64m",
        "ext3-64m", "ext4-16m-1k", "ext4-256m",
    ];
    let options = CompareOptions {
        include_identity: false,
        include_incidental: false,
        all_groups: false,
        ..Default::default()
    };

    let mut total = 0;
    for name in names {
        let ours = Filesystem::open(load(std::fs::read(format!("{dir}/{name}.raw"))?).await).await?;
        let theirs = Filesystem::open(golden(name).await).await?;
        let report = compare(&ours, &theirs, &options).await?;
        let structural: Vec<_> = report
            .differences
            .iter()
            .filter(|d| d.significance == Significance::Structural)
            .collect();
        if structural.is_empty() {
            println!("{name}: none");
            continue;
        }
        println!("\n{name}: {} structural", structural.len());
        for d in &structural {
            println!("  [{:?}] {} — ours {} / mke2fs {}", d.area, d.field, d.left, d.right);
            total += 1;
        }
    }
    println!("\n=== {total} structural differences ===");
    Ok(())
}
