//! Geometry.
//!
//! Where the filesystem's metadata goes, and how much of it there is. Mirrors
//! `ext2fs_initialize()` in `lib/ext2fs/initialize.c` and
//! `ext2fs_allocate_group_table()` in `lib/ext2fs/alloc_tables.c`.
//!
//! This module computes and never writes. [`Geometry::compute`] takes a device
//! size and [`Params`] and produces every number the formatter needs, so the
//! geometry can be tested — and diffed against a real `mke2fs` filesystem —
//! without touching a device.

#[cfg(not(feature = "std"))]
use alloc::{string::ToString, vec::Vec};

use crate::error::{Error, Result};
use crate::features::{CompatFeatures, FeatureMasks, IncompatFeatures, RoCompatFeatures};
use crate::params::{Params, SizeType};
use crate::structs::superblock::{MIN_DESC_SIZE, MIN_DESC_SIZE_64BIT};

/// Metadata blocks a single block group holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupLayout {
    /// Group number.
    pub group: u32,
    /// First block of the group.
    pub first_block: u64,
    /// Last block of the group, inclusive.
    pub last_block: u64,
    /// Whether a superblock copy and group descriptor table live here.
    pub has_super: bool,
    /// Block holding the block bitmap.
    pub block_bitmap: u64,
    /// Block holding the inode bitmap.
    pub inode_bitmap: u64,
    /// First block of the inode table.
    pub inode_table: u64,
}

impl GroupLayout {
    /// Blocks in this group — the last one may be short.
    pub fn block_count(&self) -> u32 {
        (self.last_block - self.first_block + 1) as u32
    }
}

/// The complete geometry of a filesystem.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    /// Block size in bytes.
    pub block_size: u32,
    /// `s_log_block_size`.
    pub log_block_size: u32,
    /// Total blocks.
    pub blocks_count: u64,
    /// `s_first_data_block` — 1 when blocks are 1 KiB, else 0.
    pub first_data_block: u32,
    /// Blocks per group.
    pub blocks_per_group: u32,
    /// Blocks reserved for root.
    pub r_blocks_count: u64,
    /// Total inodes.
    pub inodes_count: u32,
    /// Inodes per group.
    pub inodes_per_group: u32,
    /// Inode size in bytes.
    pub inode_size: u16,
    /// Blocks one group's inode table occupies.
    pub itable_blocks_per_group: u32,
    /// Number of block groups.
    pub group_count: u32,
    /// Group descriptor size, 32 or 64.
    pub desc_size: u16,
    /// Blocks the group descriptor table occupies.
    pub desc_blocks: u32,
    /// Reserved group descriptor blocks, for growing the filesystem later.
    pub reserved_gdt_blocks: u16,
    /// `s_log_groups_per_flex`, zero when `flex_bg` is off.
    pub log_groups_per_flex: u8,
    /// Whether descriptors are distributed per meta block group.
    pub meta_bg: bool,
    /// `s_first_meta_bg` — the first meta block group using the new layout.
    pub first_meta_bg: u32,
    /// The resolved feature masks.
    pub features: FeatureMasks,
}

impl Geometry {
    /// Compute the geometry for a device of `size_bytes`.
    pub fn compute(size_bytes: u64, params: &Params) -> Result<Self> {
        let features = params
            .resolve_features()
            .map_err(Error::IncompatibleFeatures)?;

        let class = SizeType::of(size_bytes);

        // The device's sector is the floor. `mke2fs` raises its default block
        // size to the logical sector size, which is why the same 256 MiB image
        // gets 1 KiB blocks on a 512-byte-sector device and 4 KiB blocks on a
        // 4 KiB-sector one. Getting this wrong produces a filesystem that is
        // valid on paper and unwritable on the device it was made for.
        let sector_size = params.sector_size.unwrap_or(512).max(1);
        let block_size = match params.block_size {
            Some(explicit) => {
                if explicit < sector_size {
                    return Err(Error::invalid(format!(
                        "a block size of {explicit} is smaller than the device's \
                         {sector_size}-byte sector; the filesystem could not be written \
                         a block at a time"
                    )));
                }
                explicit
            }
            None => class.block_size().max(sector_size),
        };
        validate_block_size(block_size)?;
        let log_block_size = block_size.trailing_zeros() - 10;

        let inode_size = params.inode_size.unwrap_or(256);
        validate_inode_size(inode_size, block_size)?;

        // A 1 KiB filesystem cannot put block 0 to use: the superblock lives at
        // byte 1024, which *is* block 1, so data starts there.
        let first_data_block = if block_size == 1024 { 1 } else { 0 };

        let mut blocks_count = size_bytes / block_size as u64;
        if blocks_count <= first_data_block as u64 {
            return Err(Error::DeviceTooSmall {
                available: blocks_count,
                needed: first_data_block as u64 + 1,
                block_size,
            });
        }

        // Requested inode count, before it is rounded to fill inode tables.
        let requested_inodes = params.inodes_count.unwrap_or_else(|| {
            let ratio = params
                .inode_ratio
                .unwrap_or_else(|| class.inode_ratio()) as u64;
            ((blocks_count * block_size as u64) / ratio).max(16) as u32
        });

        let mut blocks_per_group = params
            .blocks_per_group
            .unwrap_or(block_size * 8)
            .min(max_blocks_per_group());

        let desc_size = if features.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT) {
            MIN_DESC_SIZE_64BIT
        } else {
            MIN_DESC_SIZE
        };
        let desc_per_block = block_size / desc_size as u32;

