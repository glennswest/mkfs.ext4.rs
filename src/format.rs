//! Laying the filesystem down.
//!
//! Mirrors what `mke2fs` writes, in the order it matters: geometry, then the
//! metadata each block group owns, then the root directory and `lost+found`,
//! then the superblock and its backups.
//!
//! Block groups are disjoint byte ranges, so they are written concurrently
//! rather than one after another — the reason [`BlockDevice`] takes `&self`.

use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::bytes::put_u32;
use crate::csum::{self, GroupDescCsum};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::features::{CompatFeatures, IncompatFeatures, RoCompatFeatures};
use crate::journal::{self, JournalSuperblock};
use crate::layout::Geometry;
use crate::params::{JournalSize, Params};
use crate::structs::dirent::{self, file_type, DirEntry};
use crate::structs::extent::{self, Extent};
use crate::structs::group_desc::{bg_flags, GroupDesc};
use crate::structs::inode::{iflags, mode, Inode};
use crate::structs::superblock::{self, ino, Superblock, SUPERBLOCK_OFFSET};

/// What a format produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Total blocks.
    pub blocks_count: u64,
    /// Block size in bytes.
    pub block_size: u32,
    /// Total inodes.
    pub inodes_count: u32,
    /// Blocks not used by metadata, the root directory or the journal.
    pub free_blocks_count: u64,
    /// Inodes still available.
    pub free_inodes_count: u32,
    /// Block groups.
    pub group_count: u32,
    /// Filesystem UUID.
    pub uuid: [u8; 16],
    /// Volume label.
    pub label: String,
    /// Journal blocks, or zero when there is no journal.
    pub journal_blocks: u32,
}

impl Report {
    /// The UUID in canonical hyphenated form.
    pub fn uuid_string(&self) -> String {
        uuid::Uuid::from_bytes(self.uuid).hyphenated().to_string()
    }
}

/// A run of blocks set aside for something other than group metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    start: u64,
    len: u64,
}

impl Run {
    fn contains(&self, block: u64) -> bool {
        block >= self.start && block < self.start + self.len
    }
}

/// Everything decided before a byte is written.
struct Plan {
    geom: Geometry,
    uuid: [u8; 16],
    hash_seed: [u32; 4],
    mkfs_time: u64,
    label: String,
    /// Non-metadata allocations: root directory, `lost+found`, the resize
    /// inode's indirect block and the journal.
    runs: Vec<Run>,
    root_block: u64,
    lost_found_start: u64,
    lost_found_blocks: u32,
    resize_dind: Option<u64>,
    journal_start: Option<u64>,
    journal_blocks: u32,
    /// The journal's blocks, which need not be one contiguous run.
    journal_runs: Vec<Run>,
    /// Extents mapping the journal on a filesystem with extents.
    journal_extents: Vec<Extent>,
    /// Block holding the journal's extent leaf, when more extents are needed
    /// than the four that fit inside the inode.
    journal_extent_leaf: Option<u64>,
    /// Indirect blocks mapping the journal on a filesystem without extents.
    journal_indirect: IndirectMap,
    /// The journal inode's `i_block` image, backed up into `s_jnl_blocks`.
    journal_i_block: [u8; crate::structs::inode::I_BLOCK_LEN],
    /// Inode holding the orphan file, when the feature is on.
    orphan_inode: Option<u32>,
    /// The orphan file's blocks.
    orphan_runs: Vec<Run>,
    /// The orphan inode's `i_block` image.
    orphan_i_block: [u8; crate::structs::inode::I_BLOCK_LEN],
    /// Extent leaf for the orphan file, if it needed one.
    orphan_extent_leaf: Option<u64>,
    /// Indirect blocks mapping the orphan file without extents.
    orphan_indirect: IndirectMap,
    /// Block holding the multiple-mount-protection structure.
    mmp_block: Option<u64>,
    /// Seconds a holder promises to heartbeat within.
    mmp_update_interval: u16,
    csum_scheme: GroupDescCsum,
    csum_seed: u32,
    lazy_itable: bool,
    zeroed_medium: bool,
    resuid: u16,
    resgid: u16,
}

impl Plan {
    /// `s_jnl_blocks` — a copy of the journal inode's block map and size.
    ///
    /// Fifteen words of `i_block`, then the high and low halves of the size.
    /// If the journal inode is ever lost, this is what lets e2fsck find the
    /// journal again, which is why `dumpe2fs` reports "Journal backup: inode
    /// blocks" on any journalled filesystem.
    fn jnl_blocks_backup(&self) -> [u32; 17] {
        let mut out = [0u32; 17];
        if self.journal_blocks == 0 {
            return out;
        }
        for (i, slot) in out.iter_mut().take(15).enumerate() {
            *slot = crate::bytes::get_u32(&self.journal_i_block, i * 4);
        }
        let size = self.journal_blocks as u64 * self.geom.block_size as u64;
        out[15] = (size >> 32) as u32;
        out[16] = size as u32;
        out
    }
}

/// A simple forward-scanning allocator over the blocks metadata left alone.
///
/// Holds the few numbers it needs rather than a reference to the geometry, so
/// the geometry can move into the [`Plan`] afterwards.
struct Allocator<F: Fn(u64) -> bool> {
    blocks_count: u64,
    first_data_block: u64,
    block_size: u32,
    runs: Vec<Run>,
    metadata: F,
}

impl<F: Fn(u64) -> bool> Allocator<F> {
    fn taken(&self, block: u64) -> bool {
        (self.metadata)(block) || self.runs.iter().any(|r| r.contains(block))
    }

    /// Allocate `count` free blocks at or after `goal`, in as few runs as the
    /// free space allows.
    ///
    /// A large file need not be contiguous, and on a filesystem without
    /// `flex_bg` it usually cannot be: every group opens with its own
    /// superblock copy, bitmaps and inode table, so the longest free run is
    /// always shorter than a group. An 8192-block journal on a 256 MiB ext3
    /// filesystem has nowhere contiguous to go, and demanding one run fails a
    /// format that should succeed.
    fn alloc_runs(&mut self, goal: u64, count: u64) -> Result<Vec<Run>> {
        let mut runs: Vec<Run> = Vec::new();
        let mut found = 0u64;

        for start in [goal, self.first_data_block] {
            let mut block = start;
            while found < count && block < self.blocks_count {
                if self.taken(block) {
                    block += 1;
                    continue;
                }
                // Extend this run as far as the free space goes.
                let run_start = block;
                let mut len = 0u64;
                while block < self.blocks_count && !self.taken(block) && found + len < count {
                    len += 1;
                    block += 1;
                }
                runs.push(Run {
                    start: run_start,
                    len,
                });
                // Record it so the next probe sees these blocks as taken.
                self.runs.push(Run {
                    start: run_start,
                    len,
                });
                found += len;
            }
            if found >= count {
                return Ok(runs);
            }
        }

        Err(Error::DeviceTooSmall {
            available: self.blocks_count,
            needed: count,
            block_size: self.block_size,
        })
    }

    /// Allocate `len` contiguous free blocks at or after `goal`, falling back
    /// to the start of the filesystem if the tail cannot hold them.
    fn alloc(&mut self, goal: u64, len: u64) -> Result<u64> {
        for start in [goal, self.first_data_block] {
            let mut block = start;
            'scan: while block + len <= self.blocks_count {
                for offset in 0..len {
                    if self.taken(block + offset) {
                        block += offset + 1;
                        continue 'scan;
                    }
                }
                self.runs.push(Run { start: block, len });
                return Ok(block);
            }
        }
        Err(Error::DeviceTooSmall {
            available: self.blocks_count,
            needed: len,
            block_size: self.block_size,
        })
    }
}

/// A block map built from indirect blocks, for filesystems without extents.
///
/// ext2 and ext3 reach beyond twelve blocks through a single, double and
/// triple indirect block. Those blocks are themselves allocated from the
/// filesystem and counted against the inode, which is why this is built during
/// planning rather than at write time.
#[derive(Debug, Default)]
struct IndirectMap {
    pointers: [u32; crate::structs::inode::N_BLOCKS],
    /// Indirect blocks to write, as (block number, the pointers it holds).
    blocks: Vec<(u64, Vec<u32>)>,
}

impl IndirectMap {
    /// Blocks spent on the indirect blocks themselves.
    fn overhead(&self) -> u64 {
        self.blocks.len() as u64
    }
}

/// Collapse a list of blocks into contiguous runs.
fn runs_of(blocks: &[u64]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for &block in blocks {
        match runs.last_mut() {
            Some(last) if last.start + last.len == block => last.len += 1,
            _ => runs.push(Run {
                start: block,
                len: 1,
            }),
        }
    }
    runs
}

