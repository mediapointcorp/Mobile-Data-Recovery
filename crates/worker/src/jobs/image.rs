//! Evidence-image and drive-scan job: register → filesystem analysis
//! (FAT/NTFS records) → carving → verify.

use crate::case::Case;
use crate::classify::{self, CarveContext};
use crate::jobs::JobRecord;
use crate::util::{self, JsonlAppender};
use crate::Progress;
use anyhow::{bail, Context, Result};
use pr_carve::export::export_candidate;
use pr_carve::scanner::{scan, Found, ScanConfig, ScanOutcome, ScanSink, ScanState};
use pr_carve::{EndStatus, FormatSet};
use pr_fs::fat::FatVolume;
use pr_fs::ntfs::{DataLocation, NtfsVolume};
use pr_fs::{analyze_layout, ImageLayout, SupportLevel, VolumeKind};
use pr_model::evidence::{self, EvidenceError, EvidenceMode, EvidenceRecord};
use pr_model::model::{
    Classification, ImageFormat, RecoveryMethod, RecoveryResult, Relation, ValidationStatus,
};
use pr_model::source::{read_full_at, FileSource, ReadAt, SliceSource};
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Largest single file read from a filesystem record.
const MAX_FS_FILE: u64 = 512 * 1024 * 1024;

fn yes() -> bool {
    true
}
fn default_mode() -> EvidenceMode {
    EvidenceMode::ReadOnlyOriginal
}

/// Where carving looks for the start of files.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CarveScope {
    /// Every byte of the source.
    #[default]
    All,
    /// Only space that the FAT/NTFS allocation maps mark as free (where
    /// deleted data lives). Space of unrecognised volumes is still scanned.
    Unallocated,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ImageScanParams {
    /// Absolute path of a raw image (`dd`, `.img`, `.bin`, `.raw`).
    pub image_path: String,
    #[serde(default = "default_mode")]
    pub evidence_mode: EvidenceMode,
    /// Comma-separated formats (`jpeg,png,webp,heic`); default all.
    #[serde(default)]
    pub formats: Option<String>,
    #[serde(default = "yes")]
    pub filesystem_analysis: bool,
    #[serde(default = "yes")]
    pub carve: bool,
    #[serde(default)]
    pub chunk_size_mib: Option<u32>,
    #[serde(default)]
    pub carve_scope: CarveScope,
    /// Also report images that still exist in the filesystem (default true
    /// for images; drive scans set false to report deleted data only).
    #[serde(default = "yes")]
    pub include_existing: bool,
}

