//! `mkfs.ext4` — create an ext2, ext3 or ext4 filesystem.
//!
//! Flags follow `mke2fs` where they mean the same thing, so muscle memory and
//! existing scripts carry over.

use clap::Parser;

use mkfs_ext4::device::{BlockDevice, FileDevice};
use mkfs_ext4::format::format;
use mkfs_ext4::params::{JournalSize, Params, Profile};

#[derive(Parser, Debug)]
#[command(
    name = "mkfs.ext4",
    about = "Create an ext2/ext3/ext4 filesystem",
    version
)]
struct Args {
    /// Device or image file to format.
    device: String,

    /// Size in blocks. Defaults to the whole device.
    blocks_count: Option<u64>,

    /// Filesystem type: ext2, ext3 or ext4.
    #[arg(short = 't', long = "type", default_value = "ext4")]
    fs_type: String,

    /// Block size in bytes.
    #[arg(short = 'b', long)]
    block_size: Option<u32>,

    /// The device's logical sector size. The block size is never smaller.
    #[arg(long)]
    sector_size: Option<u32>,

    /// Inode size in bytes.
    #[arg(short = 'I')]
    inode_size: Option<u16>,

    /// Total number of inodes.
    #[arg(short = 'N')]
    inodes_count: Option<u32>,

    /// Bytes per inode.
    #[arg(short = 'i')]
    inode_ratio: Option<u32>,

    /// Percentage of blocks reserved for the super-user.
    #[arg(short = 'm', default_value_t = 5.0)]
    reserved_percent: f64,

    /// Volume label.
    #[arg(short = 'L', long)]
    label: Option<String>,

    /// Filesystem UUID.
    #[arg(short = 'U', long)]
    uuid: Option<String>,

    /// Feature list, as `mke2fs -O` takes it. `^feature` clears one.
    #[arg(short = 'O', long)]
    features: Option<String>,

    /// Blocks per group.
    #[arg(short = 'g')]
    blocks_per_group: Option<u32>,

    /// Groups per flex group.
    #[arg(short = 'G')]
    flex_bg_size: Option<u32>,

    /// Journal size in blocks. Zero creates no journal.
    #[arg(short = 'J', long)]
    journal_blocks: Option<u32>,

    /// Create no journal, whatever the filesystem type would default to.
    #[arg(long)]
    no_journal: bool,

    /// Do not write inode tables; mark the groups uninitialised instead.
    #[arg(long)]
    lazy_itable_init: bool,

    /// Creation timestamp, for a reproducible image.
    #[arg(long)]
    mkfs_time: Option<u64>,

    /// Seconds between multiple-mount-protection heartbeats. Implies -O mmp.
    #[arg(long)]
    mmp_update_interval: Option<u16>,

    /// Report what would be done and write nothing.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Say less.
    #[arg(short = 'q', long)]
    quiet: bool,
}

fn parse_uuid(s: &str) -> anyhow::Result<[u8; 16]> {
    Ok(*uuid::Uuid::parse_str(s)
        .map_err(|e| anyhow::anyhow!("invalid UUID '{s}': {e}"))?
        .as_bytes())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let profile: Profile = args
        .fs_type
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;

    let mut params = Params::new(profile);
    params.block_size = args.block_size;
    params.sector_size = args.sector_size;
    params.inode_size = args.inode_size;
    params.inodes_count = args.inodes_count;
    params.inode_ratio = args.inode_ratio;
    params.reserved_percent = args.reserved_percent;
    params.label = args.label;
    params.blocks_per_group = args.blocks_per_group;
    params.flex_bg_size = args.flex_bg_size;
    params.feature_spec = args.features;
    params.mkfs_time = args.mkfs_time;
    params.lazy_itable_init = args.lazy_itable_init;
    params.mmp_update_interval = args.mmp_update_interval;
    if args.mmp_update_interval.is_some() {
        // Asking for an interval is asking for the fence.
        params.feature_spec = Some(match params.feature_spec.take() {
            Some(spec) => format!("{spec},mmp"),
            None => "mmp".to_string(),
        });
    }
    if let Some(u) = args.uuid.as_deref() {
        params.uuid = Some(parse_uuid(u)?);
    }
    params.journal = match (args.no_journal, args.journal_blocks) {
        (true, _) | (_, Some(0)) => JournalSize::None,
        (_, Some(n)) => JournalSize::Blocks(n),
        _ => JournalSize::Default,
    };

    // Open before sizing: a block device reports its size, and a caller asking
    // for a block count wants a filesystem that large inside it.
    let device = FileDevice::open(&args.device).await.map_err(|e| {
        anyhow::anyhow!("cannot open {}: {e}", args.device)
    })?;

    if args.dry_run {
        let size = args.blocks_count.map_or(device.size(), |b| {
            b * params.block_size.unwrap_or(4096) as u64
        });
        let mut params = params.clone();
        if params.sector_size.is_none() {
            params.sector_size = Some(device.logical_sector_size());
        }
        let geom = mkfs_ext4::layout::Geometry::compute(size, &params)?;
        println!("Filesystem type:      {}", profile.name());
        println!("Block size:           {}", geom.block_size);
        println!("Blocks:               {}", geom.blocks_count);
        println!("Inodes:               {}", geom.inodes_count);
        println!("Block groups:         {}", geom.group_count);
        println!("Blocks per group:     {}", geom.blocks_per_group);
        println!("Inodes per group:     {}", geom.inodes_per_group);
        println!("Reserved GDT blocks:  {}", geom.reserved_gdt_blocks);
        println!("Features:             {}", geom.features.to_spec());
        println!("\nNothing was written (-n).");
        return Ok(());
    }

    let report = format(&device, &params).await?;

    if !args.quiet {
        println!("Creating filesystem with {} {}-byte blocks and {} inodes",
            report.blocks_count, report.block_size, report.inodes_count);
        println!("Filesystem UUID: {}", report.uuid_string());
        if !report.label.is_empty() {
            println!("Filesystem label: {}", report.label);
        }
        println!("Block groups: {}", report.group_count);
        if report.journal_blocks > 0 {
            println!(
                "Journal: {} blocks ({} KiB)",
                report.journal_blocks,
                report.journal_blocks as u64 * report.block_size as u64 / 1024
            );
        } else {
            println!("Journal: none");
        }
        println!("Free blocks: {}", report.free_blocks_count);
        println!("done");
    }

    Ok(())
}
