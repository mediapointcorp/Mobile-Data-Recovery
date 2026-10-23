//! JPEG structure parser for carving.
//!
//! Walks marker segments using their declared lengths, validates the tables
//! that matter (SOFn, DQT, DHT, DRI, SOS), and scans entropy-coded data
//! byte-by-byte honouring the escaping rules:
//!
//! * `FF 00` — stuffed 0xFF data byte
//! * `FF FF…` — fill bytes before a marker
//! * `FF D0`–`FF D7` — restart markers, which must cycle modulo 8
//! * any other `FF xx` — a marker that ends the current scan
//!
//! The candidate ends at EOI. A structural break (invalid marker, SOI inside
//! scan data, restart sequence break, long zero-filled run) ends the
//! candidate as [`EndStatus::Corrupt`] at the last trustworthy byte.

use crate::{CarveLimits, EndStatus, ParseOutcome, ParsedCandidate};
use pr_model::model::ImageFormat;
use pr_model::source::{Cursor, ReadAt};
use std::io;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DHT: u8 = 0xC4;
const DRI: u8 = 0xDD;
const DNL: u8 = 0xDC;

/// Bytes appended to a partial JPEG on export so decoders render what exists.
pub const EOI_TRAILER: [u8; 2] = [0xFF, EOI];

fn is_sof(m: u8) -> bool {
    matches!(m, 0xC0..=0xCF) && !matches!(m, DHT | 0xC8 | 0xCC)
}

fn is_rst(m: u8) -> bool {
    (0xD0..=0xD7).contains(&m)
}

/// Markers that carry a 2-byte length field and may appear outside scans.
fn is_length_marker(m: u8) -> bool {
    is_sof(m)
        || matches!(
            m,
            DHT | 0xCC /* DAC */ | DQT | DRI | DNL | SOS | 0xDE /* DHP */ | 0xDF /* EXP */ | 0xFE /* COM */
        )
        || (0xE0..=0xEF).contains(&m) // APPn
        || (0xF0..=0xFD).contains(&m) // JPGn extensions
}

#[derive(Default)]
struct State {
    sof: Option<u8>,
    width: Option<u32>,
    height: Option<u32>,
    components: u8,
    scans: u32,
    dqt: u32,
    dht: u32,
    restart_interval: u16,
    restarts: u64,
    segments: u32,
    /// Start of the first long zero run seen in scan data (suspected end).
    zero_cut: Option<u64>,
    /// Components covered by sequential (non-progressive) scans so far.
    seq_components: u32,
    /// End of the most recent scan's entropy data. If the segments that
    /// follow fail to validate, the scan-terminating marker was probably
    /// foreign data, so the candidate is cut here.
    pending_cut: Option<u64>,
}

impl State {
    fn progressive(&self) -> bool {
        matches!(self.sof, Some(0xC2 | 0xC6 | 0xCA | 0xCE))
    }
}