        // The loop mirrors initialize.c's `retry:` label. Two things can send
        // it round again: too many inodes to fit one group's bitmap, and a
        // final group too short to hold its own metadata.
        let (group_count, desc_blocks, inodes_per_group, itable_blocks_per_group, reserved_gdt) = loop {
            let group_count =
                (blocks_count - first_data_block as u64).div_ceil(blocks_per_group as u64) as u32;
            if group_count == 0 {
                return Err(Error::DeviceTooSmall {
                    available: blocks_count,
                    needed: 1,
                    block_size,
                });
            }
            let desc_blocks = group_count.div_ceil(desc_per_block);

            let mut ipg = requested_inodes.div_ceil(group_count);
            if ipg > block_size * 8 {
                // One group's inode bitmap is a single block; more inodes than
                // it has bits cannot be addressed. mke2fs shrinks the group
                // and tries again rather than failing.
                if blocks_per_group >= 256 {
                    blocks_per_group -= 8;
                    continue;
                }
                return Err(Error::invalid(
                    "too many inodes requested for this block size",
                ));
            }
            ipg = ipg.min(max_inodes_per_group(block_size, inode_size));

            // Round the inode count out to fill whole inode-table blocks, then
            // down to a multiple of 8 so the bitmap splices on byte boundaries.
            let inodes_per_block = block_size / inode_size as u32;
            let mut itable_blocks = ipg.div_ceil(inodes_per_block);
            ipg = itable_blocks * inodes_per_block;
            ipg = ipg.max(8) & !7;
            itable_blocks = (ipg * inode_size as u32).div_ceil(block_size);

            let reserved_gdt = if features.compat.contains(CompatFeatures::RESIZE_INODE) {
                calc_reserved_gdt_blocks(
                    blocks_count,
                    first_data_block,
                    blocks_per_group,
                    desc_per_block,
                    desc_blocks,
                    block_size,
                )
            } else {
                0
            };

            // Does the final, possibly short, group have room for its own
            // metadata? If not, drop it and recompute.
            let overhead_last = 2
                + itable_blocks
                + if has_super_for(group_count - 1, &features) {
                    1 + desc_blocks + reserved_gdt as u32
                } else {
                    0
                };
            let rem =
                ((blocks_count - first_data_block as u64) % blocks_per_group as u64) as u32;
            if group_count == 1 && rem != 0 && rem < overhead_last {
                return Err(Error::DeviceTooSmall {
                    available: blocks_count,
                    needed: overhead_last as u64,
                    block_size,
                });
            }
            if rem != 0 && rem < overhead_last + 50 {
                blocks_count -= rem as u64;
                continue;
            }

            break (
                group_count,
                desc_blocks,
                ipg,
                itable_blocks,
                reserved_gdt,
            );
        };

        let inodes_count = inodes_per_group
            .checked_mul(group_count)
            .ok_or_else(|| Error::invalid("inode count overflows 32 bits"))?;

        // A descriptor table that would swallow three quarters of a block group
        // is the point at which a contiguous table stops being workable, and
        // `mke2fs` switches to meta_bg: one descriptor block per meta block
        // group, kept in that meta group rather than copied whole into every
        // superblock backup. The resize inode goes with it, since there is no
        // longer a contiguous table for reserved blocks to extend.
        let mut features = features;
        let mut reserved_gdt = reserved_gdt;
        let mut meta_bg = features.incompat.contains(IncompatFeatures::META_BG);
        if reserved_gdt as u32 + desc_blocks > blocks_per_group * 3 / 4 {
            meta_bg = true;
        }
        if meta_bg {
            features.incompat |= IncompatFeatures::META_BG;
            features.compat.remove(CompatFeatures::RESIZE_INODE);
            reserved_gdt = 0;
        }
        let first_meta_bg = if meta_bg {
            params.first_meta_bg.unwrap_or(0)
        } else {
            0
        };

        // Even distributed, a single descriptor block plus a superblock copy
        // has to fit in a group.
        if meta_bg && 2 + itable_blocks_per_group + 2 > blocks_per_group {
            return Err(Error::invalid(
                "block groups are too small to hold their own metadata",
            ));
        }

        let log_groups_per_flex = if features.incompat.contains(IncompatFeatures::FLEX_BG) {
            let size = params.flex_bg_size.unwrap_or(16);
            if !size.is_power_of_two() {
                return Err(Error::invalid(format!(
                    "flex_bg size {size} is not a power of two"
                )));
            }
            size.trailing_zeros() as u8
        } else {
            0
        };