/// Allocate a block-mapped file, interleaving its indirect blocks with its
/// data the way `mke2fs` does.
///
/// The order is not incidental. Walking the file logically and taking the next
/// free block each time — data, then the indirect block at the moment it is
/// first needed, then more data — is what produces a map like
/// `(0-11):786-797, (IND):798, (12-267):799-1054, (DIND):1055, …`, and it keeps
/// each indirect block next to the data it points at. Allocating all the data
/// first and the indirect blocks afterwards is equally valid and reads worse.
fn alloc_indirect_file<F: Fn(u64) -> bool>(
    alloc: &mut Allocator<F>,
    goal: u64,
    count: u32,
    block_size: u32,
) -> Result<(Vec<u64>, IndirectMap)> {
    use crate::structs::inode::{N_BLOCKS, NDIR_BLOCKS};

    let per_block = (block_size / 4) as usize;
    let mut data: Vec<u64> = Vec::with_capacity(count as usize);
    let mut map = IndirectMap {
        pointers: [0u32; N_BLOCKS],
        blocks: Vec::new(),
    };
    let mut cursor = goal;
    let take = |alloc: &mut Allocator<F>, cursor: &mut u64| -> Result<u64> {
        let block = alloc.alloc(*cursor, 1)?;
        *cursor = block + 1;
        Ok(block)
    };

    // The twelve direct blocks.
    for i in 0..NDIR_BLOCKS.min(count as usize) {
        let block = take(alloc, &mut cursor)?;
        map.pointers[i] = block as u32;
        data.push(block);
    }
    if data.len() as u32 >= count {
        return Ok((data, map));
    }

    // One single indirect block, then the data it covers.
    let ind = take(alloc, &mut cursor)?;
    map.pointers[NDIR_BLOCKS] = ind as u32;
    let mut entries = Vec::new();
    while entries.len() < per_block && (data.len() as u32) < count {
        let block = take(alloc, &mut cursor)?;
        entries.push(block as u32);
        data.push(block);
    }
    map.blocks.push((ind, entries));
    if data.len() as u32 >= count {
        return Ok((data, map));
    }

    // A double indirect block, then a single indirect block and its data,
    // repeatedly.
    let dind = take(alloc, &mut cursor)?;
    map.pointers[NDIR_BLOCKS + 1] = dind as u32;
    let mut children = Vec::new();
    while (data.len() as u32) < count && children.len() < per_block {
        let child = take(alloc, &mut cursor)?;
        let mut child_entries = Vec::new();
        while child_entries.len() < per_block && (data.len() as u32) < count {
            let block = take(alloc, &mut cursor)?;
            child_entries.push(block as u32);
            data.push(block);
        }
        map.blocks.push((child, child_entries));
        children.push(child as u32);
    }
    map.blocks.push((dind, children));

    if (data.len() as u32) < count {
        return Err(Error::invalid(format!(
            "a {count}-block file exceeds what double indirection addresses at \
             {block_size}-byte blocks"
        )));
    }

    Ok((data, map))
}

/// Map `blocks` — a file's physical blocks in logical order — through direct
/// and indirect pointers, allocating indirect blocks with `alloc_one`.
fn build_indirect(
    blocks: &[u64],
    block_size: u32,
    mut alloc_one: impl FnMut() -> Result<u64>,
) -> Result<IndirectMap> {
    use crate::structs::inode::{N_BLOCKS, NDIR_BLOCKS};

    let len = blocks.len() as u32;
    let per_block = (block_size / 4) as usize;
    let mut map = IndirectMap {
        pointers: [0u32; N_BLOCKS],
        blocks: Vec::new(),
    };

    let mut next = 0u32;
    let take = |n: usize, next: &mut u32| -> Vec<u32> {
        let mut out = Vec::new();
        while out.len() < n && *next < len {
            out.push(blocks[*next as usize] as u32);
            *next += 1;
        }
        out
    };

    for slot in map.pointers.iter_mut().take(NDIR_BLOCKS) {
        if next >= len {
            break;
        }
        *slot = blocks[next as usize] as u32;
        next += 1;
    }

    if next < len {
        let ind = alloc_one()?;
        map.pointers[NDIR_BLOCKS] = ind as u32;
        let entries = take(per_block, &mut next);
        map.blocks.push((ind, entries));
    }

    if next < len {
        let dind = alloc_one()?;
        map.pointers[NDIR_BLOCKS + 1] = dind as u32;
        let mut children = Vec::new();
        while next < len && children.len() < per_block {
            let ind = alloc_one()?;
            let entries = take(per_block, &mut next);
            map.blocks.push((ind, entries));
            children.push(ind as u32);
        }
        map.blocks.push((dind, children));
    }

    if next < len {
        let tind = alloc_one()?;
        map.pointers[NDIR_BLOCKS + 2] = tind as u32;
        let mut grandchildren = Vec::new();
        while next < len && grandchildren.len() < per_block {
            let dind = alloc_one()?;
            let mut children = Vec::new();
            while next < len && children.len() < per_block {
                let ind = alloc_one()?;
                let entries = take(per_block, &mut next);
                map.blocks.push((ind, entries));
                children.push(ind as u32);
            }
            map.blocks.push((dind, children));
            grandchildren.push(dind as u32);
        }
        map.blocks.push((tind, grandchildren));
    }

    if next < len {
        return Err(Error::invalid(format!(
            "a {len}-block file exceeds what triple indirection addresses at {block_size}-byte blocks"
        )));
    }

    Ok(map)
}

/// Where to start looking for room for the journal.
///
/// `get_midpoint_journal_block()` in `lib/ext2fs/mkjournal.c`. Not simply the
/// middle of the filesystem: it takes the group the midpoint falls in, looks at
/// that group and its neighbours, and picks whichever has the most free blocks.
/// On a 64 MiB ext4 filesystem that lands the journal at block 16385 rather
/// than 30720 — the group either side of the midpoint carries a superblock
/// backup, and the emptier neighbour wins.
fn midpoint_journal_block<F: Fn(u64) -> bool>(geom: &Geometry, alloc: &Allocator<F>) -> u64 {
    // Free blocks in a group, counted the way things stand right now: after
    // the root directory, lost+found and the resize inode have been placed,
    // and before the journal is.
    let free_in = |group: u32| -> u64 {
        let first = geom.group_first_block(group);
        let last = geom.group_last_block(group);
        (first..=last).filter(|&b| !alloc.taken(b)).count() as u64
    };

    let midpoint = (geom.blocks_count - geom.first_data_block as u64) / 2;
    let mut group = ((midpoint.saturating_sub(geom.first_data_block as u64))
        / geom.blocks_per_group as u64) as u32;
    group = group.min(geom.group_count - 1);

    let log_flex = 1u32 << geom.log_groups_per_flex;
    let start;
    if geom.log_groups_per_flex > 0 && group > log_flex {
        // Snap to the start of the flex group, then walk forward past any that
        // are entirely full.
        group &= !(log_flex - 1);
        while group < geom.group_count && free_in(group) == 0 {
            group += 1;
        }
        if group == geom.group_count {
            group = 0;
        }
        start = group;
    } else {
        start = if group > 0 { group - 1 } else { group };
    }
    let end = if group + 1 < geom.group_count {
        group + 1
    } else {
        group
    };

    // The emptiest of the candidates, ties going to the earliest.
    let mut best = start;
    let mut best_free = free_in(start);
    for candidate in start + 1..=end {
        let free = free_in(candidate);
        if free > best_free {
            best = candidate;
            best_free = free;
        }
    }

    geom.group_first_block(best)
}

/// `EXT4_ORPHAN_BLOCK_MAGIC`
pub const ORPHAN_BLOCK_MAGIC: u32 = 0x0b10_ca04;

/// `EXT4_MAX_ORPHAN_FILE_BLOCKS`
pub const MAX_ORPHAN_FILE_BLOCKS: u32 = 512;

/// Bytes of `ext4_orphan_block_tail`, at the end of every orphan file block.
pub const ORPHAN_TAIL_LEN: usize = 8;

/// `ext2fs_default_orphan_file_blocks()` — how large an orphan file to make.
pub fn default_orphan_file_blocks(blocks_count: u64) -> u32 {
    let blks = if blocks_count < 128 * 1024 {
        32
    } else if blocks_count < 2 * 1024 * 1024 {
        (blocks_count / 4096) as u32
    } else {
        512
    };
    blks.min(MAX_ORPHAN_FILE_BLOCKS)
}

/// Inode slots one orphan block can track.
fn inodes_per_orphan_block(block_size: u32) -> usize {
    (block_size as usize - ORPHAN_TAIL_LEN) / 4
}

/// Build one orphan file block: zeroed slots, then the magic and checksum.
fn orphan_block(
    block_size: u32,
    seed: u32,
    inum: u32,
    generation: u32,
    physical: u64,
    metadata_csum: bool,
) -> Vec<u8> {
    let mut buf = vec![0u8; block_size as usize];
    let tail = block_size as usize - ORPHAN_TAIL_LEN;
    put_u32(&mut buf, tail, ORPHAN_BLOCK_MAGIC);

    if metadata_csum {
        // Seeded with the inode, its generation and the block's own number, so
        // a block cannot be moved or replayed somewhere else unnoticed.
        let mut crc = csum::crc32c(seed, &inum.to_le_bytes());
        crc = csum::crc32c(crc, &generation.to_le_bytes());
        crc = csum::crc32c(crc, &physical.to_le_bytes());
        crc = csum::crc32c(crc, &buf[..inodes_per_orphan_block(block_size) * 4]);
        put_u32(&mut buf, tail + 4, crc);
    }
    buf
}