/// Parse the JPEG starting at `start` (which must hold `FF D8`).
pub fn parse_jpeg(src: &dyn ReadAt, start: u64, limits: &CarveLimits) -> io::Result<ParseOutcome> {
    let limit = start.saturating_add(limits.max_jpeg);
    let mut c = Cursor::new(src, start, limit);
    let hit_limit = |c: &Cursor| c.limit() < src.len() && c.position() >= c.limit();

    if c.next_u8()? != Some(0xFF) || c.next_u8()? != Some(SOI) {
        return Ok(ParseOutcome::Rejected("missing SOI".into()));
    }

    let mut cand = ParsedCandidate::new(ImageFormat::Jpeg, start);
    let mut st = State::default();

    loop {
        st.segments += 1;
        if st.segments > limits.max_structures {
            return Ok(finish(cand, &st, c.position(), EndStatus::Corrupt, "too many segments".into()));
        }
        let marker_pos = c.position();
        let Some(b) = c.next_u8()? else {
            let why = if hit_limit(&c) { "maximum candidate length reached" } else { "source ended before EOI" };
            return Ok(finish(cand, &st, marker_pos, EndStatus::Truncated, why.into()));
        };
        if b != 0xFF {
            return Ok(finish(
                cand,
                &st,
                marker_pos,
                EndStatus::Corrupt,
                format!("expected marker at +{}, found 0x{b:02X}", marker_pos - start),
            ));
        }
        // Optional fill bytes.
        let mut m = 0xFF;
        while m == 0xFF {
            match c.next_u8()? {
                Some(x) => m = x,
                None => {
                    return Ok(finish(cand, &st, marker_pos, EndStatus::Truncated, "source ended inside marker".into()))
                }
            }
        }

        if m == EOI {
            if st.sof.is_none() || st.scans == 0 {
                return Ok(ParseOutcome::Rejected("EOI before any image data".into()));
            }
            cand.length = c.position() - start;
            cand.status = EndStatus::Complete;
            if st.dqt == 0 {
                cand.diagnostics.push("no DQT segment".into());
            }
            return Ok(ParseOutcome::Candidate(fill(cand, &st)));
        }
        if m == SOI || m == 0x00 || (!is_length_marker(m) && !is_rst(m) && m != 0x01) {
            return Ok(finish(
                cand,
                &st,
                marker_pos,
                EndStatus::Corrupt,
                format!("invalid marker FF{m:02X} at +{}", marker_pos - start),
            ));
        }
        if is_rst(m) || m == 0x01 {
            cand.diagnostics.push(format!("standalone marker FF{m:02X} outside scan at +{}", marker_pos - start));
            continue;
        }

        let Some(len) = c.read_u16_be()? else {
            return Ok(finish(cand, &st, marker_pos, EndStatus::Truncated, "source ended in segment length".into()));
        };
        if len < 2 {
            return Ok(finish(cand, &st, marker_pos, EndStatus::Corrupt, format!("segment FF{m:02X} length {len} < 2")));
        }
        let payload_len = (len - 2) as usize;
        let seg_start = c.position();

        // Segments we validate are small (< 64 KiB); read them whole.
        let needs_payload = is_sof(m) || matches!(m, DQT | DHT | DRI | SOS);
        if needs_payload {
            let mut p = vec![0u8; payload_len];
            if !c.read_exact(&mut p)? {
                return Ok(finish(cand, &st, marker_pos, EndStatus::Truncated, "source ended inside segment".into()));
            }
            if let Err(why) = check_segment(m, &p, &mut st) {
                return Ok(finish(cand, &st, marker_pos, EndStatus::Corrupt, why));
            }
            if m == SOS {
                st.pending_cut = None;
                if !st.progressive() {
                    st.seq_components += p[0] as u32;
                }
            }
        } else {
            if (0xE0..=0xEF).contains(&m) {
                cand.embedded_windows.push((seg_start, seg_start + payload_len as u64));
                let mut tag = [0u8; 4];
                let peek_len = payload_len.min(4);
                if c.read_exact(&mut tag[..peek_len])? {
                    let name = match (m, &tag) {
                        (0xE1, b"Exif") => Some("Exif"),
                        (0xE1, b"http") => Some("XMP"),
                        (0xE0, b"JFIF") => Some("JFIF"),
                        (0xE2, b"MPF\0") => Some("MPF"),
                        (0xE2, b"ICC_") => Some("ICC"),
                        _ => None,
                    };
                    if let Some(n) = name {
                        cand.diagnostics.push(format!("APP{} {n} ({payload_len} bytes)", m - 0xE0));
                    }
                }
                c.seek(seg_start);
            }
            if c.skip(payload_len as u64) != payload_len as u64 {
                return Ok(finish(cand, &st, marker_pos, EndStatus::Truncated, "source ended inside segment".into()));
            }
        }

        if m == SOS {
            match scan_entropy(&mut c, &mut st, limits)? {
                ScanEnd::Marker => continue,
                ScanEnd::Eof(pos) => {
                    let why = if hit_limit(&c) { "maximum candidate length reached" } else { "source ended inside scan data" };
                    return Ok(finish(cand, &st, pos, EndStatus::Truncated, why.into()));
                }
                ScanEnd::Break(pos, why) => return Ok(finish(cand, &st, pos, EndStatus::Corrupt, why)),
            }
        }
    }
}

