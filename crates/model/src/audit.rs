//! Append-only JSON-lines audit log (spec 4.I).

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuditEvent {
    pub ts: String,
    pub tool: String,
    pub tool_version: String,
    pub event: String,
    pub details: serde_json::Value,
}

/// Thread-safe audit log. Every event is flushed to disk immediately so the
/// log survives crashes and cancellations.
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl AuditLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(AuditLog {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, event: &str, details: serde_json::Value) -> io::Result<()> {
        let ev = AuditEvent {
            ts: crate::now_rfc3339(),
            tool: crate::TOOL_NAME.to_string(),
            tool_version: crate::TOOL_VERSION.to_string(),
            event: event.to_string(),
            details,
        };
        let mut line = serde_json::to_string(&ev).map_err(io::Error::other)?;
        line.push('\n');
        let mut f = self.file.lock().unwrap_or_else(|p| p.into_inner());
        f.write_all(line.as_bytes())?;
        f.flush()
    }

    pub fn read_all(path: &Path) -> io::Result<Vec<AuditEvent>> {
        let text = std::fs::read_to_string(path)?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
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
        assert_eq!(evs[2].tool_version, crate::TOOL_VERSION);
    }
}
