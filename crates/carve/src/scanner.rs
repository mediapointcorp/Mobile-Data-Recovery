//! Streaming, resumable signature scanner.
//!
//! Memory use is `chunk_size + OVERLAP` for the scan buffer plus one
//! fixed-size cursor buffer per active parser, independent of source size.

use crate::heif::{heif_brand_format, parse_heif};
use crate::jpeg::parse_jpeg;
use crate::png::{parse_png, SIGNATURE as PNG_SIG};
use crate::webp::parse_webp;
use crate::{CarveLimits, FormatSet, ParseOutcome, ParsedCandidate};
use memchr::memmem;
use pr_model::model::ImageFormat;
use pr_model::source::{read_full_at, ReadAt};
use pr_model::{is_cancelled, CancelFlag};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

/// Bytes re-read past each chunk so that signatures (max 12 bytes) that
/// straddle a chunk boundary are still matched in the chunk where they start.
pub const OVERLAP: usize = 32;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScanConfig {
    pub formats: FormatSet,
    pub chunk_size: usize,
    pub limits: CarveLimits,
    /// Absolute offset at which scanning stops (default: end of source).
    pub end_offset: Option<u64>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            formats: FormatSet::default(),
            chunk_size: 8 * 1024 * 1024,
            limits: CarveLimits::default(),
            end_offset: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ScanStats {
    pub bytes_scanned: u64,
    pub signatures: u64,
    pub candidates: u64,
    pub rejected: u64,
    /// Signature hits inside an already carved complete file (skipped).
    pub internal_skipped: u64,
    pub rejected_reasons: BTreeMap<String, u64>,
}

/// An embedded-image window carried across chunks: `(start, end, parent)`.
pub type EmbeddedWindow = (u64, u64, u64);

/// Resumable scanner state; serialised into job checkpoints.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ScanState {
    pub next_offset: u64,
    pub covered_until: u64,
    pub embedded_windows: Vec<EmbeddedWindow>,
    pub stats: ScanStats,
}