        let r_blocks_count =
            ((blocks_count as f64) * params.reserved_percent / 100.0).floor() as u64;
        if r_blocks_count >= blocks_count {
            return Err(Error::invalid(
                "reserved blocks would be the whole filesystem",
            ));
        }

        // Block numbers past 2^32 need 64-bit descriptors to address them at
        // all. 16 TiB of 4 KiB blocks is exactly the boundary.
        if blocks_count > u32::MAX as u64
            && !features.incompat.contains(IncompatFeatures::SIXTY_FOUR_BIT)
        {
            return Err(Error::IncompatibleFeatures(format!(
                "{blocks_count} blocks needs the 64bit feature; without it a filesystem \
                 tops out at {} blocks ({} TiB at {block_size}-byte blocks)",
                u32::MAX,
                (u32::MAX as u64 * block_size as u64) / (1024 * 1024 * 1024 * 1024),
            )));
        }

        let geom = Self {
            block_size,
            log_block_size,
            blocks_count,
            first_data_block,
            blocks_per_group,
            r_blocks_count,
            inodes_count,
            inodes_per_group,
            inode_size,
            itable_blocks_per_group,
            group_count,
            desc_size,
            desc_blocks,
            reserved_gdt_blocks: reserved_gdt,
            log_groups_per_flex,
            meta_bg,
            first_meta_bg,
            features,
        };

