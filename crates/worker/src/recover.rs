//! Recuva-style recovery engine.
//!
//! * A scan only **reads** the drive and keeps results in memory — nothing
//!   is written anywhere until the user recovers files, so scanning cannot
//!   overwrite the deleted data it is looking for.
//! * Each found file gets a state, like Recuva: Excellent / Poor /
//!   Very poor / Unrecoverable, from how much of its data has been reused
//!   and whether its first bytes still look like that file type.
//! * Deep scan additionally searches the drive's free space for pictures
//!   whose filesystem records are gone.

use crate::drives;
use anyhow::{bail, Result};
use pr_carve::scanner::{scan, Found as Carved, ScanConfig, ScanSink, ScanState};
use pr_carve::{EndStatus, FormatSet};
use pr_fs::files::{list_files, FsFile};
use pr_fs::{analyze_layout, SupportLevel, VolumeKind};
use pr_model::model::ImageFormat;
use pr_model::source::{read_vec_at, ReadAt, SliceSource};
use pr_model::{is_cancelled, CancelFlag};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest file read for preview or recovery in one piece.
const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PREVIEW: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    All,
    Pictures,
    Music,
    Documents,
    Video,
    Compressed,
    Emails,
}

const PICTURES: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif",
    "avif", "psd", "raw", "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "ico",
    "svg",
];
const MUSIC: &[&str] = &[
    "mp3", "wav", "wma", "aac", "m4a", "flac", "ogg", "opus", "aiff", "amr", "mid", "midi",
];
const DOCUMENTS: &[&str] = &[
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "txt", "rtf", "odt", "ods", "odp", "csv",
    "xml", "html", "htm", "epub", "md", "pages", "key", "numbers",
];
const VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "mkv", "wmv", "flv", "webm", "mpg", "mpeg", "3gp", "mts", "m2ts",
    "vob", "ts",
];
const COMPRESSED: &[&str] = &[
    "zip", "rar", "7z", "gz", "tar", "bz2", "xz", "cab", "iso", "tgz",
];
const EMAILS: &[&str] = &["eml", "msg", "pst", "ost", "dbx", "mbox", "emlx"];

fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn category_of(name: &str) -> Option<Category> {
    let e = ext_of(name);
    let e = e.as_str();
    [
        (PICTURES, Category::Pictures),
        (MUSIC, Category::Music),
        (DOCUMENTS, Category::Documents),
        (VIDEO, Category::Video),
        (COMPRESSED, Category::Compressed),
        (EMAILS, Category::Emails),
    ]
    .into_iter()
    .find(|(l, _)| l.contains(&e))
    .map(|(_, c)| c)
}

fn matches(cat: Category, name: &str) -> bool {
    match cat {
        Category::All => !name.is_empty(),
        c => category_of(name) == Some(c),
    }
}

