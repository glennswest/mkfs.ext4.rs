//! Diffing one filesystem against another.
//!
//! The reason this exists, in one sentence from stormblock#39: *"the diff is
//! the specification"*. When a filesystem is accepted by one implementation and
//! rejected by another, the productive question is not which feature flag to
//! guess at next — it is what, precisely, the working filesystem has that ours
//! does not. This module answers that field by field.
//!
//! Two filesystems of the same geometry differ in ways that do not matter
//! (a different UUID, a different creation time, the checksums that follow from
//! them) and ways that do (a feature flag, a block count, where the inode table
//! sits). [`Significance`] separates them, so a real difference is not lost in
//! a list of expected ones.

use crate::csum::GroupDescCsum;
use crate::device::BlockDevice;
use crate::error::Result;
use crate::features::FeatureMasks;
use crate::fs::Filesystem;
use crate::structs::inode::Inode;
use crate::structs::superblock::{ino, Superblock};

/// Whether a difference is worth acting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Significance {
    /// Expected to differ between any two filesystems: identity and timestamps,
    /// and the checksums derived from them.
    Identity,
    /// Differs, but describes use rather than layout — free counts, mount
    /// counts, the last-mounted path.
    Incidental,
    /// A real difference in layout, geometry or features.
    Structural,
}

/// Where a difference was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Area {
    /// The superblock.
    Superblock,
    /// Feature masks.
    Features,
    /// A block group descriptor.
    GroupDesc(u32),
    /// An inode.
    Inode(u32),
    /// The directory tree.
    Directory(String),
}

impl std::fmt::Display for Area {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Area::Superblock => write!(f, "superblock"),
            Area::Features => write!(f, "features"),
            Area::GroupDesc(g) => write!(f, "group {g}"),
            Area::Inode(i) => write!(f, "inode {i}"),
            Area::Directory(p) => write!(f, "directory {p}"),
        }
    }
}

/// One field that differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Where it was found.
    pub area: Area,
    /// The field's name, as it appears on disk.
    pub field: String,
    /// The left filesystem's value.
    pub left: String,
    /// The right filesystem's value.
    pub right: String,
    /// Whether it matters.
    pub significance: Significance,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} — {} vs {}",
            self.area, self.field, self.left, self.right
        )
    }
}

/// What to compare.
#[derive(Debug, Clone)]
pub struct CompareOptions {
    /// Report identity differences — UUID, timestamps, checksum seeds.
    pub include_identity: bool,
    /// Report incidental differences — free counts, mount counts.
    pub include_incidental: bool,
    /// Compare every group descriptor rather than stopping at the first few.
    pub all_groups: bool,
    /// Compare the directory tree.
    pub walk_directories: bool,
    /// Stop after this many differences. Zero means no limit.
    pub limit: usize,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            include_identity: false,
            include_incidental: false,
            all_groups: true,
            walk_directories: true,
            limit: 0,
        }
    }
}

impl CompareOptions {
    /// Report everything, including the differences expected of any two
    /// separately created filesystems.
    pub fn everything() -> Self {
        Self {
            include_identity: true,
            include_incidental: true,
            ..Default::default()
        }
    }

    /// Structural differences only — the default, and the useful one.
    pub fn structural() -> Self {
        Self::default()
    }
}

/// The result of a comparison.
#[derive(Debug, Clone, Default)]
pub struct ComparisonReport {
    /// Every difference found, in the order found.
    pub differences: Vec<Difference>,
    /// Whether the walk stopped early because of the limit.
    pub truncated: bool,
}

impl ComparisonReport {
    /// Whether the two filesystems match, for the significance asked about.
    pub fn is_identical(&self) -> bool {
        self.differences.is_empty()
    }

    /// Differences of at least this significance.
    pub fn at_least(&self, level: Significance) -> impl Iterator<Item = &Difference> {
        self.differences
            .iter()
            .filter(move |d| d.significance >= level)
    }

    /// Whether anything structural differs — the question that usually matters.
    pub fn structurally_identical(&self) -> bool {
        self.at_least(Significance::Structural).next().is_none()
    }

