//! Orphan release on files built here: extent-mapped (inline and depth 1)
//! and indirect, on the chain and in the orphan file.

use super::*;
use crate::device::MemDevice;
use crate::format::format;
use crate::fsck::{check, FsckOptions};
use crate::params::{Params, Profile};
use crate::structs::dirent::{self, file_type, DirEntry};
use crate::structs::inode::mode;
use crate::structs::superblock::ino;

const MIB: u64 = 1024 * 1024;

async fn formatted(profile: Profile) -> MemDevice {
    let dev = MemDevice::new(64 * MIB);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params).await.unwrap();
    dev
}

/// The free counts the superblock and group 0 report.
fn counts(fs: &Filesystem<&MemDevice>) -> (u64, u32, u32, u32) {
    let d = fs.group_descs()[0];
    (
        fs.superblock().free_blocks_count,
        fs.superblock().free_inodes_count,
        d.free_blocks_count,
        d.free_inodes_count,
    )
}

/// Set bits in group 0's bitmaps, keeping counts and checksums right.
async fn take(fs: &mut Filesystem<&MemDevice>, blocks: &[u64], inode: Option<u32>) {
    let desc = fs.group_descs()[0];
    let sb = fs.superblock().clone();
    let mut bb = fs.read_block_bitmap(0).await.unwrap();
    for &b in blocks {
        let bit = b - fs.group_first_block(0);
        assert!(!Filesystem::<&MemDevice>::test_bit(&bb, bit));
        Filesystem::<&MemDevice>::set_bit(&mut bb, bit, true);
    }
    fs.write_block(desc.block_bitmap, &bb).await.unwrap();
    let mut ib = fs.read_inode_bitmap(0).await.unwrap();
    if let Some(inum) = inode {
        Filesystem::<&MemDevice>::set_bit(&mut ib, (inum - 1) as u64, true);
        fs.write_block(desc.inode_bitmap, &ib).await.unwrap();
    }
    let seed = fs.csum_seed();
    let csum_on = fs.has_metadata_csum();
    let d = &mut fs.group_descs_mut()[0];
    d.free_blocks_count -= blocks.len() as u32;
    if let Some(inum) = inode {
        d.free_inodes_count -= 1;
        // The inode table is in use up to here now.
        d.itable_unused = d.itable_unused.min(sb.inodes_per_group - inum);
    }
    if csum_on {
        d.block_bitmap_csum = csum::bitmap_csum(seed, &bb[..(sb.blocks_per_group as usize).div_ceil(8)]);
        d.inode_bitmap_csum = csum::bitmap_csum(seed, &ib[..(sb.inodes_per_group as usize).div_ceil(8)]);
    }
    let s = fs.superblock_mut();
    s.free_blocks_count -= blocks.len() as u64;
    if inode.is_some() {
        s.free_inodes_count -= 1;
    }
    fs.flush_group_descs().await.unwrap();
    fs.flush_superblock().await.unwrap();
}

/// Free blocks in group 0, `n` of them, leaving a free block between runs
/// so each run is an extent of its own.
async fn free_blocks(fs: &Filesystem<&MemDevice>, n: u64, runs: u64) -> Vec<u64> {
    let bb = fs.read_block_bitmap(0).await.unwrap();
    let first = fs.group_first_block(0);
    let per_run = n.div_ceil(runs);
    let mut out = Vec::new();
    let mut bit = 0u64;
    // Start past the metadata, at the first long free stretch.
    while Filesystem::<&MemDevice>::test_bit(&bb, bit) || !((bit..bit + n + runs).all(|b| !Filesystem::<&MemDevice>::test_bit(&bb, b))) {
        bit += 1;
    }
    while (out.len() as u64) < n {
        for _ in 0..per_run.min(n - out.len() as u64) {
            out.push(first + bit);
            bit += 1;
        }
        bit += 1; // the gap
    }
    out
}

async fn free_inode(fs: &Filesystem<&MemDevice>) -> u32 {
    let ib = fs.read_inode_bitmap(0).await.unwrap();
    let first = fs.superblock().first_ino;
    (first..).find(|&i| !Filesystem::<&MemDevice>::test_bit(&ib, (i - 1) as u64)).unwrap()
}