/// Does `head` start like a file with extension `ext`? `None` = no known
/// signature for this type (cannot tell).
fn signature_ok(ext: &str, head: &[u8]) -> Option<bool> {
    let starts = |p: &[u8]| head.starts_with(p);
    let at = |o: usize, p: &[u8]| head.len() >= o + p.len() && &head[o..o + p.len()] == p;
    Some(match ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => starts(&[0xFF, 0xD8, 0xFF]),
        "png" => starts(b"\x89PNG"),
        "gif" => starts(b"GIF8"),
        "bmp" => starts(b"BM"),
        "tif" | "tiff" | "dng" | "nef" | "arw" | "cr2" | "orf" | "srw" => {
            starts(b"II*\0") || starts(b"MM\0*") || starts(b"IIRO") || starts(b"IIU\0")
        }
        "webp" | "wav" | "avi" => starts(b"RIFF"),
        "heic" | "heif" | "avif" | "mp4" | "m4v" | "mov" | "m4a" | "3gp" | "cr3" => {
            at(4, b"ftyp") || at(4, b"moov") || at(4, b"mdat") || at(4, b"wide")
        }
        "psd" => starts(b"8BPS"),
        "pdf" => starts(b"%PDF"),
        "docx" | "xlsx" | "pptx" | "zip" | "odt" | "ods" | "odp" | "epub" => starts(b"PK\x03\x04"),
        "doc" | "xls" | "ppt" | "msg" => starts(&[0xD0, 0xCF, 0x11, 0xE0]),
        "rar" => starts(b"Rar!"),
        "7z" => starts(&[0x37, 0x7A, 0xBC, 0xAF]),
        "gz" | "tgz" => starts(&[0x1F, 0x8B]),
        "mp3" => starts(b"ID3") || (head.len() > 1 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0),
        "flac" => starts(b"fLaC"),
        "ogg" | "opus" => starts(b"OggS"),
        "mkv" | "webm" => starts(&[0x1A, 0x45, 0xDF, 0xA3]),
        "wmv" | "wma" => starts(&[0x30, 0x26, 0xB2, 0x75]),
        "pst" | "ost" => starts(b"!BDN"),
        _ => return None,
    })
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Excellent,
    Poor,
    VeryPoor,
    Unrecoverable,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataSource {
    /// Absolute `(offset, length)` pieces on the drive; `None` = zeros.
    Pieces {
        pieces: Vec<(Option<u64>, u64)>,
    },
    Resident {
        data: Vec<u8>,
    },
    /// Deep-scan candidate; `trailer` is appended for partial pictures.
    Carved {
        offset: u64,
        length: u64,
        trailer: Vec<u8>,
    },
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FoundFile {
    pub id: u64,
    pub drive: String,
    pub name: String,
    /// Windows-style display path, e.g. `D:\Photos\IMG_1.jpg`.
    pub path: String,
    pub size: u64,
    pub modified: Option<String>,
    pub created: Option<String>,
    pub state: State,
    pub comment: String,
    pub category: Option<Category>,
    /// `deleted`, `recycle_bin` (still in the Recycle Bin), `deep_scan`.
    pub origin: String,
    pub filesystem: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(skip_serializing)]
    pub source: DataSource,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScanRequest {
    pub drives: Vec<String>,
    pub category: Category,
    #[serde(default)]
    pub deep_scan: bool,
    /// Only files whose original folder starts with this (e.g. `D:\Photos`).
    #[serde(default)]
    pub folder: Option<String>,
    /// Only files that were in the Recycle Bin.
    #[serde(default)]
    pub recycle_bin_only: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ScanSummary {
    pub found: usize,
    pub by_state: HashMap<String, usize>,
    pub notes: Vec<String>,
    pub seconds: f64,
}

/// Progress callback: (drive, phase, done, total, message).
pub type ProgressFn<'a> = dyn Fn(&str, &str, u64, u64, &str) + Sync + 'a;

fn state_for(f: &FsFile, ext: &str, head: &[u8]) -> (State, String) {
    if let Some(why) = &f.unreadable {
        return (State::Unrecoverable, why.clone());
    }
    if f.size == 0 {
        return (State::Unrecoverable, "The file is empty (0 bytes).".into());
    }
    if f.first_cluster_reused {
        return (
            State::Unrecoverable,
            "Overwritten: its space now belongs to another file.".into(),
        );
    }
    if f.pieces.is_empty() && f.resident.is_none() {
        return (
            State::Unrecoverable,
            "No record of where its data was.".into(),
        );
    }
    if signature_ok(ext, head) == Some(false) {
        return if !head.is_empty() && head.iter().all(|&b| b == 0) {
            (
                State::Unrecoverable,
                "The data reads as zeros: erased (on SSDs this is TRIM) or overwritten.".into(),
            )
        } else {
            (State::VeryPoor, "The start of the file no longer looks like this file type: it was probably overwritten.".into())
        };
    }
    let total = f.clusters_total.max(1);
    let pct = f.clusters_reused * 100 / total;
    match f.clusters_reused {
        0 if f.exact => (State::Excellent, "No overwritten clusters detected.".into()),
        0 => (
            State::Excellent,
            "No overwritten clusters detected (location assumed contiguous).".into(),
        ),
        _ if pct < 50 => (
            State::Poor,
            format!(
                "{} of {} clusters overwritten.",
                f.clusters_reused, f.clusters_total
            ),
        ),
        _ => (
            State::VeryPoor,
            format!(
                "{} of {} clusters overwritten.",
                f.clusters_reused, f.clusters_total
            ),
        ),
    }
}

fn display_path(drive: &str, inner: &str) -> String {
    let d = if drive.ends_with(':') {
        drive.to_string()
    } else {
        format!("[{drive}]")
    };
    format!("{d}{}", inner.replace('/', "\\"))
}

/// `D:\Photos` → (`D:`, `/photos`), lower-cased for comparison.
fn split_folder(folder: &str) -> (Option<String>, String) {
    let f = folder.replace('\\', "/");
    let (drive, rest) = match f.as_bytes() {
        [l, b':', ..] if l.is_ascii_alphabetic() => (
            Some(format!("{}:", (*l as char).to_ascii_uppercase())),
            f[2..].to_string(),
        ),
        _ => (None, f),
    };
    let rest = format!("/{}", rest.trim_matches('/')).to_ascii_lowercase();
    (drive, if rest == "/" { String::new() } else { rest })
}

// ----- Recycle Bin ($R data files and $I info files) -----

fn in_recycle_bin(path: &str) -> bool {
    path.to_ascii_lowercase().contains("/$recycle.bin/")
}

fn recycle_kind(name: &str) -> Option<char> {
    let b = name.as_bytes();
    if b.len() > 3 && b[0] == b'$' && b.iter().filter(|&&c| c == b'.').count() <= 1 {
        return match b[1] {
            b'R' | b'r' => Some('R'),
            b'I' | b'i' => Some('I'),
            _ => None,
        };
    }
    None
}

fn recycle_key(path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    format!(
        "{}/{}",
        dir.to_ascii_lowercase(),
        name.get(2..).unwrap_or("").to_ascii_lowercase()
    )
}

/// Original path from a Recycle Bin `$I` file (Vista–8.1: v1, 10/11: v2).
pub fn parse_recycle_info(b: &[u8]) -> Option<String> {
    let version = u64::from_le_bytes(b.get(..8)?.try_into().ok()?);
    let raw = match version {
        1 => b.get(24..24 + 520)?,
        2 => {
            let n = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            if n == 0 || n > 32_768 {
                return None;
            }
            b.get(28..28 + 2 * n)?
        }
        _ => return None,
    };
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    let s = String::from_utf16_lossy(&units);
    (!s.is_empty()).then_some(s)
}

/// Scan one source (a drive, or an image file in tests).
#[allow(clippy::too_many_arguments)]
pub fn scan_source(
    drive: &str,
    src: Arc<dyn ReadAt>,
    req: &ScanRequest,
    next_id: &mut u64,
    cancel: &CancelFlag,
    progress: &ProgressFn,
    out: &mut Vec<FoundFile>,
    notes: &mut Vec<String>,
) -> Result<()> {
    let (folder_drive, folder_prefix) = req
        .folder
        .as_deref()
        .map(split_folder)
        .unwrap_or((None, String::new()));
    if folder_drive
        .as_deref()
        .is_some_and(|d| !d.eq_ignore_ascii_case(drive))
    {
        return Ok(());
    }
    let cat = req.category;
    let layout = analyze_layout(src.as_ref());
    for n in &layout.notes {
        notes.push(format!("{drive}: {n}"));
    }
    let mut allocated: Vec<(u64, u64)> = Vec::new();
    let mut known_alloc = false;
    let mut fs_offsets: HashSet<u64> = HashSet::new();

    for v in &layout.volumes {
        if is_cancelled(cancel) {
            return Ok(());
        }
        if v.support != SupportLevel::DeletedRecordRecovery {
            notes.push(format!(
                "{drive}: {} — file records cannot be read{}",
                match v.kind {
                    VolumeKind::Unknown =>
                        "no recognised filesystem (RAW, damaged or encrypted)".to_string(),
                    k => format!("{k:?} filesystem"),
                },
                if req.deep_scan {
                    "; deep scan searches it for pictures"
                } else {
                    "; try Deep Scan"
                }
            ));
            continue;
        }
        let vol = SliceSource::new(src.clone(), v.offset, v.length);
        let want = |name: &str, deleted: bool| {
            recycle_kind(name).is_some() || (deleted && matches(cat, name))
        };
        let mut prog = |d: u64, t: u64| progress(drive, "records", d, t, "reading file records");
        let listing = match list_files(&vol, v.kind, &want, &mut prog) {
            Ok(l) => l,
            Err(e) => {
                notes.push(format!(
                    "{drive}: the {:?} filesystem could not be read: {e}",
                    v.kind
                ));
                continue;
            }
        };
        if !listing.allocated.is_empty() {
            known_alloc = true;
            allocated.extend(
                listing
                    .allocated
                    .iter()
                    .map(|&(s, e)| (v.offset + s, (v.offset + e).min(v.offset + v.length))),
            );
        }
        // Original names of Recycle Bin items, from $I files (kept even
        // after the bin is emptied, until their record is reused).
        let mut originals: HashMap<String, String> = HashMap::new();
        for f in listing
            .files
            .iter()
            .filter(|f| in_recycle_bin(&f.path) && recycle_kind(&f.name) == Some('I'))
        {
            if let Some(o) = f
                .read(&vol, 64 * 1024)
                .ok()
                .and_then(|b| parse_recycle_info(&b))
            {
                let k = recycle_key(&f.path);
                if f.deleted {
                    originals.entry(k).or_insert(o);
                } else {
                    originals.insert(k, o);
                }
            }
        }
        for f in &listing.files {
            let bin = in_recycle_bin(&f.path) && recycle_kind(&f.name) == Some('R');
            if recycle_kind(&f.name) == Some('I') || (!f.deleted && !bin) {
                continue;
            }
            if req.recycle_bin_only && !bin {
                continue;
            }
            let original = bin
                .then(|| originals.get(&recycle_key(&f.path)).cloned())
                .flatten();
            let (name, inner_path) = match &original {
                Some(o) => {
                    let (_, p) = split_folder(o);
                    let name = o.rsplit(['\\', '/']).next().unwrap_or(&f.name).to_string();
                    let inner = o
                        .get(2..)
                        .map(|s| s.replace('\\', "/"))
                        .unwrap_or_else(|| f.path.clone());
                    let _ = p;
                    (name, inner)
                }
                None => (f.name.clone(), f.path.clone()),
            };
            if !matches(cat, &name) {
                continue;
            }
            if !folder_prefix.is_empty() {
                let dir = inner_path
                    .rsplit_once('/')
                    .map(|(d, _)| d)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !(dir == folder_prefix || dir.starts_with(&format!("{folder_prefix}/"))) {
                    continue;
                }
            }
            let ext = ext_of(&name);
            let head = f.head(&vol, 16);
            let (mut state, mut comment) = state_for(f, &ext, &head);
            let origin = if bin && !f.deleted {
                state = State::Excellent;
                comment = "In the Recycle Bin (not emptied yet).".into();
                "recycle_bin"
            } else {
                "deleted"
            };
            if bin && f.deleted {
                comment = format!("{comment} It was in the Recycle Bin, which was emptied.");
            }
            if bin && original.is_none() {
                comment = format!("{comment} Original name unknown.");
            }
            let pieces: Vec<(Option<u64>, u64)> = f
                .pieces
                .iter()
                .map(|&(o, l)| (o.map(|o| v.offset + o), l))
                .collect();
            if let Some((Some(o), _)) = pieces.first() {
                fs_offsets.insert(*o);
            }
            *next_id += 1;
            out.push(FoundFile {
                id: *next_id,
                drive: drive.to_string(),
                path: display_path(drive, &inner_path),
                name,
                size: f.size,
                modified: f.modified.clone(),
                created: f.created.clone(),
                state,
                comment,
                category: category_of(&f.name).or(category_of(&ext)),
                origin: origin.into(),
                filesystem: listing.filesystem.into(),
                width: None,
                height: None,
                source: match &f.resident {
                    Some(d) => DataSource::Resident { data: d.clone() },
                    None => DataSource::Pieces { pieces },
                },
            });
        }
    }

    // Deep scan: pictures in free space (or everywhere if no filesystem).
    if req.deep_scan
        && matches!(cat, Category::All | Category::Pictures)
        && !req.recycle_bin_only
        && !is_cancelled(cancel)
    {
        allocated.sort_unstable();
        let mut ranges = Vec::new();
        let mut pos = 0u64;
        for (s, e) in allocated {
            if s > pos {
                ranges.push((pos, s));
            }
            pos = pos.max(e);
        }
        if pos < src.len() {
            ranges.push((pos, src.len()));
        }
        let free: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
        if !known_alloc {
            notes.push(format!("{drive}: deep scan covered the whole drive ({:.1} GB) because free space could not be determined.", src.len() as f64 / 1e9));
        }
        let cfg = ScanConfig {
            formats: FormatSet::default(),
            ranges: Some(ranges),
            ..Default::default()
        };
        struct Sink<'a> {
            src: &'a dyn ReadAt,
            drive: &'a str,
            skip: &'a HashSet<u64>,
            found: Vec<FoundFile>,
            next_id: &'a mut u64,
            progress: &'a ProgressFn<'a>,
            free: u64,
            done: u64,
            last: u64,
        }
        impl ScanSink for Sink<'_> {
            fn on_candidate(&mut self, c: Carved) -> std::io::Result<()> {
                let p = &c.candidate;
                if c.embedded_in.is_some() || self.skip.contains(&p.offset) {
                    return Ok(());
                }
                let complete = p.status == EndStatus::Complete;
                let trailer = pr_carve::export::partial_trailer(p).to_vec();
                let (mut w, mut h) = (p.width, p.height);
                let state = if p.length <= MAX_PREVIEW {
                    let mut data = read_vec_at(self.src, p.offset, p.length as usize)?;
                    data.extend_from_slice(&trailer);
                    let v = pr_validate::validate_bytes(&data, p.format, complete);
                    w = v.width.or(w);
                    h = v.height.or(h);
                    match v.status {
                        pr_model::model::ValidationStatus::Valid
                        | pr_model::model::ValidationStatus::ValidWithWarnings => State::Excellent,
                        pr_model::model::ValidationStatus::StructureOnly => State::Poor,
                        _ if !complete => State::Poor,
                        _ => return Ok(()), // complete structure but undecodable: noise
                    }
                } else {
                    State::Poor
                };
                if w.zip(h).is_some_and(|(w, h)| w.max(h) <= 160) {
                    return Ok(()); // tiny previews/icons: not useful results
                }
                *self.next_id += 1;
                let ext = p.format.extension();
                self.found.push(FoundFile {
                    id: *self.next_id,
                    drive: self.drive.to_string(),
                    name: format!("deep_{:012x}.{ext}", p.offset),
                    path: format!(
                        "{}\\(deep scan)\\deep_{:012x}.{ext}",
                        display_path(self.drive, ""),
                        p.offset
                    ),
                    size: p.length,
                    modified: None,
                    created: None,
                    state,
                    comment: if complete {
                        "Found by deep scan in free space; original name and folder unknown.".into()
                    } else {
                        format!(
                            "Found by deep scan; incomplete ({}).",
                            p.end_reason.clone().unwrap_or_default()
                        )
                    },
                    category: Some(Category::Pictures),
                    origin: "deep_scan".into(),
                    filesystem: String::new(),
                    width: w,
                    height: h,
                    source: DataSource::Carved {
                        offset: p.offset,
                        length: p.length,
                        trailer,
                    },
                });
                Ok(())
            }
            fn on_checkpoint(&mut self, st: &ScanState, _total: u64) -> std::io::Result<()> {
                self.done = st.stats.bytes_scanned.min(self.free);
                if self.done - self.last >= 64 << 20 || self.done == self.free {
                    self.last = self.done;
                    (self.progress)(
                        self.drive,
                        "deep",
                        self.done,
                        self.free,
                        &format!("{} pictures found", self.found.len()),
                    );
                }
                Ok(())
            }
        }
        let mut sink = Sink {
            src: src.as_ref(),
            drive,
            skip: &fs_offsets,
            found: Vec::new(),
            next_id,
            progress,
            free,
            done: 0,
            last: 0,
        };
        let mut st = ScanState::default();
        scan(src.as_ref(), &cfg, &mut st, cancel, &mut sink)?;
        out.append(&mut sink.found);
    }
    Ok(())
}

