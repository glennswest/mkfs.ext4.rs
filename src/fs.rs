//! An opened filesystem.
//!
//! Everything above the raw device and below a particular task: decode the
//! superblock and group descriptors, find an inode, resolve a file's logical
//! block to a physical one through either an extent tree or the classic
//! indirect blocks, and walk a directory.
//!
//! [`crate::fsck`] checks with it and `fio.ext4.rs` reads and writes files with
//! it. Neither needs a kernel, a mount or a loop device.

use crate::bytes::{get_u32, put_u32};
use crate::csum::{self, GroupDescCsum};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::features::{IncompatFeatures, RoCompatFeatures};
use crate::structs::dirent::{self, DirEntry};
use crate::structs::extent::{self, Extent, ExtentHeader, ExtentIdx};
use crate::structs::group_desc::GroupDesc;
use crate::structs::htree;
use crate::structs::inode::{iflags, Inode, N_BLOCKS, NDIR_BLOCKS};
use crate::structs::superblock::{Superblock, SUPERBLOCK_LEN, SUPERBLOCK_OFFSET};

/// What a block in a file's block map is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A block of the file's contents.
    Data,
    /// A block of the map itself — an extent tree node or an indirect block.
    Metadata,
}

/// One block belonging to an inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    /// Logical block within the file, for data blocks.
    pub logical: Option<u64>,
    /// Physical block on the device.
    pub physical: u64,
    /// Whether this block holds contents or map structure.
    pub kind: BlockKind,
}

/// An opened ext2/ext3/ext4 filesystem.
pub struct Filesystem<D: BlockDevice> {
    device: D,
    superblock: Superblock,
    group_descs: Vec<GroupDesc>,
    csum_scheme: GroupDescCsum,
    csum_seed: u32,
}

impl<D: BlockDevice> Filesystem<D> {
    /// Open a filesystem, reading its superblock and group descriptors.
    pub async fn open(device: D) -> Result<Self> {
        let mut buf = [0u8; SUPERBLOCK_LEN];
        device.read_at(SUPERBLOCK_OFFSET, &mut buf).await?;
        let superblock = Superblock::decode(&buf)?;

        let mut fs = Self {
            device,
            superblock,
            group_descs: Vec::new(),
            csum_scheme: GroupDescCsum::None,
            csum_seed: 0,
        };
        fs.csum_scheme = if fs
            .superblock
            .feature_ro_compat
            .contains(RoCompatFeatures::METADATA_CSUM)
        {
            GroupDescCsum::Crc32c
        } else if fs
            .superblock
            .feature_ro_compat
            .contains(RoCompatFeatures::GDT_CSUM)
        {
            GroupDescCsum::Crc16
        } else {
            GroupDescCsum::None
        };
        fs.csum_seed = fs.superblock.csum_seed();
        fs.group_descs = fs.read_group_descs().await?;
        Ok(fs)
    }

    /// Open using a superblock backup rather than the primary.
    ///
    /// What `e2fsck -b` does when the primary is unreadable.
    pub async fn open_with_backup(device: D, backup_block: u64) -> Result<Self> {
        // The block size is not known until a superblock is read, so try the
        // sizes a backup could be at.
        for block_size in [1024u32, 2048, 4096, 8192, 16384, 32768, 65536] {
            let at = backup_block * block_size as u64;
            if at + SUPERBLOCK_LEN as u64 > device.size() {
                continue;
            }
            let mut buf = [0u8; SUPERBLOCK_LEN];
            if device.read_at(at, &mut buf).await.is_err() {
                continue;
            }
            if let Ok(sb) = Superblock::decode(&buf) {
                if sb.block_size() == block_size {
                    let mut fs = Self {
                        device,
                        superblock: sb,
                        group_descs: Vec::new(),
                        csum_scheme: GroupDescCsum::None,
                        csum_seed: 0,
                    };
                    fs.csum_scheme = if fs
                        .superblock
                        .feature_ro_compat
                        .contains(RoCompatFeatures::METADATA_CSUM)
                    {
                        GroupDescCsum::Crc32c
                    } else if fs
                        .superblock
                        .feature_ro_compat
                        .contains(RoCompatFeatures::GDT_CSUM)
                    {
                        GroupDescCsum::Crc16
                    } else {
                        GroupDescCsum::None
                    };
                    fs.csum_seed = fs.superblock.csum_seed();
                    fs.group_descs = fs.read_group_descs().await?;
                    return Ok(fs);
                }
            }
        }
        Err(Error::corrupt(
            "superblock",
            format!("no usable superblock backup at block {backup_block}"),
        ))
    }
}