/// Link `inum` into the root directory as `name`.
async fn link(fs: &Filesystem<&MemDevice>, inum: u32, name: &[u8]) {
    let root = fs.read_inode(ino::ROOT).await.unwrap();
    let block = fs.resolve_block(&root, 0).await.unwrap().unwrap();
    let buf = fs.read_block(block).await.unwrap();
    let mut entries: Vec<DirEntry> = dirent::parse_block(&buf)
        .unwrap()
        .into_iter()
        .filter(|e| e.inode != 0 && !e.is_tail())
        .collect();
    entries.push(DirEntry::new(inum, name, file_type::REG_FILE).unwrap());
    let csum_on = fs.has_metadata_csum();
    let mut out = dirent::build_block(&entries, buf.len(), csum_on).unwrap();
    if csum_on {
        let tail = out.len() - dirent::TAIL_LEN;
        dirent::write_tail_header(&mut out[tail..]);
    }
    fs.stamp_dir_block(&mut out, ino::ROOT, root.generation);
    fs.write_block(block, &out).await.unwrap();
}

/// A regular file of `n` data blocks in `runs` extents (or an indirect map
/// on a filesystem without extents), `size` bytes long, `links` links.
async fn make_file(
    fs: &mut Filesystem<&MemDevice>,
    n: u64,
    runs: u64,
    size: u64,
    links: u16,
) -> (u32, Vec<u64>) {
    let inum = free_inode(fs).await;
    let bs = fs.block_size() as u64;
    let extents = fs.uses_extents();
    let map_blocks = if extents { (runs > 4) as u64 } else { (n > 12) as u64 };
    let all = free_blocks(fs, n + map_blocks, if extents { runs } else { 1 }).await;
    let (map, data) = all.split_at(map_blocks as usize);
    let data = data.to_vec();

    let inode_size = fs.superblock().inode_size as usize;
    let mut inode = Inode::new(inode_size, if inode_size > 128 { 32 } else { 0 });
    inode.mode = mode::IFREG | 0o644;
    inode.links_count = links;
    inode.size = size;
    inode.blocks = (n + map_blocks) * bs / 512;
    inode.generation = 0x5eed;
    if extents {
        inode.flags |= iflags::EXTENTS;
        let mut list = Vec::new();
        let mut logical = 0u32;
        let mut i = 0;
        while i < data.len() {
            let mut j = i + 1;
            while j < data.len() && data[j] == data[j - 1] + 1 {
                j += 1;
            }
            list.push(Extent { block: logical, len: (j - i) as u16, start: data[i] });
            logical += (j - i) as u32;
            i = j;
        }
        assert_eq!(list.len() as u64, runs);
        if list.len() <= 4 {
            inode.block = extent::build_inline(&list).unwrap();
        } else {
            let leaf = map[0];
            let mut node = vec![0u8; bs as usize];
            let max = ExtentHeader::max_entries(bs as usize, false);
            ExtentHeader { entries: list.len() as u16, max, depth: 0, generation: 0 }.encode_into(&mut node);
            for (k, e) in list.iter().enumerate() {
                e.encode_into(&mut node[extent::HEADER_LEN + k * extent::ENTRY_LEN..]);
            }
            if fs.has_metadata_csum() {
                let at = extent::tail_offset(max);
                let c = csum::extent_block_csum(fs.csum_seed(), inum, inode.generation, &node[..at]);
                put_u32(&mut node, at, c);
            }
            fs.write_block(leaf, &node).await.unwrap();
            let mut root = [0u8; extent::INLINE_LEN];
            ExtentHeader { entries: 1, max: 4, depth: 1, generation: 0 }.encode_into(&mut root);
            ExtentIdx { block: 0, leaf }.encode_into(&mut root[extent::HEADER_LEN..]);
            inode.block = root;
        }
    } else {
        let mut pointers = [0u32; 15];
        for (k, &b) in data.iter().take(NDIR_BLOCKS).enumerate() {
            pointers[k] = b as u32;
        }
        if n > NDIR_BLOCKS as u64 {
            let mut ind = vec![0u8; bs as usize];
            for (k, &b) in data.iter().skip(NDIR_BLOCKS).enumerate() {
                put_u32(&mut ind, k * 4, b as u32);
            }
            fs.write_block(map[0], &ind).await.unwrap();
            pointers[NDIR_BLOCKS] = map[0] as u32;
        }
        inode.set_block_pointers(&pointers);
    }
    for &b in &data {
        fs.write_block(b, &vec![0x5a; bs as usize]).await.unwrap();
    }
    fs.write_inode(inum, &inode).await.unwrap();
    take(fs, &all, Some(inum)).await;
    if links > 0 {
        link(fs, inum, format!("f{inum}").as_bytes()).await;
    }
    (inum, all)
}

