//! Releasing orphan inodes.
//!
//! An orphan is an inode the kernel was part way through freeing or
//! truncating when it stopped: unlinked while still open, or cut short by a
//! `truncate` that had not finished. The kernel tracks them so that the next
//! mount can finish the job — in the `s_last_orphan` chain, threaded through
//! each inode's `i_dtime`, and with `orphan_file` in the slots of the orphan
//! file's blocks. `e2fsck` finishes the job itself before checking
//! (`release_orphan_inodes` in `e2fsck/super.c`), and so does this, in its
//! order: the chain, then the orphan file.
//!
//! For each orphan:
//!
//! - **No links left**: every block it owns is freed, its extended-attribute
//!   block loses a reference (and is freed with the last), the inode is freed
//!   and given a deletion time. Its block map is left as it was, as `e2fsck`
//!   leaves it: a freed inode's map is not read again.
//! - **Still linked**: it is truncated to `i_size`. Blocks past the end are
//!   freed, extent nodes and indirect blocks emptied by that go too, and
//!   `i_blocks` comes down to match. Its `i_dtime`, which held the next link
//!   of the chain, goes back to zero.
//!
//! Bitmaps, group descriptors and the superblock's free counts are updated as
//! `ext2fs_block_alloc_stats2` and `ext2fs_inode_alloc_stats2` update them, so
//! the check that follows finds them already right. Only the primary
//! descriptor table is written, as `e2fsck` (`EXT2_FLAG_MASTER_SB_ONLY`)
//! writes it.
//!
//! When the superblock records errors the chain is not trusted, as in
//! `e2fsck`: it is dropped unwalked, and the full check that the errors force
//! deals with the inodes. Quota usage is not adjusted (this crate does not
//! maintain quota files), and neither is a bigalloc filesystem's, whose
//! bitmaps count clusters (#12): there the orphans are left for the kernel.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use core::future::Future;
use core::pin::Pin;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use crate::bytes::{get_u32, put_u32};
use crate::csum;
use crate::device::BlockDevice;
use crate::error::Result;
use crate::features::{CompatFeatures, RoCompatFeatures};
use crate::fs::Filesystem;
use crate::structs::extent::{self, Extent, ExtentHeader, ExtentIdx, INIT_MAX_LEN};
use crate::structs::inode::{iflags, Inode, NDIR_BLOCKS};
use crate::structs::superblock::state;
use crate::structs::xattr;

pub use crate::format::{ORPHAN_BLOCK_MAGIC, ORPHAN_TAIL_LEN};

/// One orphan released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    /// The inode.
    pub inum: u32,
    /// Truncated to its size (it still has links) rather than cleared.
    pub truncated: bool,
    /// Owner, group, mode and size, as `e2fsck` prints them.
    pub uid: u32,
    /// `i_gid`
    pub gid: u32,
    /// `i_mode`
    pub mode: u16,
    /// `i_size`
    pub size: u64,
    /// Blocks freed.
    pub blocks_freed: u64,
}

impl Released {
    /// `e2fsck`'s `PR_0_ORPHAN_CLEAR_INODE` line.
    pub fn message(&self) -> String {
        format!(
            "{} orphaned inode {} (uid={}, gid={}, mode=0{:o}, size={})",
            if self.truncated { "Truncating" } else { "Clearing" },
            self.inum,
            self.uid,
            self.gid,
            self.mode,
            self.size
        )
    }
}

/// What orphan release did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanRelease {
    /// Every orphan released, in the order released.
    pub released: Vec<Released>,
    /// What stopped the release, if anything: an orphan number outside the
    /// inode table, a chain that loops, an orphan file block that fails its
    /// checksum. `e2fsck` then marks the filesystem not clean, to force a
    /// full check; [`crate::fsck`] does the same.
    pub errors: Vec<String>,
    /// The chain was dropped unwalked because the filesystem records errors.
    pub skipped_for_errors: bool,
}