/// Validate a table/frame segment payload and update parser state.
fn check_segment(m: u8, p: &[u8], st: &mut State) -> Result<(), String> {
    match m {
        _ if is_sof(m) => {
            if st.sof.is_some() {
                return Err("second SOF segment".into());
            }
            if p.len() < 6 {
                return Err("SOF too short".into());
            }
            let precision = p[0];
            let height = u16::from_be_bytes([p[1], p[2]]);
            let width = u16::from_be_bytes([p[3], p[4]]);
            let nc = p[5];
            if !(2..=16).contains(&precision) {
                return Err(format!("SOF precision {precision} invalid"));
            }
            if nc == 0 || nc > 4 || p.len() != 6 + 3 * nc as usize {
                return Err(format!("SOF component count {nc} inconsistent with length"));
            }
            if width == 0 {
                return Err("SOF width 0".into());
            }
            for i in 0..nc as usize {
                let hv = p[6 + 3 * i + 1];
                let (h, v) = (hv >> 4, hv & 0x0F);
                if !(1..=4).contains(&h) || !(1..=4).contains(&v) || p[6 + 3 * i + 2] > 3 {
                    return Err("SOF component sampling/table invalid".into());
                }
            }
            st.sof = Some(m);
            st.width = Some(width as u32);
            st.height = (height != 0).then_some(height as u32);
            st.components = nc;
        }
        DQT => {
            let mut i = 0;
            while i < p.len() {
                let pq = p[i] >> 4;
                let tq = p[i] & 0x0F;
                if pq > 1 || tq > 3 {
                    return Err("DQT table spec invalid".into());
                }
                i += 1 + 64 * (1 + pq as usize);
            }
            if i != p.len() || p.is_empty() {
                return Err("DQT length mismatch".into());
            }
            st.dqt += 1;
        }
        DHT => {
            let mut i = 0;
            while i < p.len() {
                let tc = p[i] >> 4;
                let th = p[i] & 0x0F;
                if tc > 1 || th > 3 || i + 17 > p.len() {
                    return Err("DHT table spec invalid".into());
                }
                let total: usize = p[i + 1..i + 17].iter().map(|&x| x as usize).sum();
                if total == 0 || total > 256 {
                    return Err("DHT symbol count invalid".into());
                }
                i += 17 + total;
            }
            if i != p.len() || p.is_empty() {
                return Err("DHT length mismatch".into());
            }
            st.dht += 1;
        }
        DRI => {
            if p.len() != 2 {
                return Err("DRI length invalid".into());
            }
            st.restart_interval = u16::from_be_bytes([p[0], p[1]]);
        }
        SOS => {
            if st.sof.is_none() {
                return Err("SOS before SOF".into());
            }
            if p.is_empty() {
                return Err("SOS empty".into());
            }
            let ns = p[0];
            if ns == 0 || ns > 4 || ns > st.components || p.len() != 4 + 2 * ns as usize {
                return Err("SOS component count inconsistent".into());
            }
            st.scans += 1;
        }
        _ => {}
    }
    Ok(())
}

enum ScanEnd {
    /// A non-RST marker follows; cursor is positioned on its 0xFF.
    Marker,
    /// Source/limit ended at this absolute position.
    Eof(u64),
    /// Structure broke; keep data up to this absolute position.
    Break(u64, String),
}

