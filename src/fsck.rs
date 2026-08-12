//! Checking and repairing.
//!
//! The passes are e2fsck's, and in its order, because the order is the design:
//! each pass establishes what the next one needs.
//!
//! | Pass | Question |
//! |---|---|
//! | 0 | is the superblock and the descriptor table self-consistent? |
//! | 1 | which blocks does each inode claim, and does any block have two owners? |
//! | 2 | are the directory entries well formed, and what do they point at? |
//! | 3 | is every directory reachable from the root? |
//! | 4 | does each inode's link count match the names that refer to it? |
//! | 5 | do the bitmaps and free counters match what passes 1 to 4 found? |
//!
//! Checking never writes. Repair writes only what a pass proved wrong, and
//! records every change in the report, so a caller can see what was done rather
//! than trusting that something was.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::csum::{self, GroupDescCsum};
use crate::device::BlockDevice;
use crate::error::Result;
use crate::fs::Filesystem;
use crate::structs::dirent::{self, file_type};
use crate::structs::inode::{mode, Inode};
use crate::structs::superblock::{ino, state};

/// How to run the check.
#[derive(Debug, Clone, Default)]
pub struct FsckOptions {
    /// Write the fixes rather than only reporting them.
    pub repair: bool,
    /// Check even when the superblock says the filesystem is clean.
    pub force: bool,
}

impl FsckOptions {
    /// Report only; never write. The default.
    pub fn check_only() -> Self {
        Self::default()
    }

    /// Report and repair.
    pub fn repair() -> Self {
        Self {
            repair: true,
            force: true,
        }
    }
}

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth saying, but not wrong.
    Info,
    /// Wrong, and safe to correct.
    Fixable,
    /// Wrong in a way this implementation will not correct on its own.
    Serious,
}

/// One thing found wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Which pass found it.
    pub pass: u8,
    /// Stable short identifier, for matching in tests and logs.
    pub code: &'static str,
    /// How bad it is.
    pub severity: Severity,
    /// What is wrong, in a sentence.
    pub message: String,
    /// Whether this run corrected it.
    pub fixed: bool,
}

/// The result of a check.
#[derive(Debug, Clone, Default)]
pub struct FsckReport {
    /// Everything found wrong.
    pub problems: Vec<Problem>,
    /// Inodes in use.
    pub inodes_used: u32,
    /// Blocks in use.
    pub blocks_used: u64,
    /// Total inodes.
    pub inodes_count: u32,
    /// Total blocks.
    pub blocks_count: u64,
    /// Directories found.
    pub directories: u32,
}

impl FsckReport {
    /// Nothing wrong at all.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// Problems that remain after this run.
    pub fn unfixed(&self) -> impl Iterator<Item = &Problem> {
        self.problems.iter().filter(|p| !p.fixed)
    }

    /// Whether anything was corrected.
    pub fn repaired_anything(&self) -> bool {
        self.problems.iter().any(|p| p.fixed)
    }

    /// An `e2fsck`-compatible exit code.
    ///
    /// 0 clean, 1 errors corrected, 4 errors left uncorrected.
    pub fn exit_code(&self) -> i32 {
        if self.unfixed().next().is_some() {
            4
        } else if self.repaired_anything() {
            1
        } else {
            0
        }
    }

    fn push(&mut self, pass: u8, code: &'static str, severity: Severity, message: String) {
        self.problems.push(Problem {
            pass,
            code,
            severity,
            message,
            fixed: false,
        });
    }

    /// Record a problem and mark it fixed if repairing.
    fn push_fixed(
        &mut self,
        pass: u8,
        code: &'static str,
        severity: Severity,
        message: String,
        repairing: bool,
    ) {
        self.problems.push(Problem {
            pass,
            code,
            severity,
            message,
            fixed: repairing,
        });
    }
}

/// A bitmap sized to the filesystem, used to rebuild what should be on disk.
///
/// One bit per block. A 1 TiB filesystem of 4 KiB blocks needs 32 MiB of this,
/// which is the same trade e2fsck makes.
struct Bitmap {
    bits: Vec<u64>,
    len: u64,
}

impl Bitmap {
    fn new(len: u64) -> Self {
        Self {
            bits: vec![0u64; (len as usize).div_ceil(64)],
            len,
        }
    }

    fn set(&mut self, index: u64) {
        if index < self.len {
            self.bits[(index / 64) as usize] |= 1u64 << (index % 64);
        }
    }

    fn get(&self, index: u64) -> bool {
        index < self.len && self.bits[(index / 64) as usize] & (1u64 << (index % 64)) != 0
    }

    fn count(&self) -> u64 {
        self.bits.iter().map(|w| w.count_ones() as u64).sum()
    }
}