/// The `i_block` image for an extent-mapped journal.
///
/// Four extents fit inside the inode. Beyond that the inode holds a depth-1
/// header and a single index pointing at `leaf`, which carries the extents.
fn journal_inode_i_block(
    extents: &[Extent],
    leaf: Option<u64>,
    block_size: u32,
) -> Result<[u8; crate::structs::inode::I_BLOCK_LEN]> {
    match leaf {
        None => extent::build_inline(extents),
        Some(leaf_block) => {
            let mut out = [0u8; crate::structs::inode::I_BLOCK_LEN];
            let header = extent::ExtentHeader {
                entries: 1,
                max: extent::ExtentHeader::max_entries(extent::INLINE_LEN, false),
                depth: 1,
                generation: 0,
            };
            header.encode_into(&mut out);
            let idx = extent::ExtentIdx {
                block: 0,
                leaf: leaf_block,
            };
            idx.encode_into(&mut out[extent::HEADER_LEN..extent::HEADER_LEN + extent::ENTRY_LEN]);
            let _ = block_size;
            Ok(out)
        }
    }
}

/// The extent leaf block's contents, when the journal needs one.
fn journal_extent_leaf_block(extents: &[Extent], block_size: u32, with_tail: bool) -> Vec<u8> {
    let mut buf = vec![0u8; block_size as usize];
    let header = extent::ExtentHeader {
        entries: extents.len() as u16,
        max: extent::ExtentHeader::max_entries(block_size as usize, with_tail),
        depth: 0,
        generation: 0,
    };
    header.encode_into(&mut buf);
    for (i, ext) in extents.iter().enumerate() {
        let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
        ext.encode_into(&mut buf[at..at + extent::ENTRY_LEN]);
    }
    buf
}

/// Format `dev` according to `params`.
///
/// The device is written in place; everything previously on it is lost.
pub async fn format<D: BlockDevice + ?Sized>(dev: &D, params: &Params) -> Result<Report> {
    // The device knows its own sector size; a caller may override it, but must
    // not have to state it.
    let mut params = params.clone();
    if params.sector_size.is_none() {
        params.sector_size = Some(dev.logical_sector_size());
    }
    let params = &params;
    let plan = plan(dev.size(), params)?;
    write_filesystem(dev, &plan).await
}

/// Work out the whole layout without touching the device.
///
/// Separated so the plan can be inspected and tested — and diffed against a
/// reference filesystem — without writing anything.
fn plan(device_size: u64, params: &Params) -> Result<Plan> {
    let geom = Geometry::compute(device_size, params)?;

    let uuid = params.uuid.unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes());
    let hash_seed_bytes = params
        .hash_seed
        .unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes());
    let mut hash_seed = [0u32; 4];
    for (i, seed) in hash_seed.iter_mut().enumerate() {
        *seed = u32::from_le_bytes([
            hash_seed_bytes[i * 4],
            hash_seed_bytes[i * 4 + 1],
            hash_seed_bytes[i * 4 + 2],
            hash_seed_bytes[i * 4 + 3],
        ]);
    }

    let mkfs_time = params.mkfs_time.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    let label = params.label.clone().unwrap_or_default();

    let csum_scheme = if geom
        .features
        .ro_compat
        .contains(RoCompatFeatures::METADATA_CSUM)
    {
        GroupDescCsum::Crc32c
    } else if geom.features.ro_compat.contains(RoCompatFeatures::GDT_CSUM) {
        GroupDescCsum::Crc16
    } else {
        GroupDescCsum::None
    };

    let csum_seed = if geom.features.incompat.contains(IncompatFeatures::CSUM_SEED) {
        csum::seed_from_uuid(&uuid)
    } else if csum_scheme == GroupDescCsum::Crc32c {
        csum::seed_from_uuid(&uuid)
    } else {
        0
    };

    // Allocate the non-metadata blocks in the order mke2fs does: the root
    // directory first, then lost+found, then the resize inode's indirect
    // block, then the journal near the middle of the filesystem.
    // Metadata placement is the same question the bitmaps ask, so it is asked
    // the same way. Runs are cached per flex group: the allocator walks many
    // blocks looking for room for the journal, and recomputing placement for
    // each one would be needlessly quadratic.
    let metadata_probe = {
        let geom = geom.clone();
        let cache: std::cell::RefCell<Option<(u32, Vec<(u64, u64)>)>> =
            std::cell::RefCell::new(None);
        move |block: u64| -> bool {
            let g = &geom;
            if g.in_super_region(block) {
                return true;
            }
            let group =
                ((block - g.first_data_block as u64) / g.blocks_per_group as u64) as u32;
            let flex_first = g.flex_first_group(group);

            let mut cache = cache.borrow_mut();
            if cache.as_ref().map(|(f, _)| *f) != Some(flex_first) {
                match g.flex_metadata_runs(group) {
                    Ok(runs) => *cache = Some((flex_first, runs)),
                    Err(_) => return true,
                }
            }
            cache
                .as_ref()
                .map(|(_, runs)| {
                    runs.iter()
                        .any(|&(start, len)| block >= start && block < start + len)
                })
                .unwrap_or(true)
        }
    };

    let mut alloc = Allocator {
        blocks_count: geom.blocks_count,
        first_data_block: geom.first_data_block as u64,
        block_size: geom.block_size,
        runs: Vec::new(),
        metadata: metadata_probe,
    };

    let first_data = geom.first_data_block as u64;
    let root_block = alloc.alloc(first_data, 1)?;

    // lost+found is grown to 16 KiB, but never past the 12 direct block
    // pointers a classic inode has. `create_lost_and_found()` in mke2fs.c.
    let lost_found_blocks = (16 * 1024u32).div_ceil(geom.block_size).clamp(1, 12);
    let lost_found_start = alloc.alloc(first_data, lost_found_blocks as u64)?;

    let resize_dind = if geom.features.compat.contains(CompatFeatures::RESIZE_INODE)
        && geom.reserved_gdt_blocks > 0
    {
        Some(alloc.alloc(first_data, 1)?)
    } else {
        None
    };

    let journal_blocks = match params.journal {
        _ if !geom.features.compat.contains(CompatFeatures::HAS_JOURNAL) => 0,
        JournalSize::None => 0,
        JournalSize::Blocks(n) => n,
        JournalSize::Default => journal::default_journal_blocks(geom.blocks_count).unwrap_or(0),
    };
    let extents = geom.features.incompat.contains(IncompatFeatures::EXTENTS);
    let mut journal_indirect = IndirectMap::default();
    let mut journal_runs: Vec<Run> = Vec::new();
    let mut journal_extents: Vec<Extent> = Vec::new();
    let journal_start = if journal_blocks > 0 {
        // An extent-mapped journal is one contiguous run placed near the middle
        // of the filesystem. A block-mapped one is allocated block by block
        // from the first free block with its indirect blocks interleaved,
        // which is what mke2fs produces and what keeps each indirect block
        // beside the data it describes.
        let ordered: Vec<u64> = if extents {
            let goal = midpoint_journal_block(&geom, &alloc);
            journal_runs = alloc.alloc_runs(goal, journal_blocks as u64)?;

            let mut logical = 0u32;
            for run in &journal_runs {
                let mut done = 0u64;
                while done < run.len {
                    let chunk = (run.len - done).min(extent::INIT_MAX_LEN as u64 - 1);
                    journal_extents.push(Extent {
                        block: logical,
                        len: chunk as u16,
                        start: run.start + done,
                    });
                    logical += chunk as u32;
                    done += chunk;
                }
            }
            journal_runs
                .iter()
                .flat_map(|r| r.start..r.start + r.len)
                .collect()
        } else {
            let (data, map) = alloc_indirect_file(
                &mut alloc,
                geom.first_data_block as u64,
                journal_blocks,
                geom.block_size,
            )?;
            journal_indirect = map;
            journal_runs = runs_of(&data);
            data
        };
        ordered.first().copied()
    } else {
        None
    };

    // The orphan file tracks inodes awaiting deletion so a crash does not
    // strand them. It takes the first inode after lost+found.
    let orphan_blocks = if geom.features.compat.contains(CompatFeatures::ORPHAN_FILE) {
        default_orphan_file_blocks(geom.blocks_count)
    } else {
        0
    };
    let mut orphan_runs: Vec<Run> = Vec::new();
    let mut orphan_extents: Vec<Extent> = Vec::new();
    let mut orphan_indirect = IndirectMap::default();
    let orphan_inode = if orphan_blocks > 0 {
        orphan_runs = alloc.alloc_runs(geom.first_data_block as u64, orphan_blocks as u64)?;
        let ordered: Vec<u64> = orphan_runs
            .iter()
            .flat_map(|r| r.start..r.start + r.len)
            .collect();
        if extents {
            let mut logical = 0u32;
            for run in &orphan_runs {
                orphan_extents.push(Extent {
                    block: logical,
                    len: run.len as u16,
                    start: run.start,
                });
                logical += run.len as u32;
            }
        } else {
            let first_data = geom.first_data_block as u64;
            orphan_indirect =
                build_indirect(&ordered, geom.block_size, || alloc.alloc(first_data, 1))?;
        }
        Some(superblock::GOOD_OLD_FIRST_INO + 1)
    } else {
        None
    };

    // Multiple mount protection: one block, somewhere in the middle of the
    // filesystem so a stray write to the start does not take the fence with it.
    let (mmp_block, mmp_update_interval) =
        if geom.features.incompat.contains(IncompatFeatures::MMP) {
            let goal = geom.blocks_count / 2;
            let block = alloc.alloc(goal, 1)?;
            let interval = params
                .mmp_update_interval
                .unwrap_or(crate::mmp::UPDATE_INTERVAL)
                .clamp(crate::mmp::MIN_CHECK_INTERVAL, crate::mmp::MAX_UPDATE_INTERVAL);
            (Some(block), interval)
        } else {
            (None, 0)
        };

    // Four extents fit inside an inode. More than that needs a leaf block, and
    // the inode becomes a one-level tree pointing at it.
    let inline_max = extent::ExtentHeader::max_entries(extent::INLINE_LEN, false) as usize;
    let journal_extent_leaf = if extents && journal_extents.len() > inline_max {
        Some(alloc.alloc(geom.first_data_block as u64, 1)?)
    } else {
        None
    };

    let orphan_extent_leaf = if extents && orphan_extents.len() > inline_max {
        Some(alloc.alloc(geom.first_data_block as u64, 1)?)
    } else {
        None
    };
    let mut orphan_i_block = [0u8; crate::structs::inode::I_BLOCK_LEN];
    if orphan_inode.is_some() {
        if extents {
            orphan_i_block =
                journal_inode_i_block(&orphan_extents, orphan_extent_leaf, geom.block_size)?;
        } else {
            let mut probe = Inode::default();
            probe.set_block_pointers(&orphan_indirect.pointers);
            orphan_i_block = probe.block;
        }
    }

    // The journal inode's block map, computed here so the superblock can carry
    // the backup copy of it that mke2fs records in s_jnl_blocks.
    let mut journal_i_block = [0u8; crate::structs::inode::I_BLOCK_LEN];
    if journal_start.is_some() {
        if extents {
            journal_i_block = journal_inode_i_block(
                &journal_extents,
                journal_extent_leaf,
                geom.block_size,
            )?;
        } else {
            let mut probe = Inode::default();
            probe.set_block_pointers(&journal_indirect.pointers);
            journal_i_block = probe.block;
        }
    }

    Ok(Plan {
        geom,
        uuid,
        hash_seed,
        mkfs_time,
        label,
        runs: alloc.runs,
        root_block,
        lost_found_start,
        lost_found_blocks,
        resize_dind,
        journal_start,
        journal_blocks,
        journal_runs,
        journal_extents,
        journal_extent_leaf,
        journal_indirect,
        journal_i_block,
        orphan_inode,
        orphan_runs,
        orphan_i_block,
        orphan_extent_leaf,
        orphan_indirect,
        mmp_block,
        mmp_update_interval,
        csum_scheme,
        csum_seed,
        lazy_itable: params.lazy_itable_init,
        zeroed_medium: params.zeroed_medium,
        resuid: params.resuid,
        resgid: params.resgid,
    })
}