/// Scan the requested drives.
pub fn scan_drives(
    req: &ScanRequest,
    cancel: &CancelFlag,
    progress: &ProgressFn,
) -> Result<(Vec<FoundFile>, ScanSummary)> {
    let t = std::time::Instant::now();
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let mut next_id = 0u64;
    if req.drives.is_empty() {
        bail!("choose at least one drive");
    }
    for d in &req.drives {
        if is_cancelled(cancel) {
            break;
        }
        let path = drives::device_path(d)?;
        let src: Arc<dyn ReadAt> = match pr_model::device::open_device(&path) {
            Ok(s) => Arc::new(s),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                bail!("Windows refused access to {d}. PhotoRecovery must run as administrator to scan drives.")
            }
            Err(e) => {
                notes.push(format!("{d}: could not be opened ({e})."));
                continue;
            }
        };
        progress(d, "records", 0, 1, "starting");
        scan_source(
            d,
            src,
            req,
            &mut next_id,
            cancel,
            progress,
            &mut out,
            &mut notes,
        )?;
    }
    let mut s = ScanSummary {
        found: out.len(),
        notes,
        seconds: t.elapsed().as_secs_f64(),
        ..Default::default()
    };
    for f in &out {
        *s.by_state
            .entry(format!("{:?}", f.state).to_lowercase())
            .or_default() += 1;
    }
    Ok((out, s))
}