/// Check a filesystem, and repair it if asked.
pub async fn check<D: BlockDevice>(device: D, options: &FsckOptions) -> Result<FsckReport> {
    let mut fs = Filesystem::open(device).await?;
    check_opened(&mut fs, options).await
}

/// Check an already-opened filesystem.
pub async fn check_opened<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    options: &FsckOptions,
) -> Result<FsckReport> {
    let mut report = FsckReport {
        inodes_count: fs.superblock().inodes_count,
        blocks_count: fs.superblock().blocks_count,
        ..Default::default()
    };

    let mut state = ScanState::new(fs);

    pass0_superblock(fs, &mut report, &mut state).await?;
    pass1_inodes(fs, &mut report, &mut state).await?;
    pass2_directories(fs, &mut report, &mut state).await?;
    pass3_connectivity(fs, &mut report, &mut state).await?;
    pass4_link_counts(fs, &mut report, &mut state, options).await?;
    pass5_bitmaps(fs, &mut report, &mut state, options).await?;

    report.blocks_used = state.blocks.count();
    report.inodes_used = state.inodes_in_use.len() as u32;
    report.directories = state.directories.len() as u32;

    if options.repair && report.repaired_anything() {
        // A repaired filesystem is a clean one; say so where a mounter looks.
        let sb = fs.superblock_mut();
        sb.state = state::VALID_FS;
        fs.flush_superblock().await?;
        fs.flush_group_descs().await?;
        fs.device().flush().await?;
    }

    Ok(report)
}

/// What the passes accumulate.
struct ScanState {
    /// Blocks claimed by metadata or by some inode.
    blocks: Bitmap,
    /// Blocks claimed more than once.
    duplicates: BTreeSet<u64>,
    /// Inodes that are in use, and whether each is a directory.
    inodes_in_use: BTreeSet<u32>,
    /// Directories found in pass 1.
    directories: BTreeSet<u32>,
    /// Link counts observed from directory entries.
    observed_links: BTreeMap<u32, u16>,
    /// Parent directory of each directory, from its "..".
    parents: BTreeMap<u32, u32>,
    /// Which directories contain which subdirectories.
    children: BTreeMap<u32, Vec<u32>>,
    /// Blocks metadata owns, per group, so pass 5 can rebuild bitmaps.
    inodes_per_group: u32,
    first_ino: u32,
}

impl ScanState {
    fn new<D: BlockDevice>(fs: &Filesystem<D>) -> Self {
        Self {
            blocks: Bitmap::new(fs.superblock().blocks_count),
            duplicates: BTreeSet::new(),
            inodes_in_use: BTreeSet::new(),
            directories: BTreeSet::new(),
            observed_links: BTreeMap::new(),
            parents: BTreeMap::new(),
            children: BTreeMap::new(),
            inodes_per_group: fs.superblock().inodes_per_group,
            first_ino: fs.superblock().first_ino,
        }
    }

    /// Claim a block, noting a collision if someone already had it.
    fn claim(&mut self, block: u64) {
        if self.blocks.get(block) {
            self.duplicates.insert(block);
        } else {
            self.blocks.set(block);
        }
    }
}