/// Whether the superblock says there are orphans to release.
pub fn has_orphans(sb: &crate::structs::Superblock) -> bool {
    sb.last_orphan != 0 || sb.feature_ro_compat.contains(RoCompatFeatures::ORPHAN_PRESENT)
}

/// Release every orphan the superblock records, and write the result.
///
/// `now` is the deletion time given to cleared inodes. The superblock,
/// descriptors and bitmaps are written before this returns.
pub async fn release_orphans<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    now: u32,
) -> Result<OrphanRelease> {
    let mut out = OrphanRelease::default();
    let sb = fs.superblock().clone();
    if !has_orphans(&sb) {
        return Ok(out);
    }

    let head = sb.last_orphan;
    let orphan_file = sb.feature_compat.contains(CompatFeatures::ORPHAN_FILE)
        && sb.feature_ro_compat.contains(RoCompatFeatures::ORPHAN_PRESENT);

    // Win or lose, the head of the chain is not used again.
    fs.superblock_mut().last_orphan = 0;

    if sb.state & state::ERROR_FS != 0 {
        out.skipped_for_errors = true;
        if orphan_file {
            clear_orphan_file(fs, &mut out).await?;
        }
        fs.superblock_mut().feature_ro_compat.remove(RoCompatFeatures::ORPHAN_PRESENT);
        fs.flush_superblock().await?;
        return Ok(out);
    }
    if sb.feature_ro_compat.contains(RoCompatFeatures::BIGALLOC) {
        out.errors.push("orphans on a bigalloc filesystem are left for the kernel".into());
        fs.flush_superblock().await?;
        return Ok(out);
    }

    let mut stats = AllocStats::default();
    let first_ino = sb.first_ino;
    let legal = |inum: u32| inum >= first_ino && inum <= sb.inodes_count;

    if head != 0 && !legal(head) {
        out.errors.push(format!("illegal inode {head} in the superblock's orphan list"));
    } else {
        let mut inum = head;
        let mut seen = BTreeSet::new();
        while inum != 0 {
            if !seen.insert(inum) {
                out.errors.push(format!("orphan list loops back to inode {inum}"));
                break;
            }
            let next = match release_one(fs, &mut stats, inum, now, &mut out).await? {
                Some(next) => next,
                None => break,
            };
            if next != 0 && !legal(next) {
                out.errors.push(format!(
                    "illegal inode {next} in the orphan list, after inode {inum}"
                ));
                break;
            }
            inum = next;
        }
    }

    if out.errors.is_empty() && orphan_file {
        process_orphan_file(fs, &mut stats, now, &mut out).await?;
    }
    if out.errors.is_empty() {
        fs.superblock_mut().feature_ro_compat.remove(RoCompatFeatures::ORPHAN_PRESENT);
    }

    stats.flush(fs).await?;
    fs.flush_superblock().await?;
    fs.device().flush().await?;
    Ok(out)
}

/// Release one orphan, returning the next in the chain (its `i_dtime`), or
/// `None` when it could not be released.
async fn release_one<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    stats: &mut AllocStats,
    inum: u32,
    now: u32,
    out: &mut OrphanRelease,
) -> Result<Option<u32>> {
    let mut inode = fs.read_inode(inum).await?;
    let next = inode.dtime;
    let truncated = inode.links_count > 0;

    let mut freed = Vec::new();
    if has_valid_blocks(&inode) {
        if truncated {
            let cut = inode.size.div_ceil(fs.block_size() as u64);
            if let Err(e) = truncate(fs, &mut inode, inum, cut, &mut freed).await {
                out.errors.push(format!("truncating orphaned inode {inum}: {e}"));
                return Ok(None);
            }
            let unit = if inode.flags & iflags::HUGE_FILE != 0
                && fs.superblock().feature_ro_compat.contains(RoCompatFeatures::HUGE_FILE)
            {
                1
            } else {
                fs.block_size() as u64 / 512
            };
            inode.blocks = inode.blocks.saturating_sub(freed.len() as u64 * unit);
        } else {
            let walked = fs.walk_blocks(&inode, |b| freed.push(b.physical)).await;
            if let Err(e) = walked {
                out.errors.push(format!("clearing orphaned inode {inum}: {e}"));
                return Ok(None);
            }
            if inode.file_acl != 0 {
                release_xattr_block(fs, stats, inode.file_acl).await?;
                inode.file_acl = 0;
            }
        }
    }
    for &block in &freed {
        stats.free_block(fs, block).await?;
    }

    if truncated {
        inode.dtime = 0;
    } else {
        stats.free_inode(fs, inum, inode.is_dir()).await?;
        inode.dtime = now;
    }
    fs.write_inode(inum, &inode).await?;

    out.released.push(Released {
        inum,
        truncated,
        uid: inode.uid,
        gid: inode.gid,
        mode: inode.mode,
        size: inode.size,
        blocks_freed: freed.len() as u64,
    });
    Ok(Some(next))
}

