//! Async, parallel **ext2 / ext3 / ext4** formatter and checker in pure Rust.
//!
//! A from-scratch reimplementation of `mke2fs` and `e2fsck`, written against the
//! [e2fsprogs](https://github.com/tytso/e2fsprogs) source as the reference for
//! every on-disk field, default and geometry rule.
//!
//! # Why this exists
//!
//! Two properties the C tools cannot offer a Rust storage engine:
//!
//! - **Async, and parallel.** [`BlockDevice`] takes `&self` for reads *and*
//!   writes, so one format fans out across block groups and many formats run at
//!   once. A storage engine provisioning volumes formats them concurrently, not
//!   one after another.
//! - **No device round trip.** [`BlockDevice`] is the seam: a consumer formats
//!   its own in-memory or network-backed volume directly, with no loopback, no
//!   `/dev` node and no `mkfs.ext4` subprocess.
//!
//! # Layout of this crate
//!
//! | Module | What it owns |
//! |---|---|
//! | [`device`] | the [`BlockDevice`] trait and its file / memory implementations |
//! | [`structs`] | byte-exact on-disk structures |
//! | [`features`] | feature masks and `mke2fs -O` parsing |
//! | [`csum`] | crc32c and crc16 metadata checksums |
//! | [`error`] | [`Error`] and [`Result`] |

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod bytes;

pub mod compare;
pub mod csum;
pub mod device;
pub mod error;
pub mod features;
pub mod format;
pub mod fs;
pub mod fsck;
pub mod journal;
pub mod layout;
pub mod params;
pub mod structs;

pub use device::{BlockDevice, FileDevice, MemDevice};
pub use error::{Error, Result};
pub use features::{CompatFeatures, FeatureMasks, IncompatFeatures, RoCompatFeatures};
pub use structs::{
    DirEntry, Extent, GroupDesc, Inode, Superblock, SUPERBLOCK_LEN, SUPERBLOCK_OFFSET,
};