/// Pass 0 — the superblock and the group descriptors.
async fn pass0_superblock<D: BlockDevice>(
    fs: &Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
) -> Result<()> {
    let sb = fs.superblock().clone();

    if sb.state & state::ERROR_FS != 0 {
        report.push(
            0,
            "fs-has-errors",
            Severity::Info,
            "superblock records that errors were detected on this filesystem".into(),
        );
    }
    if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 {
        report.push(
            0,
            "zero-geometry",
            Severity::Serious,
            "superblock geometry is zero; the filesystem cannot be walked".into(),
        );
        return Ok(());
    }
    if sb.inode_size == 0 || (sb.inode_size as u32) > sb.block_size() {
        report.push(
            0,
            "bad-inode-size",
            Severity::Serious,
            format!(
                "inode size {} is impossible for {}-byte blocks",
                sb.inode_size,
                sb.block_size()
            ),
        );
    }

    // The superblock's own checksum, when it carries one.
    if sb.has_metadata_csum() {
        let mut buf = [0u8; crate::structs::superblock::SUPERBLOCK_LEN];
        fs.device()
            .read_at(crate::structs::superblock::SUPERBLOCK_OFFSET, &mut buf)
            .await?;
        if !sb.verify_checksum(&buf) {
            report.push(
                0,
                "superblock-csum",
                Severity::Serious,
                "superblock checksum does not match its contents".into(),
            );
        }
    }

    // Claim the blocks the filesystem's own structure occupies. Doing it here
    // rather than in pass 5 means pass 1 can notice a file that claims a block
    // metadata already owns.
    for group in 0..sb.group_count() {
        let first = fs.group_first_block(group);
        // Superblock copy and descriptors, however many this group holds.
        // Under meta_bg a group can carry a superblock backup and no
        // descriptor block, or a descriptor block and no superblock.
        for b in first..first + fs.super_overhead(group) as u64 {
            state.claim(b);
        }

        let Some(desc) = fs.group_descs().get(group as usize) else {
            continue;
        };

        // Are the descriptor's own pointers inside the filesystem?
        for (what, block) in [
            ("block bitmap", desc.block_bitmap),
            ("inode bitmap", desc.inode_bitmap),
            ("inode table", desc.inode_table),
        ] {
            if block == 0 || block >= sb.blocks_count {
                report.push(
                    0,
                    "bad-desc-pointer",
                    Severity::Serious,
                    format!("group {group} {what} points at block {block}, outside the filesystem"),
                );
            }
        }

        state.claim(desc.block_bitmap);
        state.claim(desc.inode_bitmap);
        for i in 0..sb.itable_blocks_per_group() as u64 {
            state.claim(desc.inode_table + i);
        }

        // The descriptor checksum.
        if fs.csum_scheme() != GroupDescCsum::None {
            let desc_size = sb.desc_size() as usize;
            let mut buf = vec![0u8; desc_size];
            desc.encode_into(&mut buf, desc_size);
            let expect = csum::group_desc_csum(
                fs.csum_scheme(),
                fs.csum_seed(),
                &sb.uuid,
                group,
                &buf,
            );
            if desc.checksum != expect {
                report.push(
                    0,
                    "group-desc-csum",
                    Severity::Fixable,
                    format!(
                        "group {group} descriptor checksum is {:#06x}, computed {expect:#06x}",
                        desc.checksum
                    ),
                );
            }
        }
    }

    // Block 0 of a 1 KiB filesystem is not part of any group.
    for b in 0..sb.first_data_block as u64 {
        state.claim(b);
    }

    // The multiple-mount-protection block belongs to no inode and no group's
    // metadata, so nothing else would ever claim it.
    if sb
        .feature_incompat
        .contains(crate::features::IncompatFeatures::MMP)
        && sb.mmp_block >= sb.first_data_block as u64
        && sb.mmp_block < sb.blocks_count
    {
        state.claim(sb.mmp_block);
    }

    Ok(())
}

/// Pass 1 — inodes, and the blocks they claim.
async fn pass1_inodes<D: BlockDevice>(
    fs: &Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
) -> Result<()> {
    let sb = fs.superblock().clone();
    let inode_size = sb.inode_size as usize;
    let sectors_per_block = sb.block_size() as u64 / 512;

    for group in 0..sb.group_count() {
        // Inodes at the end of a group that were never used need not be read.
        // Skipping them is what makes checking a large, empty filesystem quick,
        // and it is only safe because the group descriptor is checksummed.
        let unused = fs
            .group_descs()
            .get(group as usize)
            .filter(|_| fs.csum_scheme() != GroupDescCsum::None)
            .map(|d| d.itable_unused.min(sb.inodes_per_group))
            .unwrap_or(0);
        let live = sb.inodes_per_group - unused;

        for within in 0..live {
            let inum = group * sb.inodes_per_group + within + 1;
            if inum > sb.inodes_count {
                break;
            }

            let raw = fs.read_inode_raw(inum).await?;
            let inode = Inode::decode(&raw, inode_size)?;

            let reserved = inum < sb.first_ino;
            let in_use = reserved || inode.links_count > 0;
            if !in_use {
                continue;
            }
            state.inodes_in_use.insert(inum);

            // The inode's own checksum.
            if fs.has_metadata_csum()
                && !Inode::verify_checksum(&raw, inode_size, true, fs.csum_seed(), inum)?
            {
                report.push(
                    1,
                    "inode-csum",
                    Severity::Serious,
                    format!("inode {inum} checksum does not match its contents"),
                );
            }

            if inode.is_dir() {
                state.directories.insert(inum);
            }

            // A deleted inode with links is a contradiction.
            if inode.links_count > 0 && inode.dtime != 0 {
                report.push(
                    1,
                    "deleted-but-linked",
                    Severity::Fixable,
                    format!(
                        "inode {inum} has {} links but a deletion time of {}",
                        inode.links_count, inode.dtime
                    ),
                );
            }

            // Most of the reserved range carries no user data. The root
            // directory, the journal and the resize inode are walked.
            let is_resize = inum == ino::RESIZE;
            if reserved && inum != ino::JOURNAL && inum != ino::ROOT && !is_resize {
                continue;
            }

            let mut counted = 0u64;
            let mut bad = Vec::new();
            let walk = fs
                .walk_blocks(&inode, |b| {
                    counted += 1;
                    if b.physical == 0 || b.physical >= sb.blocks_count {
                        bad.push(b.physical);
                        return;
                    }
                    // The resize inode is the one inode that shares blocks by
                    // design: its double indirect block belongs to it, but
                    // everything that block points at is the reserved group
                    // descriptor blocks, which pass 0 already claimed as
                    // metadata. Claim without collision detection so the
                    // shared blocks are not reported as owned twice, and the
                    // indirect block it really does own still gets counted.
                    if is_resize {
                        state.blocks.set(b.physical);
                    } else if state.blocks.get(b.physical) {
                        state.duplicates.insert(b.physical);
                    } else {
                        state.blocks.set(b.physical);
                    }
                })
                .await;

            if let Err(e) = walk {
                report.push(
                    1,
                    "unwalkable-block-map",
                    Severity::Serious,
                    format!("inode {inum} block map could not be walked: {e}"),
                );
                continue;
            }

            for block in bad.iter().take(4) {
                report.push(
                    1,
                    "block-out-of-range",
                    Severity::Serious,
                    format!("inode {inum} claims block {block}, outside the filesystem"),
                );
            }

            // i_blocks counts every block the inode owns, in 512-byte sectors.
            // The resize inode is the exception: it also counts the reserved
            // descriptor blocks it indexes but does not exclusively own.
            if inum != ino::RESIZE {
                let expect = counted * sectors_per_block;
                if inode.blocks != expect {
                    report.push(
                        1,
                        "i-blocks-wrong",
                        Severity::Fixable,
                        format!(
                            "inode {inum} i_blocks is {}, but it owns {counted} blocks ({expect} sectors)",
                            inode.blocks
                        ),
                    );
                }
            }
        }
    }

    for block in state.duplicates.iter().take(8).copied().collect::<Vec<_>>() {
        report.push(
            1,
            "duplicate-block",
            Severity::Serious,
            format!("block {block} is claimed more than once"),
        );
    }

    Ok(())
}