/// What is being scanned.
pub enum Target {
    /// An image file, hashed before and after.
    Image(PathBuf),
    /// A mounted PC drive read in place (`id` like `D:` / `PhysicalDrive1`).
    Device { id: String, path: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Checkpoint {
    evidence_done: bool,
    fs_done: bool,
    carve_done: bool,
    verify_done: bool,
    scan: Option<ScanState>,
}

pub fn run(
    case: &Case,
    rec: &mut JobRecord,
    p: &ImageScanParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    let image = PathBuf::from(&p.image_path);
    if !image.is_absolute() || !image.is_file() {
        bail!(
            "evidence image must be an existing file given by absolute path: {}",
            image.display()
        );
    }
    evidence::check_destination(&case.root, &image)?;
    run_target(case, rec, Target::Image(image), p, cancel, progress)
}

fn open_target(t: &Target, ev: &EvidenceRecord) -> Result<Arc<dyn ReadAt>> {
    Ok(match t {
        Target::Image(_) => {
            let p = PathBuf::from(&ev.analysed_path);
            Arc::new(FileSource::open(&p).with_context(|| format!("opening {}", p.display()))?)
        }
        Target::Device { id, path } => match pr_model::device::open_device(path) {
            Ok(d) => Arc::new(d),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
                "Windows refused raw access to {id}. Drive scans need administrator rights: close PhotoRecovery and start it with right-click → Run as administrator."
            ),
            Err(e) => return Err(e).with_context(|| format!("opening drive {id}")),
        },
    })
}

pub fn run_target(
    case: &Case,
    rec: &mut JobRecord,
    target: Target,
    p: &ImageScanParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    let dir = rec.dir(case);
    let cp_path = dir.join("checkpoint.json");
    let mut cp: Checkpoint = util::read_json(&cp_path).unwrap_or_default();
    let formats = match &p.formats {
        Some(f) => FormatSet::parse(f).map_err(anyhow::Error::msg)?,
        None => FormatSet::default(),
    };
    let live = matches!(target, Target::Device { .. });

    // 1. Evidence registration (hash before, optional working copy). A live
    //    drive is recorded but not hashed: the running system writes to it.
    let ev_path = dir.join("evidence.json");
    let mut ev: EvidenceRecord = if cp.evidence_done {
        util::read_json(&ev_path)?
    } else {
        let r = match &target {
            Target::Image(image) => {
                let id = rec.id.clone();
                let r = evidence::register_evidence(
                    image,
                    p.evidence_mode,
                    &case.root.join("evidence"),
                    Some(cancel),
                    |phase, n| progress.report(&id, phase, n, 0, "hashing evidence"),
                );
                match r {
                    Err(EvidenceError::Cancelled) => return Ok(false),
                    other => other?,
                }
            }
            Target::Device { id, path } => {
                let dummy = EvidenceRecord {
                    source_id: format!("drive:{id}"),
                    original_path: path.clone(),
                    analysed_path: path.clone(),
                    mode: EvidenceMode::LiveDevice,
                    size: 0,
                    sha256_before: String::new(),
                    sha256_after: None,
                    working_copy_sha256: None,
                    registered_at: pr_model::now_rfc3339(),
                    verified_at: None,
                    unchanged: None,
                };
                let src = open_target(&target, &dummy)?;
                EvidenceRecord {
                    size: src.len(),
                    ..dummy
                }
            }
        };
        util::write_json(&ev_path, &r)?;
        case.audit("evidence_registered", serde_json::to_value(&r)?);
        cp.evidence_done = true;
        util::write_json(&cp_path, &cp)?;
        r
    };
    let src = open_target(&target, &ev)?;
    if live {
        let note = "Drive scanned while in use: it was read in place and not hashed, because Windows can write to it during the scan. For evidential work, image the drive first and scan the image.".to_string();
        if !rec.limitations.contains(&note) {
            rec.limitations.push(note);
        }
    } else if src.len() != ev.size {
        bail!(
            "evidence size changed since registration ({} != {})",
            src.len(),
            ev.size
        );
    }
    let source_id = ev.source_id.clone();
    let mut stats = serde_json::Map::new();

    // 2. Filesystem identification and record-based recovery.
    let mut layout: Option<ImageLayout> = None;
    if p.filesystem_analysis && !cp.fs_done {
        progress.report(
            &rec.id,
            "filesystem",
            0,
            1,
            "identifying partitions and filesystems",
        );
        let l = analyze_layout(src.as_ref());
        util::write_json(&dir.join("layout.json"), &l)?;
        case.audit("layout_analyzed", serde_json::to_value(&l)?);
        rec.limitations.extend(l.notes.iter().cloned());
        let mut results = Vec::new();
        for v in &l.volumes {
            if v.support != SupportLevel::DeletedRecordRecovery {
                for n in &v.notes {
                    rec.limitations
                        .push(format!("Volume at offset {} ({:?}): {n}", v.offset, v.kind));
                }
                continue;
            }
            let slice = SliceSource::new(src.clone(), v.offset, v.length);
            let outcome = if v.kind == VolumeKind::Ntfs {
                ntfs_recover(
                    case,
                    &slice,
                    v.offset,
                    &source_id,
                    p.include_existing,
                    &rec.id,
                    progress,
                    &mut results,
                )
            } else {
                fat_recover(
                    case,
                    &slice,
                    v.offset,
                    &source_id,
                    p.include_existing,
                    &mut results,
                )
            };
            match outcome {
                Ok(n) => {
                    stats.insert(format!("fs_entries_at_{}", v.offset), n.into());
                }
                Err(e) => rec.limitations.push(format!(
                    "{:?} volume at offset {} could not be parsed: {e:#}",
                    v.kind, v.offset
                )),
            }
        }
        util::write_jsonl(&dir.join("results_fs.jsonl"), &results)?;
        stats.insert("fs_results".into(), results.len().into());
        layout = Some(l);
        cp.fs_done = true;
        util::write_json(&cp_path, &cp)?;
        rec.save(case)?;
    }

    // 3. Carving.
    if p.carve && !cp.carve_done {
        let mut state = cp.scan.clone().unwrap_or_default();
        let carve_path = dir.join("results_carve.jsonl");
        // Results found after the last checkpoint will be found again.
        let kept: Vec<RecoveryResult> = util::read_jsonl::<RecoveryResult>(&carve_path)?
            .into_iter()
            .filter(|r| r.source_offset.is_some_and(|o| o < state.next_offset))
            .collect();
        util::write_jsonl(&carve_path, &kept)?;
        if state.next_offset > 0 {
            case.audit("carve_resumed", serde_json::json!({ "job_id": rec.id, "offset": state.next_offset, "kept_results": kept.len() }));
        }
        let mut cfg = ScanConfig {
            formats,
            ..Default::default()
        };
        if let Some(m) = p.chunk_size_mib {
            cfg.chunk_size = (m.clamp(1, 256) as usize) << 20;
        }
        if p.carve_scope == CarveScope::Unallocated {
            let l = layout
                .take()
                .unwrap_or_else(|| analyze_layout(src.as_ref()));
            let (ranges, known) = unallocated_ranges(&src, &l);
            let free: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
            stats.insert("carve_ranges_bytes".into(), free.into());
            if known {
                rec.limitations.push(format!(
                    "Carving was limited to {:.1} GB of unallocated space (where deleted data can remain); allocated space holds existing files and was not carved.",
                    free as f64 / 1e9
                ));
            }
            cfg.ranges = Some(ranges);
        }
        let mut sink = CarveSink {
            case,
            src: src.as_ref(),
            job_id: rec.id.clone(),
            source_id: source_id.clone(),
            ctx: CarveContext::RawImage,
            name_prefix: "carve".into(),
            out: JsonlAppender::open(&carve_path)?,
            ids: kept
                .iter()
                .filter_map(|r| r.source_offset.map(|o| (o, r.id.clone())))
                .collect(),
            checkpoint: Some((cp_path.clone(), cp.clone())),
            progress,
            started: Instant::now(),
        };
        let outcome = scan(src.as_ref(), &cfg, &mut state, cancel, &mut sink)?;
        if outcome == ScanOutcome::Cancelled {
            return Ok(false);
        }
        cp.scan = Some(state.clone());
        cp.carve_done = true;
        util::write_json(&cp_path, &cp)?;
        stats.insert("scan".into(), serde_json::to_value(&state.stats)?);
        stats.insert(
            "scan_seconds".into(),
            sink.started.elapsed().as_secs_f64().into(),
        );
    }

    // 4. Verify the evidence is unchanged (images only).
    if !cp.verify_done && !live {
        let id = rec.id.clone();
        let total = ev.size;
        match evidence::verify_evidence(&mut ev, Some(cancel), |n| {
            progress.report(&id, "verify", n, total, "re-hashing evidence")
        }) {
            Err(EvidenceError::Cancelled) => return Ok(false),
            Err(e) => return Err(e.into()),
            Ok(same) => {
                if !same {
                    rec.limitations.push("EVIDENCE CHANGED: the original image's SHA-256 after analysis differs from before. It was modified by something else during analysis.".into());
                }
            }
        }
        if ev.mode == EvidenceMode::WorkingCopy {
            let (h, _) = evidence::sha256_file(Path::new(&ev.analysed_path), Some(cancel), |_| {})?;
            if Some(&h) != ev.working_copy_sha256.as_ref() {
                rec.limitations
                    .push("Working copy hash changed during analysis.".into());
            }
        }
        util::write_json(&ev_path, &ev)?;
        case.audit("evidence_verified", serde_json::to_value(&ev)?);
        cp.verify_done = true;
        util::write_json(&cp_path, &cp)?;
    }
    stats.insert("evidence_unchanged".into(), ev.unchanged.into());
    if let serde_json::Value::Object(old) = &mut rec.stats {
        old.extend(stats);
    } else {
        rec.stats = serde_json::Value::Object(stats);
    }
    Ok(true)
}

/// Absolute byte ranges not allocated by any recognised FAT/NTFS volume.
/// Returns the ranges and whether any allocation map was available.
fn unallocated_ranges(src: &Arc<dyn ReadAt>, layout: &ImageLayout) -> (Vec<(u64, u64)>, bool) {
    let mut allocated: Vec<(u64, u64)> = Vec::new();
    let mut known = false;
    for v in &layout.volumes {
        let slice = SliceSource::new(src.clone(), v.offset, v.length);
        let ext = match v.kind {
            VolumeKind::Ntfs => NtfsVolume::open(&slice)
                .ok()
                .filter(|n| n.has_bitmap())
                .map(|n| n.allocated_extents()),
            VolumeKind::Fat12 | VolumeKind::Fat16 | VolumeKind::Fat32 => {
                FatVolume::open(&slice).ok().map(|f| f.allocated_extents())
            }
            _ => None,
        };
        if let Some(ext) = ext {
            known = true;
            allocated.extend(
                ext.into_iter()
                    .map(|(s, e)| (v.offset + s, (v.offset + e).min(v.offset + v.length))),
            );
        }
    }
    allocated.sort_unstable();
    let mut free = Vec::new();
    let mut pos = 0u64;
    for (s, e) in allocated {
        if s > pos {
            free.push((pos, s));
        }
        pos = pos.max(e);
    }
    if pos < src.len() {
        free.push((pos, src.len()));
    }
    (free, known)
}

/// Sink that exports, validates and classifies every carved candidate and
/// persists results and checkpoints as it goes.
pub struct CarveSink<'a> {
    pub case: &'a Case,
    pub src: &'a dyn ReadAt,
    pub job_id: String,
    pub source_id: String,
    pub ctx: CarveContext,
    pub name_prefix: String,
    pub out: JsonlAppender,
    /// candidate offset → result id (to link embedded thumbnails).
    pub ids: HashMap<u64, String>,
    /// (checkpoint path, checkpoint template) — `None` for container carving.
    checkpoint: Option<(PathBuf, Checkpoint)>,
    pub progress: &'a dyn Progress,
    started: Instant,
}

