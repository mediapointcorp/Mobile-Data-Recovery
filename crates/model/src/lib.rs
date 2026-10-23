//! Shared types for the PhotoRecovery engine.
//!
//! * [`model`] — the recovery result model every engine produces.
//! * [`source`] — bounded-memory random-access readers over evidence.
//! * [`evidence`] — evidence registration, hashing and working copies.
//! * [`audit`] — append-only JSON-lines audit log.

pub mod audit;
pub mod device;
pub mod evidence;
pub mod model;
pub mod source;

/// Version string recorded in every audit log and report.
pub const TOOL_NAME: &str = "PhotoRecovery";
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared cancellation flag passed to long-running operations.
pub type CancelFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

pub fn new_cancel_flag() -> CancelFlag {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

pub fn is_cancelled(flag: &CancelFlag) -> bool {
    flag.load(std::sync::atomic::Ordering::Relaxed)
}

/// RFC 3339 UTC timestamp for audit records.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}
