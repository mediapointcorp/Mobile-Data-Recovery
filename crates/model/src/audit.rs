//! Append-only, hash-chained JSON-lines audit log (spec 4.I).
//!
//! Every event stores `prev`, the SHA-256 of the previous line's exact
//! bytes (64 zeros for the first). Editing, inserting or deleting any
//! earlier line breaks the chain, which [`AuditLog::verify_chain`] detects.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuditEvent {
    pub ts: String,
    pub tool: String,
    pub tool_version: String,
    pub event: String,
    pub details: serde_json::Value,
    /// SHA-256 of the previous line (hex).
    #[serde(default)]
    pub prev: String,
}

struct Inner {
    file: File,
    last_hash: String,
}

/// Thread-safe audit log. Every event is flushed to disk immediately so the
/// log survives crashes and cancellations.
pub struct AuditLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

fn line_hash(line: &[u8]) -> String {
    hex::encode(Sha256::digest(line))
}

impl AuditLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let last_hash = match std::fs::read(path) {
            Ok(bytes) => bytes
                .split(|&b| b == b'\n')
                .rfind(|l| !l.is_empty())
                .map(line_hash)
                .unwrap_or_else(|| GENESIS.to_string()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => GENESIS.to_string(),
            Err(e) => return Err(e),
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(AuditLog {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner { file, last_hash }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, event: &str, details: serde_json::Value) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let ev = AuditEvent {
            ts: crate::now_rfc3339(),
            tool: crate::TOOL_NAME.to_string(),
            tool_version: crate::TOOL_VERSION.to_string(),
            event: event.to_string(),
            details,
            prev: inner.last_hash.clone(),
        };
        let line = serde_json::to_vec(&ev).map_err(io::Error::other)?;
        let mut buf = line.clone();
        buf.push(b'\n');
        inner.file.write_all(&buf)?;
        inner.file.flush()?;
        inner.last_hash = line_hash(&line);
        Ok(())
    }

    pub fn read_all(path: &Path) -> io::Result<Vec<AuditEvent>> {
        let text = std::fs::read_to_string(path)?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// Verify the hash chain. Returns the number of events, or the 1-based
    /// line number of the first event whose `prev` does not match.
    pub fn verify_chain(path: &Path) -> io::Result<Result<usize, usize>> {
        let bytes = std::fs::read(path)?;
        let mut prev = GENESIS.to_string();
        let mut n = 0;
        for (i, line) in bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .enumerate()
        {
            let ev: AuditEvent = match serde_json::from_slice(line) {
                Ok(e) => e,
                Err(_) => return Ok(Err(i + 1)),
            };
            if ev.prev != prev {
                return Ok(Err(i + 1));
            }
            prev = line_hash(line);
            n += 1;
        }
        Ok(Ok(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_reopen_and_verify_chain() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        log.record("start", serde_json::json!({"x": 1})).unwrap();
        log.record("end", serde_json::json!({})).unwrap();
        drop(log);
        let log = AuditLog::open(&p).unwrap();
        log.record("again", serde_json::json!(null)).unwrap();
        let evs = AuditLog::read_all(&p).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].event, "start");
        assert_eq!(evs[0].prev, GENESIS);
        assert_eq!(evs[2].tool_version, crate::TOOL_VERSION);
        assert_eq!(AuditLog::verify_chain(&p).unwrap(), Ok(3));
    }

    #[test]
    fn tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        for i in 0..4 {
            log.record("e", serde_json::json!({ "i": i })).unwrap();
        }
        drop(log);
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("{\"i\":1}", "{\"i\":9}")).unwrap();
        assert_eq!(AuditLog::verify_chain(&p).unwrap(), Err(3));
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(&p, format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3])).unwrap();
        assert_eq!(AuditLog::verify_chain(&p).unwrap(), Err(2));
    }
}
