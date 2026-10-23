//! Structure-aware raw image carving (spec 4.E).
//!
//! The scanner streams the source in fixed-size chunks (bounded memory),
//! finds format signatures — including signatures that straddle chunk
//! boundaries — and hands every hit to a format parser that walks the real
//! file structure (JPEG segments and entropy-coded data, PNG chunks with
//! CRCs, RIFF chunks, ISO-BMFF boxes). Parsers decide where a file ends and
//! whether it is complete, truncated or corrupt; they never trust a bare
//! header/footer pair.
//!
//! Fragmented files are *not* reassembled. A fragmentation point is detected
//! where the structure breaks (invalid marker, restart-marker sequence
//! break, CRC failure, zero-filled run) and everything before it is reported
//! as a partial candidate.

pub mod export;
pub mod heif;
pub mod jpeg;
pub mod png;
pub mod scanner;
pub mod webp;

use pr_model::model::ImageFormat;
use serde::{Deserialize, Serialize};

/// Hard upper bounds that keep every parser's work and output bounded.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CarveLimits {
    pub max_jpeg: u64,
    pub max_png: u64,
    pub max_webp: u64,
    pub max_heif: u64,
    /// A run of this many 0x00 bytes inside JPEG entropy data is treated as
    /// the end of valid data (trimmed / zero-filled blocks).
    pub zero_run_cutoff: u64,
    /// A shorter zero run is remembered as a *suspected* end: if the file
    /// later fails to reach EOI, the partial candidate is cut there instead
    /// of at the failure point. Complete files are unaffected.
    pub zero_run_soft: u64,
    /// Maximum number of segments/chunks/boxes walked per candidate.
    pub max_structures: u32,
    /// Maximum bytes of ISO-BMFF `meta` box parsed into memory.
    pub max_meta_box: u64,
}

impl Default for CarveLimits {
    fn default() -> Self {
        CarveLimits {
            max_jpeg: 128 * 1024 * 1024,
            max_png: 256 * 1024 * 1024,
            max_webp: 64 * 1024 * 1024,
            max_heif: 128 * 1024 * 1024,
            zero_run_cutoff: 64 * 1024,
            zero_run_soft: 4096,
            max_structures: 100_000,
            max_meta_box: 4 * 1024 * 1024,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndStatus {
    /// The format's end structure (EOI, IEND, RIFF size, box walk) was reached.
    Complete,
    /// Source data or the length limit ended before the end structure.
    Truncated,
    /// The structure broke part way through; data before the break is kept.
    Corrupt,
}

/// A parsed candidate. Offsets are absolute in the scanned source.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParsedCandidate {
    pub format: ImageFormat,
    pub offset: u64,
    /// Number of source bytes belonging to the candidate.
    pub length: u64,
    pub status: EndStatus,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Why the candidate is truncated/corrupt (None when complete).
    pub end_reason: Option<String>,
    pub diagnostics: Vec<String>,
    /// Absolute byte ranges inside this candidate where an embedded image
    /// (e.g. an EXIF thumbnail in APP1) may legitimately start.
    pub embedded_windows: Vec<(u64, u64)>,
}

impl ParsedCandidate {
    pub(crate) fn new(format: ImageFormat, offset: u64) -> Self {
        ParsedCandidate {
            format,
            offset,
            length: 0,
            status: EndStatus::Complete,
            width: None,
            height: None,
            end_reason: None,
            diagnostics: Vec::new(),
            embedded_windows: Vec::new(),
        }
    }

    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseOutcome {
    Candidate(ParsedCandidate),
    /// Signature matched but the structure is not a usable image.
    Rejected(String),
}

/// Formats enabled for a scan.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatSet {
    pub jpeg: bool,
    pub png: bool,
    pub webp: bool,
    pub heif: bool,
}

impl Default for FormatSet {
    fn default() -> Self {
        FormatSet {
            jpeg: true,
            png: true,
            webp: true,
            heif: true,
        }
    }
}

impl FormatSet {
    pub fn jpeg_only() -> Self {
        FormatSet {
            jpeg: true,
            png: false,
            webp: false,
            heif: false,
        }
    }

    /// Parse a comma-separated list such as `jpeg,png`.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut s = FormatSet {
            jpeg: false,
            png: false,
            webp: false,
            heif: false,
        };
        for item in list.split(',').map(|x| x.trim().to_ascii_lowercase()) {
            match item.as_str() {
                "jpeg" | "jpg" => s.jpeg = true,
                "png" => s.png = true,
                "webp" => s.webp = true,
                "heic" | "heif" | "avif" => s.heif = true,
                "all" => s = FormatSet::default(),
                "" => {}
                other => return Err(format!("unsupported format '{other}'")),
            }
        }
        Ok(s)
    }
}