/// Pass 2 — directory contents.
async fn pass2_directories<D: BlockDevice>(
    fs: &Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
) -> Result<()> {
    let sb = fs.superblock().clone();
    let filetype = sb
        .feature_incompat
        .contains(crate::features::IncompatFeatures::FILETYPE);

    let dirs: Vec<u32> = state.directories.iter().copied().collect();
    for dir_ino in dirs {
        let dir = fs.read_inode(dir_ino).await?;
        let blocks = dir.size.div_ceil(sb.block_size() as u64);

        let mut entries = Vec::new();
        let mut walk_failed = false;
        for logical in 0..blocks {
            let Some(physical) = fs.resolve_block(&dir, logical).await? else {
                continue;
            };
            let buf = match fs.read_block(physical).await {
                Ok(b) => b,
                Err(e) => {
                    report.push(
                        2,
                        "dir-block-unreadable",
                        Severity::Serious,
                        format!("directory {dir_ino} block {logical}: {e}"),
                    );
                    walk_failed = true;
                    break;
                }
            };

            // A directory block's checksum, when the filesystem carries them.
            if fs.has_metadata_csum() {
                if let Some(stored) = dirent::block_csum(&buf) {
                    let limit = buf.len() - dirent::TAIL_LEN;
                    let expect =
                        csum::dirent_csum(fs.csum_seed(), dir_ino, dir.generation, &buf[..limit]);
                    if stored != expect {
                        report.push(
                            2,
                            "dir-block-csum",
                            Severity::Fixable,
                            format!(
                                "directory {dir_ino} block {logical} checksum is {stored:#010x}, computed {expect:#010x}"
                            ),
                        );
                    }
                }
            }

            match dirent::parse_block(&buf) {
                Ok(parsed) => entries.extend(
                    parsed
                        .into_iter()
                        .filter(|e| e.inode != 0 && !e.is_tail() && !e.name.is_empty()),
                ),
                Err(e) => {
                    report.push(
                        2,
                        "dir-block-malformed",
                        Severity::Serious,
                        format!("directory {dir_ino} block {logical}: {e}"),
                    );
                    walk_failed = true;
                }
            }
        }
        if walk_failed {
            continue;
        }

        // "." and ".." must be the first two entries and must point where they
        // say. Everything above depends on it: pass 3 walks parents this way.
        match entries.first() {
            Some(e) if e.name == b"." => {
                if e.inode != dir_ino {
                    report.push(
                        2,
                        "dot-wrong",
                        Severity::Fixable,
                        format!("directory {dir_ino} has '.' pointing at inode {}", e.inode),
                    );
                }
            }
            _ => report.push(
                2,
                "dot-missing",
                Severity::Serious,
                format!("directory {dir_ino} has no '.' entry"),
            ),
        }
        match entries.get(1) {
            Some(e) if e.name == b".." => {
                state.parents.insert(dir_ino, e.inode);
                state.children.entry(e.inode).or_default().push(dir_ino);
            }
            _ => report.push(
                2,
                "dotdot-missing",
                Severity::Serious,
                format!("directory {dir_ino} has no '..' entry"),
            ),
        }

        let mut seen_names = BTreeSet::new();
        for entry in &entries {
            if entry.inode == 0 || entry.inode > sb.inodes_count {
                report.push(
                    2,
                    "entry-inode-out-of-range",
                    Severity::Serious,
                    format!(
                        "directory {dir_ino} entry '{}' points at inode {}, outside 1..={}",
                        entry.name_string(),
                        entry.inode,
                        sb.inodes_count
                    ),
                );
                continue;
            }
            if !seen_names.insert(entry.name.clone()) {
                report.push(
                    2,
                    "duplicate-name",
                    Severity::Serious,
                    format!(
                        "directory {dir_ino} has more than one entry named '{}'",
                        entry.name_string()
                    ),
                );
            }
            if !state.inodes_in_use.contains(&entry.inode) {
                report.push(
                    2,
                    "entry-points-at-free-inode",
                    Severity::Serious,
                    format!(
                        "directory {dir_ino} entry '{}' points at inode {}, which is not in use",
                        entry.name_string(),
                        entry.inode
                    ),
                );
                continue;
            }

            // The file type byte must agree with the inode it names.
            if filetype && entry.file_type != file_type::UNKNOWN {
                let target = fs.read_inode(entry.inode).await?;
                let expect = match target.mode & mode::IFMT {
                    mode::IFREG => file_type::REG_FILE,
                    mode::IFDIR => file_type::DIR,
                    mode::IFCHR => file_type::CHRDEV,
                    mode::IFBLK => file_type::BLKDEV,
                    mode::IFIFO => file_type::FIFO,
                    mode::IFSOCK => file_type::SOCK,
                    mode::IFLNK => file_type::SYMLINK,
                    _ => file_type::UNKNOWN,
                };
                if expect != file_type::UNKNOWN && entry.file_type != expect {
                    report.push(
                        2,
                        "filetype-mismatch",
                        Severity::Fixable,
                        format!(
                            "directory {dir_ino} entry '{}' has file type {} but inode {} is type {expect}",
                            entry.name_string(),
                            entry.file_type,
                            entry.inode
                        ),
                    );
                }
            }

            // Every name is a link. "." and ".." count too, which is why a
            // directory's link count is two plus its subdirectories.
            *state.observed_links.entry(entry.inode).or_insert(0) += 1;
        }
    }

    Ok(())
}