impl<'a> CarveSink<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn for_container(
        case: &'a Case,
        src: &'a dyn ReadAt,
        job_id: &str,
        source_id: &str,
        container_path: &str,
        prefix: &str,
        out: JsonlAppender,
        progress: &'a dyn Progress,
    ) -> Self {
        CarveSink {
            case,
            src,
            job_id: job_id.to_string(),
            source_id: source_id.to_string(),
            ctx: CarveContext::ThumbnailContainer {
                container_path: container_path.to_string(),
            },
            name_prefix: prefix.to_string(),
            out,
            ids: HashMap::new(),
            checkpoint: None,
            progress,
            started: Instant::now(),
        }
    }
}

impl ScanSink for CarveSink<'_> {
    fn on_candidate(&mut self, found: Found) -> std::io::Result<()> {
        let c = &found.candidate;
        let name = match found.embedded_in {
            Some(parent) => format!(
                "{}_{parent:012x}_embedded_{:012x}.{}",
                self.name_prefix,
                c.offset,
                c.format.extension()
            ),
            None => format!(
                "{}_{:012x}.{}",
                self.name_prefix,
                c.offset,
                c.format.extension()
            ),
        };
        let staging = self
            .case
            .recovered_dir()
            .join(".staging")
            .join(&self.job_id)
            .join(&name);
        let info = export_candidate(self.src, c, &staging, true)?;
        let bytes = std::fs::read(&staging)?;
        let v = pr_validate::validate_bytes(&bytes, c.format, c.status == EndStatus::Complete);
        drop(bytes);
        // Classification decides the destination folder; compute it first.
        let probe = classify::from_carved(&self.source_id, &found, &info, "", &v, &self.ctx);
        let rel = format!("recovered/{}/{}", probe.classification.dir_name(), name);
        let mut r = probe;
        r.output_path = Some(rel.clone());
        let dest = self.case.root.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&staging, &dest)?;
        if let Some(parent) = found.embedded_in {
            if let Some(pid) = self.ids.get(&parent) {
                r.associate(
                    pid,
                    Relation::EmbeddedIn,
                    "found inside the parent image's APP segment (EXIF thumbnail)",
                );
            }
        }
        self.ids.insert(c.offset, r.id.clone());
        self.out.append(&r)
    }

    fn on_checkpoint(&mut self, state: &ScanState, total: u64) -> std::io::Result<()> {
        if let Some((path, cp)) = &mut self.checkpoint {
            self.out.sync()?;
            cp.scan = Some(state.clone());
            util::write_json(path, cp)?;
        }
        // Report every checkpoint; UI-side sinks throttle.
        {
            let secs = self.started.elapsed().as_secs_f64().max(0.001);
            let msg = format!(
                "{} candidates, {} rejected signatures, {:.1} MB/s",
                state.stats.candidates,
                state.stats.rejected,
                state.stats.bytes_scanned as f64 / 1e6 / secs
            );
            self.progress
                .report(&self.job_id, "carve", state.next_offset, total, &msg);
        }
        Ok(())
    }
}

