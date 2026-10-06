//! `fsck.ext4` — check, and optionally repair, an ext2/ext3/ext4 filesystem.
//!
//! Exit codes follow `e2fsck`: 0 clean, 1 errors corrected, 4 errors left
//! uncorrected, 8 an operational error, 16 a usage error. A caller that
//! already scripts around `e2fsck` does not have to learn anything new.
//!
//! As with `e2fsck`, a filesystem that is clean and not due for a check is
//! skipped unless `-f` is given, and `-y` alone does not force a check.
//!
//! The command line is `e2fsck`'s. `-n`, `-y`, `-p`/`-a`, `-f`, `-v`, `-t`
//! and `-C fd` work. The flags of `e2fsck`'s this checker cannot honour —
//! `-b -B -c -D -E -j -k -l -L -z` — are refused with exit 16 and say so;
//! none of them is ever accepted and ignored. Anything else is a usage error,
//! also 16: never clap's 2, which to an `e2fsck` caller means "errors
//! corrected, reboot".

use std::time::Instant;

use clap::error::ErrorKind;
use clap::Parser;

use mkfs_ext4::device::FileDevice;
use mkfs_ext4::fsck::{self, CheckScope, FsckOptions, Severity};

/// `e2fsck`'s `FSCK_ERROR`: an operational error, and an option conflict.
const EXIT_ERROR: i32 = 8;
/// `e2fsck`'s `FSCK_USAGE`.
const EXIT_USAGE: i32 = 16;

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

    /// Preen (-a is the same): repair what is safe without asking, and stop
    /// with exit 4 on anything that needs a person.
    #[arg(short = 'p', short_alias = 'a', long)]
    preen: bool,

    /// Check even if the filesystem is marked clean.
    #[arg(short = 'f', long)]
    force: bool,

    /// Say more.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Print how long the check took.
    #[arg(short = 't', action = clap::ArgAction::Count)]
    timing: u8,

    /// Progress to this file descriptor. Accepted and ignored: this checker
    /// reports no progress.
    #[arg(short = 'C', value_name = "FD", allow_negative_numbers = true)]
    progress_fd: Option<i32>,

    // e2fsck's flags this checker cannot honour. Declared so they are refused
    // by name, with e2fsck's usage exit, rather than as unknown arguments.
    #[arg(short = 'b', hide = true)]
    superblock: Option<String>,
    #[arg(short = 'B', hide = true)]
    blocksize: Option<String>,
    #[arg(short = 'c', hide = true, action = clap::ArgAction::Count)]
    badblocks: u8,
    #[arg(short = 'D', hide = true)]
    optimize_dirs: bool,
    #[arg(short = 'E', hide = true)]
    extended: Option<String>,
    #[arg(short = 'j', hide = true)]
    journal: Option<String>,
    #[arg(short = 'k', hide = true)]
    keep_badblocks: bool,
    #[arg(short = 'l', hide = true)]
    bad_blocks_file: Option<String>,
    #[arg(short = 'L', hide = true)]
    set_bad_blocks_file: Option<String>,
    #[arg(short = 'z', hide = true)]
    undo_file: Option<String>,
}

impl Args {
    /// The first `e2fsck` flag given that this checker does not support, with
    /// what it would have done.
    fn unsupported(&self) -> Option<&'static str> {
        [
            (self.superblock.is_some(), "-b (use a backup superblock)"),
            (self.blocksize.is_some(), "-B (superblock block size)"),
            (self.badblocks > 0, "-c (scan for bad blocks)"),
            (self.optimize_dirs, "-D (optimize directories)"),
            (self.extended.is_some(), "-E (extended options)"),
            (self.journal.is_some(), "-j (external journal)"),
            (self.keep_badblocks, "-k (keep the bad blocks list)"),
            (self.bad_blocks_file.is_some(), "-l (add to the bad blocks list)"),
            (self.set_bad_blocks_file.is_some(), "-L (set the bad blocks list)"),
            (self.undo_file.is_some(), "-z (undo file)"),
        ]
        .into_iter()
        .find_map(|(given, flag)| given.then_some(flag))
    }
}

#[tokio::main]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let _ = e.print();
            std::process::exit(match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => EXIT_USAGE,
            });
        }
    };

    let started = Instant::now();
    let code = run(&args).await;
    if args.timing > 0 {
        println!(
            "{}: time: {:.2} s",
            args.device,
            started.elapsed().as_secs_f64()
        );
    }
    std::process::exit(code);
}

async fn run(args: &Args) -> i32 {
    if let Some(flag) = args.unsupported() {
        eprintln!("fsck.ext4: {flag} is not supported by this checker");
        return EXIT_USAGE;
    }
    if [args.no, args.yes, args.preen].iter().filter(|&&f| f).count() > 1 {
        // e2fsck's wording, and its fatal_error exit.
        eprintln!("fsck.ext4: Only one of the options -p/-a, -n or -y may be specified.");
        return EXIT_ERROR;
    }

    let device = match FileDevice::open(&args.device).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fsck.ext4: cannot open {}: {e}", args.device);
            return EXIT_ERROR;
        }
    };

    let options = FsckOptions {
        repair: args.yes,
        force: args.force,
        preen: args.preen,
    };

    let report = match fsck::check(device, &options).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fsck.ext4: {}: {e}", args.device);
            return EXIT_ERROR;
        }
    };

    match &report.scope {
        CheckScope::Skipped { next_check } => {
            let note = match next_check {
                Some(1) => " (check after next mount)".to_string(),
                Some(n) => format!(" (check in {n} mounts)"),
                None => String::new(),
            };
            println!(
                "{}: clean, {}/{} files, {}/{} blocks{note}",
                args.device,
                report.inodes_used,
                report.inodes_count,
                report.blocks_used,
                report.blocks_count
            );
            return report.exit_code();
        }
        CheckScope::Due(reason) => println!("{} {reason}, check forced.", args.device),
        CheckScope::Forced => {}
    }

    for problem in &report.problems {
        let mark = match (problem.fixed, problem.severity) {
            (true, _) => "FIXED",
            (false, Severity::Info) => "note ",
            (false, Severity::Fixable) => "FIX? ",
            (false, Severity::Serious) => "ERROR",
        };
        println!("{mark} [pass {}] {}", problem.pass, problem.message);
    }

    if report.preen_halted {
        // e2fsck's preenhalt, word for word.
        println!(
            "\n\n{}: UNEXPECTED INCONSISTENCY; RUN fsck MANUALLY.\n\t(i.e., without -a or -p options)",
            args.device
        );
        return report.exit_code();
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
    if report.unfixed().next().is_some() && !args.yes && !args.preen {
        println!("\nRun with -y to repair.");
    }

    report.exit_code()
}