/// Pass 3 — is every directory reachable from the root?
async fn pass3_connectivity<D: BlockDevice>(
    _fs: &Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
) -> Result<()> {
    if !state.directories.contains(&ino::ROOT) {
        report.push(
            3,
            "no-root-directory",
            Severity::Serious,
            "inode 2 is not a directory; the filesystem has no root".into(),
        );
        return Ok(());
    }

    let mut reached = BTreeSet::new();
    let mut queue = VecDeque::from([ino::ROOT]);
    reached.insert(ino::ROOT);

    while let Some(dir) = queue.pop_front() {
        for &child in state.children.get(&dir).into_iter().flatten() {
            if child != dir && reached.insert(child) {
                queue.push_back(child);
            }
        }
    }

    for &dir in &state.directories {
        if !reached.contains(&dir) {
            report.push(
                3,
                "disconnected-directory",
                Severity::Serious,
                format!(
                    "directory {dir} is not reachable from the root; it belongs in lost+found"
                ),
            );
        }
    }

    // A "..' that does not match the directory that actually contains it.
    for (&dir, &claimed_parent) in &state.parents {
        if dir == ino::ROOT {
            continue;
        }
        let really_in = state
            .children
            .iter()
            .find(|(_, kids)| kids.contains(&dir))
            .map(|(&p, _)| p);
        if let Some(actual) = really_in {
            if actual != claimed_parent && claimed_parent != actual {
                report.push(
                    3,
                    "parent-mismatch",
                    Severity::Serious,
                    format!("directory {dir} claims parent {claimed_parent} but is listed in {actual}"),
                );
            }
        }
    }

    Ok(())
}

