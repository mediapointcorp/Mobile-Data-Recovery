//! Local-folder job: search an existing backup or copy (e.g. a phone backup
//! folder on the PC) for images. Files are copied into the case (hash
//! verified) before analysis so the source folder is never touched.

use crate::case::Case;
use crate::classify;
use crate::jobs::image::carve_container;
use crate::jobs::JobRecord;
use crate::util::{self, JsonlAppender};
use crate::Progress;
use anyhow::{bail, Result};
use pr_adb::discovery::{classify as classify_path, is_candidate, kind_warnings, DeviceFileKind};
use pr_adb::validate::local_relative_path;
use pr_model::model::{RecoveryMethod, RecoveryResult};
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_FILES: usize = 2_000_000;
const MAX_FILE: u64 = 1024 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LocalScanParams {
    pub dir: String,
    #[serde(default)]
    pub max_files: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Checkpoint {
    listing_done: bool,
    next_index: usize,
}

/// Recursive listing without following symlinks/junctions.
pub fn list_images(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                let rel = util::rel(root, &p);
                if is_candidate(&format!("/{rel}")) {
                    out.push(rel);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }
    out.sort();
    out
}

pub fn run(
    case: &Case,
    rec: &mut JobRecord,
    p: &LocalScanParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    let root = PathBuf::from(&p.dir);
    if !root.is_absolute() || !root.is_dir() {
        bail!("folder must be an existing directory given by absolute path");
    }
    let root = root.canonicalize()?;
    if case.root.starts_with(&root) {
        bail!(
            "the case folder is inside the folder being scanned; choose a case location outside it"
        );
    }
    let dir = rec.dir(case);
    let cp_path = dir.join("checkpoint.json");
    let mut cp: Checkpoint = util::read_json(&cp_path).unwrap_or_default();
    let listing_path = dir.join("listing.json");
    if !cp.listing_done {
        progress.report(&rec.id, "discover", 0, 1, "listing images");
        let files = list_images(&root, p.max_files.unwrap_or(MAX_FILES).min(MAX_FILES));
        util::write_json(&listing_path, &files)?;
        case.audit("local_listing", serde_json::json!({ "job_id": rec.id, "dir": root.display().to_string(), "files": files.len() }));
        cp.listing_done = true;
        util::write_json(&cp_path, &cp)?;
    }
    let files: Vec<String> = util::read_json(&listing_path)?;
    let tag = &pr_model::evidence::sha256_bytes(root.display().to_string().as_bytes())[..12];
    let source_id = format!("dir:{tag}");
    let dest_root = case.root.join("acquired").join(format!("local-{tag}"));

    let results_path = dir.join("results.jsonl");
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

    for (i, rel_src) in files.iter().enumerate().skip(cp.next_index) {
        if pr_model::is_cancelled(cancel) {
            return Ok(false);
        }
        progress.report(&rec.id, "copy", i as u64, files.len() as u64, rel_src);
        let src_path = root.join(rel_src);
        let dest = dest_root.join(local_relative_path(rel_src));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = match std::fs::metadata(&src_path) {
            Ok(m) => m.len(),
            Err(e) => {
                rec.limitations
                    .push(format!("Could not read {rel_src}: {e}"));
                cp.next_index = i + 1;
                continue;
            }
        };
        if size > MAX_FILE {
            rec.limitations
                .push(format!("{rel_src} exceeds {MAX_FILE} bytes; skipped"));
            cp.next_index = i + 1;
            continue;
        }
        let (src_hash, _) = pr_model::evidence::sha256_file(&src_path, Some(cancel), |_| {})?;
        std::fs::copy(&src_path, &dest)?;
        let (copy_hash, _) = pr_model::evidence::sha256_file(&dest, Some(cancel), |_| {})?;
        let device_like = format!("/{rel_src}");
        let kind = classify_path(&device_like);
        let rel_out = util::rel(&case.root, &dest);
        let mut produced = Vec::new();
        if kind == DeviceFileKind::ThumbnailContainer {
            let tmp = dir.join(format!("container_{i}.jsonl"));
            let _ = std::fs::remove_file(&tmp);
            carve_container(
                case,
                &rec.id,
                &source_id,
                &dest,
                &src_path.display().to_string(),
                &tmp,
                cancel,
                progress,
            )?;
            produced.extend(util::read_jsonl::<RecoveryResult>(&tmp)?);
            let _ = std::fs::remove_file(&tmp);
        } else {
            let bytes = std::fs::read(&dest)?;
            let name = rel_src.rsplit('/').next().unwrap_or("");
            let mut r = classify::from_file_bytes(
                &source_id,
                RecoveryMethod::LocalDirectoryScan,
                kind.classification(),
                &src_path.display().to_string(),
                Some(rel_out),
                &bytes,
                name,
            );
            r.warnings.extend(kind_warnings(kind));
            r.metadata
                .insert("location_kind".into(), kind.label().into());
            if src_hash != copy_hash {
                r.warnings.push(
                    "Source and copy hashes differ: the source changed while copying.".into(),
                );
            }
            produced.push(r);
        }
        for mut r in produced {
            r.metadata.insert("listing_index".into(), i.to_string());
            out.append(&r)?;
        }
        cp.next_index = i + 1;
        util::write_json(&cp_path, &cp)?;
    }
    rec.stats =
        serde_json::json!({ "listed": files.len(), "source_dir": root.display().to_string() });
    Ok(true)
}