/// Per-group accounting, computed once and used for both the bitmaps and the
/// group descriptors.
struct GroupState {
    desc: GroupDesc,
    block_bitmap: Vec<u8>,
    inode_bitmap: Vec<u8>,
}

/// Build one group's bitmaps and descriptor.
fn build_group(plan: &Plan, group: u32) -> Result<GroupState> {
    let g = &plan.geom;
    let layout = g.group(group)?;
    let block_size = g.block_size as usize;

    // Block bitmap: one bit per block in the group, padded to a full block with
    // ones so nothing past the end of the group is ever considered free.
    //
    // Built by marking ranges rather than asking a predicate per block. The
    // difference is not cosmetic: a 1 TiB filesystem has 268 million blocks,
    // and one placement query each would dominate the whole format.
    let mut block_bitmap = vec![0u8; block_size];
    let group_blocks = g.blocks_per_group as u64;

    let mark = |from: u64, to_exclusive: u64, bitmap: &mut Vec<u8>| {
        // Clamp to this group's own window before setting anything.
        let lo = from.max(layout.first_block);
        let hi = to_exclusive.min(layout.first_block + group_blocks);
        for block in lo..hi {
            let i = block - layout.first_block;
            bitmap[(i / 8) as usize] |= 1 << (i % 8);
        }
    };

    // Superblock copy, descriptor blocks and reserved descriptor blocks at the
    // front of this group. Not guarded on `has_super`: under meta_bg the last
    // group of a meta block group holds a descriptor block copy without
    // carrying a superblock backup, and that block is in use all the same.
    {
        let start = g.group_first_block(layout.group);
        mark(
            start,
            start + g.super_overhead(layout.group) as u64,
            &mut block_bitmap,
        );
    }
    // Bitmaps and inode tables of this group's flex group.
    for (start, len) in g.flex_metadata_runs(layout.group)? {
        mark(start, start + len, &mut block_bitmap);
    }
    // The root directory, lost+found, resize indirect block and journal.
    for run in &plan.runs {
        mark(run.start, run.start + run.len, &mut block_bitmap);
    }
    // Anything past the end of a short final group is not a block at all.
    mark(
        layout.last_block + 1,
        layout.first_block + group_blocks,
        &mut block_bitmap,
    );

    let used_blocks: u32 = block_bitmap
        .iter()
        .take((group_blocks as usize).div_ceil(8))
        .map(|b| b.count_ones())
        .sum::<u32>();
    let free_blocks = g.blocks_per_group - used_blocks;

    // Bits past blocks_per_group in the final bitmap block belong to no block.
    for bit in g.blocks_per_group as usize..block_size * 8 {
        block_bitmap[bit / 8] |= 1 << (bit % 8);
    }

    // Inode bitmap: inodes 1 through first_ino-1 are reserved, and lost+found
    // takes the first non-reserved one.
    let mut inode_bitmap = vec![0u8; block_size];
    let mut free_inodes = g.inodes_per_group;
    let mut used_dirs = 0u32;
    if group == 0 {
        let reserved = superblock::GOOD_OLD_FIRST_INO - 1; // inodes 1..=10
        for i in 0..reserved {
            inode_bitmap[(i / 8) as usize] |= 1 << (i % 8);
            free_inodes -= 1;
        }
        // lost+found, inode 11.
        let lpf = superblock::GOOD_OLD_FIRST_INO - 1;
        inode_bitmap[(lpf / 8) as usize] |= 1 << (lpf % 8);
        free_inodes -= 1;
        used_dirs = 2; // root and lost+found

        // The orphan file, inode 12, is a regular file and not a directory.
        if let Some(orphan) = plan.orphan_inode {
            let bit = (orphan - 1) as u64;
            inode_bitmap[(bit / 8) as usize] |= 1 << (bit % 8);
            free_inodes -= 1;
        }
    }
    for bit in g.inodes_per_group as usize..block_size * 8 {
        inode_bitmap[bit / 8] |= 1 << (bit % 8);
    }

    // Inodes never yet used, counted from the end of the table. e2fsck uses it
    // to skip reading the tail, and it is what makes a lazily written inode
    // table safe.
    let used_inodes = g.inodes_per_group - free_inodes;
    let itable_unused = if plan.csum_scheme == GroupDescCsum::None {
        0
    } else {
        g.inodes_per_group - used_inodes
    };

    // Group flags, exactly as `ext2fs_initialize()` and `mke2fs.c` set them.
    let mut flags = 0u16;

    // INODE_ZEROED says the table on disk really is zeroed, so the kernel need
    // not do it. Set whenever the table was written, checksums or not — which
    // is why a plain ext2 filesystem carries it and nothing else.
    if !plan.lazy_itable {
        flags |= bg_flags::INODE_ZEROED;
    }

    // The UNINIT flags only mean anything where a group descriptor checksum
    // can vouch for them.
    if plan.csum_scheme != GroupDescCsum::None {
        if used_inodes == 0 {
            flags |= bg_flags::INODE_UNINIT;
        }
        // Never on the last group: its bitmap carries padding bits for the
        // blocks past the end of the filesystem, so it cannot be reconstructed
        // from the geometry alone.
        if used_blocks == 0 && layout.group != g.group_count - 1 {
            flags |= bg_flags::BLOCK_UNINIT;
        }
    }

    let mut desc = GroupDesc {
        block_bitmap: layout.block_bitmap,
        inode_bitmap: layout.inode_bitmap,
        inode_table: layout.inode_table,
        free_blocks_count: free_blocks,
        free_inodes_count: free_inodes,
        used_dirs_count: used_dirs,
        flags,
        exclude_bitmap: 0,
        block_bitmap_csum: 0,
        inode_bitmap_csum: 0,
        itable_unused,
        checksum: 0,
    };

    if plan.csum_scheme == GroupDescCsum::Crc32c {
        // The bitmap checksums cover only the meaningful part of each bitmap,
        // not the whole block.
        let bb_len = (g.blocks_per_group as usize).div_ceil(8);
        let ib_len = (g.inodes_per_group as usize).div_ceil(8);
        desc.block_bitmap_csum = csum::bitmap_csum(plan.csum_seed, &block_bitmap[..bb_len]);
        desc.inode_bitmap_csum = csum::bitmap_csum(plan.csum_seed, &inode_bitmap[..ib_len]);
    }

    Ok(GroupState {
        desc,
        block_bitmap,
        inode_bitmap,
    })
}

