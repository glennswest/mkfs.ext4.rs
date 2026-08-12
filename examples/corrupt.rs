//! Damage a filesystem in a specific, named way, so a checker can be tested.
//!
//! ```text
//! cargo run --example corrupt -- image.img free-count
//! ```
//!
//! Each mode breaks one thing and nothing else, which is what makes the repair
//! verifiable: after `fsck -y`, real `e2fsck` must call the filesystem clean.

use mkfs_ext4::device::FileDevice;
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::structs::superblock::ino;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: corrupt <image> <free-count|block-bitmap|inode-bitmap|link-count|dir-count>"
        );
        std::process::exit(2);
    }
    let path = &args[1];
    let mode = args[2].as_str();

    let dev = FileDevice::open(path).await?;
    let mut fs = Filesystem::open(&dev).await?;

    match mode {
        // The superblock's idea of free space no longer matches the bitmaps.
        "free-count" => {
            fs.superblock_mut().free_blocks_count = 42;
            fs.superblock_mut().free_inodes_count = 7;
            fs.flush_superblock().await?;
        }

        // Blocks marked in use that nothing owns.
        //
        // Targets bytes that are actually free: scribbling over a region the
        // metadata already owns changes nothing and tests nothing.
        "block-bitmap" => {
            let desc = fs.group_descs()[0];
            let mut bitmap = fs.read_block_bitmap(0).await?;
            let free: Vec<usize> = bitmap
                .iter()
                .enumerate()
                .filter(|(_, &b)| b == 0)
                .map(|(i, _)| i)
                .take(32)
                .collect();
            if free.is_empty() {
                eprintln!("group 0 has no wholly free bitmap byte to corrupt");
                std::process::exit(2);
            }
            for i in &free {
                bitmap[*i] = 0xff;
            }
            fs.write_block(desc.block_bitmap, &bitmap).await?;
            println!("marked {} bitmap bytes in use", free.len());
        }

        // Inodes marked in use that no directory refers to.
        "inode-bitmap" => {
            let desc = fs.group_descs()[0];
            let mut bitmap = fs.read_inode_bitmap(0).await?;
            bitmap[20] = 0xff;
            bitmap[21] = 0xff;
            fs.write_block(desc.inode_bitmap, &bitmap).await?;
        }

        // The root's link count no longer matches the names pointing at it.
        "link-count" => {
            let mut root = fs.read_inode(ino::ROOT).await?;
            root.links_count = 99;
            fs.write_inode(ino::ROOT, &root).await?;
        }

        // The group's directory tally is wrong.
        "dir-count" => {
            fs.group_descs_mut()[0].used_dirs_count = 77;
            fs.group_descs_mut()[0].free_blocks_count = 1;
            fs.flush_group_descs().await?;
        }

        other => {
            eprintln!("unknown corruption mode '{other}'");
            std::process::exit(2);
        }
    }

    println!("corrupted {path}: {mode}");
    Ok(())
}
