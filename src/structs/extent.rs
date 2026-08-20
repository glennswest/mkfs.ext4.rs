//! Extent trees.
//!
//! Mirrors `lib/ext2fs/ext3_extents.h`. An extent-mapped inode stores a
//! [`ExtentHeader`] followed by entries in the 60 bytes of `i_block`; deeper
//! trees put the same header and entries in whole blocks, with a four-byte
//! checksum tail when `metadata_csum` is on.

#[cfg(not(feature = "std"))]
use alloc::{vec::Vec};

use crate::bytes::*;
use crate::error::{Error, Result};

/// `EXT3_EXT_MAGIC`
pub const EXTENT_MAGIC: u16 = 0xf30a;

/// Size of `struct ext3_extent_header`.
pub const HEADER_LEN: usize = 12;

/// Size of `struct ext3_extent` and of `struct ext3_extent_idx` — both 12.
pub const ENTRY_LEN: usize = 12;

/// Size of `struct ext3_extent_tail`.
pub const TAIL_LEN: usize = 4;

/// `EXT_INIT_MAX_LEN` — the longest initialised extent.
pub const INIT_MAX_LEN: u32 = 1 << 15;

/// The 60 bytes of `i_block` available for an inline extent tree.
pub const INLINE_LEN: usize = 60;

/// Header at the start of every extent node, inline or in a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtentHeader {
    /// Entries in use.
    pub entries: u16,
    /// Entries this node can hold.
    pub max: u16,
    /// Tree depth. Zero means the entries are leaf extents rather than indices.
    pub depth: u16,
    /// Tree generation.
    pub generation: u32,
}

impl ExtentHeader {
    /// Decode a header.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let magic = get_u16(buf, 0);
        if magic != EXTENT_MAGIC {
            return Err(Error::corrupt(
                "extent header",
                format!("magic {magic:#06x}, expected {EXTENT_MAGIC:#06x}"),
            ));
        }
        Ok(Self {
            entries: get_u16(buf, 2),
            max: get_u16(buf, 4),
            depth: get_u16(buf, 6),
            generation: get_u32(buf, 8),
        })
    }

    /// Encode a header.
    pub fn encode_into(&self, buf: &mut [u8]) {
        put_u16(buf, 0, EXTENT_MAGIC);
        put_u16(buf, 2, self.entries);
        put_u16(buf, 4, self.max);
        put_u16(buf, 6, self.depth);
        put_u32(buf, 8, self.generation);
    }

    /// How many entries fit in a node of `space` bytes, reserving the checksum
    /// tail when the filesystem carries one.
    pub fn max_entries(space: usize, with_tail: bool) -> u16 {
        let usable = space
            .saturating_sub(HEADER_LEN)
            .saturating_sub(if with_tail { TAIL_LEN } else { 0 });
        (usable / ENTRY_LEN) as u16
    }
}

/// Where a node's checksum tail sits: `EXT4_EXTENT_TAIL_OFFSET` in
/// `fs/ext4/ext4_extents.h`.
///
/// Not the end of the block. The tail follows the last entry `max` accounts
/// for, and the checksum covers only the bytes before it. Those agree only
/// when the space after the header divides into entries with exactly
/// `TAIL_LEN` spare — true at 1 KiB and 4 KiB blocks, false at 2 KiB, 8 KiB
/// and 32 KiB, where the end of the block is four bytes further out and no
/// other reader looks there.
pub fn tail_offset(max: u16) -> usize {
    HEADER_LEN + max as usize * ENTRY_LEN
}

/// A leaf extent: a run of contiguous physical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Extent {
    /// First logical block this extent covers.
    pub block: u32,
    /// Length in blocks. Lengths above [`INIT_MAX_LEN`] mark the extent
    /// uninitialised (preallocated but never written).
    pub len: u16,
    /// First physical block, 48 bits.
    pub start: u64,
}

impl Extent {
    /// Decode a leaf extent.
    pub fn decode(buf: &[u8]) -> Self {
        Self {
            block: get_u32(buf, 0),
            len: get_u16(buf, 4),
            start: get_u32(buf, 8) as u64 | ((get_u16(buf, 6) as u64) << 32),
        }
    }

    /// Encode a leaf extent.
    pub fn encode_into(&self, buf: &mut [u8]) {
        put_u32(buf, 0, self.block);
        put_u16(buf, 4, self.len);
        put_u16(buf, 6, (self.start >> 32) as u16);
        put_u32(buf, 8, self.start as u32);
    }