impl ScanState {
    pub fn starting_at(offset: u64) -> Self {
        ScanState {
            next_offset: offset,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub candidate: ParsedCandidate,
    /// Offset of the complete parent image this candidate is embedded in.
    pub embedded_in: Option<u64>,
}

pub trait ScanSink {
    fn on_candidate(&mut self, found: Found) -> io::Result<()>;
    /// Called after each chunk; `state` is safe to persist and resume from.
    fn on_checkpoint(&mut self, state: &ScanState, total: u64) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutcome {
    Completed,
    Cancelled,
}

/// Collects candidates in memory; convenient for tests and small sources.
#[derive(Default)]
pub struct VecSink {
    pub found: Vec<Found>,
    pub checkpoints: u64,
}

impl ScanSink for VecSink {
    fn on_candidate(&mut self, found: Found) -> io::Result<()> {
        self.found.push(found);
        Ok(())
    }
    fn on_checkpoint(&mut self, _: &ScanState, _: u64) -> io::Result<()> {
        self.checkpoints += 1;
        Ok(())
    }
}

/// Parse a candidate of a given format at `offset`.
pub fn parse_at(src: &dyn ReadAt, format: ImageFormat, offset: u64, limits: &CarveLimits) -> io::Result<ParseOutcome> {
    match format {
        ImageFormat::Jpeg => parse_jpeg(src, offset, limits),
        ImageFormat::Png => parse_png(src, offset, limits),
        ImageFormat::Webp => parse_webp(src, offset, limits),
        ImageFormat::Heic | ImageFormat::Avif => parse_heif(src, offset, limits),
        _ => Ok(ParseOutcome::Rejected("unsupported format".into())),
    }
}

struct Matchers {
    png: memmem::Finder<'static>,
    riff: memmem::Finder<'static>,
    ftyp: memmem::Finder<'static>,
}

impl Matchers {
    fn new() -> Self {
        Matchers {
            png: memmem::Finder::new(&PNG_SIG),
            riff: memmem::Finder::new(b"RIFF"),
            ftyp: memmem::Finder::new(b"ftyp"),
        }
    }
}

/// Find signature starts `s` with `base + s` in `[base, region_end)`.
fn find_signatures(m: &Matchers, buf: &[u8], base: u64, region_len: usize, f: FormatSet) -> Vec<(u64, ImageFormat)> {
    let mut hits = Vec::new();
    if f.jpeg {
        for p in memchr::memchr_iter(0xFF, buf) {
            if p >= region_len {
                break;
            }
            if p + 2 < buf.len() && buf[p + 1] == 0xD8 && buf[p + 2] == 0xFF {
                hits.push((base + p as u64, ImageFormat::Jpeg));
            }
        }
    }
    if f.png {
        for p in m.png.find_iter(buf) {
            if p < region_len {
                hits.push((base + p as u64, ImageFormat::Png));
            }
        }
    }
    if f.webp {
        for p in m.riff.find_iter(buf) {
            if p < region_len && p + 12 <= buf.len() && &buf[p + 8..p + 12] == b"WEBP" {
                hits.push((base + p as u64, ImageFormat::Webp));
            }
        }
    }
    if f.heif {
        for p in m.ftyp.find_iter(buf) {
            if p < 4 || p - 4 >= region_len || p + 8 > buf.len() {
                continue;
            }
            if let Some(fmt) = heif_brand_format(&buf[p + 4..p + 8]) {
                hits.push((base + (p - 4) as u64, fmt));
            }
        }
    }
    hits.sort_by_key(|h| h.0);
    hits
}

/// Scan `src` from `state.next_offset`, reporting candidates to `sink`.
/// Returns `Cancelled` if the flag is raised; `state` then describes the
/// last checkpoint and can be passed back in to resume.
pub fn scan(
    src: &dyn ReadAt,
    cfg: &ScanConfig,
    state: &mut ScanState,
    cancel: &CancelFlag,
    sink: &mut dyn ScanSink,
) -> io::Result<ScanOutcome> {
    let end = cfg.end_offset.unwrap_or(u64::MAX).min(src.len());
    let chunk = cfg.chunk_size.max(64);
    let mut buf = vec![0u8; chunk + OVERLAP];
    let matchers = Matchers::new();

    while state.next_offset < end {
        if is_cancelled(cancel) {
            return Ok(ScanOutcome::Cancelled);
        }
        let pos = state.next_offset;
        // Skip the interior of a carved file when no embedded window remains.
        if state.covered_until > pos && state.embedded_windows.iter().all(|w| w.1 <= pos) {
            let to = state.covered_until.min(end);
            state.stats.bytes_scanned += to - pos;
            state.next_offset = to;
            state.embedded_windows.clear();
            continue;
        }
        let region_end = (pos + chunk as u64).min(end);
        let region_len = (region_end - pos) as usize;
        let want = (region_len + OVERLAP).min((src.len() - pos) as usize);
        let n = read_full_at(src, pos, &mut buf[..want])?;
        let hits = find_signatures(&matchers, &buf[..n], pos, region_len.min(n), cfg.formats);

        for (off, fmt) in hits {
            let parent = state
                .embedded_windows
                .iter()
                .find(|w| off >= w.0 && off < w.1)
                .map(|w| w.2);
            if off < state.covered_until && parent.is_none() {
                state.stats.internal_skipped += 1;
                continue;
            }
            state.stats.signatures += 1;
            match parse_at(src, fmt, off, &cfg.limits)? {
                ParseOutcome::Rejected(reason) => {
                    state.stats.rejected += 1;
                    let key = format!("{}: {}", fmt.extension(), reason_key(&reason));
                    if state.stats.rejected_reasons.len() < 64 || state.stats.rejected_reasons.contains_key(&key) {
                        *state.stats.rejected_reasons.entry(key).or_default() += 1;
                    }
                }
                ParseOutcome::Candidate(c) => {
                    state.stats.candidates += 1;
                    if parent.is_none() {
                        state.covered_until = state.covered_until.max(c.end());
                        state
                            .embedded_windows
                            .extend(c.embedded_windows.iter().map(|w| (w.0, w.1, c.offset)));
                    }
                    sink.on_candidate(Found {
                        candidate: c,
                        embedded_in: parent,
                    })?;
                }
            }
        }
        state.embedded_windows.retain(|w| w.1 > region_end);
        state.stats.bytes_scanned += region_end - pos;
        state.next_offset = region_end;
        sink.on_checkpoint(state, end)?;
    }
    Ok(ScanOutcome::Completed)
}

/// Strip offsets/numbers so rejection reasons aggregate into few buckets.
fn reason_key(reason: &str) -> String {
    let mut out = String::new();
    let mut last_digit = false;
    for ch in reason.chars() {
        if ch.is_ascii_hexdigit() && (last_digit || ch.is_ascii_digit()) {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            last_digit = false;
            out.push(ch);
        }
    }
    out.chars().take(80).collect()
}