/// Write the filesystem described by `plan`.
async fn write_filesystem<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan) -> Result<Report> {
    let g = &plan.geom;
    let block_size = g.block_size as u64;

    // Every group's descriptor is needed before any group descriptor table can
    // be written, so the accounting pass comes first. It touches no device.
    let mut states = Vec::with_capacity(g.group_count as usize);
    for group in 0..g.group_count {
        states.push(build_group(plan, group)?);
    }

    let free_blocks_count: u64 = states.iter().map(|s| s.desc.free_blocks_count as u64).sum();
    let free_inodes_count: u32 = states.iter().map(|s| s.desc.free_inodes_count).sum();

    // The group descriptor table, as it will appear in every copy.
    let desc_size = g.desc_size as usize;
    let mut gdt = vec![0u8; g.desc_blocks as usize * g.block_size as usize];
    for (group, state) in states.iter().enumerate() {
        let at = group * desc_size;
        state.desc.encode_with_csum(
            &mut gdt[at..at + desc_size],
            desc_size,
            plan.csum_scheme,
            plan.csum_seed,
            &plan.uuid,
            group as u32,
        );
    }

    let superblk = build_superblock(plan, free_blocks_count, free_inodes_count);

    // Zero the leading blocks so no previous filesystem's magic survives to
    // confuse blkid, and so block 0 of a 1 KiB filesystem is clean.
    dev.write_zeroes(0, SUPERBLOCK_OFFSET.min(dev.size())).await?;

    // Bitmaps, inode tables and superblock copies, fanned out across groups.
    let concurrency = plan.concurrency();
    let mut pending = FuturesUnordered::new();
    let gdt = Arc::new(gdt);
    let states = Arc::new(states);

    for group in 0..g.group_count {
        let gdt = Arc::clone(&gdt);
        let states = Arc::clone(&states);
        pending.push(write_group(dev, plan, group, gdt, states, &superblk));

        if pending.len() >= concurrency {
            if let Some(result) = pending.next().await {
                result?;
            }
        }
    }
    while let Some(result) = pending.next().await {
        result?;
    }

    // Content the filesystem needs before it is coherent: the root directory,
    // lost+found, the resize inode's indirect block, the journal, and the
    // inodes that point at all of them.
    write_root_and_lost_found(dev, plan).await?;
    if let Some(start) = plan.journal_start {
        write_journal(dev, plan, start).await?;
    }
    if plan.resize_dind.is_some() {
        write_resize_inode_blocks(dev, plan).await?;
    }
    if plan.orphan_inode.is_some() {
        write_orphan_file(dev, plan).await?;
    }
    if let Some(block) = plan.mmp_block {
        // Clean: nobody holds a filesystem that has just been created.
        let mmp = crate::mmp::Mmp {
            seq: crate::mmp::SEQ_CLEAN,
            time: plan.mkfs_time,
            nodename: String::new(),
            bdevname: String::new(),
            check_interval: plan.mmp_update_interval,
        };
        let buf = mmp.encode(
            g.block_size as usize,
            plan.csum_scheme == GroupDescCsum::Crc32c,
            plan.csum_seed,
        );
        dev.write_at(block * block_size, &buf).await?;
    }
    write_reserved_inodes(dev, plan).await?;

    // The primary superblock last: until it lands, a torn format is not a
    // filesystem at all, which is the failure mode to prefer.
    let mut sb_buf = superblk.encode();
    dev.write_at(SUPERBLOCK_OFFSET, &sb_buf).await?;
    // Backups carry their own group number.
    let mut backup = superblk.clone();
    for group in 1..g.group_count {
        if !g.has_super(group) {
            continue;
        }
        backup.block_group_nr = group as u16;
        sb_buf = backup.encode();
        let at = g.group_first_block(group) * block_size;
        dev.write_at(at, &sb_buf).await?;
    }

    dev.flush().await?;

    Ok(Report {
        blocks_count: g.blocks_count,
        block_size: g.block_size,
        inodes_count: g.inodes_count,
        free_blocks_count,
        free_inodes_count,
        group_count: g.group_count,
        uuid: plan.uuid,
        label: plan.label.clone(),
        journal_blocks: plan.journal_blocks,
    })
}

impl Plan {
    fn concurrency(&self) -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(8)
            .clamp(1, 64)
    }
}

/// Write one group's descriptor table copy, bitmaps and inode table.
async fn write_group<D: BlockDevice + ?Sized>(
    dev: &D,
    plan: &Plan,
    group: u32,
    gdt: Arc<Vec<u8>>,
    states: Arc<Vec<GroupState>>,
    _sb: &Superblock,
) -> Result<()> {
    let g = &plan.geom;
    let block_size = g.block_size as u64;
    let state = &states[group as usize];
    let layout = g.group(group)?;

    // Descriptors follow the superblock copy. Under the classic layout that is
    // the whole table; under meta_bg it is the one block covering this group's
    // meta block group, which is why a filesystem too large for a contiguous
    // table can still describe itself.
    if let Some(desc_at) = g.desc_block_location(group) {
        let at = desc_at * block_size;
        if g.meta_bg && g.meta_bg_of(group) >= g.first_meta_bg {
            let dpb = g.desc_per_block() as usize;
            let meta = g.meta_bg_of(group) as usize;
            let bytes = g.block_size as usize;
            let from = meta * bytes;
            let mut one = vec![0u8; bytes];
            let available = gdt.len().saturating_sub(from).min(bytes);
            one[..available].copy_from_slice(&gdt[from..from + available]);
            let _ = dpb;
            dev.write_at(at, &one).await?;
        } else {
            dev.write_at(at, &gdt).await?;
            // Reserved GDT blocks are only written where they hold something.
            // In group 0 they are the resize inode's indirect blocks and list
            // every backup copy; in a backup group they are reserved space and
            // nothing more, so `mke2fs` writes the superblock and descriptors
            // there and stops. On a 1 TiB filesystem that is eighteen copies of
            // 1024 blocks — 72 MiB of zeros written for no reader.
            if g.reserved_gdt_blocks > 0 && group == 0 && !plan.zeroed_medium {
                dev.write_zeroes(
                    at + gdt.len() as u64,
                    g.reserved_gdt_blocks as u64 * block_size,
                )
                .await?;
            }
        }
    }

    // A group whose descriptor says BLOCK_UNINIT or INODE_UNINIT has no
    // authoritative bitmap on disk: the flag says to compute it from the
    // geometry, and the descriptor checksum vouches for the flag. `mke2fs`
    // does not write those bitmaps, and on a 1 TiB filesystem that is 7,653
    // groups out of 8,192 — 62 MiB of blocks nothing will ever read.
    //
    // The bitmaps are still built, because their checksums go in the
    // descriptor either way.
    if state.desc.flags & bg_flags::BLOCK_UNINIT == 0 {
        dev.write_at(layout.block_bitmap * block_size, &state.block_bitmap)
            .await?;
    }
    if state.desc.flags & bg_flags::INODE_UNINIT == 0 {
        dev.write_at(layout.inode_bitmap * block_size, &state.inode_bitmap)
            .await?;
    }

    // Inode tables are zeroed unless the caller asked for lazy initialisation,
    // which leaves them untouched and marks the group uninitialised instead.
    // Group 0 always gets a table: the reserved inodes live in it.
    //
    // On a medium that already reads as zeros there is nothing to do either —
    // and unlike lazy initialisation this changes no flag, because the table
    // really is zeroed. It is the difference between "the kernel must zero
    // this later" and "this is already what it needs to be". `mke2fs` takes
    // the same view, which is why it marks every group ITABLE_ZEROED and
    // writes no inode table at all.
    let skip = plan.zeroed_medium || (plan.lazy_itable && group != 0);
    if !skip {
        dev.write_zeroes(
            layout.inode_table * block_size,
            g.itable_blocks_per_group as u64 * block_size,
        )
        .await?;
    }

    Ok(())
}

