//! Export of carved candidates to a separate destination.

use crate::{EndStatus, ParsedCandidate};
use pr_model::model::ImageFormat;
use pr_model::source::{read_full_at, ReadAt};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct ExportInfo {
    pub bytes_written: u64,
    /// SHA-256 of the exported file (including any trailer).
    pub sha256: String,
    /// SHA-256 of the source byte range only.
    pub source_sha256: String,
    /// True when an end marker was appended so decoders accept a partial file.
    pub trailer_appended: bool,
}

/// End-of-file trailer appended to partial candidates on export.
pub fn partial_trailer(c: &ParsedCandidate) -> &'static [u8] {
    if c.status == EndStatus::Complete {
        return &[];
    }
    match c.format {
        ImageFormat::Jpeg => &crate::jpeg::EOI_TRAILER,
        ImageFormat::Png => &crate::png::IEND_TRAILER,
        _ => &[],
    }
}

/// Copy `[offset, offset+length)` of `src` to a new file at `dest`.
/// Refuses to overwrite an existing file unless `overwrite` is set (used
/// when resuming a job that may have partially exported this candidate).
pub fn export_candidate(
    src: &dyn ReadAt,
    c: &ParsedCandidate,
    dest: &Path,
    overwrite: bool,
) -> io::Result<ExportInfo> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true);
    if overwrite {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    let mut out = io::BufWriter::new(opts.open(dest)?);
    let mut file_hash = Sha256::new();
    let mut src_hash = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    while done < c.length {
        let want = ((c.length - done) as usize).min(buf.len());
        let n = read_full_at(src, c.offset + done, &mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source shorter than candidate",
            ));
        }
        out.write_all(&buf[..n])?;
        file_hash.update(&buf[..n]);
        src_hash.update(&buf[..n]);
        done += n as u64;
    }
    let trailer = partial_trailer(c);
    out.write_all(trailer)?;
    file_hash.update(trailer);
    out.flush()?;
    Ok(ExportInfo {
        bytes_written: done + trailer.len() as u64,
        sha256: hex::encode(file_hash.finalize()),
        source_sha256: hex::encode(src_hash.finalize()),
        trailer_appended: !trailer.is_empty(),
    })
}