    fn push(
        &mut self,
        options: &CompareOptions,
        area: Area,
        field: &str,
        left: impl std::fmt::Display,
        right: impl std::fmt::Display,
        significance: Significance,
    ) {
        let wanted = match significance {
            Significance::Identity => options.include_identity,
            Significance::Incidental => options.include_incidental,
            Significance::Structural => true,
        };
        if !wanted {
            return;
        }
        if options.limit > 0 && self.differences.len() >= options.limit {
            self.truncated = true;
            return;
        }
        self.differences.push(Difference {
            area,
            field: field.to_string(),
            left: left.to_string(),
            right: right.to_string(),
            significance,
        });
    }
}

/// Compare two filesystems.
pub async fn compare<A: BlockDevice, B: BlockDevice>(
    left: &Filesystem<A>,
    right: &Filesystem<B>,
    options: &CompareOptions,
) -> Result<ComparisonReport> {
    let mut report = ComparisonReport::default();

    compare_features(left.superblock(), right.superblock(), options, &mut report);
    compare_superblock(left.superblock(), right.superblock(), options, &mut report);
    compare_group_descs(left, right, options, &mut report)?;
    compare_reserved_inodes(left, right, options, &mut report).await?;
    if options.walk_directories {
        compare_directories(left, right, options, &mut report).await?;
    }

    Ok(report)
}

/// Feature masks, name by name, so the report names the feature rather than
/// printing two hexadecimal masks and leaving the reader to decode them.
fn compare_features(
    left: &Superblock,
    right: &Superblock,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) {
    let l = FeatureMasks {
        compat: left.feature_compat,
        incompat: left.feature_incompat,
        ro_compat: left.feature_ro_compat,
    };
    let r = FeatureMasks {
        compat: right.feature_compat,
        incompat: right.feature_incompat,
        ro_compat: right.feature_ro_compat,
    };

    let left_names: Vec<String> = l.to_spec().split(',').map(str::to_string).collect();
    let right_names: Vec<String> = r.to_spec().split(',').map(str::to_string).collect();

    for name in &left_names {
        if !name.is_empty() && !right_names.contains(name) {
            report.push(
                options,
                Area::Features,
                name,
                "set",
                "not set",
                Significance::Structural,
            );
        }
    }
    for name in &right_names {
        if !name.is_empty() && !left_names.contains(name) {
            report.push(
                options,
                Area::Features,
                name,
                "not set",
                "set",
                Significance::Structural,
            );
        }
    }
}