/// Build the superblock every copy is made from.
fn build_superblock(plan: &Plan, free_blocks: u64, free_inodes: u32) -> Superblock {
    let g = &plan.geom;
    let mut volume_name = [0u8; 16];
    let label = plan.label.as_bytes();
    let n = label.len().min(16);
    volume_name[..n].copy_from_slice(&label[..n]);

    let inode_size = g.inode_size;
    let extra_isize = if inode_size > superblock::GOOD_OLD_INODE_SIZE {
        32
    } else {
        0
    };

    Superblock {
        inodes_count: g.inodes_count,
        blocks_count: g.blocks_count,
        r_blocks_count: g.r_blocks_count,
        free_blocks_count: free_blocks,
        free_inodes_count: free_inodes,
        first_data_block: g.first_data_block,
        log_block_size: g.log_block_size,
        log_cluster_size: g.log_block_size,
        blocks_per_group: g.blocks_per_group,
        clusters_per_group: g.blocks_per_group,
        inodes_per_group: g.inodes_per_group,
        mtime: 0,
        wtime: plan.mkfs_time,
        mnt_count: 0,
        max_mnt_count: -1,
        state: superblock::state::VALID_FS,
        errors: superblock::errors::CONTINUE,
        minor_rev_level: 0,
        lastcheck: plan.mkfs_time,
        checkinterval: 0,
        creator_os: 0,
        rev_level: superblock::DYNAMIC_REV,
        def_resuid: plan.resuid,
        def_resgid: plan.resgid,
        first_ino: superblock::GOOD_OLD_FIRST_INO,
        inode_size,
        block_group_nr: 0,
        feature_compat: g.features.compat,
        feature_incompat: g.features.incompat,
        feature_ro_compat: g.features.ro_compat,
        uuid: plan.uuid,
        volume_name,
        last_mounted: [0; 64],
        algorithm_usage_bitmap: 0,
        prealloc_blocks: 0,
        prealloc_dir_blocks: 0,
        reserved_gdt_blocks: g.reserved_gdt_blocks,
        journal_uuid: [0; 16],
        journal_inum: if plan.journal_blocks > 0 {
            ino::JOURNAL
        } else {
            0
        },
        journal_dev: 0,
        last_orphan: 0,
        hash_seed: plan.hash_seed,
        def_hash_version: superblock::hash::HALF_MD4,
        jnl_backup_type: if plan.journal_blocks > 0 {
            superblock::JNL_BACKUP_BLOCKS
        } else {
            0
        },
        desc_size: if g
            .features
            .incompat
            .contains(IncompatFeatures::SIXTY_FOUR_BIT)
        {
            g.desc_size
        } else {
            0
        },
        default_mount_opts: crate::params::DEFAULT_MNTOPTS,
        first_meta_bg: g.first_meta_bg,
        mkfs_time: plan.mkfs_time,
        jnl_blocks: plan.jnl_blocks_backup(),
        min_extra_isize: extra_isize,
        want_extra_isize: extra_isize,
        flags: superblock::flags::SIGNED_HASH,
        raid_stride: 0,
        mmp_update_interval: plan.mmp_update_interval,
        mmp_block: plan.mmp_block.unwrap_or(0),
        raid_stripe_width: 0,
        log_groups_per_flex: g.log_groups_per_flex,
        checksum_type: if plan.csum_scheme == GroupDescCsum::Crc32c {
            csum::CRC32C_CHKSUM_TYPE
        } else {
            0
        },
        encryption_level: 0,
        kbytes_written: 0,
        snapshot_inum: 0,
        snapshot_id: 0,
        snapshot_r_blocks_count: 0,
        snapshot_list: 0,
        error_count: 0,
        first_error_time: 0,
        first_error_ino: 0,
        first_error_block: 0,
        first_error_func: [0; 32],
        first_error_line: 0,
        last_error_time: 0,
        last_error_ino: 0,
        last_error_line: 0,
        last_error_block: 0,
        last_error_func: [0; 32],
        first_error_errcode: 0,
        last_error_errcode: 0,
        mount_opts: [0; 64],
        usr_quota_inum: 0,
        grp_quota_inum: 0,
        overhead_clusters: 0,
        backup_bgs: [0; 2],
        encrypt_algos: [0; 4],
        encrypt_pw_salt: [0; 16],
        // mke2fs leaves s_lpf_ino at zero; e2fsck finds lost+found by name.
        lpf_ino: 0,
        prj_quota_inum: 0,
        checksum_seed: if g.features.incompat.contains(IncompatFeatures::CSUM_SEED) {
            plan.csum_seed
        } else {
            0
        },
        encoding: 0,
        encoding_flags: 0,
        orphan_file_inum: plan.orphan_inode.unwrap_or(0),
        checksum: 0,
    }
}

/// Write the root directory block and `lost+found`'s blocks.
async fn write_root_and_lost_found<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan) -> Result<()> {
    let g = &plan.geom;
    let block_size = g.block_size as usize;
    let with_tail = plan.csum_scheme == GroupDescCsum::Crc32c;
    let filetype = g.features.incompat.contains(IncompatFeatures::FILETYPE);
    let ft = |t: u8| if filetype { t } else { file_type::UNKNOWN };

    // Root: ".", "..", "lost+found".
    let root_entries = vec![
        DirEntry::new(ino::ROOT, b".", ft(file_type::DIR))?,
        DirEntry::new(ino::ROOT, b"..", ft(file_type::DIR))?,
        DirEntry::new(
            superblock::GOOD_OLD_FIRST_INO,
            b"lost+found",
            ft(file_type::DIR),
        )?,
    ];
    let mut root = dirent::build_block(&root_entries, block_size, with_tail)?;
    if with_tail {
        let c = csum::dirent_csum(
            plan.csum_seed,
            ino::ROOT,
            0,
            &root[..block_size - dirent::TAIL_LEN],
        );
        dirent::set_block_csum(&mut root, c);
    }
    dev.write_at(plan.root_block * g.block_size as u64, &root)
        .await?;

    // lost+found: "." and ".." in the first block, the rest empty but linked
    // into the directory so it is a valid, if empty, directory.
    let lpf_ino = superblock::GOOD_OLD_FIRST_INO;
    let lpf_entries = vec![
        DirEntry::new(lpf_ino, b".", ft(file_type::DIR))?,
        DirEntry::new(ino::ROOT, b"..", ft(file_type::DIR))?,
    ];
    let mut first = dirent::build_block(&lpf_entries, block_size, with_tail)?;
    if with_tail {
        let c = csum::dirent_csum(
            plan.csum_seed,
            lpf_ino,
            0,
            &first[..block_size - dirent::TAIL_LEN],
        );
        dirent::set_block_csum(&mut first, c);
    }
    dev.write_at(plan.lost_found_start * g.block_size as u64, &first)
        .await?;

    // Every further block of lost+found is one empty entry spanning the block.
    for i in 1..plan.lost_found_blocks as u64 {
        let mut empty = vec![0u8; block_size];
        let limit = block_size - if with_tail { dirent::TAIL_LEN } else { 0 };
        put_u32(&mut empty, 0, 0);
        empty[4..6].copy_from_slice(&(limit as u16).to_le_bytes());
        if with_tail {
            dirent::write_tail_header(&mut empty[limit..limit + dirent::TAIL_LEN]);
            let c = csum::dirent_csum(plan.csum_seed, lpf_ino, 0, &empty[..limit]);
            dirent::set_block_csum(&mut empty, c);
        }
        dev.write_at((plan.lost_found_start + i) * g.block_size as u64, &empty)
            .await?;
    }

    Ok(())
}

/// Write the journal's superblock and zero the rest of it.
async fn write_journal<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan, start: u64) -> Result<()> {
    let g = &plan.geom;
    let block_size = g.block_size as u64;
    let jsb = JournalSuperblock::new(g.block_size, plan.journal_blocks, plan.uuid);
    let buf = jsb.encode(g.block_size as usize);
    dev.write_at(start * block_size, &buf).await?;

    // The rest of the journal must be zero: a stale block with a plausible
    // magic would be replayed as a transaction. The journal need not be one
    // run, so every run is cleared — unless the medium is already zero, in
    // which case there is no stale block to find.
    for (i, run) in plan.journal_runs.iter().enumerate().filter(|_| !plan.zeroed_medium) {
        let (from, len) = if i == 0 {
            (run.start + 1, run.len - 1)
        } else {
            (run.start, run.len)
        };
        if len > 0 {
            dev.write_zeroes(from * block_size, len * block_size).await?;
        }
    }

    // The extent leaf, when the journal needed more extents than fit inline.
    //
    // This block is an extent tree node like any other, so with metadata_csum
    // it carries a tail holding its checksum — and the tail costs an entry's
    // worth of room, so `max` has to know about it too. Getting either wrong
    // leaves a journal that `e2fsck` cannot read at all.
    if let Some(leaf) = plan.journal_extent_leaf {
        let with_tail = plan.csum_scheme != GroupDescCsum::None;
        let mut buf = journal_extent_leaf_block(&plan.journal_extents, g.block_size, with_tail);
        if with_tail {
            let at = buf.len() - extent::TAIL_LEN;
            let crc = csum::extent_block_csum(plan.csum_seed, ino::JOURNAL, 0, &buf[..at]);
            put_u32(&mut buf, at, crc);
        }
        dev.write_at(leaf * block_size, &buf).await?;
    }

    // Indirect blocks mapping the journal, on a filesystem without extents.
    for (block, entries) in &plan.journal_indirect.blocks {
        let mut buf = vec![0u8; g.block_size as usize];
        for (i, entry) in entries.iter().enumerate() {
            put_u32(&mut buf, i * 4, *entry);
        }
        dev.write_at(block * block_size, &buf).await?;
    }

    Ok(())
}

/// Groups carrying a superblock backup, excluding group 0.
///
/// `ext2fs_list_backups()` — group 1, then every power of 3, 5 and 7.
fn backup_groups(geom: &Geometry) -> impl Iterator<Item = u32> + '_ {
    (1..geom.group_count).filter(move |&g| geom.has_super(g))
}

/// Blocks the resize inode owns: its double indirect block, the primary
/// reserved GDT blocks it points at, and every backup copy those list.
fn resize_inode_blocks(geom: &Geometry) -> u64 {
    let backups = backup_groups(geom).count() as u64;
    1 + geom.reserved_gdt_blocks as u64 * (1 + backups)
}