/// Walk entropy-coded data after an SOS header.
fn scan_entropy(c: &mut Cursor, st: &mut State, limits: &CarveLimits) -> io::Result<ScanEnd> {
    let mut expected_rst: u8 = 0;
    let mut last_good = c.position();
    let mut zero_run: u64 = 0;
    let mut zero_start = 0u64;

    loop {
        let base = c.position();
        let buf = c.buffered()?;
        if buf.is_empty() {
            return Ok(ScanEnd::Eof(base));
        }
        // Fast path: search the buffer for 0xFF while tracking zero runs.
        let mut ff_at = None;
        for (i, &b) in buf.iter().enumerate() {
            if b == 0 {
                if zero_run == 0 {
                    zero_start = base + i as u64;
                }
                zero_run += 1;
                if zero_run >= limits.zero_run_cutoff {
                    return Ok(ScanEnd::Break(
                        zero_start,
                        format!("zero-filled run of {zero_run}+ bytes in scan data (trimmed or unallocated blocks?)"),
                    ));
                }
            } else {
                if zero_run >= limits.zero_run_soft && st.zero_cut.is_none() {
                    st.zero_cut = Some(zero_start);
                }
                zero_run = 0;
                if b == 0xFF {
                    ff_at = Some(i);
                    break;
                }
            }
        }
        let Some(i) = ff_at else {
            if zero_run >= limits.zero_run_soft && st.zero_cut.is_none() {
                st.zero_cut = Some(zero_start);
            }
            let n = buf.len() as u64;
            c.skip(n);
            continue;
        };
        let ff_pos = base + i as u64;
        c.seek(ff_pos + 1);
        // Collapse fill bytes.
        let m = loop {
            match c.next_u8()? {
                None => return Ok(ScanEnd::Eof(ff_pos)),
                Some(0xFF) => continue,
                Some(x) => break x,
            }
        };
        if m == 0x00 {
            continue; // stuffed byte
        }
        if is_rst(m) {
            let n = m - 0xD0;
            if st.restart_interval == 0 {
                return Ok(ScanEnd::Break(ff_pos, "restart marker without DRI".into()));
            }
            if n != expected_rst {
                return Ok(ScanEnd::Break(
                    last_good,
                    format!(
                        "restart marker sequence break (expected RST{expected_rst}, found RST{n}) — possible fragmentation"
                    ),
                ));
            }
            expected_rst = (expected_rst + 1) & 7;
            st.restarts += 1;
            last_good = c.position();
            continue;
        }
        if m == SOI {
            return Ok(ScanEnd::Break(ff_pos, "SOI inside scan data — another file starts here".into()));
        }
        if m == EOI || is_length_marker(m) {
            let sequential_done = !st.progressive() && st.seq_components >= st.components as u32;
            let allowed = if sequential_done {
                matches!(m, EOI | DNL)
            } else {
                matches!(m, EOI | DNL | SOS | DHT | DQT | DRI | 0xCC | 0xFE) || (0xE0..=0xEF).contains(&m)
            };
            if !allowed {
                return Ok(ScanEnd::Break(ff_pos, format!("unexpected marker FF{m:02X} after scan data")));
            }
            c.seek(ff_pos);
            st.pending_cut = Some(ff_pos);
            return Ok(ScanEnd::Marker);
        }
        return Ok(ScanEnd::Break(ff_pos, format!("invalid byte pair FF{m:02X} in scan data")));
    }
}

fn fill(mut cand: ParsedCandidate, st: &State) -> ParsedCandidate {
    cand.width = st.width;
    cand.height = st.height;
    if let Some(sof) = st.sof {
        let kind = match sof {
            0xC0 => "baseline",
            0xC1 => "extended sequential",
            0xC2 => "progressive",
            0xC3 => "lossless",
            _ => "other SOF",
        };
        cand.diagnostics.insert(0, format!("SOF{} {kind}, {} components, {} scan(s)", sof - 0xC0, st.components, st.scans));
    }
    if st.restart_interval > 0 {
        cand.diagnostics.push(format!("restart interval {}, {} restart markers", st.restart_interval, st.restarts));
    }
    cand
}

