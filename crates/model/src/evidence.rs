//! Evidence registration, hashing and working copies (spec 4.I).

use crate::{is_cancelled, CancelFlag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const HASH_BLOCK: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("operation cancelled")]
    Cancelled,
    #[error("working copy hash mismatch: source {source_hash}, copy {copy_hash}")]
    CopyMismatch {
        source_hash: String,
        copy_hash: String,
    },
    #[error("refusing to write output inside or onto the evidence: {0}")]
    UnsafeDestination(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> EvidenceError + '_ {
    move |source| EvidenceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Streaming SHA-256 of a file. `progress(bytes_done)` is called per block.
pub fn sha256_file(
    path: &Path,
    cancel: Option<&CancelFlag>,
    mut progress: impl FnMut(u64),
) -> Result<(String, u64), EvidenceError> {
    let mut f = File::open(path).map_err(io_err(path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BLOCK];
    let mut total = 0u64;
    loop {
        if cancel.is_some_and(is_cancelled) {
            return Err(EvidenceError::Cancelled);
        }
        let n = f.read(&mut buf).map_err(io_err(path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        progress(total);
    }
    Ok((hex::encode(hasher.finalize()), total))
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceMode {
    /// The original image is analysed directly, opened read-only.
    ReadOnlyOriginal,
    /// A hash-verified working copy is created and analysed instead.
    WorkingCopy,
}

/// Integrity record for one evidence item.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EvidenceRecord {
    pub source_id: String,
    pub original_path: String,
    pub analysed_path: String,
    pub mode: EvidenceMode,
    pub size: u64,
    pub sha256_before: String,
    pub sha256_after: Option<String>,
    pub working_copy_sha256: Option<String>,
    pub registered_at: String,
    pub verified_at: Option<String>,
    /// `Some(true)` when the after-analysis hash matches.
    pub unchanged: Option<bool>,
}

impl EvidenceRecord {
    pub fn source_id_for_hash(sha256: &str) -> String {
        format!("image:{}", &sha256[..16.min(sha256.len())])
    }
}

/// Ensure `dest` is not the evidence file and not located inside a source
/// directory that is being scanned.
pub fn check_destination(dest: &Path, evidence: &Path) -> Result<(), EvidenceError> {
    let d = canonicalize_lenient(dest);
    let e = canonicalize_lenient(evidence);
    if d == e || (e.is_dir() && d.starts_with(&e)) {
        return Err(EvidenceError::UnsafeDestination(dest.display().to_string()));
    }
    Ok(())
}

/// Canonicalize a path that may not exist yet: canonicalize the nearest
/// existing ancestor and re-append the remaining components. Comparing a
/// canonical path with a non-canonical one is unreliable (on Windows,
/// canonical paths carry a `\\?\` prefix), so both sides of every
/// containment check must go through this.
pub fn canonicalize_lenient(p: &Path) -> PathBuf {
    let mut existing = p;
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            return rest.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Hash the evidence and (optionally) create a verified working copy.
pub fn register_evidence(
    original: &Path,
    mode: EvidenceMode,
    working_dir: &Path,
    cancel: Option<&CancelFlag>,
    mut progress: impl FnMut(&str, u64),
) -> Result<EvidenceRecord, EvidenceError> {
    let (sha, size) = sha256_file(original, cancel, |n| progress("hash_before", n))?;
    let mut rec = EvidenceRecord {
        source_id: EvidenceRecord::source_id_for_hash(&sha),
        original_path: original.display().to_string(),
        analysed_path: original.display().to_string(),
        mode,
        size,
        sha256_before: sha.clone(),
        sha256_after: None,
        working_copy_sha256: None,
        registered_at: crate::now_rfc3339(),
        verified_at: None,
        unchanged: None,
    };
    if mode == EvidenceMode::WorkingCopy {
        std::fs::create_dir_all(working_dir).map_err(io_err(working_dir))?;
        let name = original
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "evidence.img".into());
        let dest = working_dir.join(format!("{}-{}", &sha[..12], name));
        check_destination(&dest, original)?;
        let copy_hash = copy_with_hash(original, &dest, cancel, |n| progress("working_copy", n))?;
        if copy_hash != sha {
            return Err(EvidenceError::CopyMismatch {
                source_hash: sha,
                copy_hash,
            });
        }
        rec.working_copy_sha256 = Some(copy_hash);
        rec.analysed_path = dest.display().to_string();
    }
    Ok(rec)
}

/// Re-hash the original after analysis and record whether it is unchanged.
pub fn verify_evidence(
    rec: &mut EvidenceRecord,
    cancel: Option<&CancelFlag>,
    mut progress: impl FnMut(u64),
) -> Result<bool, EvidenceError> {
    let (after, _) = sha256_file(Path::new(&rec.original_path), cancel, &mut progress)?;
    let same = after == rec.sha256_before;
    rec.sha256_after = Some(after);
    rec.verified_at = Some(crate::now_rfc3339());
    rec.unchanged = Some(same);
    Ok(same)
}

fn copy_with_hash(
    src: &Path,
    dest: &Path,
    cancel: Option<&CancelFlag>,
    mut progress: impl FnMut(u64),
) -> Result<String, EvidenceError> {
    let mut input = File::open(src).map_err(io_err(src))?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(io_err(dest))?;
    let mut buf = vec![0u8; HASH_BLOCK];
    let mut total = 0;
    loop {
        if cancel.is_some_and(is_cancelled) {
            drop(out);
            let _ = std::fs::remove_file(dest);
            return Err(EvidenceError::Cancelled);
        }
        let n = input.read(&mut buf).map_err(io_err(src))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(io_err(dest))?;
        total += n as u64;
        progress(total);
    }
    out.sync_all().map_err(io_err(dest))?;
    drop(out);
    // Hash what actually landed on disk, not what we think we wrote.
    Ok(sha256_file(dest, cancel, |_| {})?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_copy_is_verified_and_original_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let ev = dir.path().join("ev.img");
        std::fs::write(&ev, vec![7u8; 100_000]).unwrap();
        let mut rec = register_evidence(
            &ev,
            EvidenceMode::WorkingCopy,
            &dir.path().join("work"),
            None,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            rec.working_copy_sha256.as_deref(),
            Some(rec.sha256_before.as_str())
        );
        assert_ne!(rec.analysed_path, rec.original_path);
        assert!(verify_evidence(&mut rec, None, |_| {}).unwrap());
        assert_eq!(rec.unchanged, Some(true));
    }

    #[test]
    fn detects_modified_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let ev = dir.path().join("ev.img");
        std::fs::write(&ev, b"abc").unwrap();
        let mut rec = register_evidence(
            &ev,
            EvidenceMode::ReadOnlyOriginal,
            dir.path(),
            None,
            |_, _| {},
        )
        .unwrap();
        std::fs::write(&ev, b"abd").unwrap();
        assert!(!verify_evidence(&mut rec, None, |_| {}).unwrap());
    }

    #[test]
    fn destination_cannot_be_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let ev = dir.path().join("ev.img");
        std::fs::write(&ev, b"x").unwrap();
        assert!(check_destination(&ev, &ev).is_err());
        assert!(check_destination(&dir.path().join("sub"), dir.path()).is_err());
        assert!(check_destination(&dir.path().join("out"), &ev).is_ok());
        // Not-yet-existing nested destination inside an evidence folder.
        assert!(check_destination(&dir.path().join("a").join("b"), dir.path()).is_err());
    }
}