/// `ext2fs_inode_has_valid_blocks2`: whether `i_block` is a block map.
fn has_valid_blocks(inode: &Inode) -> bool {
    inode.flags & iflags::INLINE_DATA == 0 && inode.has_block_map()
}

/// Drop one reference to an extended-attribute block, freeing it with the
/// last: `ext2fs_adjust_ea_refcount3(.., -1, ..)`.
async fn release_xattr_block<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    stats: &mut AllocStats,
    block: u64,
) -> Result<()> {
    if block >= fs.superblock().blocks_count {
        return Ok(());
    }
    let mut buf = fs.read_block(block).await?;
    let refs = get_u32(&buf, xattr::block_off::H_REFCOUNT).saturating_sub(1);
    put_u32(&mut buf, xattr::block_off::H_REFCOUNT, refs);
    if fs.has_metadata_csum() {
        xattr::stamp_block_csum(&mut buf, fs.csum_seed(), block);
    }
    fs.write_block(block, &buf).await?;
    if refs == 0 {
        stats.free_block(fs, block).await?;
    }
    Ok(())
}

/// The orphan file's blocks, in logical order.
async fn orphan_file_blocks<D: BlockDevice>(
    fs: &Filesystem<D>,
    inode: &Inode,
) -> Result<Vec<u64>> {
    let mut blocks = BTreeMap::new();
    fs.walk_blocks(inode, |b| {
        if let Some(logical) = b.logical {
            blocks.insert(logical, b.physical);
        }
    })
    .await?;
    Ok(blocks.into_values().collect())
}

/// `ext4_orphan_file_block_csum`: seeded with the orphan inode and its
/// generation, then the block's own number, then the slots.
fn orphan_block_csum(seed: u32, inum: u32, generation: u32, physical: u64, slots: &[u8]) -> u32 {
    let mut crc = csum::crc32c(seed, &inum.to_le_bytes());
    crc = csum::crc32c(crc, &generation.to_le_bytes());
    crc = csum::crc32c(crc, &physical.to_le_bytes());
    csum::crc32c(crc, slots)
}

