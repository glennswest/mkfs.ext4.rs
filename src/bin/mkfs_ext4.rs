//! `mkfs.ext4` — create an ext2, ext3 or ext4 filesystem.
//!
//! Flags follow `mke2fs` where they mean the same thing, so muscle memory and
//! existing scripts carry over.

use clap::Parser;

use mkfs_ext4::device::{BlockDevice, FileDevice, Prefix};
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

    /// Size of the filesystem, as `mke2fs` reads it: a plain number counts
    /// blocks of the `-b` size (1 KiB without `-b`); a `s`, `K`, `M`, `G` or
    /// `T` suffix gives 512-byte sectors, KiB, MiB, GiB or TiB. Defaults to
    /// the whole device, and may not exceed it.
    blocks_count: Option<String>,

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

    /// Write only the part of each inode table in use, as `mke2fs -E
    /// lazy_itable_init=1`. Needs descriptor checksums (`metadata_csum` or
    /// `uninit_bg`) to vouch for the rest; without them every table is
    /// still zeroed.
    #[arg(long)]
    lazy_itable_init: bool,

    /// The device already reads back as zeros, so the inode tables and the
    /// journal body need not be written. Produces an identical filesystem
    /// from far fewer writes. True of a fresh sparse file, an untouched thin
    /// volume, or a device just discarded — and of nothing else.
    #[arg(long)]
    zeroed_medium: bool,

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

/// The filesystem's size in bytes for `mke2fs`'s `BLOCKS` argument, as its
/// `parse_num_blocks2` reads it: a plain number in units of the `-b` block
/// size, or of 1 KiB when the block size is still to be chosen (`mke2fs`
/// divides that count by `blocksize / 1024` once it has chosen); a suffix
/// names its own unit. Rounded down to that unit, and never zero.
fn requested_bytes(arg: &str, block_size: Option<u32>) -> anyhow::Result<u64> {
    let unit = u64::from(block_size.unwrap_or(1024));
    let (digits, scale) = match arg.char_indices().last() {
        Some((i, c)) if !c.is_ascii_digit() => {
            let scale: u64 = match c {
                's' => 512,
                'k' | 'K' => 1 << 10,
                'm' | 'M' => 1 << 20,
                'g' | 'G' => 1 << 30,
                't' | 'T' => 1 << 40,
                _ => anyhow::bail!("invalid blocks '{arg}'"),
            };
            (&arg[..i], scale)
        }
        _ => (arg, unit),
    };
    let bytes = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| anyhow::anyhow!("invalid blocks '{arg}'"))?;
    let bytes = bytes / unit * unit;
    if bytes == 0 {
        anyhow::bail!("invalid blocks '{arg}': the filesystem would be empty");
    }
    Ok(bytes)
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
    params.zeroed_medium = args.zeroed_medium;
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

    // One size for the preview and the format, and the format sees only that
    // much of the device (#46): it plans from the device's size, so the bound
    // is what keeps every byte past the filesystem untouched.
    let size = match args.blocks_count.as_deref() {
        Some(arg) => requested_bytes(arg, params.block_size)?,
        None => device.size(),
    };
    if size > device.size() {
        anyhow::bail!(
            "Filesystem larger than apparent device size: {size} bytes asked for, \
             {} holds {}",
            args.device,
            device.size()
        );
    }
    let device = Prefix::new(device, size)?;

    if args.dry_run {
        // The device's sector is a floor, as `format` takes it.
        let mut params = params.clone();
        params.sector_size = Some(
            params
                .sector_size
                .unwrap_or(0)
                .max(device.logical_sector_size()),
        );
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