/// Every superblock field.
fn compare_superblock(
    l: &Superblock,
    r: &Superblock,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) {
    // A small helper per significance class keeps the list below readable —
    // and the list is the point of this module, so it should read as a list.
    macro_rules! structural {
        ($field:ident) => {
            if l.$field != r.$field {
                report.push(
                    options,
                    Area::Superblock,
                    stringify!($field),
                    &l.$field,
                    &r.$field,
                    Significance::Structural,
                );
            }
        };
    }
    macro_rules! incidental {
        ($field:ident) => {
            if l.$field != r.$field {
                report.push(
                    options,
                    Area::Superblock,
                    stringify!($field),
                    &l.$field,
                    &r.$field,
                    Significance::Incidental,
                );
            }
        };
    }
    macro_rules! identity {
        ($field:ident) => {
            if l.$field != r.$field {
                report.push(
                    options,
                    Area::Superblock,
                    stringify!($field),
                    &l.$field,
                    &r.$field,
                    Significance::Identity,
                );
            }
        };
    }

    // Geometry and layout: anything here differing means the two filesystems
    // are not the same shape.
    structural!(inodes_count);
    structural!(blocks_count);
    structural!(r_blocks_count);
    structural!(first_data_block);
    structural!(log_block_size);
    structural!(log_cluster_size);
    structural!(blocks_per_group);
    structural!(clusters_per_group);
    structural!(inodes_per_group);
    structural!(first_ino);
    structural!(inode_size);
    structural!(desc_size);
    structural!(reserved_gdt_blocks);
    structural!(log_groups_per_flex);
    structural!(first_meta_bg);
    structural!(rev_level);
    structural!(minor_rev_level);
    structural!(creator_os);
    structural!(min_extra_isize);
    structural!(want_extra_isize);
    structural!(def_hash_version);
    structural!(checksum_type);
    structural!(default_mount_opts);
    structural!(errors);
    structural!(def_resuid);
    structural!(def_resgid);
    structural!(journal_inum);
    structural!(journal_dev);
    structural!(jnl_backup_type);
    structural!(lpf_ino);
    structural!(orphan_file_inum);
    structural!(usr_quota_inum);
    structural!(grp_quota_inum);
    structural!(prj_quota_inum);
    structural!(raid_stride);
    structural!(raid_stripe_width);
    structural!(encoding);
    structural!(encoding_flags);
    structural!(mmp_block);
    structural!(mmp_update_interval);
    structural!(flags);

    // How the filesystem has been used, rather than how it is laid out.
    incidental!(free_blocks_count);
    incidental!(free_inodes_count);
    incidental!(mnt_count);
    incidental!(max_mnt_count);
    incidental!(state);
    incidental!(checkinterval);
    incidental!(kbytes_written);
    incidental!(last_orphan);
    incidental!(error_count);
    incidental!(overhead_clusters);

    // Identity, and everything that follows from it.
    identity!(mkfs_time);
    identity!(mtime);
    identity!(wtime);
    identity!(lastcheck);
    identity!(checksum_seed);
    identity!(checksum);

    // Byte and word arrays, rendered as what they represent rather than as a
    // list of numbers — a UUID should read as a UUID.
    if l.uuid != r.uuid {
        report.push(
            options,
            Area::Superblock,
            "uuid",
            l.uuid_string(),
            r.uuid_string(),
            Significance::Identity,
        );
    }
    if l.volume_name != r.volume_name {
        report.push(
            options,
            Area::Superblock,
            "volume_name",
            format!("'{}'", l.label()),
            format!("'{}'", r.label()),
            Significance::Structural,
        );
    }
    if l.hash_seed != r.hash_seed {
        let render = |seed: &[u32; 4]| {
            seed.iter()
                .map(|w| format!("{w:08x}"))
                .collect::<Vec<_>>()
                .join("-")
        };
        report.push(
            options,
            Area::Superblock,
            "hash_seed",
            render(&l.hash_seed),
            render(&r.hash_seed),
            Significance::Identity,
        );
    }
    if l.jnl_blocks != r.jnl_blocks {
        report.push(
            options,
            Area::Superblock,
            "jnl_blocks",
            "journal inode backup",
            "differs",
            Significance::Identity,
        );
    }

    if l.last_mounted != r.last_mounted {
        report.push(
            options,
            Area::Superblock,
            "last_mounted",
            crate::bytes::field_to_string(&l.last_mounted),
            crate::bytes::field_to_string(&r.last_mounted),
            Significance::Incidental,
        );
    }
}

