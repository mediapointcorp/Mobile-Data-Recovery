//! Small I/O helpers: atomic writes and JSON-lines files.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

/// Write `bytes` to `path` atomically (temp file + rename).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn write_json<T: Serialize>(path: &Path, v: &T) -> io::Result<()> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(v).map_err(io::Error::other)?,
    )
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let f = File::open(path)?;
    serde_json::from_reader(BufReader::new(f)).map_err(io::Error::other)
}

/// Read a JSON-lines file, skipping a torn last line (crash mid-write).
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    Ok(out)
}

pub fn write_jsonl<T: Serialize>(path: &Path, items: &[T]) -> io::Result<()> {
    let mut buf = Vec::new();
    for it in items {
        serde_json::to_writer(&mut buf, it).map_err(io::Error::other)?;
        buf.push(b'\n');
    }
    atomic_write(path, &buf)
}

/// Append-only JSON-lines writer that flushes every record.
pub struct JsonlAppender {
    f: File,
}

impl JsonlAppender {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(JsonlAppender {
            f: OpenOptions::new().create(true).append(true).open(path)?,
        })
    }

    pub fn append<T: Serialize>(&mut self, v: &T) -> io::Result<()> {
        let mut line = serde_json::to_vec(v).map_err(io::Error::other)?;
        line.push(b'\n');
        self.f.write_all(&line)?;
        self.f.flush()
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.f.sync_data()
    }
}

/// Path of `p` relative to `base`, with `/` separators (for reports).
pub fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}
