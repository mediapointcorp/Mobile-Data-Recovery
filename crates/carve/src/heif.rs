//! ISO-BMFF (HEIC/HEIF/AVIF) top-level box walker.
//!
//! The candidate ends after the last recognised top-level box. The `meta`
//! box is parsed (bounded) to read image dimensions (`ispe`) and to check
//! that every `iloc` item extent lies inside the carved bytes. Pixel
//! decoding of HEVC/AV1 is out of scope; these candidates are validated
//! structurally only.

use crate::{CarveLimits, EndStatus, ParseOutcome, ParsedCandidate};
use pr_model::model::ImageFormat;
use pr_model::source::{read_vec_at, Cursor, ReadAt};
use std::io;

const TOP_LEVEL: [&[u8; 4]; 12] = [
    b"ftyp", b"meta", b"mdat", b"free", b"skip", b"uuid", b"moov", b"moof", b"mfra", b"idat", b"wide", b"pdin",
];

pub fn heif_brand_format(brand: &[u8]) -> Option<ImageFormat> {
    match brand {
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1" => Some(ImageFormat::Heic),
        b"avif" | b"avis" => Some(ImageFormat::Avif),
        _ => None,
    }
}

pub fn parse_heif(src: &dyn ReadAt, start: u64, limits: &CarveLimits) -> io::Result<ParseOutcome> {
    let limit = start.saturating_add(limits.max_heif);
    let mut c = Cursor::new(src, start, limit);
    let Some(ftyp_size) = c.read_u32_be()? else {
        return Ok(ParseOutcome::Rejected("short".into()));
    };
    let mut t = [0u8; 8];
    if !c.read_exact(&mut t)? || &t[..4] != b"ftyp" {
        return Ok(ParseOutcome::Rejected("no ftyp".into()));
    }
    let Some(format) = heif_brand_format(&t[4..8]) else {
        return Ok(ParseOutcome::Rejected("unsupported ftyp brand".into()));
    };
    if !(16..=4096).contains(&ftyp_size) || ftyp_size % 4 != 0 {
        return Ok(ParseOutcome::Rejected(format!("implausible ftyp size {ftyp_size}")));
    }
    let mut cand = ParsedCandidate::new(format, start);
    cand.diagnostics.push(format!("major brand {}", String::from_utf8_lossy(&t[4..8])));
    c.seek(start);

    let mut have_meta = false;
    let mut have_mdat = false;
    let mut max_extent: u64 = 0;
    let mut boxes = 0u32;
    let mut end = start;

    loop {
        boxes += 1;
        let pos = c.position();
        if boxes > limits.max_structures {
            break;
        }
        let Some(size32) = c.read_u32_be()? else {
            break; // clean end of source: decide below
        };
        let mut ty = [0u8; 4];
        if !c.read_exact(&mut ty)? {
            break;
        }
        if !TOP_LEVEL.contains(&&ty) {
            break; // not part of this file
        }
        let size = match size32 {
            0 => {
                cand.diagnostics.push(format!("box '{}' extends to end of file; cannot bound", String::from_utf8_lossy(&ty)));
                break;
            }
            1 => match c.read_u64_be()? {
                Some(s) => s,
                None => break,
            },
            s => s as u64,
        };
        let hdr = c.position() - pos;
        if size < hdr {
            break;
        }
        if pos + size > limit {
            return Ok(finish(cand, end, pos + size, EndStatus::Truncated, "box exceeds maximum candidate length".into(), have_meta));
        }
        if &ty == b"meta" {
            if size - hdr > limits.max_meta_box {
                return Ok(ParseOutcome::Rejected("meta box too large".into()));
            }
            let payload = match read_vec_at(src, pos + hdr, (size - hdr) as usize) {
                Ok(p) => p,
                Err(_) => return Ok(finish(cand, end, pos + size, EndStatus::Truncated, "source ended inside meta".into(), have_meta)),
            };
            let mut info = MetaInfo::default();
            if payload.len() < 4 || parse_children(&payload[4..], &mut info, 0).is_err() {
                return Ok(finish(cand, end, pos + size, EndStatus::Corrupt, "malformed meta box".into(), have_meta));
            }
            if let Some((w, h)) = info.largest_ispe {
                cand.width = Some(w);
                cand.height = Some(h);
            }
            max_extent = info.max_extent;
            cand.diagnostics.push(format!("meta: {} item extents, {} ispe", info.extents, info.ispe_count));
            have_meta = true;
        }
        if &ty == b"mdat" {
            have_mdat = true;
        }
        c.seek(pos + hdr);
        if c.skip(size - hdr) != size - hdr {
            return Ok(finish(cand, end, pos + size, EndStatus::Truncated, "source ended inside box".into(), have_meta));
        }
        end = pos + size;
    }

    if !have_meta {
        return Ok(ParseOutcome::Rejected("no meta box".into()));
    }
    cand.length = end - start;
    if max_extent > cand.length {
        cand.status = EndStatus::Corrupt;
        cand.end_reason = Some(format!("item data extends to +{max_extent}, beyond carved length {}", cand.length));
    } else if !have_mdat && max_extent == 0 {
        cand.status = EndStatus::Corrupt;
        cand.end_reason = Some("no mdat and no item extents".into());
    } else {
        cand.status = EndStatus::Complete;
    }
    Ok(ParseOutcome::Candidate(cand))
}