fn safe_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    n.chars()
        .rev()
        .take(100)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Recover live and deleted image files from one FAT volume.
fn fat_recover(
    case: &Case,
    vol: &SliceSource,
    vol_offset: u64,
    source_id: &str,
    include_existing: bool,
    out: &mut Vec<RecoveryResult>,
) -> Result<usize> {
    let fv = FatVolume::open(vol)?;
    let entries = fv.walk();
    for e in entries.iter().filter(|e| !e.is_dir) {
        let name = e.path.rsplit('/').next().unwrap_or("");
        let ext_fmt = ImageFormat::from_extension(name);
        let plan = fv.plan(e);
        let mut head = [0u8; 16];
        let head_fmt = match plan.ranges.first() {
            Some(&(off, _)) if read_full_at(vol, off, &mut head)? >= 12 => {
                ImageFormat::sniff(&head)
            }
            _ => ImageFormat::Unknown,
        };
        if ext_fmt == ImageFormat::Unknown && head_fmt == ImageFormat::Unknown {
            continue; // not an image
        }
        let abs_offset = plan.ranges.first().map(|r| vol_offset + r.0);
        let meta = |r: &mut RecoveryResult| {
            r.source_path = Some(e.path.clone());
            r.source_offset = abs_offset;
            r.metadata
                .insert("fs".into(), format!("{:?}", fv.geo.fat_type));
            r.metadata
                .insert("fs_volume_offset".into(), vol_offset.to_string());
            r.metadata.insert(
                "fs_entry_offset".into(),
                (vol_offset + e.entry_offset).to_string(),
            );
            r.metadata
                .insert("fs_recorded_size".into(), e.size.to_string());
            if let Some(m) = &e.modified {
                r.metadata.insert("fs_modified".into(), m.clone());
            }
            if let Some(c) = &e.created {
                r.metadata.insert("fs_created".into(), c.clone());
            }
            if e.name_reconstructed {
                r.warnings.push("Deleted entry had no long name; the first character of the short name is lost and shown as '_'.".into());
            }
            r.diagnostics.extend(plan.notes.iter().cloned());
        };
        let unavailable = |why: &str| {
            let mut r = RecoveryResult::new(
                source_id,
                RecoveryMethod::FilesystemDeletedEntry,
                ext_fmt,
                Classification::UnavailableSourceData,
            );
            r.limitations.push(why.to_string());
            r
        };
        if e.size as u64 > MAX_FS_FILE {
            let mut r = unavailable(&format!(
                "File of {} bytes exceeds the {} byte read limit.",
                e.size, MAX_FS_FILE
            ));
            meta(&mut r);
            out.push(r);
            continue;
        }
        if !e.deleted {
            if !include_existing {
                continue;
            }
            let bytes = fv.read(&plan, MAX_FS_FILE)?;
            let name_out = format!(
                "fs_{:x}_{:x}_{}",
                vol_offset,
                e.entry_offset,
                safe_name(name)
            );
            let rel = format!(
                "recovered/{}/{}",
                Classification::ExistingOriginal.dir_name(),
                name_out
            );
            util::atomic_write(&case.root.join(&rel), &bytes)?;
            let mut r = classify::from_file_bytes(
                source_id,
                RecoveryMethod::FilesystemLiveEntry,
                Classification::ExistingOriginal,
                &e.path,
                Some(rel),
                &bytes,
                name,
            );
            meta(&mut r);
            if plan.ranges.len() > 1 {
                r.diagnostics
                    .push(format!("{} fragments (chain followed)", plan.ranges.len()));
            }
            out.push(r);
            continue;
        }
        // Deleted entry.
        if plan.ranges.is_empty() {
            let mut r = unavailable("Deleted record has no data clusters recorded.");
            meta(&mut r);
            out.push(r);
            continue;
        }
        let first_reused = fv.is_allocated(e.first_cluster);
        if first_reused || plan.clusters_now_allocated == plan.clusters_total {
            let mut r = unavailable("The clusters of this deleted file are now allocated to other data: content is overwritten.");
            meta(&mut r);
            out.push(r);
            continue;
        }
        if head_fmt == ImageFormat::Unknown {
            let mut r = unavailable("Data at the recorded start cluster is not an image: it was overwritten or wiped after deletion.");
            meta(&mut r);
            out.push(r);
            continue;
        }
        let bytes = fv.read(&plan, MAX_FS_FILE)?;
        let probe = pr_validate::validate_bytes(&bytes, head_fmt, true);
        // Any reused cluster in the assumed range means foreign data.
        let class = if probe.status == pr_model::model::ValidationStatus::Valid
            && plan.clusters_now_allocated == 0
        {
            Classification::DeletedFileRecord
        } else {
            Classification::PartialImage
        };
        let name_out = format!(
            "fsdel_{:x}_{:x}_{}",
            vol_offset,
            e.entry_offset,
            safe_name(name)
        );
        let rel = format!("recovered/{}/{}", class.dir_name(), name_out);
        util::atomic_write(&case.root.join(&rel), &bytes)?;
        let mut r = classify::from_file_bytes(
            source_id,
            RecoveryMethod::FilesystemDeletedEntry,
            class,
            &e.path,
            Some(rel),
            &bytes,
            name,
        );
        meta(&mut r);
        r.limitations.push(
            "Recovered from a deleted FAT directory entry assuming contiguous allocation.".into(),
        );
        if class == Classification::PartialImage {
            r.limitations.push("The data does not fully validate: the file was probably fragmented or partly overwritten.".into());
        }
        if plan.clusters_now_allocated > 0 {
            r.warnings.push(format!(
                "{} of {} clusters are now allocated to other data.",
                plan.clusters_now_allocated, plan.clusters_total
            ));
        }
        out.push(r);
    }
    Ok(entries.len())
}

