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
//! # If you implement `BlockDevice` yourself
//!
//! **Report your sector size.** [`BlockDevice::logical_sector_size`] defaults to
//! 512, and the block size is never smaller than it. A volume that really
//! exports 4 KiB sectors but inherits the default gets a 1 KiB-block
//! filesystem — valid on paper, and unwritable a block at a time on the device
//! it was made for.
//!
//! ```
//! # use mkfs_ext4::device::BlockDevice;
//! # struct MyVolume;
//! impl MyVolume {
//!     // In your `impl BlockDevice for MyVolume`:
//!     fn logical_sector_size(&self) -> u32 {
//!         4096
//!     }
//! }
//! ```
//!
//! [`FileDevice`] asks the kernel, so a real block device needs nothing. For
//! anything else, either override the method or set
//! [`params::Params::sector_size`], which wins over whatever the device says.
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
pub mod mmp;
pub mod params;
pub mod structs;

// The things a caller reaches for first, so a simple use looks simple.
pub use compare::{compare, CompareOptions, ComparisonReport};
pub use device::{BlockDevice, FileDevice, MemDevice};
pub use format::{format, Report};
pub use fs::Filesystem;
pub use fsck::{FsckOptions, FsckReport};
pub use layout::Geometry;
pub use params::{JournalSize, Params, Profile};
pub use error::{Error, Result};
pub use features::{CompatFeatures, FeatureMasks, IncompatFeatures, RoCompatFeatures};
pub use structs::{
    DirEntry, Extent, GroupDesc, Inode, Superblock, SUPERBLOCK_LEN, SUPERBLOCK_OFFSET,
};
