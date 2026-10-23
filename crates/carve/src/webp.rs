//! WebP (RIFF) chunk walker.

use crate::{CarveLimits, EndStatus, ParseOutcome, ParsedCandidate};
use pr_model::model::ImageFormat;
use pr_model::source::{Cursor, ReadAt};
use std::io;

fn fourcc_ok(t: &[u8; 4]) -> bool {
    t.iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ')
}

pub fn parse_webp(src: &dyn ReadAt, start: u64, limits: &CarveLimits) -> io::Result<ParseOutcome> {
    let mut c = Cursor::new(
        src,
        start,
        start.saturating_add(limits.max_webp.saturating_add(8)),
    );
    let mut hdr = [0u8; 12];
    if !c.read_exact(&mut hdr)? || &hdr[..4] != b"RIFF" || &hdr[8..12] != b"WEBP" {
        return Ok(ParseOutcome::Rejected("bad RIFF/WEBP header".into()));
    }
    let riff_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
    if riff_size < 4 + 8 + 10 || riff_size % 2 == 1 {
        return Ok(ParseOutcome::Rejected(format!(
            "implausible RIFF size {riff_size}"
        )));
    }
    if riff_size + 8 > limits.max_webp {
        return Ok(ParseOutcome::Rejected(format!(
            "RIFF size {riff_size} exceeds limit"
        )));
    }
    let total_end = start + 8 + riff_size;
    let mut cand = ParsedCandidate::new(ImageFormat::Webp, start);
    let mut chunks = 0u32;
    let mut have_image = false;

    let finish = |mut cand: ParsedCandidate, cut: u64, status, why: String, have_image: bool| {
        if !have_image {
            return ParseOutcome::Rejected(format!("no image chunk: {why}"));
        }
        cand.length = cut - cand.offset;
        cand.status = status;
        cand.end_reason = Some(why);
        ParseOutcome::Candidate(cand)
    };

    while c.position() < total_end {
        chunks += 1;
        let pos = c.position();
        let mut ch = [0u8; 8];
        if !c.read_exact(&mut ch)? {
            return Ok(finish(
                cand,
                pos,
                EndStatus::Truncated,
                "source ended before RIFF end".into(),
                have_image,
            ));
        }
        let ty = [ch[0], ch[1], ch[2], ch[3]];
        let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
        let padded = size + (size & 1);
        if !fourcc_ok(&ty) || pos + 8 + padded > total_end || chunks > limits.max_structures {
            return Ok(finish(
                cand,
                pos,
                EndStatus::Corrupt,
                format!("invalid chunk at +{}", pos - start),
                have_image,
            ));
        }
        if chunks == 1 && !matches!(&ty, b"VP8 " | b"VP8L" | b"VP8X") {
            return Ok(ParseOutcome::Rejected(
                "first chunk is not VP8/VP8L/VP8X".into(),
            ));
        }
        let mut head = [0u8; 10];
        let hn = (size as usize).min(10);
        if !c.read_exact(&mut head[..hn])? {
            return Ok(finish(
                cand,
                pos,
                EndStatus::Truncated,
                "source ended inside chunk".into(),
                have_image,
            ));
        }
        match &ty {
            b"VP8X" if hn >= 10 => {
                cand.width = Some(1 + u32::from_le_bytes([head[4], head[5], head[6], 0]));
                cand.height = Some(1 + u32::from_le_bytes([head[7], head[8], head[9], 0]));
            }
            b"VP8 " if hn >= 10 => {
                if head[3..6] != [0x9D, 0x01, 0x2A] {
                    return Ok(finish(
                        cand,
                        pos,
                        EndStatus::Corrupt,
                        "VP8 start code missing".into(),
                        have_image,
                    ));
                }
                if cand.width.is_none() {
                    cand.width = Some(u16::from_le_bytes([head[6], head[7]]) as u32 & 0x3FFF);
                    cand.height = Some(u16::from_le_bytes([head[8], head[9]]) as u32 & 0x3FFF);
                }
                have_image = true;
            }
            b"VP8L" if hn >= 5 => {
                if head[0] != 0x2F {
                    return Ok(finish(
                        cand,
                        pos,
                        EndStatus::Corrupt,
                        "VP8L signature missing".into(),
                        have_image,
                    ));
                }
                if cand.width.is_none() {
                    let bits = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
                    cand.width = Some((bits & 0x3FFF) + 1);
                    cand.height = Some(((bits >> 14) & 0x3FFF) + 1);
                }
                have_image = true;
            }
            b"ANMF" => have_image = true,
            _ => {}
        }
        c.seek(pos + 8);
        if c.skip(padded) != padded {
            let at = c.position();
            return Ok(finish(
                cand,
                at,
                EndStatus::Truncated,
                "source ended inside chunk".into(),
                have_image,
            ));
        }
    }
    if !have_image {
        return Ok(ParseOutcome::Rejected(
            "RIFF ended without image data".into(),
        ));
    }
    cand.length = total_end - start;
    cand.status = EndStatus::Complete;
    cand.diagnostics.push(format!("{chunks} RIFF chunks"));
    Ok(ParseOutcome::Candidate(cand))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::source::MemSource;

    #[test]
    fn minimal_lossless_header() {
        let mut payload = vec![0x2F];
        let bits: u32 = (9) | (19 << 14); // 10 x 20
        payload.extend_from_slice(&bits.to_le_bytes());
        payload.extend_from_slice(&[0u8; 9]);
        let mut d = b"RIFF".to_vec();
        d.extend_from_slice(&((4 + 8 + payload.len()) as u32).to_le_bytes());
        d.extend_from_slice(b"WEBPVP8L");
        d.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        d.extend_from_slice(&payload);
        let n = d.len();
        d.extend_from_slice(&[0u8; 16]);
        match parse_webp(&MemSource::new(d), 0, &CarveLimits::default()).unwrap() {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Complete);
                assert_eq!(c.length as usize, n);
                assert_eq!((c.width, c.height), (Some(10), Some(20)));
            }
            o => panic!("{o:?}"),
        }
    }
}
