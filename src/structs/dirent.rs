//! Directory entries.
//!
//! Mirrors `struct ext2_dir_entry_2` and `struct ext2_dir_entry_tail` from
//! `lib/ext2fs/ext2_fs.h`. Entries are a singly-linked run through a block:
//! each carries the distance to the next, and the last stretches to the end of
//! the block (or to the checksum tail).

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use crate::bytes::*;
use crate::error::{Error, Result};

/// `EXT2_NAME_LEN`
pub const NAME_LEN_MAX: usize = 255;

/// `EXT2_DIR_ENTRY_HEADER_LEN`
pub const ENTRY_HEADER_LEN: usize = 8;

/// `EXT2_DIR_PAD` — entries are 4-byte aligned.
pub const DIR_PAD: usize = 4;

/// Length of the fake entry holding a directory block's checksum.
pub const TAIL_LEN: usize = 12;

/// `EXT2_DIR_NAME_LEN_CSUM` — the impossible name length that marks the tail.
pub const NAME_LEN_CSUM: u16 = 0xDE00;

/// File types stored in `file_type` when the `filetype` feature is on.
pub mod file_type {
    /// `EXT2_FT_UNKNOWN`
    pub const UNKNOWN: u8 = 0;
    /// `EXT2_FT_REG_FILE`
    pub const REG_FILE: u8 = 1;
    /// `EXT2_FT_DIR`
    pub const DIR: u8 = 2;
    /// `EXT2_FT_CHRDEV`
    pub const CHRDEV: u8 = 3;
    /// `EXT2_FT_BLKDEV`
    pub const BLKDEV: u8 = 4;
    /// `EXT2_FT_FIFO`
    pub const FIFO: u8 = 5;
    /// `EXT2_FT_SOCK`
    pub const SOCK: u8 = 6;
    /// `EXT2_FT_SYMLINK`
    pub const SYMLINK: u8 = 7;
}

/// `EXT2_DIR_REC_LEN` — the space an entry with this name length occupies.
pub fn rec_len(name_len: usize) -> usize {
    (name_len + ENTRY_HEADER_LEN + DIR_PAD - 1) & !(DIR_PAD - 1)
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Inode this name refers to. Zero marks the entry unused.
    pub inode: u32,
    /// Distance in bytes to the next entry in the block.
    pub rec_len: u16,
    /// File type, or [`file_type::UNKNOWN`] without the `filetype` feature.
    pub file_type: u8,
    /// The name. Not NUL-terminated on disk.
    pub name: Vec<u8>,
}

impl DirEntry {
    /// A new entry occupying exactly the space its name needs.
    pub fn new(inode: u32, name: &[u8], file_type: u8) -> Result<Self> {
        if name.len() > NAME_LEN_MAX {
            return Err(Error::invalid(format!(
                "directory name is {} bytes; the maximum is {NAME_LEN_MAX}",
                name.len()
            )));
        }
        Ok(Self {
            inode,
            rec_len: rec_len(name.len()) as u16,
            file_type,
            name: name.to_vec(),
        })
    }

    /// Decode the entry starting at `buf[0]`.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < ENTRY_HEADER_LEN {
            return Err(Error::corrupt("dir entry", "truncated header"));
        }
        let rec_len = get_u16(buf, 4);
        let name_len = get_u8(buf, 6) as usize;
        let file_type = get_u8(buf, 7);

        if (rec_len as usize) < ENTRY_HEADER_LEN || rec_len as usize > buf.len() {
            return Err(Error::corrupt(
                "dir entry",
                format!("rec_len {rec_len} does not fit in {} bytes", buf.len()),
            ));
        }
        if ENTRY_HEADER_LEN + name_len > rec_len as usize {
            return Err(Error::corrupt(
                "dir entry",
                format!("name of {name_len} bytes overflows rec_len {rec_len}"),
            ));
        }

        Ok(Self {
            inode: get_u32(buf, 0),
            rec_len,
            file_type,
            name: buf[ENTRY_HEADER_LEN..ENTRY_HEADER_LEN + name_len].to_vec(),
        })
    }

    /// Encode into `buf`, which must hold at least `rec_len` bytes.
    pub fn encode_into(&self, buf: &mut [u8]) {
        put_u32(buf, 0, self.inode);
        put_u16(buf, 4, self.rec_len);
        put_u8(buf, 6, self.name.len() as u8);
        put_u8(buf, 7, self.file_type);
        buf[ENTRY_HEADER_LEN..ENTRY_HEADER_LEN + self.name.len()].copy_from_slice(&self.name);
        // Pad to rec_len so no stale bytes leak into the gap.
        for b in &mut buf[ENTRY_HEADER_LEN + self.name.len()..self.rec_len as usize] {
            *b = 0;
        }
    }

    /// The name as a string, for display.
    pub fn name_string(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }

    /// Whether this is the fake entry carrying the block's checksum.
    pub fn is_tail(&self) -> bool {
        self.inode == 0 && self.rec_len as usize == TAIL_LEN && self.name.is_empty()
    }
}

