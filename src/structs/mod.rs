//! On-disk structures, byte for byte.
//!
//! Every type here decodes from and encodes to the exact layout in
//! `lib/ext2fs/ext2_fs.h` and `lib/ext2fs/ext3_extents.h`. Structures are
//! encoded field by field rather than by casting, so the code is free of
//! `unsafe` and correct on a big-endian host.
//!
//! Decoding is lossless: a structure read from disk and written back is
//! byte-identical, including fields this crate does not otherwise use. That is
//! what allows [`crate::compare`] to diff a reference filesystem against ours
//! without silently dropping whatever it did not expect.

pub mod dirent;
pub mod extent;
pub mod group_desc;
pub mod inode;
pub mod superblock;
pub mod xattr;

pub use dirent::DirEntry;
pub use extent::{Extent, ExtentHeader, ExtentIdx};
pub use group_desc::GroupDesc;
pub use inode::Inode;
pub use superblock::{Superblock, SUPERBLOCK_LEN, SUPERBLOCK_OFFSET};
pub use xattr::Xattr;