fn is_image_name(name: &str) -> bool {
    ImageFormat::from_extension(name) != ImageFormat::Unknown
}

/// Recover live (optional) and deleted image files from one NTFS volume
/// using MFT records. Returns the number of image records examined.
#[allow(clippy::too_many_arguments)]
fn ntfs_recover(
    case: &Case,
    vol: &SliceSource,
    vol_offset: u64,
    source_id: &str,
    include_existing: bool,
    job_id: &str,
    progress: &dyn Progress,
    out: &mut Vec<RecoveryResult>,
) -> Result<usize> {
    let nv = NtfsVolume::open(vol)?;
    let mut prog = |done: u64, total: u64| {
        progress.report(
            job_id,
            "filesystem",
            done,
            total,
            "reading NTFS master file table",
        )
    };
    // Only image file names are examined; photos renamed to other
    // extensions are still found by carving.
    let entries = nv.walk(
        &|name, deleted| is_image_name(name) && (deleted || include_existing),
        &mut prog,
    );
    let mut examined = 0;
    for e in &entries {
        if !e.deleted && !include_existing {
            continue;
        }
        let head = nv.head(e, 16);
        let head_fmt = ImageFormat::sniff(&head);
        let ext_fmt = ImageFormat::from_extension(&e.name);
        if !is_image_name(&e.name) && head_fmt == ImageFormat::Unknown {
            continue;
        }
        examined += 1;
        let plan = nv.plan(e);
        let runs = match &e.data {
            DataLocation::NonResident { runs, .. } => runs.len(),
            _ => 0,
        };
        let abs_offset = plan.pieces.iter().find_map(|p| p.0).map(|o| vol_offset + o);
        let meta = |r: &mut RecoveryResult| {
            r.source_path = Some(e.path.clone());
            r.source_offset = abs_offset;
            r.metadata.insert("fs".into(), "NTFS".into());
            r.metadata
                .insert("fs_volume_offset".into(), vol_offset.to_string());
            r.metadata.insert("mft_record".into(), e.record.to_string());
            r.metadata
                .insert("fs_recorded_size".into(), e.size.to_string());
            r.metadata.insert("data_runs".into(), runs.to_string());
            if let Some(m) = &e.modified {
                r.metadata.insert("fs_modified".into(), m.clone());
            }
            if let Some(c) = &e.created {
                r.metadata.insert("fs_created".into(), c.clone());
            }
            if e.orphan {
                r.warnings.push("The folder this file was in was also deleted and reused; only part of the original path is known.".into());
            }
            r.diagnostics.extend(plan.notes.iter().cloned());
        };
        let fmt = if head_fmt != ImageFormat::Unknown {
            head_fmt
        } else {
            ext_fmt
        };
        let unavailable = |why: &str| {
            let mut r = RecoveryResult::new(
                source_id,
                RecoveryMethod::FilesystemDeletedEntry,
                fmt,
                Classification::UnavailableSourceData,
            );
            r.limitations.push(why.to_string());
            r
        };
        let reject = |r: RecoveryResult, out: &mut Vec<RecoveryResult>| {
            let mut r = r;
            meta(&mut r);
            out.push(r);
        };
        if e.size > MAX_FS_FILE {
            reject(
                unavailable(&format!(
                    "File of {} bytes exceeds the {} byte read limit.",
                    e.size, MAX_FS_FILE
                )),
                out,
            );
            continue;
        }
        let name_out = format!("ntfs_{:x}_{}_{}", vol_offset, e.record, safe_name(&e.name));
        if !e.deleted {
            let Ok(bytes) = nv.read_entry(e, MAX_FS_FILE) else {
                continue;
            };
            let rel = format!(
                "recovered/{}/{}",
                Classification::ExistingOriginal.dir_name(),
                name_out
            );
            util::atomic_write(&case.root.join(&rel), &bytes)?;
            let mut r = classify::from_file_bytes(
                source_id,
                RecoveryMethod::FilesystemLiveEntry,
                Classification::ExistingOriginal,
                &e.path,
                Some(rel),
                &bytes,
                &e.name,
            );
            meta(&mut r);
            out.push(r);
            continue;
        }
        if e.encrypted || e.compressed {
            reject(
                unavailable(if e.encrypted {
                    "The file was EFS-encrypted; its data cannot be recovered without the user's certificate."
                } else {
                    "The file was NTFS-compressed; decompression of deleted compressed data is not supported."
                }),
                out,
            );
            continue;
        }
        if matches!(e.data, DataLocation::Missing)
            || (plan.pieces.is_empty() && !matches!(e.data, DataLocation::Resident(_)))
        {
            reject(
                unavailable("The deleted record no longer describes where its data was."),
                out,
            );
            continue;
        }
        if plan.first_cluster_allocated
            || (plan.clusters_total > 0 && plan.clusters_now_allocated == plan.clusters_total)
        {
            reject(unavailable("The clusters of this deleted file are now used by other data: its content was overwritten."), out);
            continue;
        }
        if head_fmt == ImageFormat::Unknown {
            let why = if !head.is_empty() && head.iter().all(|&b| b == 0) {
                "The deleted file's clusters now read as zeros. On an SSD this is typically TRIM erasing deleted data; on other drives the space was wiped or overwritten."
            } else {
                "Data at the file's recorded location is no longer an image: it was overwritten after deletion."
            };
            reject(unavailable(why), out);
            continue;
        }
        let bytes = nv.read_entry(e, MAX_FS_FILE)?;
        let probe = pr_validate::validate_bytes(&bytes, head_fmt, true);
        let class = if probe.status == ValidationStatus::Valid && plan.clusters_now_allocated == 0 {
            Classification::DeletedFileRecord
        } else {
            Classification::PartialImage
        };
        let rel = format!("recovered/{}/{}", class.dir_name(), name_out);
        util::atomic_write(&case.root.join(&rel), &bytes)?;
        let mut r = classify::from_file_bytes(
            source_id,
            RecoveryMethod::FilesystemDeletedEntry,
            class,
            &e.path,
            Some(rel),
            &bytes,
            &e.name,
        );
        meta(&mut r);
        r.limitations.push(
            "Recovered from a deleted NTFS MFT record using its preserved data-run list.".into(),
        );
        if class == Classification::PartialImage {
            r.limitations.push("The data does not fully validate: part of the file was probably overwritten after deletion.".into());
        }
        if plan.clusters_now_allocated > 0 {
            r.warnings.push(format!(
                "{} of {} clusters are now used by other data.",
                plan.clusters_now_allocated, plan.clusters_total
            ));
        }
        out.push(r);
    }
    Ok(examined)
}

/// Carve a thumbnail container file (e.g. `.thumbdata`) already copied to
/// the case. Returns the number of thumbnails recovered.
#[allow(clippy::too_many_arguments)]
pub fn carve_container(
    case: &Case,
    job_id: &str,
    source_id: &str,
    local: &Path,
    origin_path: &str,
    results_path: &Path,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<u64> {
    let src = FileSource::open(local)?;
    let prefix = format!(
        "thumbdata_{}",
        &pr_model::evidence::sha256_file(local, None, |_| {})?.0[..12]
    );
    let mut sink = CarveSink::for_container(
        case,
        &src,
        job_id,
        source_id,
        origin_path,
        &prefix,
        JsonlAppender::open(results_path)?,
        progress,
    );
    let cfg = ScanConfig {
        formats: FormatSet::jpeg_only(),
        ..Default::default()
    };
    let mut st = ScanState::default();
    scan(&src, &cfg, &mut st, cancel, &mut sink)?;
    Ok(st.stats.candidates)
}
