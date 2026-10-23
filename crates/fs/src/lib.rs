//! Filesystem recovery engine (spec 4.D).
//!
//! 1. [`detect::analyze_layout`] identifies the partition scheme and every
//!    volume's filesystem *before* anything is parsed, and records
//!    encryption indicators.
//! 2. Only validated formats are parsed for deleted records. Currently:
//!    NTFS ([`ntfs`], from the MFT), FAT12/16/32 ([`fat`]) and exFAT
//!    ([`exfat`]); [`files`] lists files of any of them uniformly.
//!
//! Why ext4/F2FS deleted-record recovery is not offered: on deletion ext4
//! zeroes the inode's extent tree and F2FS invalidates node blocks out of
//! place, so reliable record-based recovery is not possible from metadata
//! alone. Their free space is still searched by deep scan.

pub mod detect;
pub mod exfat;
pub mod fat;
pub mod files;
pub mod ntfs;

pub use detect::{
    analyze_layout, EncryptionState, ImageLayout, PartitionScheme, SupportLevel, VolumeKind,
    VolumeReport,
};

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a supported filesystem: {0}")]
    Unsupported(String),
    #[error("filesystem structure invalid: {0}")]
    Invalid(String),
}
