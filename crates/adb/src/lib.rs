//! Android device acquisition through ADB (spec 4.B).
//!
//! Design rules:
//! * Only a fixed set of operations exists ([`client::AdbClient`]); the UI
//!   can never pass a command line. Device paths and serials are validated
//!   ([`validate`]) and quoted for the device shell.
//! * All operations are read-only. Root operations are disabled unless the
//!   caller enables them *and* passes an explicit confirmation; they only
//!   read block devices.
//! * Nothing here assumes USB/ADB/root gives access to deleted data.

pub mod client;
pub mod discovery;
pub mod parse;
pub mod runner;
pub mod validate;

pub use client::{AdbClient, DeviceCapabilities, DeviceInfo};
pub use runner::{CommandRunner, ProcessRunner};

#[derive(Debug, thiserror::Error)]
pub enum AdbError {
    #[error("adb executable not found; install Android platform-tools or set its path")]
    AdbNotFound,
    #[error("failed to run adb: {0}")]
    Spawn(std::io::Error),
    #[error("adb timed out after {0} s")]
    Timeout(u64),
    #[error("adb failed (exit {code:?}): {stderr}")]
    Failed { code: Option<i32>, stderr: String },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("device is not ready: {0}")]
    DeviceState(String),
    #[error("operation requires explicit confirmation: {0}")]
    ConfirmationRequired(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cancelled")]
    Cancelled,
}