/// Read a found file's data from its drive.
pub fn read_found(f: &FoundFile, src: &dyn ReadAt, max: u64) -> Result<Vec<u8>> {
    Ok(match &f.source {
        DataSource::Resident { data } => data.clone(),
        DataSource::Pieces { pieces } => {
            let total: u64 = pieces.iter().map(|p| p.1).sum();
            if total > max {
                bail!("file is larger than {max} bytes");
            }
            let mut out = Vec::with_capacity(total as usize);
            for (off, len) in pieces {
                match off {
                    Some(o) => out.extend_from_slice(&read_vec_at(src, *o, *len as usize)?),
                    None => out.resize(out.len() + *len as usize, 0),
                }
            }
            out
        }
        DataSource::Carved {
            offset,
            length,
            trailer,
        } => {
            if *length > max {
                bail!("file is larger than {max} bytes");
            }
            let mut d = read_vec_at(src, *offset, *length as usize)?;
            d.extend_from_slice(trailer);
            d
        }
        DataSource::None => bail!("no data"),
    })
}

/// A small JPEG thumbnail of a found picture (for the preview pane).
pub fn preview(f: &FoundFile, src: &dyn ReadAt) -> Result<Vec<u8>> {
    if f.category != Some(Category::Pictures) {
        bail!("no preview for this file type");
    }
    let data = read_found(f, src, MAX_PREVIEW)?;
    let fmt = ImageFormat::sniff(&data);
    let img = image::load_from_memory(&data)
        .map_err(|e| anyhow::anyhow!("cannot display this picture ({e}); fmt {fmt:?}"))?;
    let t = img.thumbnail(640, 640).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&t)?;
    Ok(out)
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct RecoverSummary {
    pub recovered: usize,
    pub failed: Vec<String>,
    pub bytes: u64,
    pub destination: String,
}