/// Release the inodes in the orphan file's slots, and empty them:
/// `process_orphan_file`.
async fn process_orphan_file<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    stats: &mut AllocStats,
    now: u32,
    out: &mut OrphanRelease,
) -> Result<()> {
    let sb = fs.superblock().clone();
    let file_inum = sb.orphan_file_inum;
    if file_inum == 0 || file_inum > sb.inodes_count {
        out.errors.push(format!("orphan file inode {file_inum} is not an inode"));
        return Ok(());
    }
    let file = fs.read_inode(file_inum).await?;
    let block_size = fs.block_size() as usize;
    let tail = block_size - ORPHAN_TAIL_LEN;
    let slots = tail / 4;
    let blocks = match orphan_file_blocks(fs, &file).await {
        Ok(b) => b,
        Err(e) => {
            out.errors.push(format!("orphan file: {e}"));
            return Ok(());
        }
    };
    for physical in blocks {
        let mut buf = fs.read_block(physical).await?;
        if get_u32(&buf, tail) != ORPHAN_BLOCK_MAGIC {
            out.errors.push(format!("orphan file block {physical} has no orphan block magic"));
            return Ok(());
        }
        let seed = fs.csum_seed();
        if fs.has_metadata_csum()
            && get_u32(&buf, tail + 4)
                != orphan_block_csum(seed, file_inum, file.generation, physical, &buf[..slots * 4])
        {
            out.errors.push(format!("orphan file block {physical} checksum does not match"));
            return Ok(());
        }
        let mut emptied = false;
        for slot in 0..slots {
            let inum = get_u32(&buf, slot * 4);
            if inum == 0 {
                continue;
            }
            if inum < sb.first_ino || inum > sb.inodes_count {
                out.errors.push(format!("illegal inode {inum} in orphan file block {physical}"));
                return Ok(());
            }
            if release_one(fs, stats, inum, now, out).await?.is_none() {
                return Ok(());
            }
            put_u32(&mut buf, slot * 4, 0);
            emptied = true;
        }
        if emptied {
            write_orphan_block(fs, file_inum, file.generation, physical, &mut buf).await?;
        }
    }
    Ok(())
}

/// Empty every slot of the orphan file without releasing anything.
async fn clear_orphan_file<D: BlockDevice>(
    fs: &mut Filesystem<D>,
    out: &mut OrphanRelease,
) -> Result<()> {
    let sb = fs.superblock().clone();
    let file_inum = sb.orphan_file_inum;
    if file_inum == 0 || file_inum > sb.inodes_count {
        return Ok(());
    }
    let file = fs.read_inode(file_inum).await?;
    let tail = fs.block_size() as usize - ORPHAN_TAIL_LEN;
    let blocks = match orphan_file_blocks(fs, &file).await {
        Ok(b) => b,
        Err(e) => {
            out.errors.push(format!("orphan file: {e}"));
            return Ok(());
        }
    };
    for physical in blocks {
        let mut buf = fs.read_block(physical).await?;
        if buf[..tail].iter().any(|&b| b != 0) {
            buf[..tail].fill(0);
            write_orphan_block(fs, file_inum, file.generation, physical, &mut buf).await?;
        }
    }
    Ok(())
}