        // Validate placement at the boundaries rather than materialising every
        // group: a 64 TiB filesystem has over half a million of them, and the
        // only one that can run past the end of the device is the last.
        geom.group(geom.group_count - 1)?;
        Ok(geom)
    }

    /// Whether group `g` carries a superblock backup and descriptor table.
    pub fn has_super(&self, group: u32) -> bool {
        has_super_for(group, &self.features)
    }

    /// First block of a group.
    pub fn group_first_block(&self, group: u32) -> u64 {
        self.first_data_block as u64 + group as u64 * self.blocks_per_group as u64
    }

    /// Last block of a group, inclusive, clamped to the end of the filesystem.
    pub fn group_last_block(&self, group: u32) -> u64 {
        (self.group_first_block(group) + self.blocks_per_group as u64 - 1)
            .min(self.blocks_count - 1)
    }

    /// Group descriptors that fit in one block.
    pub fn desc_per_block(&self) -> u32 {
        self.block_size / self.desc_size as u32
    }

    /// Groups covered by one descriptor block — a meta block group.
    pub fn meta_bg_size(&self) -> u32 {
        self.desc_per_block()
    }

    /// Which meta block group a group belongs to.
    pub fn meta_bg_of(&self, group: u32) -> u32 {
        group / self.meta_bg_size()
    }

    /// Whether `group` keeps a copy of a descriptor block.
    ///
    /// Under meta_bg, a meta block group's single descriptor block is kept in
    /// its first, second and last group — three copies near the descriptors
    /// they describe, rather than the whole table repeated in every superblock
    /// backup. `ext2fs_super_and_bgd_loc2()`.
    pub fn group_has_desc(&self, group: u32) -> bool {
        if !self.meta_bg || self.meta_bg_of(group) < self.first_meta_bg {
            return self.has_super(group);
        }
        let size = self.meta_bg_size();
        let within = group % size;
        within == 0 || within == 1 || within == size - 1
    }

    /// Descriptor blocks group `group` stores: the whole table under the
    /// classic layout, a single block under meta_bg.
    pub fn desc_blocks_in_group(&self, group: u32) -> u32 {
        if !self.group_has_desc(group) {
            return 0;
        }
        if !self.meta_bg || self.meta_bg_of(group) < self.first_meta_bg {
            self.desc_blocks + self.reserved_gdt_blocks as u32
        } else {
            1
        }
    }

    /// Where group `group`'s descriptor block copy starts, if it has one.
    pub fn desc_block_location(&self, group: u32) -> Option<u64> {
        if !self.group_has_desc(group) {
            return None;
        }
        Some(self.group_first_block(group) + self.has_super(group) as u64)
    }

    /// Blocks group `group` spends on a superblock copy and descriptors.
    ///
    /// Always a contiguous prefix of the group, under either layout.
    pub fn super_overhead(&self, group: u32) -> u32 {
        self.has_super(group) as u32 + self.desc_blocks_in_group(group)
    }

    /// Groups per flex group.
    pub fn flex_bg_size(&self) -> u32 {
        1 << self.log_groups_per_flex
    }

    /// Total blocks spent on metadata across the filesystem.
    pub fn overhead_blocks(&self) -> u64 {
        (0..self.group_count)
            .map(|g| {
                self.super_overhead(g) as u64 + 2 + self.itable_blocks_per_group as u64
            })
            .sum()
    }

    /// Whether a block belongs to some group's superblock copy, group
    /// descriptor table or reserved descriptor blocks.
    pub fn in_super_region(&self, block: u64) -> bool {
        if block < self.first_data_block as u64 || block >= self.blocks_count {
            return true;
        }
        let group =
            ((block - self.first_data_block as u64) / self.blocks_per_group as u64) as u32;
        if group >= self.group_count {
            return true;
        }
        // Not guarded on `has_super`: under meta_bg a group can hold a
        // descriptor block copy and no superblock backup at all, and the
        // descriptor still occupies the front of the group. `super_overhead`
        // is already zero when the group holds neither.
        block < self.group_first_block(group) + self.super_overhead(group) as u64
    }

    /// The first group of the flex group `group` belongs to.
    pub fn flex_first_group(&self, group: u32) -> u32 {
        if self.log_groups_per_flex == 0 {
            return group;
        }
        let flex = self.flex_bg_size();
        (group / flex) * flex
    }

    /// How many groups that flex group actually has — the last may be short.
    pub fn flex_members(&self, flex_first: u32) -> u32 {
        if self.log_groups_per_flex == 0 {
            return 1;
        }
        self.flex_bg_size().min(self.group_count - flex_first)
    }

    /// Place the bitmaps and inode tables for one flex group.
    ///
    /// Not arithmetic: metadata is *allocated*, taking the next free blocks and
    /// stepping over any superblock copy, descriptor table or reserved
    /// descriptor blocks in the way. A flex group's inode tables can run past
    /// the end of its leading group, and when they meet the next group's
    /// superblock backup they resume after it.
    ///
    /// The golden 256 MiB reference shows exactly this: fifteen inode tables
    /// run contiguously from block 292, and the sixteenth — which would have
    /// landed on group 1's backup superblock at 8193 — starts at 8452 instead.
    /// Treating the region as one contiguous run instead produces group
    /// descriptors e2fsck rejects with "bad block for inode table".
    fn place_flex(&self, flex_first: u32) -> Result<Vec<(u64, u64, u64)>> {
        let members = self.flex_members(flex_first) as usize;
        let itable_len = self.itable_blocks_per_group as u64;
        let mut cursor = self.group_first_block(flex_first);

        // The next single free block.
        let take_one = |cursor: &mut u64| -> Result<u64> {
            while *cursor < self.blocks_count && self.in_super_region(*cursor) {
                *cursor += 1;
            }
            if *cursor >= self.blocks_count {
                return Err(Error::DeviceTooSmall {
                    available: self.blocks_count,
                    needed: *cursor + 1,
                    block_size: self.block_size,
                });
            }
            let block = *cursor;
            *cursor += 1;
            Ok(block)
        };

        let mut block_bitmaps = Vec::with_capacity(members);
        for _ in 0..members {
            block_bitmaps.push(take_one(&mut cursor)?);
        }
        let mut inode_bitmaps = Vec::with_capacity(members);
        for _ in 0..members {
            inode_bitmaps.push(take_one(&mut cursor)?);
        }

        // An inode table is one contiguous run, so a partial gap is no use.
        let mut inode_tables = Vec::with_capacity(members);
        for _ in 0..members {
            let start = loop {
                while cursor < self.blocks_count && self.in_super_region(cursor) {
                    cursor += 1;
                }
                if cursor + itable_len > self.blocks_count {
                    return Err(Error::DeviceTooSmall {
                        available: self.blocks_count,
                        needed: cursor + itable_len,
                        block_size: self.block_size,
                    });
                }
                // Does the whole run clear the obstructions?
                match (cursor..cursor + itable_len).find(|&b| self.in_super_region(b)) {
                    None => break cursor,
                    Some(blocked) => cursor = blocked + 1,
                }
            };
            inode_tables.push(start);
            cursor = start + itable_len;
        }

        Ok((0..members)
            .map(|i| (block_bitmaps[i], inode_bitmaps[i], inode_tables[i]))
            .collect())
    }

    /// Where group `g` keeps its bitmaps and inode table.
    ///
    /// Without `flex_bg` a group's metadata sits at the front of that group.
    /// With it, the metadata for every group in a flex group is packed together
    /// at the front of the flex group: all the block bitmaps, then all the
    /// inode bitmaps, then all the inode tables.
    ///
    /// Computed rather than stored — a 64 TiB filesystem has over half a
    /// million groups. The cost is bounded by the flex group size, not by the
    /// filesystem, so any single group is cheap to place.
    pub fn group(&self, g: u32) -> Result<GroupLayout> {
        if g >= self.group_count {
            return Err(Error::invalid(format!(
                "group {g} is past the last group ({})",
                self.group_count - 1
            )));
        }

        let flex_first = self.flex_first_group(g);
        let placement = self.place_flex(flex_first)?;
        let (block_bitmap, inode_bitmap, inode_table) = placement[(g - flex_first) as usize];

        Ok(GroupLayout {
            group: g,
            first_block: self.group_first_block(g),
            last_block: self.group_last_block(g),
            has_super: self.has_super(g),
            block_bitmap,
            inode_bitmap,
            inode_table,
        })
    }

    /// Every metadata block belonging to the flex group containing `group`.
    ///
    /// Returned as `(start, len)` runs, which is what a bitmap builder wants:
    /// marking ranges rather than testing every block keeps a large filesystem
    /// from costing one predicate call per block.
    pub fn flex_metadata_runs(&self, group: u32) -> Result<Vec<(u64, u64)>> {
        let flex_first = self.flex_first_group(group);
        let placement = self.place_flex(flex_first)?;
        let itable_len = self.itable_blocks_per_group as u64;

        let mut runs = Vec::with_capacity(placement.len() * 3);
        for (bb, ib, it) in placement {
            runs.push((bb, 1));
            runs.push((ib, 1));
            runs.push((it, itable_len));
        }
        Ok(runs)
    }

    /// Every group's layout, computed as the iterator advances.
    pub fn groups(&self) -> impl Iterator<Item = Result<GroupLayout>> + '_ {
        (0..self.group_count).map(move |g| self.group(g))
    }
}

