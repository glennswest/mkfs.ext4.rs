//! Write a filesystem image to a file, for checking against real e2fsck.
//!
//! ```text
//! cargo run --example mkimage -- out.img 64 ext4 nojournal
//! ```

use mkfs_ext4::device::FileDevice;
use mkfs_ext4::format::format;
use mkfs_ext4::params::{Params, Profile};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: mkimage <path> <size-mib> <ext2|ext3|ext4> [nojournal] [block=N]");
        std::process::exit(2);
    }

    let path = &args[1];
    let size_mib: u64 = args[2].parse()?;
    let profile: Profile = args[3].parse()?;

    let mut params = Params::new(profile)
        // Pinned so the image is reproducible, matching the golden references.
        .uuid(*b"\x12\x34\x56\x78\x12\x34\x56\x78\x9a\xbc\x12\x34\x56\x78\x9a\xbc")
        .mkfs_time(1_700_000_000);
    params.hash_seed = Some(*b"\x87\x65\x43\x21\x43\x21\x87\x65\xcb\xa9\x98\x76\x54\x32\x1c\xba");

    for arg in &args[4..] {
        if arg == "nojournal" {
            params = params.no_journal();
        } else if let Some(n) = arg.strip_prefix("block=") {
            params = params.block_size(n.parse()?);
        } else if let Some(l) = arg.strip_prefix("label=") {
            params = params.label(l);
        }
    }

    let size = size_mib * 1024 * 1024;
    let dev = FileDevice::create(path, size).await?;
    let report = format(&dev, &params).await?;

    println!(
        "{path}: {} blocks of {} bytes, {} inodes, {} groups, {} free blocks, journal {} blocks",
        report.blocks_count,
        report.block_size,
        report.inodes_count,
        report.group_count,
        report.free_blocks_count,
        report.journal_blocks,
    );
    println!("uuid {}", report.uuid_string());
    Ok(())
}