    /// Blocks actually mapped, with the uninitialised flag removed.
    pub fn effective_len(&self) -> u32 {
        if self.len as u32 > INIT_MAX_LEN {
            self.len as u32 - INIT_MAX_LEN
        } else {
            self.len as u32
        }
    }

    /// Whether this extent is preallocated but not yet written.
    pub fn is_uninit(&self) -> bool {
        self.len as u32 > INIT_MAX_LEN
    }
}

/// An interior index entry pointing at a lower node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtentIdx {
    /// First logical block covered below this index.
    pub block: u32,
    /// Physical block of the child node, 48 bits.
    pub leaf: u64,
}

impl ExtentIdx {
    /// Decode an index entry.
    pub fn decode(buf: &[u8]) -> Self {
        Self {
            block: get_u32(buf, 0),
            leaf: get_u32(buf, 4) as u64 | ((get_u16(buf, 8) as u64) << 32),
        }
    }

    /// Encode an index entry.
    pub fn encode_into(&self, buf: &mut [u8]) {
        put_u32(buf, 0, self.block);
        put_u32(buf, 4, self.leaf as u32);
        put_u16(buf, 8, (self.leaf >> 32) as u16);
        put_u16(buf, 10, 0);
    }
}

/// Build a depth-zero extent tree inline in `i_block`.
///
/// Returns the 60 bytes to place in the inode. Fails if the extents do not fit,
/// which for an inline node means more than four of them.
pub fn build_inline(extents: &[Extent]) -> Result<[u8; INLINE_LEN]> {
    let max = ExtentHeader::max_entries(INLINE_LEN, false);
    if extents.len() > max as usize {
        return Err(Error::invalid(format!(
            "{} extents do not fit inline; at most {max} do",
            extents.len()
        )));
    }

    let mut out = [0u8; INLINE_LEN];
    let header = ExtentHeader {
        entries: extents.len() as u16,
        max,
        depth: 0,
        generation: 0,
    };
    header.encode_into(&mut out);
    for (i, ext) in extents.iter().enumerate() {
        let at = HEADER_LEN + i * ENTRY_LEN;
        ext.encode_into(&mut out[at..at + ENTRY_LEN]);
    }
    Ok(out)
}

/// Read the leaf extents of an inline, depth-zero tree.
pub fn read_inline(i_block: &[u8]) -> Result<Vec<Extent>> {
    let header = ExtentHeader::decode(i_block)?;
    if header.depth != 0 {
        return Err(Error::corrupt(
            "extent header",
            format!("expected an inline depth-0 node, found depth {}", header.depth),
        ));
    }
    let max = ExtentHeader::max_entries(INLINE_LEN, false);
    if header.entries > max {
        return Err(Error::corrupt(
            "extent header",
            format!("{} entries claimed, only {max} fit inline", header.entries),
        ));
    }
    Ok((0..header.entries as usize)
        .map(|i| {
            let at = HEADER_LEN + i * ENTRY_LEN;
            Extent::decode(&i_block[at..at + ENTRY_LEN])
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_tree_round_trips() {
        let extents = vec![
            Extent {
                block: 0,
                len: 1,
                start: 1234,
            },
            Extent {
                block: 1,
                len: 8,
                start: 0x1_0000_5678,
            },
        ];
        let i_block = build_inline(&extents).unwrap();
        assert_eq!(read_inline(&i_block).unwrap(), extents);
    }

    #[test]
    fn four_extents_fit_inline_and_five_do_not() {
        assert_eq!(ExtentHeader::max_entries(INLINE_LEN, false), 4);
        let one = Extent {
            block: 0,
            len: 1,
            start: 1,
        };
        assert!(build_inline(&vec![one; 4]).is_ok());
        assert!(build_inline(&vec![one; 5]).is_err());
    }

    #[test]
    fn a_block_node_reserves_space_for_its_tail() {
        // 4096-byte block: 12-byte header, 4-byte tail, 340 entries of 12.
        assert_eq!(ExtentHeader::max_entries(4096, true), 340);
        assert_eq!(ExtentHeader::max_entries(4096, false), 340);
    }

    #[test]
    fn uninitialised_extents_report_their_real_length() {
        let ext = Extent {
            block: 0,
            len: (INIT_MAX_LEN + 16) as u16,
            start: 100,
        };
        assert!(ext.is_uninit());
        assert_eq!(ext.effective_len(), 16);
    }

    #[test]
    fn rejects_a_node_with_no_magic() {
        assert!(ExtentHeader::decode(&[0u8; HEADER_LEN]).is_err());
    }
}