/// Whether a group carries a superblock backup.
///
/// With `sparse_super` that is groups 0 and 1 and every power of 3, 5 and 7.
/// Without it, every group does. `ext2fs_bg_has_super()`.
pub fn has_super_for(group: u32, features: &FeatureMasks) -> bool {
    if !features
        .ro_compat
        .contains(RoCompatFeatures::SPARSE_SUPER)
    {
        return true;
    }
    if group <= 1 {
        return true;
    }
    if group % 2 == 0 {
        return false;
    }
    [3u32, 5, 7].iter().any(|&base| is_power_of(group, base))
}

fn is_power_of(mut n: u32, base: u32) -> bool {
    if n == 0 {
        return false;
    }
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

/// `calc_reserved_gdt_blocks()` from `initialize.c`.
///
/// Enough reserved descriptor blocks to grow the filesystem 1024-fold, capped
/// at what one indirect block can address — the resize inode reaches them
/// through a single indirect block, so that is a hard ceiling.
fn calc_reserved_gdt_blocks(
    blocks_count: u64,
    first_data_block: u32,
    blocks_per_group: u32,
    desc_per_block: u32,
    desc_blocks: u32,
    block_size: u32,
) -> u16 {
    let max_blocks = if blocks_count < u32::MAX as u64 / 1024 {
        blocks_count * 1024
    } else {
        u32::MAX as u64
    };
    let rsv_groups = (max_blocks - first_data_block as u64).div_ceil(blocks_per_group as u64);
    let rsv_gdb = (rsv_groups.div_ceil(desc_per_block as u64) as u32).saturating_sub(desc_blocks);
    let addr_per_block = block_size / 4;
    rsv_gdb.min(addr_per_block) as u16
}

/// `EXT2_MAX_BLOCKS_PER_GROUP` — the 16-bit free-block counter sets the ceiling.
fn max_blocks_per_group() -> u32 {
    (1 << 16) - 8
}

/// `EXT2_MAX_INODES_PER_GROUP`
fn max_inodes_per_group(block_size: u32, inode_size: u16) -> u32 {
    (1 << 16) - (block_size / inode_size as u32)
}

fn validate_block_size(block_size: u32) -> Result<()> {
    if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(Error::invalid(format!(
            "block size {block_size} must be a power of two between 1024 and 65536"
        )));
    }
    Ok(())
}