/// Group descriptors, group by group.
fn compare_group_descs<A: BlockDevice, B: BlockDevice>(
    left: &Filesystem<A>,
    right: &Filesystem<B>,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) -> Result<()> {
    let ld = left.group_descs();
    let rd = right.group_descs();

    if ld.len() != rd.len() {
        report.push(
            options,
            Area::Superblock,
            "group count",
            ld.len(),
            rd.len(),
            Significance::Structural,
        );
    }

    let groups = ld.len().min(rd.len());
    let limit = if options.all_groups { groups } else { groups.min(8) };

    for g in 0..limit {
        let (l, r) = (&ld[g], &rd[g]);
        let area = || Area::GroupDesc(g as u32);

        // Where the metadata sits is the whole question when one
        // implementation refuses a filesystem another accepts.
        if l.block_bitmap != r.block_bitmap {
            report.push(options, area(), "block_bitmap", l.block_bitmap, r.block_bitmap, Significance::Structural);
        }
        if l.inode_bitmap != r.inode_bitmap {
            report.push(options, area(), "inode_bitmap", l.inode_bitmap, r.inode_bitmap, Significance::Structural);
        }
        if l.inode_table != r.inode_table {
            report.push(options, area(), "inode_table", l.inode_table, r.inode_table, Significance::Structural);
        }
        if l.flags != r.flags {
            report.push(options, area(), "flags", format!("{:#06x}", l.flags), format!("{:#06x}", r.flags), Significance::Structural);
        }
        if l.used_dirs_count != r.used_dirs_count {
            report.push(options, area(), "used_dirs_count", l.used_dirs_count, r.used_dirs_count, Significance::Incidental);
        }
        if l.free_blocks_count != r.free_blocks_count {
            report.push(options, area(), "free_blocks_count", l.free_blocks_count, r.free_blocks_count, Significance::Incidental);
        }
        if l.free_inodes_count != r.free_inodes_count {
            report.push(options, area(), "free_inodes_count", l.free_inodes_count, r.free_inodes_count, Significance::Incidental);
        }
        if l.itable_unused != r.itable_unused {
            report.push(options, area(), "itable_unused", l.itable_unused, r.itable_unused, Significance::Incidental);
        }
        if l.checksum != r.checksum {
            report.push(options, area(), "checksum", format!("{:#06x}", l.checksum), format!("{:#06x}", r.checksum), Significance::Identity);
        }
    }

    Ok(())
}

/// The reserved inodes, which describe the filesystem rather than its contents.
async fn compare_reserved_inodes<A: BlockDevice, B: BlockDevice>(
    left: &Filesystem<A>,
    right: &Filesystem<B>,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) -> Result<()> {
    let last = left
        .superblock()
        .first_ino
        .min(right.superblock().first_ino);

    for inum in 1..=last {
        if inum > left.superblock().inodes_count || inum > right.superblock().inodes_count {
            break;
        }
        let l = left.read_inode(inum).await?;
        let r = right.read_inode(inum).await?;
        compare_inode(inum, &l, &r, options, report);
    }
    Ok(())
}

/// One inode, field by field.
fn compare_inode(
    inum: u32,
    l: &Inode,
    r: &Inode,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) {
    let area = || Area::Inode(inum);

    if l.mode != r.mode {
        report.push(options, area(), "mode", format!("{:o}", l.mode), format!("{:o}", r.mode), Significance::Structural);
    }
    if l.links_count != r.links_count {
        report.push(options, area(), "links_count", l.links_count, r.links_count, Significance::Structural);
    }
    if l.size != r.size {
        report.push(options, area(), "size", l.size, r.size, Significance::Structural);
    }
    if l.blocks != r.blocks {
        report.push(options, area(), "blocks", l.blocks, r.blocks, Significance::Structural);
    }
    if l.flags != r.flags {
        report.push(options, area(), "flags", format!("{:#010x}", l.flags), format!("{:#010x}", r.flags), Significance::Structural);
    }
    if l.uid != r.uid {
        report.push(options, area(), "uid", l.uid, r.uid, Significance::Structural);
    }
    if l.gid != r.gid {
        report.push(options, area(), "gid", l.gid, r.gid, Significance::Structural);
    }
    if l.block != r.block {
        // The block map differing is normal between two filesystems whose
        // allocators ran differently; it is structural all the same, because
        // it is where the data actually is.
        report.push(options, area(), "i_block", "…", "…", Significance::Structural);
    }
    if l.mtime != r.mtime || l.ctime != r.ctime || l.crtime != r.crtime {
        report.push(options, area(), "timestamps", l.mtime, r.mtime, Significance::Identity);
    }
}

