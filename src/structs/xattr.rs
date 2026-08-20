//! Extended attributes.
//!
//! Mirrors `lib/ext2fs/ext2_ext_attr.h` and the writer in
//! `lib/ext2fs/ext_attr.c`. These are what carry SELinux labels
//! (`security.selinux`) and POSIX ACLs (`system.posix_acl_access`), so an image
//! destined for a RHEL host or a container runtime needs them.
//!
//! Attributes live in two places. Small ones sit **inside the inode**, in the
//! space between the end of the declared extra fields and the end of the inode
//! — a 256-byte inode with 32 bytes of extras has 92 bytes going spare, which
//! is enough for a typical SELinux label. Larger sets spill into a block of
//! their own, pointed at by `i_file_acl`.
//!
//! This module implements the in-inode form, which is where the attributes an
//! image builder sets almost always fit.

#[cfg(not(feature = "std"))]
use alloc::{string::String, string::ToString, vec::Vec};

use crate::bytes::*;
use crate::error::{Error, Result};

/// `EXT2_EXT_ATTR_MAGIC`
pub const XATTR_MAGIC: u32 = 0xEA02_0000;

/// Size of one entry header, before the name.
pub const ENTRY_LEN: usize = 16;

/// Entries and values are padded to four bytes.
pub const PAD: usize = 4;

/// Field offsets within an entry.
#[allow(missing_docs)]
pub mod off {
    pub const E_NAME_LEN: usize = 0x0;
    pub const E_NAME_INDEX: usize = 0x1;
    pub const E_VALUE_OFFS: usize = 0x2;
    pub const E_VALUE_INUM: usize = 0x4;
    pub const E_VALUE_SIZE: usize = 0x8;
    pub const E_HASH: usize = 0xc;
}

/// The name prefixes ext4 stores as a single byte instead of as text.
///
/// Ordered by decreasing specificity, as in `ext_attr.c` — `system.posix_acl_access`
/// must be matched before the shorter `system.` prefix.
pub const PREFIXES: &[(u8, &str)] = &[
    (10, "gnu."),
    (3, "system.posix_acl_default"),
    (2, "system.posix_acl_access"),
    (8, "system.richacl"),
    (6, "security."),
    (4, "trusted."),
    (7, "system."),
    (1, "user."),
];

/// Split a full attribute name into its prefix index and remainder.
pub fn split_name(name: &str) -> (u8, &str) {
    for &(index, prefix) in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            // The two ACL names are whole names, not prefixes: nothing follows.
            if (index == 2 || index == 3 || index == 8) && !rest.is_empty() {
                continue;
            }
            return (index, rest);
        }
    }
    (0, name)
}

/// Rebuild a full name from its prefix index and remainder.
pub fn join_name(index: u8, rest: &str) -> String {
    match PREFIXES.iter().find(|(i, _)| *i == index) {
        Some((_, prefix)) => format!("{prefix}{rest}"),
        None => rest.to_string(),
    }
}

/// One extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    /// The full name, such as `security.selinux`.
    pub name: String,
    /// The value, which is bytes rather than text.
    pub value: Vec<u8>,
}