/// Finish a candidate that did not reach EOI. Data before `cut` is kept if
/// at least one scan's image data was reached; otherwise the hit is
/// rejected as a false positive / header-only fragment.
fn finish(mut cand: ParsedCandidate, st: &State, cut: u64, status: EndStatus, why: String) -> ParseOutcome {
    if st.sof.is_none() || st.scans == 0 {
        return ParseOutcome::Rejected(format!("no image data: {why}"));
    }
    let cut = st.pending_cut.map_or(cut, |p| p.min(cut));
    let (cut, why) = match st.zero_cut {
        Some(z) if z < cut => (z, format!("{why}; cut at earlier zero-filled run at +{}", z - cand.offset)),
        _ => (cut, why),
    };
    cand.length = cut - cand.offset;
    cand.status = status;
    cand.end_reason = Some(why);
    // Embedded windows beyond the cut are meaningless.
    let end = cand.offset + cand.length;
    cand.embedded_windows.retain(|w| w.0 < end);
    ParseOutcome::Candidate(fill(cand, st))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::source::MemSource;

    /// Minimal hand-built baseline JPEG skeleton (structure only).
    fn skeleton(scan: &[u8], dri: Option<u16>) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        // DQT: one 8-bit table
        v.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        v.extend(std::iter::repeat_n(1u8, 64));
        // SOF0 16x16, 1 component
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
        // DHT: one table with one symbol
        v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x00, 1]);
        v.extend(std::iter::repeat_n(0u8, 15));
        v.push(0);
        if let Some(i) = dri {
            v.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
            v.extend_from_slice(&i.to_be_bytes());
        }
        // SOS
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0x00, 0, 63, 0]);
        v.extend_from_slice(scan);
        v
    }

    fn parse(data: Vec<u8>) -> ParseOutcome {
        let src = MemSource::new(data);
        parse_jpeg(&src, 0, &CarveLimits::default()).unwrap()
    }

    #[test]
    fn complete_with_stuffing_and_trailing_garbage() {
        let mut d = skeleton(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xFF, 0xD9], None);
        let n = d.len();
        d.extend_from_slice(b"garbage after EOI");
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Complete);
                assert_eq!(c.length as usize, n);
                assert_eq!((c.width, c.height), (Some(16), Some(16)));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn restart_markers_in_sequence() {
        let mut scan = vec![];
        for i in 0..10u8 {
            scan.extend_from_slice(&[0x55, 0xAA, 0xFF, 0xD0 + (i % 8)]);
        }
        scan.extend_from_slice(&[0x01, 0xFF, 0xD9]);
        match parse(skeleton(&scan, Some(4))) {
            ParseOutcome::Candidate(c) => assert_eq!(c.status, EndStatus::Complete),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn restart_sequence_break_cuts_at_last_good_restart() {
        let scan = [0x11, 0xFF, 0xD0, 0x22, 0xFF, 0xD1, 0x33, 0xFF, 0xD5, 0x44, 0xFF, 0xD9];
        let d = skeleton(&scan, Some(1));
        let sos_end = d.len() - scan.len();
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Corrupt);
                assert_eq!(c.length as usize, sos_end + 6); // after FF D1
                assert!(c.end_reason.unwrap().contains("restart marker sequence"));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn soi_inside_scan_is_a_break() {
        let scan = [0x10, 0x20, 0xFF, 0xD8, 0xFF, 0xE0];
        let d = skeleton(&scan, None);
        let at = d.len() - 4;
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Corrupt);
                assert_eq!(c.length as usize, at);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn truncated_in_scan() {
        let d = skeleton(&[0x10, 0x20, 0x30], None);
        let n = d.len();
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Truncated);
                assert_eq!(c.length as usize, n);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn header_only_and_random_are_rejected() {
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        d.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        assert!(matches!(parse(d.clone()), ParseOutcome::Rejected(_)));
        d.extend_from_slice(&[0x12, 0x34, 0x56]);
        assert!(matches!(parse(d), ParseOutcome::Rejected(_)));
        assert!(matches!(parse(vec![0xFF, 0xD8, 0xFF, 0x02, 0, 0]), ParseOutcome::Rejected(_)));
    }

    #[test]
    fn zero_run_ends_candidate() {
        let mut scan = vec![0x12; 10];
        scan.extend(std::iter::repeat_n(0u8, 70_000));
        let d = skeleton(&scan, None);
        let at = d.len() - 70_000;
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Corrupt);
                assert_eq!(c.length as usize, at);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn soft_zero_run_used_only_for_incomplete_files() {
        let mut scan = vec![0x12; 10];
        scan.extend(std::iter::repeat_n(0u8, 5000));
        scan.extend_from_slice(&[0x34, 0xFF, 0xD9]);
        match parse(skeleton(&scan, None)) {
            ParseOutcome::Candidate(c) => assert_eq!(c.status, EndStatus::Complete),
            o => panic!("{o:?}"),
        }
        let mut scan = vec![0x12; 10];
        scan.extend(std::iter::repeat_n(0u8, 5000));
        scan.extend_from_slice(&[0x34, 0xFF, 0x02]);
        let d = skeleton(&scan, None);
        let at = d.len() - 5003;
        match parse(d) {
            ParseOutcome::Candidate(c) => {
                assert_eq!(c.status, EndStatus::Corrupt);
                assert_eq!(c.length as usize, at);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn bad_table_rejected() {
        let mut d = skeleton(&[0xFF, 0xD9], None);
        d[4] = 0x00;
        d[5] = 0x40; // DQT length off by one
        assert!(matches!(parse(d), ParseOutcome::Rejected(_)));
    }
}