/// Walk the entries in a directory block.
///
/// Stops at the end of the block. Entries with inode zero are returned too —
/// they are holes, and a checker needs to see them.
pub fn parse_block(block: &[u8]) -> Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + ENTRY_HEADER_LEN <= block.len() {
        let entry = DirEntry::decode(&block[at..])?;
        let step = entry.rec_len as usize;
        out.push(entry);
        if step == 0 {
            return Err(Error::corrupt("dir entry", "rec_len of zero would not advance"));
        }
        at += step;
    }
    Ok(out)
}

/// Build a directory block holding `entries`, with the last one stretched to
/// fill the block.
///
/// When `with_tail` is set, [`TAIL_LEN`] bytes are reserved at the end for the
/// checksum entry, which the caller stamps once it knows the inode number.
pub fn build_block(entries: &[DirEntry], block_size: usize, with_tail: bool) -> Result<Vec<u8>> {
    let limit = block_size - if with_tail { TAIL_LEN } else { 0 };
    let needed: usize = entries.iter().map(|e| rec_len(e.name.len())).sum();
    if needed > limit {
        return Err(Error::invalid(format!(
            "directory entries need {needed} bytes; the block has {limit}"
        )));
    }

    let mut block = vec![0u8; block_size];
    let mut at = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        let mut entry = entry.clone();
        entry.rec_len = if i == entries.len() - 1 {
            // The last entry owns the rest of the usable block.
            (limit - at) as u16
        } else {
            rec_len(entry.name.len()) as u16
        };
        entry.encode_into(&mut block[at..]);
        at += entry.rec_len as usize;
    }

    if with_tail {
        write_tail_header(&mut block[limit..limit + TAIL_LEN]);
    }
    Ok(block)
}

/// Write the fake tail entry's header, leaving the checksum zero.
pub fn write_tail_header(tail: &mut [u8]) {
    put_u32(tail, 0, 0);
    put_u16(tail, 4, TAIL_LEN as u16);
    put_u16(tail, 6, NAME_LEN_CSUM);
    put_u32(tail, 8, 0);
}

/// Stamp the checksum into a directory block's tail.
pub fn set_block_csum(block: &mut [u8], csum: u32) {
    let at = block.len() - TAIL_LEN;
    put_u32(block, at + 8, csum);
}

/// Read the checksum from a directory block's tail, if it has one.
pub fn block_csum(block: &[u8]) -> Option<u32> {
    let at = block.len().checked_sub(TAIL_LEN)?;
    if get_u32(block, at) == 0
        && get_u16(block, at + 4) == TAIL_LEN as u16
        && get_u16(block, at + 6) == NAME_LEN_CSUM
    {
        Some(get_u32(block, at + 8))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rec_len_rounds_up_to_four() {
        assert_eq!(rec_len(1), 12); // "." -> 8 + 1 -> 12
        assert_eq!(rec_len(2), 12); // ".." -> 8 + 2 -> 12
        assert_eq!(rec_len(4), 12);
        assert_eq!(rec_len(5), 16);
        assert_eq!(rec_len(10), 20);
        assert_eq!(rec_len(255), 264);
    }

    #[test]
    fn root_directory_block_round_trips() {
        let entries = vec![
            DirEntry::new(2, b".", file_type::DIR).unwrap(),
            DirEntry::new(2, b"..", file_type::DIR).unwrap(),
            DirEntry::new(11, b"lost+found", file_type::DIR).unwrap(),
        ];
        let block = build_block(&entries, 4096, false).unwrap();
        let back = parse_block(&block).unwrap();

        assert_eq!(back.len(), 3);
        assert_eq!(back[0].name_string(), ".");
        assert_eq!(back[0].rec_len, 12);
        assert_eq!(back[1].name_string(), "..");
        assert_eq!(back[1].rec_len, 12);
        assert_eq!(back[2].name_string(), "lost+found");
        // The last entry stretches to the end of the block.
        assert_eq!(back[2].rec_len as usize, 4096 - 24);
    }

    #[test]
    fn a_tail_shortens_the_last_entry_and_is_not_walked_as_a_name() {
        let entries = vec![
            DirEntry::new(2, b".", file_type::DIR).unwrap(),
            DirEntry::new(2, b"..", file_type::DIR).unwrap(),
        ];
        let mut block = build_block(&entries, 4096, true).unwrap();
        let back = parse_block(&block).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[1].rec_len as usize, 4096 - TAIL_LEN - 12);
        assert!(back[2].is_tail());

        set_block_csum(&mut block, 0xabcd_1234);
        assert_eq!(block_csum(&block), Some(0xabcd_1234));
    }

    #[test]
    fn a_block_with_no_tail_reports_none() {
        let entries = vec![DirEntry::new(2, b".", file_type::DIR).unwrap()];
        let block = build_block(&entries, 1024, false).unwrap();
        assert_eq!(block_csum(&block), None);
    }

    #[test]
    fn refuses_a_name_that_cannot_fit() {
        assert!(DirEntry::new(2, &[b'x'; 256], file_type::REG_FILE).is_err());
    }

    #[test]
    fn refuses_entries_that_overflow_the_block() {
        let many: Vec<_> = (0..100)
            .map(|i| DirEntry::new(i + 12, &[b'x'; 200], file_type::REG_FILE).unwrap())
            .collect();
        assert!(build_block(&many, 4096, false).is_err());
    }
}
