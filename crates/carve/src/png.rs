//! PNG chunk walker with per-chunk CRC verification.

use crate::{CarveLimits, EndStatus, ParseOutcome, ParsedCandidate};
use pr_model::model::ImageFormat;
use pr_model::source::{Cursor, ReadAt};
use std::io;

pub const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
/// IEND chunk appended to partial PNGs on export.
pub const IEND_TRAILER: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

pub fn parse_png(src: &dyn ReadAt, start: u64, limits: &CarveLimits) -> io::Result<ParseOutcome> {
    let limit = start.saturating_add(limits.max_png);
    let mut c = Cursor::new(src, start, limit);
    let mut sig = [0u8; 8];
    if !c.read_exact(&mut sig)? || sig != SIGNATURE {
        return Ok(ParseOutcome::Rejected("bad PNG signature".into()));
    }
    let mut cand = ParsedCandidate::new(ImageFormat::Png, start);
    let mut chunks = 0u32;
    let mut idat_bytes = 0u64;
    let mut have_ihdr = false;

    let finish = |mut cand: ParsedCandidate, cut: u64, status: EndStatus, why: String, have_ihdr: bool, idat: u64| {
        if !have_ihdr || idat == 0 {
            return ParseOutcome::Rejected(format!("no image data: {why}"));
        }
        cand.length = cut - cand.offset;
        cand.status = status;
        cand.end_reason = Some(why);
        ParseOutcome::Candidate(cand)
    };

    loop {
        chunks += 1;
        let chunk_pos = c.position();
        if chunks > limits.max_structures {
            return Ok(finish(cand, chunk_pos, EndStatus::Corrupt, "too many chunks".into(), have_ihdr, idat_bytes));
        }
        let mut hdr = [0u8; 8];
        if !c.read_exact(&mut hdr)? {
            return Ok(finish(cand, chunk_pos, EndStatus::Truncated, "source ended before IEND".into(), have_ihdr, idat_bytes));
        }
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let ty = [hdr[4], hdr[5], hdr[6], hdr[7]];
        if len > 0x7FFF_FFFF || !ty.iter().all(|b| b.is_ascii_alphabetic()) {
            return Ok(finish(
                cand,
                chunk_pos,
                EndStatus::Corrupt,
                format!("invalid chunk header at +{}", chunk_pos - start),
                have_ihdr,
                idat_bytes,
            ));
        }
        if chunks == 1 && (&ty != b"IHDR" || len != 13) {
            return Ok(ParseOutcome::Rejected("first chunk is not a valid IHDR".into()));
        }
        // Stream the chunk data through the CRC.
        let mut crc = crc32fast::Hasher::new();
        crc.update(&ty);
        let mut ihdr = [0u8; 13];
        let mut remaining = len as u64;
        let mut first = true;
        while remaining > 0 {
            let buf = c.buffered()?;
            if buf.is_empty() {
                let pos = c.position();
                return Ok(finish(cand, if &ty == b"IDAT" { pos } else { chunk_pos }, EndStatus::Truncated,
                    "source ended inside chunk".into(), have_ihdr, idat_bytes + if &ty == b"IDAT" { len as u64 - remaining } else { 0 }));
            }
            let n = (buf.len() as u64).min(remaining) as usize;
            if first && &ty == b"IHDR" && n >= 13 {
                ihdr.copy_from_slice(&buf[..13]);
            }
            first = false;
            crc.update(&buf[..n]);
            c.skip(n as u64);
            remaining -= n as u64;
        }
        let Some(stored) = c.read_u32_be()? else {
            return Ok(finish(cand, chunk_pos, EndStatus::Truncated, "source ended before chunk CRC".into(), have_ihdr, idat_bytes));
        };
        if stored != crc.finalize() {
            let tyname = String::from_utf8_lossy(&ty).to_string();
            if chunks == 1 {
                return Ok(ParseOutcome::Rejected("IHDR CRC mismatch".into()));
            }
            return Ok(finish(
                cand,
                chunk_pos,
                EndStatus::Corrupt,
                format!("CRC mismatch in {tyname} chunk at +{} — possible corruption or fragmentation", chunk_pos - start),
                have_ihdr,
                idat_bytes,
            ));
        }
        match &ty {
            b"IHDR" => {
                let w = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
                let h = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
                let depth = ihdr[8];
                let color = ihdr[9];
                if w == 0 || h == 0 || !matches!(depth, 1 | 2 | 4 | 8 | 16) || !matches!(color, 0 | 2 | 3 | 4 | 6) {
                    return Ok(ParseOutcome::Rejected("IHDR values invalid".into()));
                }
                cand.width = Some(w);
                cand.height = Some(h);
                cand.diagnostics.push(format!("IHDR {w}x{h}, depth {depth}, color type {color}"));
                have_ihdr = true;
            }
            b"IDAT" => idat_bytes += len as u64,
            b"IEND" => {
                if idat_bytes == 0 {
                    return Ok(ParseOutcome::Rejected("IEND without IDAT".into()));
                }
                cand.length = c.position() - start;
                cand.status = EndStatus::Complete;
                cand.diagnostics.push(format!("{chunks} chunks, {idat_bytes} IDAT bytes"));
                return Ok(ParseOutcome::Candidate(cand));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::source::MemSource;

    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(data);
        let mut h = crc32fast::Hasher::new();
        h.update(ty);
        h.update(data);
        v.extend_from_slice(&h.finalize().to_be_bytes());
        v
    }

    fn png(idat: &[u8]) -> Vec<u8> {
        let mut v = SIGNATURE.to_vec();
        let mut ihdr = vec![];
        ihdr.extend_from_slice(&3u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        v.extend(chunk(b"IHDR", &ihdr));
        v.extend(chunk(b"IDAT", idat));
        v.extend(chunk(b"IEND", &[]));
        v
    }

    #[test]
    fn complete_png() {
        let d = png(&[1, 2, 3, 4]);
        let n = d.len();
        let mut d2 = d.clone();
        d2.extend_from_slice(&[0xAA; 100]);
        match parse_png(&MemSource::new(d2), 0, &CarveLimits::default()).unwrap() {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Complete);
                assert_eq!(c.length as usize, n);
                assert_eq!(c.width, Some(3));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn crc_failure_is_corrupt() {
        let mut d = png(&[1, 2, 3, 4]);
        let idat_data = 8 + 25 + 8;
        d[idat_data] ^= 0xFF;
        match parse_png(&MemSource::new(d), 0, &CarveLimits::default()).unwrap() {
            ParseOutcome::Rejected(_) => {} // no valid IDAT before the break
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn signature_only_rejected() {
        let mut d = SIGNATURE.to_vec();
        d.extend_from_slice(&[0x12; 40]);
        assert!(matches!(
            parse_png(&MemSource::new(d), 0, &CarveLimits::default()).unwrap(),
            ParseOutcome::Rejected(_)
        ));
    }
}