impl<D: BlockDevice> Filesystem<D> {
    /// The superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// The superblock, mutably. Call [`Self::flush_superblock`] to persist it.
    pub fn superblock_mut(&mut self) -> &mut Superblock {
        &mut self.superblock
    }

    /// The device underneath.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Group descriptors.
    pub fn group_descs(&self) -> &[GroupDesc] {
        &self.group_descs
    }

    /// Group descriptors, mutably. Call [`Self::flush_group_descs`] to persist.
    pub fn group_descs_mut(&mut self) -> &mut Vec<GroupDesc> {
        &mut self.group_descs
    }

    /// Block size in bytes.
    pub fn block_size(&self) -> u32 {
        self.superblock.block_size()
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u32 {
        self.superblock.group_count()
    }

    /// The checksum scheme in use.
    pub fn csum_scheme(&self) -> GroupDescCsum {
        self.csum_scheme
    }

    /// The metadata checksum seed.
    pub fn csum_seed(&self) -> u32 {
        self.csum_seed
    }

    /// Whether this filesystem carries crc32c metadata checksums.
    pub fn has_metadata_csum(&self) -> bool {
        self.csum_scheme == GroupDescCsum::Crc32c
    }

    /// Whether inodes use extent trees rather than indirect blocks.
    pub fn uses_extents(&self) -> bool {
        self.superblock
            .feature_incompat
            .contains(IncompatFeatures::EXTENTS)
    }

    /// Byte offset of a block.
    pub fn block_offset(&self, block: u64) -> u64 {
        block * self.block_size() as u64
    }

    /// Read one block.
    pub async fn read_block(&self, block: u64) -> Result<Vec<u8>> {
        if block >= self.superblock.blocks_count {
            return Err(Error::corrupt(
                "block pointer",
                format!(
                    "block {block} is past the end of the {}-block filesystem",
                    self.superblock.blocks_count
                ),
            ));
        }
        let mut buf = vec![0u8; self.block_size() as usize];
        self.device.read_at(self.block_offset(block), &mut buf).await?;
        Ok(buf)
    }

    /// Write one block.
    pub async fn write_block(&self, block: u64, buf: &[u8]) -> Result<()> {
        self.device.write_at(self.block_offset(block), buf).await
    }

    /// Read the group descriptor table.
    ///
    /// Under meta_bg the table is not contiguous: each meta block group keeps
    /// its own descriptor block near the groups it describes, so the table is
    /// gathered a block at a time rather than read in one go.
    async fn read_group_descs(&self) -> Result<Vec<GroupDesc>> {
        let sb = &self.superblock;
        let desc_size = sb.desc_size() as usize;
        let group_count = sb.group_count();
        let block_size = sb.block_size() as usize;
        let desc_per_block = (block_size / desc_size) as u32;
        let meta_bg = sb
            .feature_incompat
            .contains(IncompatFeatures::META_BG);

        let mut raw = Vec::with_capacity(group_count as usize * desc_size);

        if !meta_bg {
            let gdt_block = if sb.block_size() == 1024 { 2 } else { 1 };
            let blocks = (group_count as usize * desc_size)
                .div_ceil(block_size) as u64;
            raw.resize((blocks * block_size as u64) as usize, 0);
            self.device
                .read_at(self.block_offset(gdt_block), &mut raw)
                .await?;
        } else {
            let meta_groups = group_count.div_ceil(desc_per_block);
            for meta in 0..meta_groups {
                // The block lives in the meta group's first group, after that
                // group's superblock copy if it has one.
                let leader = meta * desc_per_block;
                let at = self.group_first_block_for(leader, sb)
                    + self.group_has_super_for(leader, sb) as u64;
                let mut block = vec![0u8; block_size];
                self.device.read_at(self.block_offset(at), &mut block).await?;
                raw.extend_from_slice(&block);
            }
        }

        Ok((0..group_count as usize)
            .map(|g| GroupDesc::decode(&raw[g * desc_size..], desc_size))
            .collect())
    }

    /// First block of a group, without needing `self.superblock` to be set.
    fn group_first_block_for(&self, group: u32, sb: &Superblock) -> u64 {
        sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64
    }

    /// Whether a group carries a superblock backup, given a superblock.
    fn group_has_super_for(&self, group: u32, sb: &Superblock) -> bool {
        if !sb
            .feature_ro_compat
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
        [3u32, 5, 7].iter().any(|&base| {
            let mut n = group;
            while n % base == 0 {
                n /= base;
            }
            n == 1
        })
    }

    /// Where an inode lives: its group, block and byte offset within it.
    pub fn inode_location(&self, inum: u32) -> Result<(u32, u64, usize)> {
        let sb = &self.superblock;
        if inum == 0 || inum > sb.inodes_count {
            return Err(Error::corrupt(
                "inode number",
                format!("inode {inum} is outside 1..={}", sb.inodes_count),
            ));
        }
        let index = inum - 1;
        let group = index / sb.inodes_per_group;
        let within = (index % sb.inodes_per_group) as u64;
        let desc = self
            .group_descs
            .get(group as usize)
            .ok_or_else(|| Error::corrupt("group descriptor", format!("no group {group}")))?;

        let byte = within * sb.inode_size as u64;
        let block = desc.inode_table + byte / sb.block_size() as u64;
        let offset = (byte % sb.block_size() as u64) as usize;
        Ok((group, block, offset))
    }

    /// Read an inode.
    pub async fn read_inode(&self, inum: u32) -> Result<Inode> {
        let raw = self.read_inode_raw(inum).await?;
        Inode::decode(&raw, self.superblock.inode_size as usize)
    }

    /// Read an inode's raw bytes, as they are on disk.
    pub async fn read_inode_raw(&self, inum: u32) -> Result<Vec<u8>> {
        let (_, block, offset) = self.inode_location(inum)?;
        let inode_size = self.superblock.inode_size as usize;
        let mut buf = vec![0u8; inode_size];
        self.device
            .read_at(self.block_offset(block) + offset as u64, &mut buf)
            .await?;
        Ok(buf)
    }

    /// Write an inode, stamping its checksum.
    pub async fn write_inode(&self, inum: u32, inode: &Inode) -> Result<()> {
        let (_, block, offset) = self.inode_location(inum)?;
        let buf = inode.encode_with_csum(
            self.superblock.inode_size as usize,
            self.has_metadata_csum(),
            self.csum_seed,
            inum,
        );
        self.device
            .write_at(self.block_offset(block) + offset as u64, &buf)
            .await
    }

    /// Write the primary superblock back.
    pub async fn flush_superblock(&self) -> Result<()> {
        let buf = self.superblock.encode();
        self.device.write_at(SUPERBLOCK_OFFSET, &buf).await
    }

    /// Write the group descriptor table back, to the primary and every backup.
    pub async fn flush_group_descs(&self) -> Result<()> {
        let sb = &self.superblock;
        let desc_size = sb.desc_size() as usize;
        let block_size = sb.block_size() as usize;
        let desc_blocks = sb.gdt_blocks() as usize;

        let mut raw = vec![0u8; desc_blocks * block_size];
        for (g, desc) in self.group_descs.iter().enumerate() {
            desc.encode_with_csum(
                &mut raw[g * desc_size..],
                desc_size,
                self.csum_scheme,
                self.csum_seed,
                &sb.uuid,
                g as u32,
            );
        }

        for group in 0..sb.group_count() {
            if !self.group_has_super(group) {
                continue;
            }
            let first = sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64;
            self.device
                .write_at(self.block_offset(first + 1), &raw)
                .await?;
        }
        Ok(())
    }

    /// Whether descriptors are distributed per meta block group.
    pub fn meta_bg(&self) -> bool {
        self.superblock
            .feature_incompat
            .contains(IncompatFeatures::META_BG)
    }

    /// Group descriptors that fit in one block.
    pub fn desc_per_block(&self) -> u32 {
        self.block_size() / self.superblock.desc_size() as u32
    }

    /// Whether `group` stores a copy of a descriptor block.
    ///
    /// Under meta_bg that is the first, second and last group of each meta
    /// block group — not every group that carries a superblock backup. A
    /// checker that assumes the two coincide counts a block too many in every
    /// backup group that is not also a descriptor group.
    pub fn group_has_desc(&self, group: u32) -> bool {
        if !self.meta_bg() || group / self.desc_per_block() < self.superblock.first_meta_bg {
            return self.group_has_super(group);
        }
        let size = self.desc_per_block();
        let within = group % size;
        within == 0 || within == 1 || within == size - 1
    }

    /// Descriptor blocks stored in `group`.
    pub fn desc_blocks_in_group(&self, group: u32) -> u32 {
        if !self.group_has_desc(group) {
            return 0;
        }
        if !self.meta_bg() || group / self.desc_per_block() < self.superblock.first_meta_bg {
            self.superblock.gdt_blocks() + self.superblock.reserved_gdt_blocks as u32
        } else {
            1
        }
    }

    /// Blocks at the front of `group` holding its superblock copy and
    /// descriptors.
    pub fn super_overhead(&self, group: u32) -> u32 {
        self.group_has_super(group) as u32 + self.desc_blocks_in_group(group)
    }

    /// Whether a group carries a superblock backup.
    pub fn group_has_super(&self, group: u32) -> bool {
        if !self
            .superblock
            .feature_ro_compat
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
        [3u32, 5, 7].iter().any(|&base| {
            let mut n = group;
            while n % base == 0 {
                n /= base;
            }
            n == 1
        })
    }

    /// Walk every block an inode owns, contents and map structure alike.
    ///
    /// Iterative rather than recursive: an extent tree or a triple indirect
    /// chain is only a few levels deep, but a corrupt one can claim otherwise,
    /// and a checker must not blow its own stack on the filesystem it is
    /// inspecting.
    pub async fn walk_blocks(
        &self,
        inode: &Inode,
        mut visit: impl FnMut(BlockRef),
    ) -> Result<()> {
        if inode.flags & iflags::INLINE_DATA != 0 {
            // The contents live in i_block itself; there are no blocks.
            return Ok(());
        }
        if !inode.has_block_map() {
            // A device, FIFO, socket or fast symlink keeps something other
            // than block pointers in i_block.
            return Ok(());
        }
        if inode.uses_extents() {
            self.walk_extents(inode, &mut visit).await
        } else {
            self.walk_indirect(inode, &mut visit).await
        }
    }

    async fn walk_extents(
        &self,
        inode: &Inode,
        visit: &mut impl FnMut(BlockRef),
    ) -> Result<()> {
        // (buffer, node capacity in bytes, depth)
        let mut stack: Vec<(Vec<u8>, usize)> = vec![(inode.block.to_vec(), extent::INLINE_LEN)];
        let mut guard = 0u32;

        while let Some((node, space)) = stack.pop() {
            guard += 1;
            if guard > 1_000_000 {
                return Err(Error::corrupt(
                    "extent tree",
                    "walk did not terminate; the tree is probably cyclic",
                ));
            }
            let header = ExtentHeader::decode(&node)?;
            let max = ExtentHeader::max_entries(space, false);
            if header.entries > max {
                return Err(Error::corrupt(
                    "extent header",
                    format!("{} entries claimed, only {max} fit", header.entries),
                ));
            }

            for i in 0..header.entries as usize {
                let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
                if header.depth == 0 {
                    let ext = Extent::decode(&node[at..at + extent::ENTRY_LEN]);
                    for k in 0..ext.effective_len() as u64 {
                        visit(BlockRef {
                            logical: Some(ext.block as u64 + k),
                            physical: ext.start + k,
                            kind: BlockKind::Data,
                        });
                    }
                } else {
                    let idx = ExtentIdx::decode(&node[at..at + extent::ENTRY_LEN]);
                    visit(BlockRef {
                        logical: None,
                        physical: idx.leaf,
                        kind: BlockKind::Metadata,
                    });
                    let child = self.read_block(idx.leaf).await?;
                    stack.push((child, self.block_size() as usize));
                }
            }
        }
        Ok(())
    }

    async fn walk_indirect(
        &self,
        inode: &Inode,
        visit: &mut impl FnMut(BlockRef),
    ) -> Result<()> {
        let per_block = (self.block_size() / 4) as u64;
        let pointers = inode.block_pointers();

        let mut logical = 0u64;
        for &p in pointers.iter().take(NDIR_BLOCKS) {
            if p != 0 {
                visit(BlockRef {
                    logical: Some(logical),
                    physical: p as u64,
                    kind: BlockKind::Data,
                });
            }
            logical += 1;
        }

        // Levels 1, 2 and 3 of indirection, each expanding the one above.
        for (slot, depth) in [(NDIR_BLOCKS, 1usize), (NDIR_BLOCKS + 1, 2), (NDIR_BLOCKS + 2, 3)] {
            let root = pointers[slot] as u64;
            if root == 0 {
                logical += per_block.pow(depth as u32);
                continue;
            }
            visit(BlockRef {
                logical: None,
                physical: root,
                kind: BlockKind::Metadata,
            });
            logical = self
                .walk_indirect_level(root, depth, logical, per_block, visit)
                .await?;
        }
        Ok(())
    }

    /// Expand one indirect block, returning the next logical block number.
    async fn walk_indirect_level(
        &self,
        block: u64,
        depth: usize,
        start_logical: u64,
        per_block: u64,
        visit: &mut impl FnMut(BlockRef),
    ) -> Result<u64> {
        // An explicit stack of (block, depth, logical base) rather than
        // recursion, which async makes awkward and a corrupt tree makes unsafe.
        let mut logical = start_logical;
        let mut stack = vec![(block, depth, start_logical)];

        while let Some((blk, d, base)) = stack.pop() {
            let buf = self.read_block(blk).await?;
            let span = per_block.pow((d - 1) as u32);
            for i in 0..per_block {
                let entry = get_u32(&buf, (i * 4) as usize) as u64;
                let child_base = base + i * span;
                if entry == 0 {
                    continue;
                }
                if d == 1 {
                    visit(BlockRef {
                        logical: Some(child_base),
                        physical: entry,
                        kind: BlockKind::Data,
                    });
                } else {
                    visit(BlockRef {
                        logical: None,
                        physical: entry,
                        kind: BlockKind::Metadata,
                    });
                    stack.push((entry, d - 1, child_base));
                }
            }
            logical = logical.max(base + per_block * span);
        }
        Ok(logical)
    }

    /// Resolve a file's logical block to its physical block, if mapped.
    pub async fn resolve_block(&self, inode: &Inode, logical: u64) -> Result<Option<u64>> {
        if !inode.has_block_map() {
            return Ok(None);
        }
        if inode.uses_extents() {
            return self.resolve_extent(inode, logical).await;
        }

        let per_block = (self.block_size() / 4) as u64;
        let pointers = inode.block_pointers();

        if logical < NDIR_BLOCKS as u64 {
            let p = pointers[logical as usize];
            return Ok((p != 0).then_some(p as u64));
        }
        let mut remaining = logical - NDIR_BLOCKS as u64;
        for (slot, depth) in [(NDIR_BLOCKS, 1u32), (NDIR_BLOCKS + 1, 2), (NDIR_BLOCKS + 2, 3)] {
            let span = per_block.pow(depth);
            if remaining < span {
                let mut block = pointers[slot] as u64;
                if block == 0 {
                    return Ok(None);
                }
                // Walk down, one index per level.
                for level in (0..depth).rev() {
                    let stride = per_block.pow(level);
                    let index = (remaining / stride) % per_block;
                    let buf = self.read_block(block).await?;
                    block = get_u32(&buf, (index * 4) as usize) as u64;
                    if block == 0 {
                        return Ok(None);
                    }
                }
                return Ok(Some(block));
            }
            remaining -= span;
        }
        Ok(None)
    }

    async fn resolve_extent(&self, inode: &Inode, logical: u64) -> Result<Option<u64>> {
        let mut node = inode.block.to_vec();

        loop {
            let header = ExtentHeader::decode(&node)?;
            if header.depth == 0 {
                for i in 0..header.entries as usize {
                    let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
                    let ext = Extent::decode(&node[at..at + extent::ENTRY_LEN]);
                    let len = ext.effective_len() as u64;
                    if logical >= ext.block as u64 && logical < ext.block as u64 + len {
                        return Ok(Some(ext.start + (logical - ext.block as u64)));
                    }
                }
                return Ok(None);
            }

            // Descend into the last index whose first block is at or below the
            // one we want.
            let mut next = None;
            for i in 0..header.entries as usize {
                let at = extent::HEADER_LEN + i * extent::ENTRY_LEN;
                let idx = ExtentIdx::decode(&node[at..at + extent::ENTRY_LEN]);
                if idx.block as u64 <= logical {
                    next = Some(idx.leaf);
                } else {
                    break;
                }
            }
            let Some(child) = next else {
                return Ok(None);
            };
            node = self.read_block(child).await?;
        }
    }

    /// Read a directory's entries.
    pub async fn read_dir(&self, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            return Err(Error::corrupt("inode", "not a directory"));
        }
        let block_size = self.block_size() as u64;
        let blocks = inode.size.div_ceil(block_size);

        let mut out = Vec::new();
        for logical in 0..blocks {
            let Some(physical) = self.resolve_block(inode, logical).await? else {
                continue;
            };
            let buf = self.read_block(physical).await?;
            for entry in dirent::parse_block(&buf)? {
                // Holes and the checksum tail are not names.
                if entry.inode != 0 && !entry.is_tail() && !entry.name.is_empty() {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }

    /// Look a name up in a directory.
    pub async fn lookup(&self, dir: &Inode, name: &[u8]) -> Result<Option<u32>> {
        // An indexed directory answers in a two-block walk instead of a read
        // of the whole thing, which is the difference between filling a
        // directory in linear time and in quadratic time.
        if dir.flags & iflags::INDEX != 0 {
            if let Some(answer) = self.lookup_indexed(dir, name).await? {
                return Ok(answer);
            }
        }
        Ok(self
            .read_dir(dir)
            .await?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.inode))
    }

    /// Look a name up through a directory's hash index.
    ///
    /// The outer `Option` says whether the index could be used at all: `None`
    /// means the directory does not have one that this code understands, and
    /// the caller should read the directory the slow way rather than conclude
    /// the name is absent.
    async fn lookup_indexed(&self, dir: &Inode, name: &[u8]) -> Result<Option<Option<u32>>> {
        let block_size = self.block_size() as usize;
        let Some(root_block) = self.resolve_block(dir, 0).await? else {
            return Ok(None);
        };
        let root = self.read_block(root_block).await?;
        if htree::count_offset(&root, block_size) != Some(htree::ROOT_COUNT_OFFSET) {
            return Ok(None);
        }

        // The root records which hash it was built with; the superblock's
        // flags say which signedness convention that hash used.
        let version = htree::version_for(htree::root_hash_version(&root), self.superblock().flags);
        let (hash, _) = htree::dirhash(version, name, &self.superblock().hash_seed);

        // Descend to the block whose index covers this hash.
        let mut node = root;
        let mut offset = htree::ROOT_COUNT_OFFSET;
        if htree::indirect_levels(&node) > 0 {
            let chosen = htree::find(&node, offset, hash)?;
            let (_, child) = htree::entry(&node, offset, chosen);
            let Some(physical) = self.resolve_block(dir, child as u64).await? else {
                return Ok(None);
            };
            node = self.read_block(physical).await?;
            match htree::count_offset(&node, block_size) {
                Some(found) => offset = found,
                None => return Ok(None),
            }
        }

        let count = htree::count(&node, offset) as usize;
        let mut at = htree::find(&node, offset, hash)?;

        // Names that share a hash can spill into the following leaves, which
        // say so by setting the low bit of their own hash — the one bit the
        // hash itself never uses. Without following that, a lookup would stop
        // at the first leaf and miss them.
        loop {
            let (_, leaf) = htree::entry(&node, offset, at);
            let Some(physical) = self.resolve_block(dir, leaf as u64).await? else {
                return Ok(None);
            };
            let buf = self.read_block(physical).await?;
            for entry in dirent::parse_block(&buf)? {
                if entry.inode != 0 && entry.name == name {
                    return Ok(Some(Some(entry.inode)));
                }
            }

            at += 1;
            if at >= count {
                return Ok(Some(None));
            }
            let (next, _) = htree::entry(&node, offset, at);
            if next & 1 == 0 {
                return Ok(Some(None));
            }
        }
    }

    /// Resolve an absolute path to an inode number.
    pub async fn resolve_path(&self, path: &str) -> Result<Option<u32>> {
        let mut inum = crate::structs::superblock::ino::ROOT;
        for component in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            let dir = self.read_inode(inum).await?;
            match self.lookup(&dir, component.as_bytes()).await? {
                Some(next) => inum = next,
                None => return Ok(None),
            }
        }
        Ok(Some(inum))
    }

    /// Read a whole file.
    pub async fn read_file(&self, inode: &Inode) -> Result<Vec<u8>> {
        let block_size = self.block_size() as u64;
        let mut out = Vec::with_capacity(inode.size as usize);
        let blocks = inode.size.div_ceil(block_size);

        for logical in 0..blocks {
            match self.resolve_block(inode, logical).await? {
                // A hole reads as zeroes, which is what a sparse file means.
                None => out.extend(std::iter::repeat_n(0u8, block_size as usize)),
                Some(physical) => out.extend_from_slice(&self.read_block(physical).await?),
            }
        }
        out.truncate(inode.size as usize);
        Ok(out)
    }

    /// Recompute a directory block's checksum tail after modifying it.
    pub fn stamp_dir_block(&self, block: &mut [u8], inum: u32, generation: u32) {
        if !self.has_metadata_csum() {
            return;
        }
        let limit = block.len() - dirent::TAIL_LEN;
        let c = csum::dirent_csum(self.csum_seed, inum, generation, &block[..limit]);
        dirent::set_block_csum(block, c);
    }

    /// Stamp a bitmap's checksum into a group descriptor.
    pub fn stamp_bitmap_csum(&self, desc: &mut GroupDesc, bitmap: &[u8], is_block_bitmap: bool) {
        if !self.has_metadata_csum() {
            return;
        }
        let c = csum::bitmap_csum(self.csum_seed, bitmap);
        if is_block_bitmap {
            desc.block_bitmap_csum = c;
        } else {
            desc.inode_bitmap_csum = c;
        }
    }

    /// Read a group's block bitmap.
    pub async fn read_block_bitmap(&self, group: u32) -> Result<Vec<u8>> {
        let desc = self.group_descs.get(group as usize).ok_or_else(|| {
            Error::corrupt("group descriptor", format!("no group {group}"))
        })?;
        self.read_block(desc.block_bitmap).await
    }

    /// Read a group's inode bitmap.
    pub async fn read_inode_bitmap(&self, group: u32) -> Result<Vec<u8>> {
        let desc = self.group_descs.get(group as usize).ok_or_else(|| {
            Error::corrupt("group descriptor", format!("no group {group}"))
        })?;
        self.read_block(desc.inode_bitmap).await
    }

    /// Set or clear a bit in a bitmap buffer.
    pub fn set_bit(bitmap: &mut [u8], index: u64, on: bool) {
        let byte = (index / 8) as usize;
        let mask = 1u8 << (index % 8);
        if on {
            bitmap[byte] |= mask;
        } else {
            bitmap[byte] &= !mask;
        }
    }

    /// Test a bit in a bitmap buffer.
    pub fn test_bit(bitmap: &[u8], index: u64) -> bool {
        bitmap[(index / 8) as usize] & (1u8 << (index % 8)) != 0
    }

    /// Write a `u32` into a buffer, for callers building indirect blocks.
    pub fn put_pointer(buf: &mut [u8], index: usize, block: u32) {
        put_u32(buf, index * 4, block);
    }

    /// Total blocks in a group, which for the last group may be short.
    pub fn group_block_count(&self, group: u32) -> u32 {
        let sb = &self.superblock;
        let first = sb.first_data_block as u64 + group as u64 * sb.blocks_per_group as u64;
        let last = (first + sb.blocks_per_group as u64 - 1).min(sb.blocks_count - 1);
        (last - first + 1) as u32
    }

    /// First block of a group.
    pub fn group_first_block(&self, group: u32) -> u64 {
        self.superblock.first_data_block as u64
            + group as u64 * self.superblock.blocks_per_group as u64
    }

    /// Which group a block belongs to.
    pub fn group_of_block(&self, block: u64) -> u32 {
        ((block - self.superblock.first_data_block as u64)
            / self.superblock.blocks_per_group as u64) as u32
    }

    /// Consume the filesystem, returning the device.
    pub fn into_device(self) -> D {
        self.device
    }
}

/// Number of block pointers in an inode. Re-exported for callers building maps.
pub const INODE_POINTERS: usize = N_BLOCKS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::format::format;
    use crate::params::{Params, Profile};
    use crate::structs::superblock::ino;