/// Pass 4 — link counts.
async fn pass4_link_counts<D: BlockDevice>(
    fs: &Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
    options: &FsckOptions,
) -> Result<()> {
    let inodes: Vec<u32> = state.inodes_in_use.iter().copied().collect();

    let orphan_file = fs.superblock().orphan_file_inum;

    for inum in inodes {
        // Reserved inodes are not referenced by any name, so there is nothing
        // to compare them against.
        if inum < state.first_ino && inum != ino::ROOT {
            continue;
        }
        // Nor is the orphan file. It sits outside the directory tree by
        // design, reachable only through s_orphan_file_inum, so "no directory
        // entry refers to it" is its normal state rather than a fault.
        if orphan_file != 0 && inum == orphan_file {
            continue;
        }
        let mut inode = fs.read_inode(inum).await?;
        let observed = state.observed_links.get(&inum).copied().unwrap_or(0);

        if observed == 0 {
            report.push(
                4,
                "unreferenced-inode",
                Severity::Serious,
                format!(
                    "inode {inum} has {} links but no directory entry refers to it",
                    inode.links_count
                ),
            );
            continue;
        }

        if inode.links_count != observed {
            let fixed = options.repair;
            report.push_fixed(
                4,
                "link-count-wrong",
                Severity::Fixable,
                format!(
                    "inode {inum} link count is {}, but {observed} names refer to it",
                    inode.links_count
                ),
                fixed,
            );
            if fixed {
                inode.links_count = observed;
                fs.write_inode(inum, &inode).await?;
            }
        }
    }

    Ok(())
}