/// Write the resize inode's double indirect block and the reserved group
/// descriptor blocks it indexes.
///
/// `ext2fs_create_resize_inode()` in `lib/ext2fs/res_gdt.c`. The reserved GDT
/// blocks are not padding: each one doubles as an indirect block listing where
/// its own backup copies live, which is how a later resize finds room to grow
/// the descriptor table. Zeroing them — the obvious reading — leaves e2fsck
/// with an unreachable resize inode and "Resize inode not valid".
async fn write_resize_inode_blocks<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan) -> Result<()> {
    let g = &plan.geom;
    let Some(dind) = plan.resize_dind else {
        return Ok(());
    };
    let block_size = g.block_size as u64;
    let addr_per_block = (g.block_size / 4) as usize;

    // The superblock sits in block 1 on a 1 KiB filesystem, block 0 otherwise.
    let sb_blk = if g.block_size == 1024 {
        1
    } else {
        g.first_data_block as u64
    };

    let mut dindir = vec![0u8; g.block_size as usize];
    let backups: Vec<u32> = backup_groups(g).collect();

    for rsv_off in 0..g.reserved_gdt_blocks as u64 {
        let gdt_off = ((g.desc_blocks as u64 + rsv_off) % addr_per_block as u64) as usize;
        let gdt_blk = sb_blk + 1 + g.desc_blocks as u64 + rsv_off;
        put_u32(&mut dindir, gdt_off * 4, gdt_blk as u32);

        // Each reserved GDT block lists the same block in every group that
        // carries a superblock backup.
        let mut gdt_buf = vec![0u8; g.block_size as usize];
        for (i, &grp) in backups.iter().enumerate() {
            if i >= addr_per_block {
                break;
            }
            let backup = gdt_blk + grp as u64 * g.blocks_per_group as u64;
            put_u32(&mut gdt_buf, i * 4, backup as u32);
        }
        dev.write_at(gdt_blk * block_size, &gdt_buf).await?;
    }

    dev.write_at(dind * block_size, &dindir).await?;
    Ok(())
}

/// Write the orphan file's blocks, and its extent leaf or indirect blocks.
///
/// Every block is empty slots plus an `ext4_orphan_block_tail` carrying the
/// magic and, on a checksummed filesystem, a checksum bound to the inode, its
/// generation and the block's own number.
async fn write_orphan_file<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan) -> Result<()> {
    let g = &plan.geom;
    let Some(inum) = plan.orphan_inode else {
        return Ok(());
    };
    let block_size = g.block_size as u64;
    let metadata_csum = plan.csum_scheme == GroupDescCsum::Crc32c;

    for run in &plan.orphan_runs {
        for block in run.start..run.start + run.len {
            let buf = orphan_block(
                g.block_size,
                plan.csum_seed,
                inum,
                0,
                block,
                metadata_csum,
            );
            dev.write_at(block * block_size, &buf).await?;
        }
    }

    if let Some(leaf) = plan.orphan_extent_leaf {
        let extents: Vec<Extent> = plan
            .orphan_runs
            .iter()
            .scan(0u32, |logical, run| {
                let e = Extent {
                    block: *logical,
                    len: run.len as u16,
                    start: run.start,
                };
                *logical += run.len as u32;
                Some(e)
            })
            .collect();
        let buf = journal_extent_leaf_block(&extents, g.block_size, false);
        dev.write_at(leaf * block_size, &buf).await?;
    }

    for (block, entries) in &plan.orphan_indirect.blocks {
        let mut buf = vec![0u8; g.block_size as usize];
        for (i, entry) in entries.iter().enumerate() {
            put_u32(&mut buf, i * 4, *entry);
        }
        dev.write_at(block * block_size, &buf).await?;
    }

    Ok(())
}

/// Write inodes 1 through 11 into group 0's inode table.
async fn write_reserved_inodes<D: BlockDevice + ?Sized>(dev: &D, plan: &Plan) -> Result<()> {
    let g = &plan.geom;
    let block_size = g.block_size as u64;
    let inode_size = g.inode_size as usize;
    let layout = g.group(0)?;
    let metadata_csum = plan.csum_scheme == GroupDescCsum::Crc32c;
    let extents = g.features.incompat.contains(IncompatFeatures::EXTENTS);
    let extra_isize = if g.inode_size > superblock::GOOD_OLD_INODE_SIZE {
        32
    } else {
        0
    };

    let sectors_per_block = block_size / 512;
    let write = |inum: u32, inode: &Inode, buf: &mut Vec<(u64, Vec<u8>)>| {
        let encoded = inode.encode_with_csum(inode_size, metadata_csum, plan.csum_seed, inum);
        let index = (inum - 1) as u64;
        let at = layout.inode_table * block_size + index * inode_size as u64;
        buf.push((at, encoded));
    };

    let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();

    // Inode 1, bad blocks: present and in use, but empty.
    let bad = Inode::new(inode_size, extra_isize);
    write(ino::BAD, &bad, &mut writes);

    // Inode 2, the root directory.
    let mut root = Inode::new(inode_size, extra_isize);
    root.mode = mode::IFDIR | 0o755;
    root.links_count = 3; // ".", "..", and lost+found's ".."
    root.size = block_size;
    root.blocks = sectors_per_block;
    root.atime = plan.mkfs_time as u32;
    root.ctime = plan.mkfs_time as u32;
    root.mtime = plan.mkfs_time as u32;
    root.crtime = plan.mkfs_time as u32;
    set_single_block(&mut root, plan.root_block, extents);
    write(ino::ROOT, &root, &mut writes);

    // Inodes 3 to 6 and 9, 10: reserved and empty.
    for inum in [
        ino::USR_QUOTA,
        ino::GRP_QUOTA,
        ino::BOOT_LOADER,
        ino::UNDEL_DIR,
        ino::EXCLUDE,
        ino::REPLICA,
    ] {
        let empty = Inode::new(inode_size, extra_isize);
        write(inum, &empty, &mut writes);
    }

    // Inode 7, the resize inode: its double-indirect block reaches the
    // reserved group descriptor blocks so the filesystem can grow later.
    let mut resize = Inode::new(inode_size, extra_isize);
    resize.mode = mode::IFREG | 0o600;
    resize.links_count = 1;
    if let Some(dind) = plan.resize_dind {
        resize.blocks = resize_inode_blocks(g) * sectors_per_block;
        // The size is what a file reaching through one double indirect block
        // could address, not what it actually uses.
        let apb = (g.block_size / 4) as u64;
        resize.size =
            (apb * apb + apb + crate::structs::inode::NDIR_BLOCKS as u64) * block_size;
        resize.ctime = plan.mkfs_time as u32;
        let mut pointers = [0u32; crate::structs::inode::N_BLOCKS];
        pointers[crate::structs::inode::NDIR_BLOCKS + 1] = dind as u32;
        resize.set_block_pointers(&pointers);
    }
    write(ino::RESIZE, &resize, &mut writes);

    // Inode 8, the journal.
    let mut journal_inode = Inode::new(inode_size, extra_isize);
    if let Some(start) = plan.journal_start {
        journal_inode.mode = mode::IFREG | 0o600;
        journal_inode.links_count = 1;
        let _ = start;
        journal_inode.size = plan.journal_blocks as u64 * block_size;
        // Extent leaves and indirect blocks are the inode's own blocks too, so
        // they count against i_blocks alongside the journal's data blocks.
        let map_overhead = if extents {
            plan.journal_extent_leaf.is_some() as u64
        } else {
            plan.journal_indirect.overhead()
        };
        journal_inode.blocks = (plan.journal_blocks as u64 + map_overhead) * sectors_per_block;
        journal_inode.atime = plan.mkfs_time as u32;
        journal_inode.ctime = plan.mkfs_time as u32;
        journal_inode.mtime = plan.mkfs_time as u32;
        if extents {
            journal_inode.flags |= iflags::EXTENTS;
        }
        journal_inode.block = plan.journal_i_block;
    }
    write(ino::JOURNAL, &journal_inode, &mut writes);

    // Inode 11, lost+found.
    let mut lpf = Inode::new(inode_size, extra_isize);
    lpf.mode = mode::IFDIR | 0o700;
    lpf.links_count = 2; // "." and root's entry
    lpf.size = plan.lost_found_blocks as u64 * block_size;
    lpf.blocks = plan.lost_found_blocks as u64 * sectors_per_block;
    lpf.atime = plan.mkfs_time as u32;
    lpf.ctime = plan.mkfs_time as u32;
    lpf.mtime = plan.mkfs_time as u32;
    lpf.crtime = plan.mkfs_time as u32;
    set_run(
        &mut lpf,
        plan.lost_found_start,
        plan.lost_found_blocks,
        extents,
    )?;
    write(superblock::GOOD_OLD_FIRST_INO, &lpf, &mut writes);

    // Inode 12, the orphan file: a plain regular file, unreferenced by any
    // directory, found through s_orphan_file_inum alone.
    if let Some(orphan_ino) = plan.orphan_inode {
        let blocks: u64 = plan.orphan_runs.iter().map(|r| r.len).sum();
        let map_overhead = if extents {
            plan.orphan_extent_leaf.is_some() as u64
        } else {
            plan.orphan_indirect.overhead()
        };
        let mut orphan = Inode::new(inode_size, extra_isize);
        orphan.mode = mode::IFREG | 0o600;
        orphan.links_count = 1;
        orphan.size = blocks * block_size;
        orphan.blocks = (blocks + map_overhead) * sectors_per_block;
        orphan.atime = plan.mkfs_time as u32;
        orphan.ctime = plan.mkfs_time as u32;
        orphan.mtime = plan.mkfs_time as u32;
        orphan.crtime = plan.mkfs_time as u32;
        if extents {
            orphan.flags |= iflags::EXTENTS;
        }
        orphan.block = plan.orphan_i_block;
        write(orphan_ino, &orphan, &mut writes);
    }

    for (at, buf) in writes {
        dev.write_at(at, &buf).await?;
    }
    Ok(())
}