/// The directory tree: which paths exist, and what they are.
async fn compare_directories<A: BlockDevice, B: BlockDevice>(
    left: &Filesystem<A>,
    right: &Filesystem<B>,
    options: &CompareOptions,
    report: &mut ComparisonReport,
) -> Result<()> {
    let mut queue = vec![("/".to_string(), ino::ROOT, ino::ROOT)];

    while let Some((path, l_ino, r_ino)) = queue.pop() {
        let l_inode = left.read_inode(l_ino).await?;
        let r_inode = right.read_inode(r_ino).await?;
        if !l_inode.is_dir() || !r_inode.is_dir() {
            continue;
        }

        let l_entries = left.read_dir(&l_inode).await?;
        let r_entries = right.read_dir(&r_inode).await?;

        let names = |entries: &[crate::structs::dirent::DirEntry]| -> Vec<String> {
            entries
                .iter()
                .map(|e| e.name_string())
                .filter(|n| n != "." && n != "..")
                .collect()
        };
        let l_names = names(&l_entries);
        let r_names = names(&r_entries);

        for name in &l_names {
            if !r_names.contains(name) {
                report.push(
                    options,
                    Area::Directory(path.clone()),
                    name,
                    "present",
                    "missing",
                    Significance::Structural,
                );
            }
        }
        for name in &r_names {
            if !l_names.contains(name) {
                report.push(
                    options,
                    Area::Directory(path.clone()),
                    name,
                    "missing",
                    "present",
                    Significance::Structural,
                );
            }
        }

        // Descend into the names both have.
        for name in l_names.iter().filter(|n| r_names.contains(n)) {
            let l_child = l_entries.iter().find(|e| &e.name_string() == name);
            let r_child = r_entries.iter().find(|e| &e.name_string() == name);
            if let (Some(l_child), Some(r_child)) = (l_child, r_child) {
                let child_path = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                let l_target = left.read_inode(l_child.inode).await?;
                let r_target = right.read_inode(r_child.inode).await?;
                if l_target.is_dir() != r_target.is_dir() {
                    report.push(
                        options,
                        Area::Directory(path.clone()),
                        name,
                        if l_target.is_dir() { "directory" } else { "file" },
                        if r_target.is_dir() { "directory" } else { "file" },
                        Significance::Structural,
                    );
                } else if l_target.is_dir() {
                    queue.push((child_path, l_child.inode, r_child.inode));
                }
            }
        }
    }

    Ok(())
}

