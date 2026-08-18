//! Run our fsck over the recorded real-mke2fs images.
use std::io::Read;
use mkfs_ext4::device::{BlockDevice, MemDevice};
use mkfs_ext4::fsck::{self, FsckOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for name in ["ext4-64m-default", "ext4-256m", "ext2-64m", "ext3-64m", "ext4-16m-1k"] {
        let path = format!("{}/tests/golden/{name}.img.gz", env!("CARGO_MANIFEST_DIR"));
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&path)?).read_to_end(&mut raw)?;
        let dev = MemDevice::new(raw.len() as u64);
        dev.write_at(0, &raw).await?;
        let report = fsck::check(&dev, &FsckOptions::check_only()).await?;
        if report.is_clean() {
            println!("  {name}: clean");
        } else {
            println!("  {name}: {} problems", report.problems.len());
            for p in report.problems.iter().take(3) {
                println!("      [pass {} {}] {}", p.pass, p.code, p.message);
            }
        }
    }
    Ok(())
}