fn finish(mut cand: ParsedCandidate, good_end: u64, _wanted: u64, status: EndStatus, why: String, have_meta: bool) -> ParseOutcome {
    if !have_meta {
        return ParseOutcome::Rejected(format!("no meta before break: {why}"));
    }
    cand.length = good_end - cand.offset;
    cand.status = status;
    cand.end_reason = Some(why);
    ParseOutcome::Candidate(cand)
}

#[derive(Default)]
struct MetaInfo {
    largest_ispe: Option<(u32, u32)>,
    ispe_count: u32,
    max_extent: u64,
    extents: u32,
}

fn be(b: &[u8], n: usize) -> Option<u64> {
    if b.len() < n {
        return None;
    }
    Some(b[..n].iter().fold(0u64, |a, &x| (a << 8) | x as u64))
}

/// Walk child boxes of a container payload.
fn parse_children(mut p: &[u8], info: &mut MetaInfo, depth: u32) -> Result<(), ()> {
    if depth > 8 {
        return Err(());
    }
    while p.len() >= 8 {
        let size = be(p, 4).ok_or(())? as usize;
        let ty = &p[4..8];
        if size < 8 || size > p.len() {
            return Err(());
        }
        let body = &p[8..size];
        match ty {
            b"iprp" | b"ipco" => parse_children(body, info, depth + 1)?,
            b"ispe" if body.len() >= 12 => {
                let w = be(&body[4..], 4).ok_or(())? as u32;
                let h = be(&body[8..], 4).ok_or(())? as u32;
                info.ispe_count += 1;
                if info.largest_ispe.is_none_or(|(a, b)| (w as u64 * h as u64) > (a as u64 * b as u64)) {
                    info.largest_ispe = Some((w, h));
                }
            }
            b"iloc" => parse_iloc(body, info)?,
            _ => {}
        }
        p = &p[size..];
    }
    Ok(())
}

fn parse_iloc(b: &[u8], info: &mut MetaInfo) -> Result<(), ()> {
    let version = *b.first().ok_or(())?;
    let mut i = 4;
    let s1 = *b.get(i).ok_or(())?;
    let s2 = *b.get(i + 1).ok_or(())?;
    i += 2;
    let (off_size, len_size, base_size) = ((s1 >> 4) as usize, (s1 & 15) as usize, (s2 >> 4) as usize);
    let idx_size = if version >= 1 { (s2 & 15) as usize } else { 0 };
    let count = if version < 2 {
        let v = be(&b[i..], 2).ok_or(())?;
        i += 2;
        v
    } else {
        let v = be(&b[i..], 4).ok_or(())?;
        i += 4;
        v
    };
    for _ in 0..count {
        i += if version < 2 { 2 } else { 4 }; // item_ID
        let mut method = 0;
        if version >= 1 {
            method = be(b.get(i..).ok_or(())?, 2).ok_or(())? & 0xF;
            i += 2;
        }
        i += 2; // data_reference_index
        let base = if base_size > 0 { be(b.get(i..).ok_or(())?, base_size).ok_or(())? } else { 0 };
        i += base_size;
        let n = be(b.get(i..).ok_or(())?, 2).ok_or(())?;
        i += 2;
        for _ in 0..n {
            i += idx_size;
            let off = if off_size > 0 { be(b.get(i..).ok_or(())?, off_size).ok_or(())? } else { 0 };
            i += off_size;
            let len = if len_size > 0 { be(b.get(i..).ok_or(())?, len_size).ok_or(())? } else { 0 };
            i += len_size;
            info.extents += 1;
            if method == 0 {
                info.max_extent = info.max_extent.max(base.saturating_add(off).saturating_add(len));
            }
        }
    }
    Ok(())
}