async fn write_orphan_block<D: BlockDevice>(
    fs: &Filesystem<D>,
    file_inum: u32,
    generation: u32,
    physical: u64,
    buf: &mut [u8],
) -> Result<()> {
    let tail = buf.len() - ORPHAN_TAIL_LEN;
    put_u32(buf, tail, ORPHAN_BLOCK_MAGIC);
    if fs.has_metadata_csum() {
        let slots = tail / 4;
        let crc = orphan_block_csum(fs.csum_seed(), file_inum, generation, physical, &buf[..slots * 4]);
        put_u32(buf, tail + 4, crc);
    }
    fs.write_block(physical, buf).await
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Free every block past logical block `cut`, through an extent tree or an
/// indirect map, collecting the blocks freed (map structure included).
async fn truncate<D: BlockDevice>(
    fs: &Filesystem<D>,
    inode: &mut Inode,
    inum: u32,
    cut: u64,
    freed: &mut Vec<u64>,
) -> Result<()> {
    if inode.uses_extents() {
        let mut root = inode.block;
        let entries = truncate_node(fs, &mut root, true, inum, inode.generation, cut, freed).await?;
        let mut header = ExtentHeader::decode(&root)?;
        if entries == 0 && header.depth > 0 {
            // ext4_ext_remove_space: a tree truncated to nothing is an empty
            // leaf again.
            header.depth = 0;
            header.max = ExtentHeader::max_entries(extent::INLINE_LEN, false);
            header.encode_into(&mut root);
        }
        inode.block = root;
        Ok(())
    } else {
        truncate_indirect(fs, inode, cut, freed).await
    }
}

/// Truncate one extent node in place, returning the entries it keeps. A node
/// in a block of its own is written back by the caller.
fn truncate_node<'a, D: BlockDevice>(
    fs: &'a Filesystem<D>,
    node: &'a mut [u8],
    is_root: bool,
    inum: u32,
    generation: u32,
    cut: u64,
    freed: &'a mut Vec<u64>,
) -> Boxed<'a, u16> {
    Box::pin(async move {
        let mut header = ExtentHeader::decode(node)?;
        let space = if is_root { extent::INLINE_LEN } else { node.len() };
        if header.entries > ExtentHeader::max_entries(space, false) {
            return Err(crate::error::Error::corrupt(
                "extent header",
                format!("{} entries claimed in a node of {space} bytes", header.entries),
            ));
        }
        let entry = |i: usize| extent::HEADER_LEN + i * extent::ENTRY_LEN;
        let count = header.entries as usize;
        let mut kept: Vec<[u8; extent::ENTRY_LEN]> = Vec::new();

        if header.depth == 0 {
            for i in 0..count {
                let mut ext = Extent::decode(&node[entry(i)..]);
                let start = ext.block as u64;
                let len = ext.effective_len() as u64;
                if start >= cut {
                    freed.extend(ext.start..ext.start + len);
                    continue;
                }
                if start + len > cut {
                    let keep = cut - start;
                    freed.extend(ext.start + keep..ext.start + len);
                    let uninit = if ext.is_uninit() { INIT_MAX_LEN } else { 0 };
                    ext.len = (keep as u32 + uninit) as u16;
                }
                let mut raw = [0u8; extent::ENTRY_LEN];
                ext.encode_into(&mut raw);
                kept.push(raw);
            }
        } else {
            for i in 0..count {
                let idx = ExtentIdx::decode(&node[entry(i)..]);
                let next = if i + 1 < count {
                    ExtentIdx::decode(&node[entry(i + 1)..]).block as u64
                } else {
                    u64::MAX
                };
                let mut raw = [0u8; extent::ENTRY_LEN];
                raw.copy_from_slice(&node[entry(i)..entry(i) + extent::ENTRY_LEN]);
                if idx.block as u64 >= cut {
                    collect_extent_subtree(fs, idx.leaf, freed).await?;
                    continue;
                }
                if next <= cut {
                    kept.push(raw);
                    continue;
                }
                let mut child = fs.read_block(idx.leaf).await?;
                let left = truncate_node(fs, &mut child, false, inum, generation, cut, freed).await?;
                if left == 0 {
                    freed.push(idx.leaf);
                    continue;
                }
                if fs.has_metadata_csum() {
                    let at = extent::tail_offset(ExtentHeader::decode(&child)?.max);
                    if at + extent::TAIL_LEN <= child.len() {
                        let c = csum::extent_block_csum(fs.csum_seed(), inum, generation, &child[..at]);
                        put_u32(&mut child, at, c);
                    }
                }
                fs.write_block(idx.leaf, &child).await?;
                kept.push(raw);
            }
        }

        for i in 0..count {
            node[entry(i)..entry(i) + extent::ENTRY_LEN].fill(0);
        }
        for (i, raw) in kept.iter().enumerate() {
            node[entry(i)..entry(i) + extent::ENTRY_LEN].copy_from_slice(raw);
        }
        header.entries = kept.len() as u16;
        header.encode_into(node);
        Ok(header.entries)
    })
}