/// Pass 5 — bitmaps and free counters.
async fn pass5_bitmaps<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    report: &mut FsckReport,
    state: &mut ScanState,
    options: &FsckOptions,
) -> Result<()> {
    let sb = fs.superblock().clone();
    let block_size = sb.block_size() as usize;
    let mut total_free_blocks = 0u64;
    let mut total_free_inodes = 0u32;

    for group in 0..sb.group_count() {
        let first = fs.group_first_block(group);
        let in_group = fs.group_block_count(group) as u64;

        // What the block bitmap should say.
        let mut expected = vec![0u8; block_size];
        let mut free_blocks = 0u32;
        for i in 0..sb.blocks_per_group as u64 {
            let used = i >= in_group || state.blocks.get(first + i);
            if used {
                expected[(i / 8) as usize] |= 1 << (i % 8);
            } else {
                free_blocks += 1;
            }
        }
        for bit in sb.blocks_per_group as usize..block_size * 8 {
            expected[bit / 8] |= 1 << (bit % 8);
        }

        let actual = fs.read_block_bitmap(group).await?;
        if actual != expected {
            let fixed = options.repair;
            report.push_fixed(
                5,
                "block-bitmap-differs",
                Severity::Fixable,
                format!("group {group} block bitmap does not match the blocks in use"),
                fixed,
            );
            if fixed {
                let desc = fs.group_descs()[group as usize];
                fs.write_block(desc.block_bitmap, &expected).await?;
            }
        }

        // What the inode bitmap should say.
        let mut expected_inodes = vec![0u8; block_size];
        let mut free_inodes = 0u32;
        let mut used_dirs = 0u32;
        for i in 0..sb.inodes_per_group as u64 {
            let inum = group * sb.inodes_per_group + i as u32 + 1;
            let used = inum <= sb.inodes_count && state.inodes_in_use.contains(&inum);
            if used {
                expected_inodes[(i / 8) as usize] |= 1 << (i % 8);
                if state.directories.contains(&inum) {
                    used_dirs += 1;
                }
            } else {
                free_inodes += 1;
            }
        }
        for bit in sb.inodes_per_group as usize..block_size * 8 {
            expected_inodes[bit / 8] |= 1 << (bit % 8);
        }

        let actual_inodes = fs.read_inode_bitmap(group).await?;
        if actual_inodes != expected_inodes {
            let fixed = options.repair;
            report.push_fixed(
                5,
                "inode-bitmap-differs",
                Severity::Fixable,
                format!("group {group} inode bitmap does not match the inodes in use"),
                fixed,
            );
            if fixed {
                let desc = fs.group_descs()[group as usize];
                fs.write_block(desc.inode_bitmap, &expected_inodes).await?;
            }
        }

        // Counters in the descriptor.
        let desc = fs.group_descs()[group as usize];
        if desc.free_blocks_count != free_blocks {
            let fixed = options.repair;
            report.push_fixed(
                5,
                "group-free-blocks-wrong",
                Severity::Fixable,
                format!(
                    "group {group} free block count is {}, counted {free_blocks}",
                    desc.free_blocks_count
                ),
                fixed,
            );
        }
        if desc.free_inodes_count != free_inodes {
            let fixed = options.repair;
            report.push_fixed(
                5,
                "group-free-inodes-wrong",
                Severity::Fixable,
                format!(
                    "group {group} free inode count is {}, counted {free_inodes}",
                    desc.free_inodes_count
                ),
                fixed,
            );
        }
        if desc.used_dirs_count != used_dirs {
            let fixed = options.repair;
            report.push_fixed(
                5,
                "group-dir-count-wrong",
                Severity::Fixable,
                format!(
                    "group {group} directory count is {}, counted {used_dirs}",
                    desc.used_dirs_count
                ),
                fixed,
            );
        }

        if options.repair {
            let seed = fs.csum_seed();
            let has_csum = fs.has_metadata_csum();
            let bb_len = (sb.blocks_per_group as usize).div_ceil(8);
            let ib_len = (sb.inodes_per_group as usize).div_ceil(8);
            let desc = &mut fs.group_descs_mut()[group as usize];
            desc.free_blocks_count = free_blocks;
            desc.free_inodes_count = free_inodes;
            desc.used_dirs_count = used_dirs;
            if has_csum {
                desc.block_bitmap_csum = csum::bitmap_csum(seed, &expected[..bb_len]);
                desc.inode_bitmap_csum =
                    csum::bitmap_csum(seed, &expected_inodes[..ib_len]);
            }
        }

        total_free_blocks += free_blocks as u64;
        total_free_inodes += free_inodes;
    }

    // And the superblock's totals.
    if sb.free_blocks_count != total_free_blocks {
        let fixed = options.repair;
        report.push_fixed(
            5,
            "superblock-free-blocks-wrong",
            Severity::Fixable,
            format!(
                "superblock free block count is {}, counted {total_free_blocks}",
                sb.free_blocks_count
            ),
            fixed,
        );
        if fixed {
            fs.superblock_mut().free_blocks_count = total_free_blocks;
        }
    }
    if sb.free_inodes_count != total_free_inodes {
        let fixed = options.repair;
        report.push_fixed(
            5,
            "superblock-free-inodes-wrong",
            Severity::Fixable,
            format!(
                "superblock free inode count is {}, counted {total_free_inodes}",
                sb.free_inodes_count
            ),
            fixed,
        );
        if fixed {
            fs.superblock_mut().free_inodes_count = total_free_inodes;
        }
    }

    let _ = state.inodes_per_group;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::format::format;
    use crate::params::{Params, Profile};

    const MIB: u64 = 1024 * 1024;

    async fn formatted(profile: Profile, size: u64) -> MemDevice {
        let dev = MemDevice::new(size);
        let params = Params::new(profile)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();
        dev
    }

    fn describe(report: &FsckReport) -> String {
        report
            .problems
            .iter()
            .map(|p| format!("[pass {} {}] {}", p.pass, p.code, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The strongest statement this crate can make about itself: what the
    /// formatter writes, the checker finds nothing wrong with.
    #[tokio::test]
    async fn a_freshly_formatted_filesystem_is_clean() {
        for profile in [Profile::Ext2, Profile::Ext3, Profile::Ext4] {
            for size in [16 * MIB, 64 * MIB, 256 * MIB] {
                let dev = formatted(profile, size).await;
                let report = check(dev, &FsckOptions::check_only()).await.unwrap();
                assert!(
                    report.is_clean(),
                    "{} at {size} bytes:\n{}",
                    profile.name(),
                    describe(&report)
                );
                assert_eq!(report.exit_code(), 0);
                assert_eq!(report.directories, 2, "root and lost+found");

                // Ten reserved inodes plus lost+found, and on ext4 the orphan
                // file as well. Only the ext4 profile asks for orphan_file —
                // ext3 has a journal but mke2fs.conf does not give it one.
                let expect = if profile == Profile::Ext4 { 12 } else { 11 };
                assert_eq!(
                    report.inodes_used,
                    expect,
                    "{} at {size} bytes",
                    profile.name()
                );
            }
        }
    }

    /// The block count real `e2fsck` reports for the golden 64 MiB
    /// journal-less ext4 reference is 5417/65536. Ours must agree exactly:
    /// the same blocks, counted the same way.
    #[tokio::test]
    async fn counts_match_the_golden_reference() {
        let dev = MemDevice::new(64 * MIB);
        let params = Params::new(Profile::Ext4)
            .no_journal()
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();

        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert_eq!(report.blocks_count, 65536);
        assert_eq!(report.inodes_count, 16384);
        assert_eq!(report.blocks_used, 5417);
        assert_eq!(report.inodes_used, 11);
    }

    /// A journalled ext4 filesystem costs 4096 blocks of journal and a further
    /// 32 for the orphan file, and both are accounted the same way.
    #[tokio::test]
    async fn a_journal_and_orphan_file_are_counted_too() {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(report.is_clean(), "{}", describe(&report));
        assert_eq!(report.blocks_used, 5417 + 4096 + 32);
        assert_eq!(report.inodes_used, 12);
    }

    /// The orphan file lives outside the directory tree, so pass 4 must not
    /// call it unreferenced — the fault it would otherwise report on every
    /// journalled ext4 filesystem in existence.
    #[tokio::test]
    async fn the_orphan_file_is_not_an_unreferenced_inode() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let orphan = fs.superblock().orphan_file_inum;
        assert_ne!(orphan, 0, "ext4 should carry an orphan file");

        let inode = fs.read_inode(orphan).await.unwrap();
        assert!(inode.is_reg());
        assert_eq!(inode.links_count, 1);
        assert!(inode.size > 0);

        // And no directory names it.
        let root = fs.read_inode(ino::ROOT).await.unwrap();
        let names = fs.read_dir(&root).await.unwrap();
        assert!(names.iter().all(|e| e.inode != orphan));

        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(report.is_clean(), "{}", describe(&report));
    }

    #[tokio::test]
    async fn notices_a_wrong_free_block_count() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let mut fs = Filesystem::open(dev).await.unwrap();
        fs.superblock_mut().free_blocks_count = 42;
        fs.flush_superblock().await.unwrap();

        let dev = fs.into_device();
        let report = check(dev, &FsckOptions::check_only()).await.unwrap();
        assert!(report
            .problems
            .iter()
            .any(|p| p.code == "superblock-free-blocks-wrong"));
        assert_eq!(report.exit_code(), 4, "reported but not fixed");
    }

    #[tokio::test]
    async fn repairs_a_wrong_free_block_count() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let mut fs = Filesystem::open(dev).await.unwrap();
        let real = fs.superblock().free_blocks_count;
        fs.superblock_mut().free_blocks_count = 42;
        fs.flush_superblock().await.unwrap();
        let dev = fs.into_device();

        let report = check(&dev, &FsckOptions::repair()).await.unwrap();
        assert!(report.repaired_anything());
        assert_eq!(report.exit_code(), 1, "corrected");

        // And a second pass finds nothing left to do.
        let again = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(again.is_clean(), "after repair:\n{}", describe(&again));

        let fs = Filesystem::open(&dev).await.unwrap();
        assert_eq!(fs.superblock().free_blocks_count, real);
    }

    #[tokio::test]
    async fn notices_and_repairs_a_corrupted_block_bitmap() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let desc = fs.group_descs()[0];
        // Scribble over the bitmap: claim a swathe of blocks nothing owns.
        let mut bitmap = fs.read_block_bitmap(0).await.unwrap();
        for byte in bitmap.iter_mut().skip(200).take(20) {
            *byte = 0xff;
        }
        fs.write_block(desc.block_bitmap, &bitmap).await.unwrap();

        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.code == "block-bitmap-differs"),
            "{}",
            describe(&report)
        );

        let report = check(&dev, &FsckOptions::repair()).await.unwrap();
        assert!(report.repaired_anything());
        let again = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(again.is_clean(), "after repair:\n{}", describe(&again));
    }

    #[tokio::test]
    async fn notices_and_repairs_a_wrong_link_count() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let mut root = fs.read_inode(ino::ROOT).await.unwrap();
        root.links_count = 99;
        fs.write_inode(ino::ROOT, &root).await.unwrap();

        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(
            report.problems.iter().any(|p| p.code == "link-count-wrong"),
            "{}",
            describe(&report)
        );

        check(&dev, &FsckOptions::repair()).await.unwrap();
        let fs = Filesystem::open(&dev).await.unwrap();
        let root = fs.read_inode(ino::ROOT).await.unwrap();
        assert_eq!(root.links_count, 3);

        let again = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(again.is_clean(), "after repair:\n{}", describe(&again));
    }

    #[tokio::test]
    async fn notices_a_corrupted_group_descriptor_checksum() {
        let dev = formatted(Profile::Ext4, 16 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        fs.group_descs_mut()[0].checksum ^= 0xffff;
        // Write the table without re-stamping, which flush would do.
        let sb = fs.superblock().clone();
        let desc_size = sb.desc_size() as usize;
        let mut raw = vec![0u8; sb.gdt_blocks() as usize * sb.block_size() as usize];
        for (g, d) in fs.group_descs().iter().enumerate() {
            d.encode_into(&mut raw[g * desc_size..], desc_size);
        }
        let gdt_block = if sb.block_size() == 1024 { 2 } else { 1 };
        fs.write_block(gdt_block, &raw).await.unwrap();

        let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
        assert!(
            report.problems.iter().any(|p| p.code == "group-desc-csum"),
            "{}",
            describe(&report)
        );
    }

    #[tokio::test]
    async fn a_device_that_is_not_a_filesystem_is_refused() {
        let dev = MemDevice::new(4 * MIB);
        assert!(check(dev, &FsckOptions::check_only()).await.is_err());
    }

    #[tokio::test]
    async fn exit_codes_follow_e2fsck() {
        let clean = FsckReport::default();
        assert_eq!(clean.exit_code(), 0);

        let mut corrected = FsckReport::default();
        corrected.push_fixed(5, "x", Severity::Fixable, "fixed".into(), true);
        assert_eq!(corrected.exit_code(), 1);

        let mut uncorrected = FsckReport::default();
        uncorrected.push(1, "y", Severity::Serious, "left alone".into());
        assert_eq!(uncorrected.exit_code(), 4);
    }
}
