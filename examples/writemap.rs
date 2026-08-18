//! Where does a format actually write?
//!
//! A device that stores nothing and records every write, so a 1 TiB format
//! costs no disk and still says exactly which bytes it would have touched.
use std::sync::Mutex;

use async_trait::async_trait;
use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::layout::Geometry;
use mkfs_ext4::params::{Params, Profile};

struct Recorder {
    size: u64,
    writes: Mutex<Vec<(u64, u64)>>,
}

#[async_trait]
impl BlockDevice for Recorder {
    fn size(&self) -> u64 {
        self.size
    }
    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> mkfs_ext4::Result<()> {
        buf.fill(0);
        Ok(())
    }
    async fn write_at(&self, offset: u64, buf: &[u8]) -> mkfs_ext4::Result<()> {
        self.writes.lock().unwrap().push((offset, buf.len() as u64));
        Ok(())
    }
    async fn flush(&self) -> mkfs_ext4::Result<()> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size: u64 = 1024 * 1024 * 1024 * 1024;
    let zeroed = std::env::args().any(|a| a == "--zeroed-medium");

    let dev = Recorder { size, writes: Mutex::new(Vec::new()) };
    let params = Params::new(Profile::Ext4).zeroed_medium(zeroed);
    mkfs_ext4::format::format(&dev, &params).await?;

    let geom = Geometry::compute(size, &params)?;
    let bs = geom.block_size as u64;
    let per_group = geom.blocks_per_group as u64;

    // Classify each written byte by what lives there.
    let mut tally: std::collections::BTreeMap<&str, u64> = Default::default();
    let mut other: Vec<u64> = Vec::new();
    let writes = dev.writes.lock().unwrap();
    // Classify block by block: a single write can span a whole flex group's
    // worth of bitmaps, and attributing all of it to the first block's kind
    // is how a measurement lies to you.
    for &(offset, len) in writes.iter() {
        let first = offset / bs;
        let last = (offset + len - 1) / bs;
        for block in first..=last {
            let group = (block.saturating_sub(geom.first_data_block as u64) / per_group) as u32;
            let g = if group < geom.group_count { geom.group(group).ok() } else { None };
            let group_start = geom.first_data_block as u64 + group as u64 * per_group;
            let has_super = g.as_ref().map(|g| g.has_super).unwrap_or(false);
            let gdt_from = group_start + 1;
            let gdt_to = gdt_from + geom.desc_blocks as u64;
            let rgdt_to = gdt_to + geom.reserved_gdt_blocks as u64;

            let kind = match g {
                Some(ref g) if block == g.block_bitmap => "block bitmaps",
                Some(ref g) if block == g.inode_bitmap => "inode bitmaps",
                Some(ref g) if block >= g.inode_table
                    && block < g.inode_table + geom.itable_blocks_per_group as u64 => "inode tables",
                _ if has_super && block == group_start => "superblocks (primary + backups)",
                _ if has_super && block >= gdt_from && block < gdt_to => "GDT (primary + backups)",
                _ if has_super && block >= gdt_to && block < rgdt_to => "reserved GDT (primary + backups)",
                _ => "journal, root, lost+found, other",
            };
            *tally.entry(kind).or_default() += bs;
            if kind == "journal, root, lost+found, other" {
                other.push(block);
            }
        }
    }

    let total: u64 = tally.values().sum();
    println!("{} groups, {} blocks/group, {}-byte blocks",
             geom.group_count, per_group, bs);
    println!("desc_blocks {}, reserved_gdt_blocks {}, groups with a superblock copy: {}",
             geom.desc_blocks, geom.reserved_gdt_blocks,
             (0..geom.group_count).filter(|&g| geom.group(g).map(|l| l.has_super).unwrap_or(false)).count());
    println!("zeroed_medium: {zeroed}");
    for (kind, bytes) in &tally {
        println!("  {:<44} {:>9.1} MiB", kind, *bytes as f64 / 1024.0 / 1024.0);
    }
    println!("  {:<44} {:>9.1} MiB in {} writes", "TOTAL", total as f64 / 1024.0 / 1024.0, writes.len());

    // What is left over, as contiguous runs, biggest first.
    other.sort_unstable();
    other.dedup();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for b in other {
        match runs.last_mut() {
            Some(last) if last.0 + last.1 == b => last.1 += 1,
            _ => runs.push((b, 1)),
        }
    }
    runs.sort_by_key(|r| std::cmp::Reverse(r.1));
    println!("\n  the leftover, as runs (block, length):");
    for (start, len) in runs.iter().take(8) {
        let group = start.saturating_sub(geom.first_data_block as u64) / per_group;
        println!("    block {start:>10} + {len:<7} = {:>6.1} MiB   (group {group})",
                 *len as f64 * bs as f64 / 1024.0 / 1024.0);
    }
    println!("    {} runs in total", runs.len());
    Ok(())
}
