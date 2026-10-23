//! Filesystem recovery engine (spec 4.D).
//!
//! 1. [`detect::analyze_layout`] identifies the partition scheme and every
//!    volume's filesystem *before* anything is parsed, and records
//!    encryption indicators.
//! 2. Only validated formats are parsed for deleted records. Currently:
//!    FAT12/16/32 ([`fat`]). ext4, F2FS, exFAT, NTFS are identified and
//!    reported, and their space is still covered by raw carving.
//!
//! Why ext4/F2FS deleted-record recovery is not offered: on deletion ext4
//! zeroes the inode's extent tree (and Android userdata is file-based
//! encrypted), and F2FS invalidates node blocks out of place; reliable
//! record-based recovery is not possible from metadata alone, so we do not
//! pretend otherwise.

pub mod detect;
pub mod fat;

pub use detect::{analyze_layout, EncryptionState, ImageLayout, PartitionScheme, SupportLevel, VolumeKind, VolumeReport};

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a supported filesystem: {0}")]
    Unsupported(String),
    #[error("filesystem structure invalid: {0}")]
    Invalid(String),
}