/// A one-line summary of what the two filesystems are, for a report header.
pub fn describe<D: BlockDevice>(fs: &Filesystem<D>) -> String {
    let sb = fs.superblock();
    let kind = if sb.feature_incompat.contains(crate::IncompatFeatures::EXTENTS) {
        "ext4"
    } else if sb
        .feature_compat
        .contains(crate::CompatFeatures::HAS_JOURNAL)
    {
        "ext3"
    } else {
        "ext2"
    };
    format!(
        "{kind}, {} blocks of {} bytes, {} inodes, {} groups, csum {}",
        sb.blocks_count,
        sb.block_size(),
        sb.inodes_count,
        sb.group_count(),
        match fs.csum_scheme() {
            GroupDescCsum::None => "none",
            GroupDescCsum::Crc16 => "crc16",
            GroupDescCsum::Crc32c => "crc32c",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::format::format;
    use crate::params::{Params, Profile};

    const MIB: u64 = 1024 * 1024;

    async fn made(profile: Profile, size: u64, uuid: &[u8; 16]) -> MemDevice {
        let dev = MemDevice::new(size);
        let params = Params::new(profile).uuid(*uuid).mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();
        dev
    }

    #[tokio::test]
    async fn a_filesystem_is_identical_to_itself() {
        let dev = made(Profile::Ext4, 16 * MIB, b"0123456789abcdef").await;
        let a = Filesystem::open(&dev).await.unwrap();
        let b = Filesystem::open(&dev).await.unwrap();

        let report = compare(&a, &b, &CompareOptions::everything()).await.unwrap();
        assert!(
            report.is_identical(),
            "differences against itself: {:?}",
            report.differences
        );
    }

    /// Two filesystems made the same way differ only in identity — which is
    /// exactly what the significance split exists to say.
    #[tokio::test]
    async fn same_parameters_differ_only_by_identity() {
        let left = made(Profile::Ext4, 16 * MIB, b"0123456789abcdef").await;
        let right = made(Profile::Ext4, 16 * MIB, b"fedcba9876543210").await;
        let a = Filesystem::open(&left).await.unwrap();
        let b = Filesystem::open(&right).await.unwrap();

        let structural = compare(&a, &b, &CompareOptions::structural()).await.unwrap();
        assert!(
            structural.is_identical(),
            "unexpected structural differences: {:?}",
            structural
                .differences
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
        );

        // But the UUID and everything derived from it does differ.
        let everything = compare(&a, &b, &CompareOptions::everything()).await.unwrap();
        assert!(!everything.is_identical());
        assert!(everything
            .differences
            .iter()
            .any(|d| d.field == "uuid" && d.significance == Significance::Identity));
        assert!(everything
            .differences
            .iter()
            .any(|d| d.field == "checksum_seed"));
    }

    #[tokio::test]
    async fn a_different_block_size_is_structural() {
        let left = made(Profile::Ext4, 64 * MIB, b"0123456789abcdef").await;

        let right = MemDevice::new(64 * MIB);
        let params = Params::new(Profile::Ext4)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000)
            .block_size(4096);
        format(&right, &params).await.unwrap();

        let a = Filesystem::open(&left).await.unwrap();
        let b = Filesystem::open(&right).await.unwrap();
        let report = compare(&a, &b, &CompareOptions::structural()).await.unwrap();

        assert!(!report.structurally_identical());
        let fields: Vec<&str> = report.differences.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"log_block_size"), "{fields:?}");
        assert!(fields.contains(&"blocks_count"), "{fields:?}");
    }

    #[tokio::test]
    async fn a_missing_feature_is_named_not_hex() {
        let left = made(Profile::Ext4, 16 * MIB, b"0123456789abcdef").await;

        let right = MemDevice::new(16 * MIB);
        let params = Params::new(Profile::Ext4)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000)
            .no_journal();
        format(&right, &params).await.unwrap();

        let a = Filesystem::open(&left).await.unwrap();
        let b = Filesystem::open(&right).await.unwrap();
        let report = compare(&a, &b, &CompareOptions::structural()).await.unwrap();

        let named: Vec<&Difference> = report
            .differences
            .iter()
            .filter(|d| d.area == Area::Features)
            .collect();
        assert!(
            named.iter().any(|d| d.field == "has_journal"),
            "expected has_journal to be named: {named:?}"
        );
        assert!(named.iter().any(|d| d.field == "orphan_file"));
    }

    #[tokio::test]
    async fn ext2_and_ext4_differ_in_every_way_that_matters() {
        let left = made(Profile::Ext2, 16 * MIB, b"0123456789abcdef").await;
        let right = made(Profile::Ext4, 16 * MIB, b"0123456789abcdef").await;
        let a = Filesystem::open(&left).await.unwrap();
        let b = Filesystem::open(&right).await.unwrap();

        let report = compare(&a, &b, &CompareOptions::structural()).await.unwrap();
        let fields: Vec<&str> = report.differences.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"extent"));
        assert!(fields.contains(&"metadata_csum"));
        assert!(fields.contains(&"desc_size"), "{fields:?}");
    }

    #[tokio::test]
    async fn the_limit_stops_the_walk() {
        let left = made(Profile::Ext2, 64 * MIB, b"0123456789abcdef").await;
        let right = made(Profile::Ext4, 64 * MIB, b"0123456789abcdef").await;
        let a = Filesystem::open(&left).await.unwrap();
        let b = Filesystem::open(&right).await.unwrap();

        let options = CompareOptions {
            limit: 3,
            ..CompareOptions::everything()
        };
        let report = compare(&a, &b, &options).await.unwrap();
        assert_eq!(report.differences.len(), 3);
        assert!(report.truncated);
    }

    #[tokio::test]
    async fn describes_what_it_is_looking_at() {
        let dev = made(Profile::Ext4, 16 * MIB, b"0123456789abcdef").await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let text = describe(&fs);
        assert!(text.starts_with("ext4,"), "{text}");
        assert!(text.contains("crc32c"), "{text}");
    }
}
