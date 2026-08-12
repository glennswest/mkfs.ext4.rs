//! Errors surfaced by formatting and checking.

use std::io;

/// Anything that can go wrong formatting or checking a filesystem.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying device failed a read, write or flush.
    #[error("device I/O failed at offset {offset}: {source}")]
    Io {
        /// Byte offset the operation targeted.
        offset: u64,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// A read or write ran past the end of the device.
    #[error("I/O of {len} bytes at offset {offset} runs past the end of the {size}-byte device")]
    OutOfBounds {
        /// Byte offset the operation targeted.
        offset: u64,
        /// Length requested.
        len: u64,
        /// Size of the device.
        size: u64,
    },

    /// The requested parameters cannot describe a valid filesystem.
    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    /// The device is too small for the requested filesystem.
    #[error("device holds {available} blocks of {block_size} bytes; {needed} are needed for metadata alone")]
    DeviceTooSmall {
        /// Blocks the device provides.
        available: u64,
        /// Blocks metadata requires.
        needed: u64,
        /// Block size in bytes.
        block_size: u32,
    },

    /// A feature was requested that this implementation does not write.
    #[error("feature {0} is not supported by this implementation")]
    UnsupportedFeature(String),

    /// A feature combination the format forbids.
    #[error("incompatible features: {0}")]
    IncompatibleFeatures(String),

    /// No ext2/3/4 superblock was found where one was expected.
    #[error("no ext2/3/4 superblock found (magic was {found:#06x}, expected 0xef53)")]
    NotExtFilesystem {
        /// The magic actually read.
        found: u16,
    },

    /// A structure on disk did not decode.
    #[error("corrupt {structure}: {detail}")]
    Corrupt {
        /// Which structure failed to decode.
        structure: &'static str,
        /// What was wrong with it.
        detail: String,
    },

    /// The filesystem uses features this checker will not risk repairing.
    #[error("cannot check filesystem: {0}")]
    CannotCheck(String),
}

impl Error {
    pub(crate) fn io(offset: u64, source: io::Error) -> Self {
        Error::Io { offset, source }
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidParams(msg.into())
    }

    pub(crate) fn corrupt(structure: &'static str, detail: impl Into<String>) -> Self {
        Error::Corrupt {
            structure,
            detail: detail.into(),
        }
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
