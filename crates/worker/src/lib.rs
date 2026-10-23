//! PhotoRecovery worker library.
//!
//! The worker owns every privileged or long-running operation. The desktop
//! UI talks to it only through the JSON-lines protocol in [`server`] over
//! the worker's stdin/stdout — there is no network listener.

pub mod case;
pub mod classify;
pub mod drives;
pub mod finalize;
pub mod jobs;
pub mod recover;
pub mod report;
pub mod server;
pub mod util;

/// Progress sink shared by CLI and server modes.
pub trait Progress: Send + Sync {
    fn report(&self, job_id: &str, phase: &str, done: u64, total: u64, message: &str);
}

/// Progress sink that discards everything (tests).
pub struct NoProgress;

impl Progress for NoProgress {
    fn report(&self, _: &str, _: &str, _: u64, _: u64, _: &str) {}
}