fn validate_inode_size(inode_size: u16, block_size: u32) -> Result<()> {
    if inode_size < 128 || !inode_size.is_power_of_two() {
        return Err(Error::invalid(format!(
            "inode size {inode_size} must be a power of two of at least 128"
        )));
    }
    if inode_size as u32 > block_size {
        return Err(Error::invalid(format!(
            "inode size {inode_size} exceeds the block size {block_size}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Profile;

    const MIB: u64 = 1024 * 1024;


    /// A device's sector size is the floor for the block size, and it changes
    /// the answer. Measured against real `mke2fs` on a 4 KiB-sector loop
    /// device: the same 256 MiB image gets 1 KiB blocks at 512-byte sectors
    /// and 4 KiB blocks at 4 KiB sectors.
    ///
    /// This is not academic. A storage engine exporting 4 KiB sectors — which
    /// is the common case for network-backed volumes — would otherwise be
    /// handed a filesystem it cannot write a block at a time.
    #[test]
    fn the_sector_size_raises_the_block_size() {
        let base = Params::new(Profile::Ext4);

        // 512-byte sectors: the size class decides, and 256 MiB is "small".
        let g = Geometry::compute(256 * MIB, &base.clone().sector_size(512)).unwrap();
        assert_eq!(g.block_size, 1024);
        assert_eq!(g.blocks_count, 262_144);

        // 4 KiB sectors: the floor wins.
        let g = Geometry::compute(256 * MIB, &base.clone().sector_size(4096)).unwrap();
        assert_eq!(g.block_size, 4096);
        assert_eq!(g.blocks_count, 65_536);

        // And a sector size larger than the class default for a big filesystem
        // changes nothing, because the default was already larger.
        let g = Geometry::compute(2048 * MIB, &base.clone().sector_size(4096)).unwrap();
        assert_eq!(g.block_size, 4096);
    }


    /// A consumer whose storage is neither a file nor a block device — a
    /// network-backed volume, say — cannot be probed, so it must be able to
    /// state its sector size. Both routes are checked here: the device
    /// reporting it, and `Params` overriding whatever the device said.
    #[test]
    fn the_sector_size_can_always_be_stated_explicitly() {
        // Params wins over anything the device reports.
        let g = Geometry::compute(
            256 * MIB,
            &Params::new(Profile::Ext4).sector_size(4096),
        )
        .unwrap();
        assert_eq!(g.block_size, 4096);

        // And downwards too, for a device that really is 512.
        let g = Geometry::compute(
            256 * MIB,
            &Params::new(Profile::Ext4).sector_size(512),
        )
        .unwrap();
        assert_eq!(g.block_size, 1024);
    }

    #[test]
    fn a_block_smaller_than_the_sector_is_refused() {
        let params = Params::new(Profile::Ext4).sector_size(4096).block_size(1024);
        let err = Geometry::compute(256 * MIB, &params).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("1024"), "{text}");
        assert!(text.contains("4096"), "{text}");
    }

    #[test]
    fn an_explicit_block_size_at_or_above_the_sector_is_allowed() {
        let params = Params::new(Profile::Ext4).sector_size(4096).block_size(4096);
        assert_eq!(
            Geometry::compute(256 * MIB, &params).unwrap().block_size,
            4096
        );
    }

    #[test]
    fn sparse_super_picks_groups_0_1_and_powers_of_3_5_7() {
        let f = Profile::Ext4.features();
        let supers: Vec<u32> = (0..100).filter(|&g| has_super_for(g, &f)).collect();
        assert_eq!(
            supers,
            vec![0, 1, 3, 5, 7, 9, 25, 27, 49, 81]
        );
    }

    #[test]
    fn without_sparse_super_every_group_has_one() {
        let mut f = Profile::Ext4.features();
        f.ro_compat.remove(RoCompatFeatures::SPARSE_SUPER);
        assert!((0..20).all(|g| has_super_for(g, &f)));
    }

    /// The numbers here are read straight off the golden 64 MiB ext4 reference
    /// produced by mke2fs 1.47.3 (`tests/golden/ext4-64m-nojournal.dump`).
    #[test]
    fn matches_the_golden_64mib_ext4_geometry() {
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(64 * MIB, &params).unwrap();

        assert_eq!(g.block_size, 1024);
        assert_eq!(g.blocks_count, 65536);
        assert_eq!(g.first_data_block, 1);
        assert_eq!(g.blocks_per_group, 8192);
        assert_eq!(g.inodes_count, 16384);
        assert_eq!(g.inodes_per_group, 2048);
        assert_eq!(g.inode_size, 256);
        assert_eq!(g.itable_blocks_per_group, 512);
        assert_eq!(g.group_count, 8);
        assert_eq!(g.desc_size, 64);
        assert_eq!(g.desc_blocks, 1);
        assert_eq!(g.reserved_gdt_blocks, 256);
        assert_eq!(g.r_blocks_count, 3276);
        assert_eq!(g.flex_bg_size(), 16);
    }

    /// Group 0 of the same reference: block bitmap 259, inode bitmap 267,
    /// inode table 275-786. With eight groups in one flex group, the eight
    /// block bitmaps occupy 259-266 and the eight inode bitmaps 267-274.
    #[test]
    fn matches_the_golden_flex_bg_table_placement() {
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(64 * MIB, &params).unwrap();

        assert_eq!(g.group(0).unwrap().block_bitmap, 259);
        assert_eq!(g.group(0).unwrap().inode_bitmap, 267);
        assert_eq!(g.group(0).unwrap().inode_table, 275);

        assert_eq!(g.group(1).unwrap().block_bitmap, 260);
        assert_eq!(g.group(1).unwrap().inode_bitmap, 268);
        assert_eq!(g.group(1).unwrap().inode_table, 275 + 512);

        assert_eq!(g.group(7).unwrap().block_bitmap, 266);
        assert_eq!(g.group(7).unwrap().inode_bitmap, 274);
        assert_eq!(g.group(7).unwrap().inode_table, 275 + 7 * 512);
    }

    #[test]
    fn group_boundaries_cover_the_filesystem_exactly() {
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(64 * MIB, &params).unwrap();
        let groups: Vec<_> = g.groups().map(|r| r.unwrap()).collect();

        assert_eq!(groups[0].first_block, 1);
        assert_eq!(groups[0].last_block, 8192);
        assert_eq!(groups[1].first_block, 8193);
        assert_eq!(groups.last().unwrap().last_block, g.blocks_count - 1);

        for pair in groups.windows(2) {
            assert_eq!(pair[0].last_block + 1, pair[1].first_block);
        }
    }

    #[test]
    fn without_flex_bg_metadata_sits_in_its_own_group() {
        let params = Params::new(Profile::Ext2);
        let g = Geometry::compute(64 * MIB, &params).unwrap();
        assert_eq!(g.log_groups_per_flex, 0);

        for grp in g.groups().map(|r| r.unwrap()) {
            assert!(grp.block_bitmap >= grp.first_block);
            assert_eq!(grp.inode_bitmap, grp.block_bitmap + 1);
            assert_eq!(grp.inode_table, grp.block_bitmap + 2);
        }
    }

    #[test]
    fn geometry_is_computable_at_every_size_class() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let cases: &[(u64, SizeType)] = &[
            (2 * MIB, SizeType::Floppy),
            (64 * MIB, SizeType::Small),
            (2 * 1024 * MIB, SizeType::Default),
            (2 * TIB, SizeType::Default),
            (8 * TIB, SizeType::Big),
            (12 * TIB, SizeType::Big),
            (32 * TIB, SizeType::Huge),
            (128 * TIB, SizeType::Huge),
        ];

        for &(size, expect_class) in cases {
            assert_eq!(SizeType::of(size), expect_class, "class for {size}");
            let params = Params::new(Profile::Ext4).no_journal();
            let g = Geometry::compute(size, &params)
                .unwrap_or_else(|e| panic!("geometry for {size} bytes: {e}"));

            assert_eq!(g.blocks_count, size / g.block_size as u64);
            assert!(g.group_count >= 1);
            assert!(g.inodes_per_group > 0);
            assert!(g.overhead_blocks() < g.blocks_count, "overhead at {size}");

            // The last group must fit, which is what compute() validates.
            let last = g.group(g.group_count - 1).unwrap();
            assert!(
                last.inode_table + g.itable_blocks_per_group as u64 <= g.blocks_count,
                "last inode table runs off the end at {size}"
            );
        }
    }

    /// 16 TiB of 4 KiB blocks is exactly 2^32 blocks — the point past which a
    /// block number no longer fits in the 32-bit fields, so `64bit` stops being
    /// optional.
    #[test]
    fn past_16tib_the_64bit_feature_is_required() {
        const TIB: u64 = 1024 * 1024 * MIB;

        // ext4 carries 64bit, so a 32 TiB filesystem is fine.
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(32 * TIB, &params).unwrap();
        assert_eq!(g.block_size, 4096);
        assert_eq!(g.blocks_count, 32 * TIB / 4096);
        assert!(g.blocks_count > u32::MAX as u64);
        assert_eq!(g.desc_size, 64);

        // Take 64bit away and the same size must be refused rather than
        // silently truncated to a filesystem a third the size.
        let params = Params::new(Profile::Ext4).no_journal().features("^64bit");
        let err = Geometry::compute(32 * TIB, &params).unwrap_err();
        assert!(
            matches!(err, Error::IncompatibleFeatures(ref m) if m.contains("64bit")),
            "unexpected error: {err}"
        );

        // Just under the boundary is fine without it.
        let params = Params::new(Profile::Ext4).no_journal().features("^64bit");
        let g = Geometry::compute(8 * TIB, &params).unwrap();
        assert_eq!(g.desc_size, 32);
        assert!(g.blocks_count <= u32::MAX as u64);
    }

    #[test]
    fn a_huge_filesystem_has_a_sane_group_count() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(64 * TIB, &params).unwrap();

        // 4 KiB blocks, 32768 blocks per group.
        assert_eq!(g.blocks_per_group, 32768);
        assert_eq!(g.group_count, (64 * TIB / 4096 / 32768) as u32);
        assert_eq!(g.group_count, 524_288);

        // Placement of an arbitrary interior group is O(1), not a walk.
        let mid = g.group(g.group_count / 2).unwrap();
        assert!(mid.block_bitmap >= mid.first_block);
        assert!(mid.inode_table > mid.inode_bitmap);
    }

    /// Past roughly 200 TiB the group descriptor table outgrows three quarters
    /// of a block group, and `mke2fs` switches to `meta_bg`: one descriptor
    /// block per meta block group, kept near the groups it describes.
    #[test]
    fn beyond_the_threshold_meta_bg_turns_itself_on() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let params = Params::new(Profile::Ext4).no_journal();

        // Below the threshold, the classic contiguous table. Reserved GDT
        // blocks are already zero at this size — they exist to let the
        // filesystem grow a thousandfold, and 128 TiB is past the 2^32-block
        // ceiling that growth would have to stay under.
        let small = Geometry::compute(128 * TIB, &params).unwrap();
        assert!(!small.meta_bg);
        assert!(small.features.compat.contains(CompatFeatures::RESIZE_INODE));

        // A more ordinary size does reserve them.
        let ordinary = Geometry::compute(64 * MIB, &params).unwrap();
        assert!(!ordinary.meta_bg);
        assert!(ordinary.reserved_gdt_blocks > 0);

        // Above it, meta_bg — and the resize inode goes, since there is no
        // longer a contiguous table for reserved blocks to extend.
        let huge = Geometry::compute(256 * TIB, &params).unwrap();
        assert!(huge.meta_bg);
        assert!(huge.features.incompat.contains(IncompatFeatures::META_BG));
        assert!(!huge.features.compat.contains(CompatFeatures::RESIZE_INODE));
        assert_eq!(huge.reserved_gdt_blocks, 0);
        assert_eq!(huge.first_meta_bg, 0);
    }

    /// Under meta_bg a descriptor block is kept in the first, second and last
    /// group of its meta block group, and nowhere else.
    #[test]
    fn meta_bg_keeps_three_copies_per_meta_group() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(256 * TIB, &params).unwrap();

        let size = g.meta_bg_size();
        assert_eq!(size, 64, "4096-byte blocks of 64-byte descriptors");

        // First, second and last group of meta group 0.
        assert!(g.group_has_desc(0));
        assert!(g.group_has_desc(1));
        assert!(g.group_has_desc(size - 1));
        // And nothing in between.
        assert!(!g.group_has_desc(2));
        assert!(!g.group_has_desc(size / 2));

        // The same pattern in a later meta group.
        assert!(g.group_has_desc(size));
        assert!(g.group_has_desc(size + 1));
        assert!(g.group_has_desc(2 * size - 1));
        assert!(!g.group_has_desc(size + 2));

        // One block each, not the whole table.
        assert_eq!(g.desc_blocks_in_group(0), 1);
        assert_eq!(g.desc_blocks_in_group(1), 1);
        assert_eq!(g.desc_blocks_in_group(2), 0);

        // Which is the point: the classic layout would need this many.
        assert!(g.desc_blocks > 1000);
    }

    #[test]
    fn meta_bg_descriptor_blocks_sit_after_the_superblock_copy() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(256 * TIB, &params).unwrap();

        // Group 0 has a superblock, so its descriptor block follows it.
        assert_eq!(g.desc_block_location(0), Some(g.group_first_block(0) + 1));
        assert_eq!(g.desc_block_location(1), Some(g.group_first_block(1) + 1));

        // The last group of meta group 0 is even, so it carries no superblock
        // backup and its descriptor block starts the group.
        let last = g.meta_bg_size() - 1;
        assert!(!g.has_super(last), "group {last} should have no superblock");
        assert_eq!(g.desc_block_location(last), Some(g.group_first_block(last)));

        assert_eq!(g.desc_block_location(2), None);
    }

    #[test]
    fn inode_ratio_follows_the_size_class() {
        const TIB: u64 = 1024 * 1024 * MIB;
        let mk = |size| {
            Geometry::compute(size, &Params::new(Profile::Ext4).no_journal()).unwrap()
        };
        // One inode per 4 KiB when small, per 16 KiB by default, per 32 KiB
        // when big, per 64 KiB when huge.
        assert_eq!(mk(64 * MIB).inodes_count, 64 * MIB as u32 / 4096);
        assert_eq!(mk(2048 * MIB).inodes_count, (2048 * MIB / 16384) as u32);
        assert_eq!(mk(12 * TIB).inodes_count, (12 * TIB / 32768) as u32);
        assert_eq!(mk(32 * TIB).inodes_count, (32 * TIB / 65536) as u32);
    }

    #[test]
    fn reserved_gdt_is_capped_at_one_indirect_block() {
        // 1 KiB blocks address 256 blocks through one indirect block, which is
        // the cap the golden reference hits.
        let params = Params::new(Profile::Ext4).no_journal();
        let g = Geometry::compute(64 * MIB, &params).unwrap();
        assert_eq!(g.reserved_gdt_blocks, 256);
        assert_eq!(g.reserved_gdt_blocks as u32, g.block_size / 4);
    }

    #[test]
    fn explicit_block_size_overrides_the_size_class() {
        let params = Params::new(Profile::Ext4).no_journal().block_size(4096);
        let g = Geometry::compute(64 * MIB, &params).unwrap();
        assert_eq!(g.block_size, 4096);
        assert_eq!(g.blocks_count, 16384);
        assert_eq!(g.first_data_block, 0);
        assert_eq!(g.blocks_per_group, 32768);
        assert_eq!(g.group_count, 1);
    }

    #[test]
    fn a_tiny_device_is_refused_rather_than_mangled() {
        let params = Params::new(Profile::Ext4).no_journal();
        assert!(Geometry::compute(8 * 1024, &params).is_err());
    }

    #[test]
    fn rejects_a_block_size_that_is_not_a_power_of_two() {
        let params = Params::new(Profile::Ext4).block_size(3000);
        assert!(Geometry::compute(64 * MIB, &params).is_err());
    }

    #[test]
    fn rejects_an_inode_larger_than_a_block() {
        let params = Params::new(Profile::Ext4)
            .block_size(1024)
            .inode_size(2048);
        assert!(Geometry::compute(64 * MIB, &params).is_err());
    }

    #[test]
    fn overhead_never_exceeds_the_filesystem() {
        for size in [16 * MIB, 64 * MIB, 256 * MIB, 1024 * MIB] {
            let params = Params::new(Profile::Ext4).no_journal();
            let g = Geometry::compute(size, &params).unwrap();
            assert!(g.overhead_blocks() < g.blocks_count, "size {size}");
        }
    }
}
