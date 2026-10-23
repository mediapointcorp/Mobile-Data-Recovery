//! Android device job: identify → discover accessible images → copy
//! (read-only, hash-verified) → validate and classify.

use crate::case::Case;
use crate::classify;
use crate::jobs::image::carve_container;
use crate::jobs::JobRecord;
use crate::util::{self, JsonlAppender};
use crate::Progress;
use anyhow::{Context, Result};
use pr_adb::client::DiscoveryResult;
use pr_adb::discovery::{classify as classify_path, kind_warnings, DeviceFileKind};
use pr_adb::validate::{local_relative_path, validate_serial};
use pr_adb::{AdbClient, AdbError, ProcessRunner};
use pr_model::model::{RecoveryMethod, RecoveryResult};
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest single file read into memory for validation.
const MAX_FILE: u64 = 1024 * 1024 * 1024;

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceParams {
    pub serial: String,
    /// Device roots to search (default `/sdcard/` plus SD cards).
    #[serde(default)]
    pub roots: Option<Vec<String>>,
    #[serde(default = "yes")]
    pub include_external_storage: bool,
    #[serde(default)]
    pub adb_path: Option<String>,
    #[serde(default)]
    pub max_files: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Checkpoint {
    info_done: bool,
    listing_done: bool,
    next_index: usize,
}

pub fn client(adb_path: Option<&str>) -> Result<AdbClient, AdbError> {
    let path = ProcessRunner::locate(adb_path.map(Path::new))?;
    Ok(AdbClient::new(Arc::new(ProcessRunner::new(path))))
}

pub fn run(
    case: &Case,
    rec: &mut JobRecord,
    p: &DeviceParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    validate_serial(&p.serial)?;
    let adb = client(p.adb_path.as_deref())?;
    let dir = rec.dir(case);
    let cp_path = dir.join("checkpoint.json");
    let mut cp: Checkpoint = util::read_json(&cp_path).unwrap_or_default();
    let source_id = format!("adb:{}", p.serial);

    if !cp.info_done {
        progress.report(&rec.id, "identify", 0, 1, "reading device properties");
        let info = adb.device_info(&p.serial)?;
        let caps = adb.capabilities(&p.serial)?;
        let adb_version = adb.version().unwrap_or_default();
        let doc =
            serde_json::json!({ "info": info, "capabilities": caps, "adb_version": adb_version });
        util::write_json(&dir.join("device.json"), &doc)?;
        case.audit("device_identified", doc);
        rec.limitations.extend(caps.limitations.iter().cloned());
        cp.info_done = true;
        util::write_json(&cp_path, &cp)?;
        rec.save(case)?;
    }
    let listing_path = dir.join("listing.json");
    if !cp.listing_done {
        let mut roots = p.roots.clone().unwrap_or_else(|| vec!["/sdcard/".into()]);
        if p.roots.is_none() && p.include_external_storage {
            if let Ok(caps) = adb.capabilities(&p.serial) {
                roots.extend(
                    caps.external_volumes
                        .iter()
                        .map(|v| format!("/storage/{v}/")),
                );
            }
        }
        progress.report(&rec.id, "discover", 0, 1, "listing accessible images");
        let listing = adb.discover(&p.serial, &roots)?;
        if !listing.inaccessible.is_empty() {
            rec.limitations.push(format!(
                "{} locations could not be read over ADB (e.g. {}).",
                listing.inaccessible.len(),
                listing
                    .inaccessible
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        util::write_json(&listing_path, &listing)?;
        case.audit("device_listing", serde_json::json!({ "job_id": rec.id, "roots": roots, "files": listing.files.len(), "inaccessible": listing.inaccessible.len() }));
        cp.listing_done = true;
        util::write_json(&cp_path, &cp)?;
        rec.save(case)?;
    }
    let listing: DiscoveryResult = util::read_json(&listing_path)?;
    let files: Vec<_> = listing
        .files
        .iter()
        .take(p.max_files.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    let results_path = dir.join("results.jsonl");
    // Drop results for files after the checkpoint (they will be redone).
    let kept: Vec<RecoveryResult> = util::read_jsonl::<RecoveryResult>(&results_path)?
        .into_iter()
        .filter(|r| {
            r.metadata
                .get("listing_index")
                .and_then(|i| i.parse::<usize>().ok())
                .is_some_and(|i| i < cp.next_index)
        })
        .collect();
    util::write_jsonl(&results_path, &kept)?;
    let mut out = JsonlAppender::open(&results_path)?;
    let dest_root = case
        .root
        .join("acquired")
        .join(local_relative_path(&format!(
            "/{}",
            p.serial.replace(':', "_")
        )));
    let mut failures = 0usize;

    for (i, f) in files.iter().enumerate().skip(cp.next_index) {
        if pr_model::is_cancelled(cancel) {
            return Ok(false);
        }
        progress.report(&rec.id, "copy", i as u64, files.len() as u64, &f.path);
        let local = dest_root.join(local_relative_path(&f.path));
        let pulled = if local.exists() {
            // Copied in an earlier, interrupted run.
            let (sha, size) = pr_model::evidence::sha256_file(&local, None, |_| {})?;
            Ok(pr_adb::client::PulledFile {
                device_path: f.path.clone(),
                local_path: local.clone(),
                size,
                sha256: sha,
                device_sha256: None,
                transfer_verified: None,
            })
        } else {
            adb.pull(&p.serial, &f.path, &dest_root)
        };
        let pulled = match pulled {
            Ok(x) => x,
            Err(e) => {
                failures += 1;
                rec.limitations
                    .push(format!("Could not copy {}: {e}", f.path));
                cp.next_index = i + 1;
                util::write_json(&cp_path, &cp)?;
                continue;
            }
        };
        case.audit("file_acquired", serde_json::to_value(&pulled)?);
        let kind = classify_path(&f.path);
        let rel = util::rel(&case.root, &pulled.local_path);
        let mut produced: Vec<RecoveryResult> = Vec::new();
        if kind == DeviceFileKind::ThumbnailContainer {
            let tmp_results = dir.join(format!("container_{i}.jsonl"));
            let _ = std::fs::remove_file(&tmp_results);
            carve_container(
                case,
                &rec.id,
                &source_id,
                &pulled.local_path,
                &f.path,
                &tmp_results,
                cancel,
                progress,
            )?;
            produced.extend(util::read_jsonl::<RecoveryResult>(&tmp_results)?);
            let _ = std::fs::remove_file(&tmp_results);
        } else if pulled.size <= MAX_FILE {
            let bytes = std::fs::read(&pulled.local_path)
                .with_context(|| format!("reading {}", pulled.local_path.display()))?;
            let name = f.path.rsplit('/').next().unwrap_or("");
            let mut r = classify::from_file_bytes(
                &source_id,
                RecoveryMethod::AdbFileCopy,
                kind.classification(),
                &f.path,
                Some(rel.clone()),
                &bytes,
                name,
            );
            r.warnings.extend(kind_warnings(kind));
            r.metadata.insert("device_kind".into(), kind.label().into());
            r.metadata
                .insert("device_mtime".into(), f.mtime.to_string());
            if let Some(h) = &pulled.device_sha256 {
                r.metadata.insert("device_sha256".into(), h.clone());
            }
            match pulled.transfer_verified {
                Some(true) => {
                    r.metadata.insert("transfer_verified".into(), "true".into());
                }
                Some(false) => r.warnings.push("SHA-256 on the device differs from the copied file: the file may have changed during copying.".into()),
                None => r.diagnostics.push("device-side hash not available; transfer not independently verified".into()),
            }
            if pulled.size != f.size {
                r.warnings.push(format!(
                    "Size changed between listing ({}) and copy ({}).",
                    f.size, pulled.size
                ));
            }
            produced.push(r);
        } else {
            rec.limitations.push(format!(
                "{} is larger than {} bytes and was copied but not validated.",
                f.path, MAX_FILE
            ));
        }
        for mut r in produced {
            r.metadata.insert("listing_index".into(), i.to_string());
            out.append(&r)?;
        }
        cp.next_index = i + 1;
        util::write_json(&cp_path, &cp)?;
    }
    rec.stats = serde_json::json!({ "listed": files.len(), "copy_failures": failures, "copied_to": util::rel(&case.root, &PathBuf::from(&dest_root)) });
    Ok(true)
}