impl Xattr {
    /// A new attribute.
    pub fn new(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Bytes this attribute occupies: its entry, padded name, and padded value.
    pub fn stored_len(&self) -> usize {
        let (_, rest) = split_name(&self.name);
        round_up(ENTRY_LEN + rest.len()) + round_up(self.value.len())
    }
}

fn round_up(n: usize) -> usize {
    (n + PAD - 1) & !(PAD - 1)
}

/// Where an inode's in-inode attribute area starts, and how long it is.
///
/// Returns `None` when the inode is too small to carry any — a 128-byte inode
/// has no extra area at all.
pub fn inode_area(inode_size: usize, extra_isize: u16) -> Option<(usize, usize)> {
    let base = 128 + extra_isize as usize;
    // Four bytes of magic, then the entries.
    if inode_size <= base + 4 {
        return None;
    }
    Some((base + 4, inode_size - base - 4))
}

/// Read the attributes stored inside an inode.
///
/// An inode with no attribute magic simply has none, which is not an error.
pub fn read_inode_xattrs(raw: &[u8], inode_size: usize, extra_isize: u16) -> Result<Vec<Xattr>> {
    let Some((start, len)) = inode_area(inode_size, extra_isize) else {
        return Ok(Vec::new());
    };
    if raw.len() < start + len {
        return Ok(Vec::new());
    }
    if get_u32(raw, 128 + extra_isize as usize) != XATTR_MAGIC {
        return Ok(Vec::new());
    }
    parse_entries(&raw[start..start + len], 0)
}

/// Walk an entry region. `value_base` is what `e_value_offs` is measured from,
/// relative to the start of that region — zero inside an inode, and the header
/// length inside a block.
fn parse_entries(area: &[u8], value_base: usize) -> Result<Vec<Xattr>> {
    let mut out = Vec::new();
    let mut at = 0usize;

    while at + 4 <= area.len() {
        // A zero word terminates the list.
        if get_u32(area, at) == 0 {
            break;
        }
        if at + ENTRY_LEN > area.len() {
            return Err(Error::corrupt("xattr entry", "truncated entry header"));
        }

        let name_len = get_u8(area, at + off::E_NAME_LEN) as usize;
        let name_index = get_u8(area, at + off::E_NAME_INDEX);
        let value_offs = get_u16(area, at + off::E_VALUE_OFFS) as usize;
        let value_inum = get_u32(area, at + off::E_VALUE_INUM);
        let value_size = get_u32(area, at + off::E_VALUE_SIZE) as usize;

        if value_inum != 0 {
            return Err(Error::UnsupportedFeature(
                "an attribute whose value lives in its own inode (ea_inode)".into(),
            ));
        }
        if at + ENTRY_LEN + name_len > area.len() {
            return Err(Error::corrupt("xattr entry", "name runs past the area"));
        }
        let name_rest =
            String::from_utf8_lossy(&area[at + ENTRY_LEN..at + ENTRY_LEN + name_len]).into_owned();

        let value = if value_size == 0 {
            Vec::new()
        } else {
            let from = value_offs
                .checked_sub(value_base)
                .ok_or_else(|| Error::corrupt("xattr entry", "value offset before the area"))?;
            if from + value_size > area.len() {
                return Err(Error::corrupt("xattr entry", "value runs past the area"));
            }
            area[from..from + value_size].to_vec()
        };

        out.push(Xattr {
            name: join_name(name_index, &name_rest),
            value,
        });

        at += round_up(ENTRY_LEN + name_len);
    }

    Ok(out)
}

/// Write attributes into an inode's spare space.
///
/// Entries grow from the start of the area and values from the end, meeting in
/// the middle — the layout `write_xattrs_to_buffer()` produces. Returns an
/// error when they would meet, so the caller can decide to spill to a block.
pub fn write_inode_xattrs(
    raw: &mut [u8],
    inode_size: usize,
    extra_isize: u16,
    attrs: &[Xattr],
) -> Result<()> {
    let Some((start, len)) = inode_area(inode_size, extra_isize) else {
        if attrs.is_empty() {
            return Ok(());
        }
        return Err(Error::invalid(
            "this inode has no room for extended attributes; a larger inode size is needed",
        ));
    };

    // Clear the whole area, magic included, so removing the last attribute
    // really removes it.
    for b in &mut raw[128 + extra_isize as usize..inode_size] {
        *b = 0;
    }
    if attrs.is_empty() {
        return Ok(());
    }

    let needed: usize = attrs.iter().map(|a| a.stored_len()).sum();
    // Plus the terminating zero word.
    if needed + 4 > len {
        return Err(Error::invalid(format!(
            "{} bytes of extended attributes do not fit in the {len} bytes an inode of \
             {inode_size} bytes has spare; a larger inode size or an attribute block is needed",
            needed + 4
        )));
    }

    put_u32(raw, 128 + extra_isize as usize, XATTR_MAGIC);

    // Sorted the way e2fsprogs sorts them, so two writers of the same set
    // produce the same bytes.
    let mut sorted: Vec<&Xattr> = attrs.iter().collect();
    sorted.sort_by(|a, b| {
        let (ai, ar) = split_name(&a.name);
        let (bi, br) = split_name(&b.name);
        ai.cmp(&bi).then(ar.len().cmp(&br.len())).then(ar.cmp(br))
    });

    let area = &mut raw[start..start + len];
    let mut entry_at = 0usize;
    let mut value_end = len;

    for attr in sorted {
        let (index, rest) = split_name(&attr.name);
        let name = rest.as_bytes();

        let value_len = round_up(attr.value.len());
        value_end -= value_len;

        put_u8(area, entry_at + off::E_NAME_LEN, name.len() as u8);
        put_u8(area, entry_at + off::E_NAME_INDEX, index);
        put_u16(
            area,
            entry_at + off::E_VALUE_OFFS,
            if attr.value.is_empty() {
                0
            } else {
                value_end as u16
            },
        );
        put_u32(area, entry_at + off::E_VALUE_INUM, 0);
        put_u32(area, entry_at + off::E_VALUE_SIZE, attr.value.len() as u32);
        // The hash is only meaningful for attributes in a shared block.
        put_u32(area, entry_at + off::E_HASH, 0);
        area[entry_at + ENTRY_LEN..entry_at + ENTRY_LEN + name.len()].copy_from_slice(name);

        if !attr.value.is_empty() {
            area[value_end..value_end + attr.value.len()].copy_from_slice(&attr.value);
        }

        entry_at += round_up(ENTRY_LEN + name.len());
    }

    // The terminator is already zero from the clear above.
    Ok(())
}

/// `NAME_HASH_SHIFT`
const NAME_HASH_SHIFT: u32 = 5;

/// `VALUE_HASH_SHIFT`
const VALUE_HASH_SHIFT: u32 = 16;

/// The hash e2fsck expects on an attribute stored in a block.
///
/// `ext2fs_ext_attr_hash_entry()`. Entries inside an inode leave this zero, but
/// a zero hash on a block entry is a hard error — "Extended attribute in inode
/// N has a hash (0) which is invalid" — because the hash is what lets two
/// inodes share one attribute block.
pub fn hash_entry(name: &[u8], value: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &b in name {
        hash = (hash << NAME_HASH_SHIFT)
            ^ (hash >> (32 - NAME_HASH_SHIFT))
            ^ b as u32;
    }

    if !value.is_empty() {
        // Over the value padded out to a whole number of words, little-endian.
        let padded = round_up(value.len());
        let mut word = [0u8; 4];
        for chunk in 0..padded / 4 {
            for (i, slot) in word.iter_mut().enumerate() {
                *slot = value.get(chunk * 4 + i).copied().unwrap_or(0);
            }
            hash = (hash << VALUE_HASH_SHIFT)
                ^ (hash >> (32 - VALUE_HASH_SHIFT))
                ^ u32::from_le_bytes(word);
        }
    }
    hash
}

/// Size of `struct ext2_ext_attr_header`, at the start of an attribute block.
pub const BLOCK_HEADER_LEN: usize = 32;

/// Field offsets within the block header.
#[allow(missing_docs)]
pub mod block_off {
    pub const H_MAGIC: usize = 0x00;
    pub const H_REFCOUNT: usize = 0x04;
    pub const H_BLOCKS: usize = 0x08;
    pub const H_HASH: usize = 0x0c;
    pub const H_CHECKSUM: usize = 0x10;
}

/// Read the attributes stored in a dedicated block.
pub fn read_block_xattrs(block: &[u8]) -> Result<Vec<Xattr>> {
    if block.len() < BLOCK_HEADER_LEN {
        return Err(Error::corrupt("xattr block", "shorter than its header"));
    }
    if get_u32(block, block_off::H_MAGIC) != XATTR_MAGIC {
        return Err(Error::corrupt(
            "xattr block",
            format!(
                "magic {:#010x}, expected {XATTR_MAGIC:#010x}",
                get_u32(block, block_off::H_MAGIC)
            ),
        ));
    }
    // Inside a block, `e_value_offs` is measured from the start of the block,
    // so the entry region's base is the header length.
    parse_entries(&block[BLOCK_HEADER_LEN..], BLOCK_HEADER_LEN)
}

/// Build an attribute block.
///
/// The checksum is stamped by [`stamp_block_csum`] once the caller knows which
/// block it will live in — it covers the block number, so it cannot be computed
/// before the block is allocated.
pub fn write_block_xattrs(block_size: usize, attrs: &[Xattr]) -> Result<Vec<u8>> {
    let mut block = vec![0u8; block_size];
    let len = block_size - BLOCK_HEADER_LEN;

    let needed: usize = attrs.iter().map(|a| a.stored_len()).sum();
    if needed + 4 > len {
        return Err(Error::invalid(format!(
            "{} bytes of extended attributes do not fit in a {block_size}-byte block",
            needed + 4
        )));
    }

    put_u32(&mut block, block_off::H_MAGIC, XATTR_MAGIC);
    put_u32(&mut block, block_off::H_REFCOUNT, 1);
    put_u32(&mut block, block_off::H_BLOCKS, 1);

    let mut sorted: Vec<&Xattr> = attrs.iter().collect();
    sorted.sort_by(|a, b| {
        let (ai, ar) = split_name(&a.name);
        let (bi, br) = split_name(&b.name);
        ai.cmp(&bi).then(ar.len().cmp(&br.len())).then(ar.cmp(br))
    });

    let area = &mut block[BLOCK_HEADER_LEN..];
    let mut entry_at = 0usize;
    let mut value_end = len;

    for attr in sorted {
        let (index, rest) = split_name(&attr.name);
        let name = rest.as_bytes();
        value_end -= round_up(attr.value.len());

        put_u8(area, entry_at + off::E_NAME_LEN, name.len() as u8);
        put_u8(area, entry_at + off::E_NAME_INDEX, index);
        put_u16(
            area,
            entry_at + off::E_VALUE_OFFS,
            if attr.value.is_empty() {
                0
            } else {
                // Measured from the start of the block, not the entry area.
                (value_end + BLOCK_HEADER_LEN) as u16
            },
        );
        put_u32(area, entry_at + off::E_VALUE_INUM, 0);
        put_u32(area, entry_at + off::E_VALUE_SIZE, attr.value.len() as u32);
        // Unlike an in-inode entry, a block entry must carry a real hash.
        put_u32(
            area,
            entry_at + off::E_HASH,
            hash_entry(name, &attr.value),
        );
        area[entry_at + ENTRY_LEN..entry_at + ENTRY_LEN + name.len()].copy_from_slice(name);
        if !attr.value.is_empty() {
            area[value_end..value_end + attr.value.len()].copy_from_slice(&attr.value);
        }
        entry_at += round_up(ENTRY_LEN + name.len());
    }

    Ok(block)
}

/// Stamp an attribute block's checksum: `crc32c(seed, block number)` then the
/// whole block with the checksum field zeroed.
pub fn stamp_block_csum(block: &mut [u8], seed: u32, block_number: u64) {
    put_u32(block, block_off::H_CHECKSUM, 0);
    let mut crc = crate::csum::crc32c(seed, &block_number.to_le_bytes());
    crc = crate::csum::crc32c(crc, block);
    put_u32(block, block_off::H_CHECKSUM, crc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_rejoins_known_prefixes() {
        assert_eq!(split_name("security.selinux"), (6, "selinux"));
        assert_eq!(split_name("user.comment"), (1, "comment"));
        assert_eq!(split_name("trusted.overlay.opaque"), (4, "overlay.opaque"));
        assert_eq!(split_name("system.posix_acl_access"), (2, ""));
        assert_eq!(split_name("nonstandard"), (0, "nonstandard"));

        assert_eq!(join_name(6, "selinux"), "security.selinux");
        assert_eq!(join_name(2, ""), "system.posix_acl_access");
        assert_eq!(join_name(0, "nonstandard"), "nonstandard");
    }

    /// `system.posix_acl_access` is a whole name, so it must not be matched by
    /// the shorter `system.` prefix and lose its tail.
    #[test]
    fn the_acl_names_are_not_treated_as_prefixes() {
        assert_eq!(split_name("system.posix_acl_default"), (3, ""));
        assert_eq!(split_name("system.other"), (7, "other"));
    }

    #[test]
    fn round_trips_through_an_inode() {
        let mut raw = vec![0u8; 256];
        let attrs = vec![
            Xattr::new("security.selinux", b"system_u:object_r:etc_t:s0\0".to_vec()),
            Xattr::new("user.comment", b"hello".to_vec()),
        ];
        write_inode_xattrs(&mut raw, 256, 32, &attrs).unwrap();

        let back = read_inode_xattrs(&raw, 256, 32).unwrap();
        assert_eq!(back.len(), 2);
        let selinux = back.iter().find(|a| a.name == "security.selinux").unwrap();
        assert_eq!(selinux.value, b"system_u:object_r:etc_t:s0\0");
        let comment = back.iter().find(|a| a.name == "user.comment").unwrap();
        assert_eq!(comment.value, b"hello");
    }

    #[test]
    fn an_inode_with_no_attributes_reads_as_empty() {
        let raw = vec![0u8; 256];
        assert!(read_inode_xattrs(&raw, 256, 32).unwrap().is_empty());
    }

    #[test]
    fn clearing_removes_the_magic_too() {
        let mut raw = vec![0u8; 256];
        write_inode_xattrs(&mut raw, 256, 32, &[Xattr::new("user.a", b"1".to_vec())]).unwrap();
        assert_eq!(get_u32(&raw, 128 + 32), XATTR_MAGIC);

        write_inode_xattrs(&mut raw, 256, 32, &[]).unwrap();
        assert_ne!(get_u32(&raw, 128 + 32), XATTR_MAGIC);
        assert!(read_inode_xattrs(&raw, 256, 32).unwrap().is_empty());
    }

    #[test]
    fn too_much_is_refused_with_a_useful_message() {
        let mut raw = vec![0u8; 256];
        let big = vec![Xattr::new("user.big", vec![0u8; 200])];
        let err = write_inode_xattrs(&mut raw, 256, 32, &big).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("do not fit"), "{text}");
    }

    #[test]
    fn a_128_byte_inode_has_nowhere_to_put_them() {
        assert!(inode_area(128, 0).is_none());
        let mut raw = vec![0u8; 128];
        assert!(write_inode_xattrs(&mut raw, 128, 0, &[]).is_ok());
        assert!(write_inode_xattrs(&mut raw, 128, 0, &[Xattr::new("user.a", b"1".to_vec())]).is_err());
    }

    #[test]
    fn round_trips_through_a_block() {
        let attrs = vec![
            Xattr::new("security.selinux", b"system_u:object_r:passwd_file_t:s0\0".to_vec()),
            Xattr::new("user.origin", b"built-in-userspace".to_vec()),
            Xattr::new("system.posix_acl_access", vec![2u8; 28]),
        ];
        let mut block = write_block_xattrs(4096, &attrs).unwrap();
        stamp_block_csum(&mut block, 0xdead_beef, 1234);

        let back = read_block_xattrs(&block).unwrap();
        assert_eq!(back.len(), 3);
        for want in &attrs {
            let got = back.iter().find(|a| a.name == want.name).unwrap();
            assert_eq!(got.value, want.value, "{}", want.name);
        }
        assert_eq!(get_u32(&block, block_off::H_REFCOUNT), 1);
        assert_ne!(get_u32(&block, block_off::H_CHECKSUM), 0);
    }

    #[test]
    fn a_zero_length_value_is_kept() {
        let mut raw = vec![0u8; 256];
        write_inode_xattrs(&mut raw, 256, 32, &[Xattr::new("user.flag", Vec::new())]).unwrap();
        let back = read_inode_xattrs(&raw, 256, 32).unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].value.is_empty());
    }
}