fn unique_path(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|i| p.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

fn safe_component(s: &str) -> String {
    let mut c: String = s
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/') {
                '_'
            } else {
                c
            }
        })
        .collect();
    while c.ends_with(['.', ' ']) {
        c.pop();
    }
    if c.is_empty() || c == "." || c == ".." {
        c = "_".into();
    }
    c.chars().take(200).collect()
}

/// Parse our timestamp formats into a system time for the recovered file.
fn parse_time(s: &str) -> Option<std::time::SystemTime> {
    use time::{
        format_description::well_known::Rfc3339, macros::format_description, PrimitiveDateTime,
    };
    let t = time::OffsetDateTime::parse(s, &Rfc3339).ok().or_else(|| {
        PrimitiveDateTime::parse(
            s,
            format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        )
        .ok()
        .map(|p| p.assume_utc())
    })?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.unix_timestamp().max(0) as u64))
}

/// Write the selected files to `dest`. Refuses a destination on a drive
/// the files come from: writing there could overwrite them.
pub fn recover(
    files: &[&FoundFile],
    dest: &Path,
    keep_folders: bool,
    open: &dyn Fn(&str) -> Result<Arc<dyn ReadAt>>,
    same_drive: &dyn Fn(&Path, &str) -> bool,
    cancel: &CancelFlag,
    progress: &dyn Fn(u64, u64),
) -> Result<RecoverSummary> {
    if !dest.is_absolute() {
        bail!("choose a destination folder");
    }
    for d in files
        .iter()
        .map(|f| f.drive.as_str())
        .collect::<HashSet<_>>()
    {
        if same_drive(dest, d) {
            bail!("The destination is on {d}, the drive you are recovering from. Saving there can overwrite the deleted files you want back. Choose a folder on another drive (for example a USB stick).");
        }
    }
    std::fs::create_dir_all(dest)?;
    let mut sources: HashMap<String, Arc<dyn ReadAt>> = HashMap::new();
    let mut s = RecoverSummary {
        destination: dest.display().to_string(),
        ..Default::default()
    };
    for (i, f) in files.iter().enumerate() {
        if is_cancelled(cancel) {
            break;
        }
        progress(i as u64, files.len() as u64);
        let src = match sources.get(&f.drive) {
            Some(s) => s.clone(),
            None => {
                let s = open(&f.drive)?;
                sources.insert(f.drive.clone(), s.clone());
                s
            }
        };
        let target = if keep_folders {
            let mut p = dest.to_path_buf();
            let parts: Vec<&str> = f.path.split('\\').collect();
            for part in &parts[..parts.len().saturating_sub(1)] {
                p.push(safe_component(part.trim_end_matches(':')));
            }
            p.join(safe_component(&f.name))
        } else {
            dest.join(safe_component(&f.name))
        };
        let result = (|| -> Result<u64> {
            let data = read_found(f, src.as_ref(), MAX_FILE)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let path = unique_path(target.clone());
            std::fs::write(&path, &data)?;
            if let Some(t) = f.modified.as_deref().and_then(parse_time) {
                if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                    let _ = file.set_modified(t);
                }
            }
            Ok(data.len() as u64)
        })();
        match result {
            Ok(n) => {
                s.recovered += 1;
                s.bytes += n;
            }
            Err(e) => s.failed.push(format!("{}: {e:#}", f.path)),
        }
    }
    progress(files.len() as u64, files.len() as u64);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_and_signatures() {
        assert_eq!(category_of("IMG_1.JPG"), Some(Category::Pictures));
        assert_eq!(category_of("report.docx"), Some(Category::Documents));
        assert_eq!(category_of("song.mp3"), Some(Category::Music));
        assert_eq!(category_of("noext"), None);
        assert_eq!(signature_ok("jpg", &[0xFF, 0xD8, 0xFF, 0xE1]), Some(true));
        assert_eq!(signature_ok("pdf", b"\0\0\0\0"), Some(false));
        assert_eq!(signature_ok("xyz", b"abcd"), None);
    }

    #[test]
    fn folder_filter_parsing() {
        assert_eq!(
            split_folder(r"D:\Photos\2024\"),
            (Some("D:".into()), "/photos/2024".into())
        );
        assert_eq!(split_folder("e:"), (Some("E:".into()), String::new()));
        assert_eq!(display_path("D:", "/Photos/a.jpg"), r"D:\Photos\a.jpg");
    }

    #[test]
    fn recycle_info_v2() {
        let path = r"C:\Users\Ann\Pictures\cat.jpg";
        let mut b = Vec::new();
        b.extend_from_slice(&2u64.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&((path.len() + 1) as u32).to_le_bytes());
        for u in path.encode_utf16().chain([0]) {
            b.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_recycle_info(&b).as_deref(), Some(path));
        assert_eq!(recycle_kind("$RAB12CD.jpg"), Some('R'));
        assert_eq!(recycle_kind("$IAB12CD.jpg"), Some('I'));
        assert_eq!(
            recycle_key("/$Recycle.Bin/S-1/$RAB12CD.jpg"),
            recycle_key("/$Recycle.Bin/S-1/$IAB12CD.jpg")
        );
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_component("a:b?.jpg"), "a_b_.jpg");
        assert_eq!(safe_component(".."), "_");
    }
}