/// Point an inode at a single block, through an extent tree or a direct
/// pointer depending on the filesystem's features.
fn set_single_block(inode: &mut Inode, block: u64, extents: bool) {
    if extents {
        inode.flags |= iflags::EXTENTS;
        let tree = extent::build_inline(&[Extent {
            block: 0,
            len: 1,
            start: block,
        }])
        .expect("one extent always fits inline");
        inode.block = tree;
    } else {
        let mut pointers = [0u32; crate::structs::inode::N_BLOCKS];
        pointers[0] = block as u32;
        inode.set_block_pointers(&pointers);
    }
}

/// Point an inode at a contiguous run of blocks.
fn set_run(inode: &mut Inode, start: u64, len: u32, extents: bool) -> Result<()> {
    if extents {
        inode.flags |= iflags::EXTENTS;
        // A run longer than one extent's maximum is split; at the sizes a
        // format produces, four extents inline are always enough.
        let mut list = Vec::new();
        let mut done = 0u32;
        while done < len {
            let chunk = (len - done).min(extent::INIT_MAX_LEN - 1);
            list.push(Extent {
                block: done,
                len: chunk as u16,
                start: start + done as u64,
            });
            done += chunk;
        }
        inode.block = extent::build_inline(&list)?;
        Ok(())
    } else {
        let mut pointers = [0u32; crate::structs::inode::N_BLOCKS];
        if len as usize > crate::structs::inode::NDIR_BLOCKS {
            return Err(Error::invalid(format!(
                "a {len}-block run needs indirect blocks, which this path does not build"
            )));
        }
        for i in 0..len as usize {
            pointers[i] = (start + i as u64) as u32;
        }
        inode.set_block_pointers(&pointers);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structs::superblock::SUPERBLOCK_LEN;
    use crate::device::MemDevice;
    use crate::params::{Params, Profile};

    const MIB: u64 = 1024 * 1024;

    fn fixed_params(profile: Profile) -> Params {
        Params::new(profile)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000)
    }

    #[tokio::test]
    async fn formats_a_readable_filesystem() {
        let dev = MemDevice::new(64 * MIB);
        let report = format(&dev, &fixed_params(Profile::Ext4).no_journal())
            .await
            .unwrap();

        assert_eq!(report.blocks_count, 65536);
        assert_eq!(report.inodes_count, 16384);
        assert_eq!(report.free_inodes_count, 16384 - 11);

        let mut buf = [0u8; SUPERBLOCK_LEN];
        dev.read_at(SUPERBLOCK_OFFSET, &mut buf).await.unwrap();
        let sb = Superblock::decode(&buf).unwrap();

        assert_eq!(sb.blocks_count, 65536);
        assert_eq!(sb.inodes_count, 16384);
        assert_eq!(sb.block_size(), 1024);
        assert_eq!(sb.first_data_block, 1);
        assert!(sb.verify_checksum(&buf));
        assert_eq!(sb.uuid_string(), "30313233-3435-3637-3839-616263646566");
    }

    #[tokio::test]
    async fn group_zero_accounting_matches_the_golden_reference() {
        // tests/golden/ext4-64m-nojournal.dump, Group 0:
        //   "3808 free blocks, 2037 free inodes, 2 directories, 2037 unused"
        let params = fixed_params(Profile::Ext4).no_journal();
        let plan = plan(64 * MIB, &params).unwrap();
        let state = build_group(&plan, 0).unwrap();

        assert_eq!(state.desc.free_blocks_count, 3808);
        assert_eq!(state.desc.free_inodes_count, 2037);
        assert_eq!(state.desc.used_dirs_count, 2);
        assert_eq!(state.desc.itable_unused, 2037);
    }

    #[tokio::test]
    async fn a_label_and_uuid_survive_the_round_trip() {
        let dev = MemDevice::new(16 * MIB);
        let params = fixed_params(Profile::Ext4).no_journal().label("scratch");
        format(&dev, &params).await.unwrap();

        let mut buf = [0u8; SUPERBLOCK_LEN];
        dev.read_at(SUPERBLOCK_OFFSET, &mut buf).await.unwrap();
        let sb = Superblock::decode(&buf).unwrap();
        assert_eq!(sb.label(), "scratch");
        assert_eq!(sb.uuid, *b"0123456789abcdef");
    }

    #[tokio::test]
    async fn backup_superblocks_land_in_the_sparse_super_groups() {
        let dev = MemDevice::new(64 * MIB);
        let params = fixed_params(Profile::Ext4).no_journal();
        let report = format(&dev, &params).await.unwrap();
        let geom = Geometry::compute(64 * MIB, &params).unwrap();

        for group in [1u32, 3, 5, 7] {
            assert!(group < report.group_count);
            let at = geom.group_first_block(group) * geom.block_size as u64;
            let mut buf = [0u8; SUPERBLOCK_LEN];
            dev.read_at(at, &mut buf).await.unwrap();
            let sb = Superblock::decode(&buf)
                .unwrap_or_else(|e| panic!("group {group} backup: {e}"));
            assert_eq!(sb.block_group_nr, group as u16);
            assert_eq!(sb.blocks_count, 65536);
        }

        // Group 2 has no superblock, so no magic should be there.
        let at = geom.group_first_block(2) * geom.block_size as u64;
        let mut buf = [0u8; SUPERBLOCK_LEN];
        dev.read_at(at, &mut buf).await.unwrap();
        assert!(Superblock::decode(&buf).is_err());
    }

    #[tokio::test]
    async fn ext2_ext3_and_ext4_all_format() {
        for profile in [Profile::Ext2, Profile::Ext3, Profile::Ext4] {
            let dev = MemDevice::new(64 * MIB);
            let report = format(&dev, &fixed_params(profile))
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", profile.name()));
            assert_eq!(report.blocks_count, 65536, "{}", profile.name());

            let mut buf = [0u8; SUPERBLOCK_LEN];
            dev.read_at(SUPERBLOCK_OFFSET, &mut buf).await.unwrap();
            let sb = Superblock::decode(&buf).unwrap();
            assert!(sb.verify_checksum(&buf), "{}", profile.name());

            let journalled = profile != Profile::Ext2;
            assert_eq!(report.journal_blocks > 0, journalled, "{}", profile.name());
            assert_eq!(sb.journal_inum != 0, journalled, "{}", profile.name());
        }
    }

    /// Many filesystems at once is the case the async surface exists for.
    #[tokio::test]
    async fn many_devices_format_concurrently() {
        let devices: Vec<Arc<MemDevice>> =
            (0..8).map(|_| Arc::new(MemDevice::new(16 * MIB))).collect();

        let mut tasks = Vec::new();
        for (i, dev) in devices.iter().enumerate() {
            let dev = Arc::clone(dev);
            tasks.push(tokio::spawn(async move {
                let params = fixed_params(Profile::Ext4)
                    .no_journal()
                    .label(format!("vol{i}"));
                format(&*dev, &params).await
            }));
        }

        for (i, task) in tasks.into_iter().enumerate() {
            let report = task.await.unwrap().unwrap();
            assert_eq!(report.label, format!("vol{i}"));
        }

        for dev in &devices {
            let mut buf = [0u8; SUPERBLOCK_LEN];
            dev.read_at(SUPERBLOCK_OFFSET, &mut buf).await.unwrap();
            assert!(Superblock::decode(&buf).is_ok());
        }
    }

    /// A device that reports 4 KiB sectors gets 4 KiB blocks without anyone
    /// asking — and a caller can still override it either way. This is the
    /// path a storage engine takes: its volumes are neither files nor block
    /// devices, so it states the sector size itself.
    #[tokio::test]
    async fn the_devices_sector_size_reaches_the_geometry() {
        // A device that says 4096 gets 4096-byte blocks, with no parameters.
        let dev = MemDevice::with_sector_size(64 * MIB, 4096);
        let report = format(&dev, &fixed_params(Profile::Ext4).no_journal())
            .await
            .unwrap();
        assert_eq!(report.block_size, 4096);

        // The same size on a 512-byte-sector device is a 1 KiB filesystem.
        let dev = MemDevice::with_sector_size(64 * MIB, 512);
        let report = format(&dev, &fixed_params(Profile::Ext4).no_journal())
            .await
            .unwrap();
        assert_eq!(report.block_size, 1024);

        // And Params overrides whatever the device claimed.
        let dev = MemDevice::with_sector_size(64 * MIB, 512);
        let params = fixed_params(Profile::Ext4).no_journal().sector_size(4096);
        let report = format(&dev, &params).await.unwrap();
        assert_eq!(report.block_size, 4096);
    }

    #[tokio::test]
    async fn refuses_a_device_too_small_to_hold_a_filesystem() {
        let dev = MemDevice::new(16 * 1024);
        assert!(format(&dev, &fixed_params(Profile::Ext4)).await.is_err());
    }
}