async fn chain(fs: &mut Filesystem<&MemDevice>, inums: &[u32]) {
    for w in inums.windows(2) {
        let mut inode = fs.read_inode(w[0]).await.unwrap();
        inode.dtime = w[1];
        fs.write_inode(w[0], &inode).await.unwrap();
    }
    fs.superblock_mut().last_orphan = inums[0];
    fs.flush_superblock().await.unwrap();
}

fn codes(report: &crate::fsck::FsckReport) -> Vec<&'static str> {
    report.problems.iter().map(|p| p.code).collect()
}

#[tokio::test]
async fn an_unlinked_orphan_is_freed_and_the_counts_come_back() {
    for (profile, runs) in [(Profile::Ext4, 2), (Profile::Ext4, 6), (Profile::Ext3, 1), (Profile::Ext2, 1)] {
        let dev = formatted(profile).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let before = counts(&fs);
        let size = 30 * fs.block_size() as u64;
        let (inum, _) = make_file(&mut fs, 30, runs, size, 0).await;
        chain(&mut fs, &[inum]).await;
        drop(fs);

        let report = check(&dev, &FsckOptions::repair()).await.unwrap();
        assert_eq!(codes(&report), ["orphan-released"], "{profile:?} {runs}: {:?}", report.problems);
        assert!(report.problems[0].message.starts_with(&format!("Clearing orphaned inode {inum} ")));
        assert_eq!(report.exit_code(), 0);

        let fs = Filesystem::open(&dev).await.unwrap();
        assert_eq!(counts(&fs), before, "{profile:?} {runs}");
        assert_eq!(fs.superblock().last_orphan, 0);
        let inode = fs.read_inode(inum).await.unwrap();
        assert_ne!(inode.dtime, 0);
        let again = check(&dev, &FsckOptions::check_only().force(true)).await.unwrap();
        assert!(again.is_clean(), "{profile:?} {runs}: {:?}", again.problems);
    }
}

#[tokio::test]
async fn a_linked_orphan_is_truncated_to_its_size() {
    // (profile, extents, blocks kept, map blocks kept)
    for (profile, runs, keep) in [
        (Profile::Ext4, 2, 7),
        (Profile::Ext4, 3, 0),
        (Profile::Ext4, 6, 11),
        (Profile::Ext4, 6, 3),
        (Profile::Ext3, 1, 10),
        (Profile::Ext3, 1, 15),
        (Profile::Ext2, 1, 0),
    ] {
        let dev = formatted(profile).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let bs = fs.block_size() as u64;
        let before = counts(&fs);
        // The size ends part way into the last block kept.
        let size = if keep == 0 { 0 } else { keep * bs - 100 };
        let (inum, all) = make_file(&mut fs, 30, runs, size, 1).await;
        chain(&mut fs, &[inum]).await;
        drop(fs);

        let report = check(&dev, &FsckOptions::repair()).await.unwrap();
        let what = format!("{profile:?} runs {runs} keep {keep}");
        assert_eq!(codes(&report), ["orphan-released"], "{what}: {:?}", report.problems);
        assert!(report.problems[0].message.starts_with("Truncating"), "{what}");
        assert_eq!(report.exit_code(), 0, "{what}");

        let fs = Filesystem::open(&dev).await.unwrap();
        let inode = fs.read_inode(inum).await.unwrap();
        assert_eq!(inode.dtime, 0, "{what}");
        let mut owned = Vec::new();
        fs.walk_blocks(&inode, |b| owned.push(b)).await.unwrap();
        let data = owned.iter().filter(|b| b.logical.is_some()).count() as u64;
        assert_eq!(data, keep, "{what}");
        assert_eq!(inode.blocks, owned.len() as u64 * bs / 512, "{what}");
        assert_eq!(counts(&fs).0, before.0 - owned.len() as u64, "{what}");
        let _ = all;
        let again = check(&dev, &FsckOptions::check_only().force(true)).await.unwrap();
        assert!(again.is_clean(), "{what}: {:?}", again.problems);
    }
}