/// Every block under an extent index — nodes and data — and the node itself.
async fn collect_extent_subtree<D: BlockDevice>(
    fs: &Filesystem<D>,
    block: u64,
    freed: &mut Vec<u64>,
) -> Result<()> {
    let mut stack = vec![block];
    let mut guard = 0u32;
    while let Some(b) = stack.pop() {
        guard += 1;
        if guard > 1_000_000 {
            return Err(crate::error::Error::corrupt("extent tree", "the tree is probably cyclic"));
        }
        freed.push(b);
        let node = fs.read_block(b).await?;
        let header = ExtentHeader::decode(&node)?;
        let count = (header.entries).min(ExtentHeader::max_entries(node.len(), false)) as usize;
        for i in 0..count {
            let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
            if header.depth == 0 {
                let ext = Extent::decode(&node[at..]);
                freed.extend(ext.start..ext.start + ext.effective_len() as u64);
            } else {
                stack.push(ExtentIdx::decode(&node[at..]).leaf);
            }
        }
    }
    Ok(())
}

/// Truncate an indirect block map.
async fn truncate_indirect<D: BlockDevice>(
    fs: &Filesystem<D>,
    inode: &mut Inode,
    cut: u64,
    freed: &mut Vec<u64>,
) -> Result<()> {
    let per_block = fs.block_size() as u64 / 4;
    let mut pointers = inode.block_pointers();
    for (i, p) in pointers.iter_mut().enumerate().take(NDIR_BLOCKS) {
        if i as u64 >= cut && *p != 0 {
            freed.push(*p as u64);
            *p = 0;
        }
    }
    let mut base = NDIR_BLOCKS as u64;
    for (slot, depth) in [(NDIR_BLOCKS, 1u32), (NDIR_BLOCKS + 1, 2), (NDIR_BLOCKS + 2, 3)] {
        let span = per_block.pow(depth);
        let root = pointers[slot] as u64;
        if root != 0 && base + span > cut {
            let empty = truncate_indirect_block(fs, root, depth, base, cut, per_block, freed).await?;
            if empty {
                freed.push(root);
                pointers[slot] = 0;
            }
        }
        base += span;
    }
    inode.set_block_pointers(&pointers);
    Ok(())
}

/// Truncate one indirect block of the given depth covering logical blocks
/// from `base`. Returns whether it is now empty (the caller frees it);
/// otherwise it has been written back.
fn truncate_indirect_block<'a, D: BlockDevice>(
    fs: &'a Filesystem<D>,
    block: u64,
    depth: u32,
    base: u64,
    cut: u64,
    per_block: u64,
    freed: &'a mut Vec<u64>,
) -> Boxed<'a, bool> {
    Box::pin(async move {
        let mut buf = fs.read_block(block).await?;
        let span = per_block.pow(depth - 1);
        let mut changed = false;
        for i in 0..per_block {
            let entry = get_u32(&buf, (i * 4) as usize) as u64;
            if entry == 0 {
                continue;
            }
            let child_base = base + i * span;
            if child_base + span <= cut {
                continue;
            }
            let drop = if depth == 1 {
                freed.push(entry);
                true
            } else if child_base >= cut {
                collect_indirect_subtree(fs, entry, depth - 1, freed).await?;
                true
            } else {
                let empty =
                    truncate_indirect_block(fs, entry, depth - 1, child_base, cut, per_block, freed)
                        .await?;
                if empty {
                    freed.push(entry);
                }
                empty
            };
            if drop {
                put_u32(&mut buf, (i * 4) as usize, 0);
                changed = true;
            }
        }
        if buf.iter().all(|&b| b == 0) {
            return Ok(true);
        }
        if changed {
            fs.write_block(block, &buf).await?;
        }
        Ok(false)
    })
}

/// An indirect block and everything below it.
async fn collect_indirect_subtree<D: BlockDevice>(
    fs: &Filesystem<D>,
    block: u64,
    depth: u32,
    freed: &mut Vec<u64>,
) -> Result<()> {
    let mut stack = vec![(block, depth)];
    while let Some((b, d)) = stack.pop() {
        freed.push(b);
        let buf = fs.read_block(b).await?;
        for i in 0..buf.len() / 4 {
            let entry = get_u32(&buf, i * 4) as u64;
            if entry == 0 {
                continue;
            }
            if d == 1 {
                freed.push(entry);
            } else {
                stack.push((entry, d - 1));
            }
        }
    }
    Ok(())
}

