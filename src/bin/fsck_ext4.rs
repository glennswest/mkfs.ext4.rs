//! `fsck.ext4` — check, and optionally repair, an ext2/ext3/ext4 filesystem.
//!
//! Exit codes follow `e2fsck`: 0 clean, 1 errors corrected, 4 errors left
//! uncorrected, 8 an operational error. A caller that already scripts around
//! `e2fsck` does not have to learn anything new.

use clap::Parser;

use mkfs_ext4::device::FileDevice;
use mkfs_ext4::fsck::{self, FsckOptions, Severity};

#[derive(Parser, Debug)]
#[command(
    name = "fsck.ext4",
    about = "Check and repair an ext2/ext3/ext4 filesystem",
    version
)]
struct Args {
    /// Device or image file to check.
    device: String,

    /// Answer no to everything: report problems, change nothing. The default.
    #[arg(short = 'n', long)]
    no: bool,

    /// Answer yes to everything: repair what can be repaired.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Check even if the filesystem is marked clean.
    #[arg(short = 'f', long)]
    force: bool,

    /// Say more.
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.no && args.yes {
        eprintln!("fsck.ext4: -n and -y contradict each other");
        std::process::exit(8);
    }

    let device = match FileDevice::open(&args.device).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fsck.ext4: cannot open {}: {e}", args.device);
            std::process::exit(8);
        }
    };

    let options = FsckOptions {
        repair: args.yes,
        force: args.force,
    };

    let report = match fsck::check(device, &options).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fsck.ext4: {}: {e}", args.device);
            std::process::exit(8);
        }
    };

    for problem in &report.problems {
        let mark = match (problem.fixed, problem.severity) {
            (true, _) => "FIXED",
            (false, Severity::Info) => "note ",
            (false, Severity::Fixable) => "FIX? ",
            (false, Severity::Serious) => "ERROR",
        };
        println!("{mark} [pass {}] {}", problem.pass, problem.message);
    }

    if report.is_clean() {
        println!("{}: clean", args.device);
    }
    println!(
        "{}: {}/{} files, {}/{} blocks",
        args.device,
        report.inodes_used,
        report.inodes_count,
        report.blocks_used,
        report.blocks_count
    );

    if args.verbose {
        println!("{} directories", report.directories);
    }
    if report.unfixed().next().is_some() && !args.yes {
        println!("\nRun with -y to repair.");
    }

    std::process::exit(report.exit_code());
}