    const MIB: u64 = 1024 * 1024;

    async fn formatted(profile: Profile, size: u64) -> Filesystem<MemDevice> {
        let dev = MemDevice::new(size);
        let params = Params::new(profile)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();
        Filesystem::open(dev).await.unwrap()
    }

    #[tokio::test]
    async fn opens_what_the_formatter_wrote() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        assert_eq!(fs.block_size(), 1024);
        assert_eq!(fs.superblock().blocks_count, 65536);
        assert_eq!(fs.group_count(), 8);
        assert_eq!(fs.group_descs().len(), 8);
        assert!(fs.has_metadata_csum());
        assert!(fs.uses_extents());
    }

    #[tokio::test]
    async fn reads_the_root_directory() {
        for profile in [Profile::Ext2, Profile::Ext3, Profile::Ext4] {
            let fs = formatted(profile, 64 * MIB).await;
            let root = fs.read_inode(ino::ROOT).await.unwrap();
            assert!(root.is_dir(), "{}", profile.name());
            assert_eq!(root.links_count, 3, "{}", profile.name());

            let entries = fs.read_dir(&root).await.unwrap();
            let names: Vec<String> = entries.iter().map(|e| e.name_string()).collect();
            assert_eq!(names, vec![".", "..", "lost+found"], "{}", profile.name());
        }
    }

    #[tokio::test]
    async fn resolves_paths() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        assert_eq!(fs.resolve_path("/").await.unwrap(), Some(ino::ROOT));
        assert_eq!(fs.resolve_path("/lost+found").await.unwrap(), Some(11));
        assert_eq!(fs.resolve_path("/nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn lost_and_found_is_a_directory_with_the_expected_links() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        let lpf_ino = fs.resolve_path("/lost+found").await.unwrap().unwrap();
        let lpf = fs.read_inode(lpf_ino).await.unwrap();
        assert!(lpf.is_dir());
        assert_eq!(lpf.links_count, 2);

        let entries = fs.read_dir(&lpf).await.unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.name_string()).collect();
        assert_eq!(names, vec![".", ".."]);
    }

    #[tokio::test]
    async fn walks_the_blocks_of_an_extent_mapped_inode() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        let lpf_ino = fs.resolve_path("/lost+found").await.unwrap().unwrap();
        let lpf = fs.read_inode(lpf_ino).await.unwrap();

        let mut data = Vec::new();
        fs.walk_blocks(&lpf, |b| {
            if b.kind == BlockKind::Data {
                data.push(b.physical);
            }
        })
        .await
        .unwrap();

        // 16 KiB of lost+found at 1 KiB blocks, capped at the 12 direct blocks.
        assert_eq!(data.len(), 12);
        // Contiguous, as the formatter allocated it.
        for pair in data.windows(2) {
            assert_eq!(pair[0] + 1, pair[1]);
        }
    }

    #[tokio::test]
    async fn walks_the_blocks_of_an_indirect_mapped_inode() {
        // ext3's journal is large enough to need indirect blocks.
        let fs = formatted(Profile::Ext3, 64 * MIB).await;
        let journal = fs.read_inode(ino::JOURNAL).await.unwrap();
        assert!(!journal.uses_extents());

        let mut data = 0u64;
        let mut meta = 0u64;
        fs.walk_blocks(&journal, |b| match b.kind {
            BlockKind::Data => data += 1,
            BlockKind::Metadata => meta += 1,
        })
        .await
        .unwrap();

        assert_eq!(data, 4096, "journal data blocks");
        assert!(meta > 0, "an indirect map must have metadata blocks");
        // i_blocks counts both, in 512-byte sectors.
        assert_eq!(journal.blocks, (data + meta) * 2);
    }

    #[tokio::test]
    async fn resolve_block_agrees_with_the_walk() {
        for profile in [Profile::Ext3, Profile::Ext4] {
            let fs = formatted(profile, 64 * MIB).await;
            let journal = fs.read_inode(ino::JOURNAL).await.unwrap();

            let mut walked = Vec::new();
            fs.walk_blocks(&journal, |b| {
                if b.kind == BlockKind::Data {
                    walked.push((b.logical.unwrap(), b.physical));
                }
            })
            .await
            .unwrap();
            walked.sort_unstable();

            for &(logical, physical) in walked.iter().step_by(97) {
                assert_eq!(
                    fs.resolve_block(&journal, logical).await.unwrap(),
                    Some(physical),
                    "{} logical block {logical}",
                    profile.name()
                );
            }
        }
    }

    #[tokio::test]
    async fn reads_the_journal_superblock_back_through_the_inode() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        let journal = fs.read_inode(ino::JOURNAL).await.unwrap();
        let first = fs.resolve_block(&journal, 0).await.unwrap().unwrap();
        let buf = fs.read_block(first).await.unwrap();
        assert_eq!(
            crate::journal::get_u32_be(&buf, crate::journal::off::H_MAGIC),
            crate::journal::JBD2_MAGIC
        );
    }

    #[tokio::test]
    async fn inode_checksums_verify() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        for inum in [ino::ROOT, ino::JOURNAL, 11] {
            let raw = fs.read_inode_raw(inum).await.unwrap();
            assert!(
                Inode::verify_checksum(
                    &raw,
                    fs.superblock().inode_size as usize,
                    true,
                    fs.csum_seed(),
                    inum
                )
                .unwrap(),
                "inode {inum}"
            );
        }
    }

    #[tokio::test]
    async fn group_descriptor_checksums_verify() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        let desc_size = fs.superblock().desc_size() as usize;
        for (g, desc) in fs.group_descs().iter().enumerate() {
            let mut buf = vec![0u8; desc_size];
            desc.encode_into(&mut buf, desc_size);
            let expect = csum::group_desc_csum(
                fs.csum_scheme(),
                fs.csum_seed(),
                &fs.superblock().uuid,
                g as u32,
                &buf,
            );
            assert_eq!(desc.checksum, expect, "group {g}");
        }
    }

    #[tokio::test]
    async fn a_backup_superblock_opens_the_filesystem() {
        let fs = formatted(Profile::Ext4, 64 * MIB).await;
        let backup_block = fs.group_first_block(1);
        let dev = fs.into_device();

        let fs = Filesystem::open_with_backup(dev, backup_block).await.unwrap();
        assert_eq!(fs.superblock().blocks_count, 65536);
        assert_eq!(fs.superblock().block_group_nr, 1);
    }
}