/// Bitmap and counter updates, gathered per group and written once.
#[derive(Default)]
struct AllocStats {
    block_bitmaps: BTreeMap<u32, Vec<u8>>,
    inode_bitmaps: BTreeMap<u32, Vec<u8>>,
}

impl AllocStats {
    /// `ext2fs_block_alloc_stats2(fs, block, -1)`. A block already free is
    /// left alone rather than counted free twice.
    async fn free_block<D: BlockDevice>(&mut self, fs: &mut Filesystem<D>, block: u64) -> Result<()> {
        let sb = fs.superblock();
        if block < sb.first_data_block as u64 || block >= sb.blocks_count {
            return Ok(());
        }
        let group = fs.group_of_block(block);
        let bit = block - fs.group_first_block(group);
        let bitmap = match self.block_bitmaps.entry(group) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(fs.read_block_bitmap(group).await?),
        };
        if !Filesystem::<D>::test_bit(bitmap, bit) {
            return Ok(());
        }
        Filesystem::<D>::set_bit(bitmap, bit, false);
        fs.group_descs_mut()[group as usize].free_blocks_count += 1;
        fs.superblock_mut().free_blocks_count += 1;
        Ok(())
    }

    /// `ext2fs_inode_alloc_stats2(fs, inum, -1, is_dir)`.
    async fn free_inode<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem<D>,
        inum: u32,
        is_dir: bool,
    ) -> Result<()> {
        let ipg = fs.superblock().inodes_per_group;
        let group = (inum - 1) / ipg;
        let bit = ((inum - 1) % ipg) as u64;
        let bitmap = match self.inode_bitmaps.entry(group) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(fs.read_inode_bitmap(group).await?),
        };
        if !Filesystem::<D>::test_bit(bitmap, bit) {
            return Ok(());
        }
        Filesystem::<D>::set_bit(bitmap, bit, false);
        let desc = &mut fs.group_descs_mut()[group as usize];
        desc.free_inodes_count += 1;
        if is_dir {
            desc.used_dirs_count = desc.used_dirs_count.saturating_sub(1);
        }
        fs.superblock_mut().free_inodes_count += 1;
        Ok(())
    }

    /// Write the bitmaps changed, with their checksums, and the descriptors.
    async fn flush<D: BlockDevice>(self, fs: &mut Filesystem<D>) -> Result<()> {
        if self.block_bitmaps.is_empty() && self.inode_bitmaps.is_empty() {
            return Ok(());
        }
        let sb = fs.superblock().clone();
        let seed = fs.csum_seed();
        let has_csum = fs.has_metadata_csum();
        let bb_len = (sb.blocks_per_group as usize).div_ceil(8);
        let ib_len = (sb.inodes_per_group as usize).div_ceil(8);
        for (group, bitmap) in &self.block_bitmaps {
            let desc = &mut fs.group_descs_mut()[*group as usize];
            desc.flags &= !crate::structs::group_desc::bg_flags::BLOCK_UNINIT;
            if has_csum {
                desc.block_bitmap_csum = csum::bitmap_csum(seed, &bitmap[..bb_len]);
            }
            let at = desc.block_bitmap;
            fs.write_block(at, bitmap).await?;
        }
        for (group, bitmap) in &self.inode_bitmaps {
            let desc = &mut fs.group_descs_mut()[*group as usize];
            desc.flags &= !crate::structs::group_desc::bg_flags::INODE_UNINIT;
            if has_csum {
                desc.inode_bitmap_csum = csum::bitmap_csum(seed, &bitmap[..ib_len]);
            }
            let at = desc.inode_bitmap;
            fs.write_block(at, bitmap).await?;
        }
        // Primary only, as e2fsck writes it here.
        fs.flush_primary_group_descs().await
    }
}

#[cfg(test)]
mod tests;