#[tokio::test]
async fn the_chain_is_followed_through_i_dtime() {
    let dev = formatted(Profile::Ext4).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as u64;
    let (a, _) = make_file(&mut fs, 8, 1, 8 * bs, 0).await;
    let (b, _) = make_file(&mut fs, 8, 1, 2 * bs, 1).await;
    let (c, _) = make_file(&mut fs, 8, 2, 8 * bs, 0).await;
    chain(&mut fs, &[a, b, c]).await;
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    let released: Vec<_> = report.notes().map(|p| p.message.clone()).collect();
    assert_eq!(released.len(), 3, "{released:?}");
    assert!(released[0].contains(&format!("inode {a} ")));
    assert!(released[1].starts_with(&format!("Truncating orphaned inode {b} ")));
    assert!(released[2].contains(&format!("inode {c} ")));
    assert_eq!(report.exit_code(), 0, "{:?}", report.problems);
    assert!(check(&dev, &FsckOptions::check_only().force(true)).await.unwrap().is_clean());
}

#[tokio::test]
async fn orphan_file_slots_are_released_and_emptied() {
    let dev = formatted(Profile::Ext4).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    assert!(fs.superblock().feature_compat.contains(CompatFeatures::ORPHAN_FILE));
    let bs = fs.block_size() as u64;
    let before = counts(&fs);
    let (a, _) = make_file(&mut fs, 5, 1, 5 * bs, 0).await;
    let (b, _) = make_file(&mut fs, 5, 1, bs, 1).await;

    let file = fs.read_inode(fs.superblock().orphan_file_inum).await.unwrap();
    let blocks = orphan_file_blocks(&fs, &file).await.unwrap();
    let mut buf = fs.read_block(blocks[1]).await.unwrap();
    put_u32(&mut buf, 0, a);
    put_u32(&mut buf, 40, b);
    let orphan_inum = fs.superblock().orphan_file_inum;
    write_orphan_block(&fs, orphan_inum, file.generation, blocks[1], &mut buf).await.unwrap();
    fs.superblock_mut().feature_ro_compat.insert(RoCompatFeatures::ORPHAN_PRESENT);
    fs.flush_superblock().await.unwrap();
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    assert_eq!(codes(&report), ["orphan-released", "orphan-released"], "{:?}", report.problems);
    assert_eq!(report.exit_code(), 0);

    let fs = Filesystem::open(&dev).await.unwrap();
    assert!(!fs.superblock().feature_ro_compat.contains(RoCompatFeatures::ORPHAN_PRESENT));
    let buf = fs.read_block(blocks[1]).await.unwrap();
    assert!(buf[..bs as usize - ORPHAN_TAIL_LEN].iter().all(|&x| x == 0));
    // a is gone, b keeps one block: the counts are back less b's inode and block.
    let after = counts(&fs);
    assert_eq!(after.0, before.0 - 1);
    assert_eq!(after.1, before.1 - 1);
    assert!(check(&dev, &FsckOptions::check_only().force(true)).await.unwrap().is_clean());
}

#[tokio::test]
async fn a_read_only_check_releases_nothing() {
    let dev = formatted(Profile::Ext4).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let size = 5 * fs.block_size() as u64;
    let (a, _) = make_file(&mut fs, 5, 1, size, 0).await;
    chain(&mut fs, &[a]).await;
    drop(fs);
    let before = dev.to_vec();
    let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
    assert!(report.notes().next().is_none(), "{:?}", report.problems);
    assert!(dev.to_vec() == before);
}

#[tokio::test]
async fn a_filesystem_with_errors_drops_the_chain_unwalked() {
    let dev = formatted(Profile::Ext4).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let size = 5 * fs.block_size() as u64;
    let (a, _) = make_file(&mut fs, 5, 1, size, 0).await;
    chain(&mut fs, &[a]).await;
    fs.superblock_mut().state |= state::ERROR_FS;
    fs.flush_superblock().await.unwrap();
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    assert!(report.notes().any(|p| p.code == "orphan-release-error"), "{:?}", report.problems);
    assert!(!report.notes().any(|p| p.code == "orphan-released"));
    // The full check that follows frees what the orphan held.
    let fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(fs.superblock().last_orphan, 0);
    assert!(check(&dev, &FsckOptions::check_only().force(true)).await.unwrap().is_clean());
}

#[tokio::test]
async fn an_illegal_orphan_stops_the_release_and_forces_the_check() {
    let dev = formatted(Profile::Ext4).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    fs.superblock_mut().last_orphan = 3;
    fs.flush_superblock().await.unwrap();
    drop(fs);

    let report = check(&dev, &FsckOptions::preen()).await.unwrap();
    assert!(report.notes().any(|p| p.code == "orphan-release-error"), "{:?}", report.problems);
    assert!(matches!(report.scope, crate::fsck::CheckScope::Due(_)), "{:?}", report.scope);
    let fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(fs.superblock().last_orphan, 0);
}
