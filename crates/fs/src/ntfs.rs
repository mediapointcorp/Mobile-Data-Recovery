//! NTFS parser with deleted-file recovery from the MFT.
//!
//! When Windows deletes a file it clears the MFT record's in-use flag and
//! the file's bits in `$Bitmap`, but normally leaves the record — name,
//! parent directory, timestamps and the `$DATA` run list — in place until
//! the record is reused. That means deleted NTFS files can often be read
//! back exactly, *including fragmented files*, as long as their clusters
//! have not been reallocated. Every recovered file reports how many of its
//! clusters are now allocated to other data.
//!
//! Supported: non-resident and resident unnamed `$DATA`, fragmented run
//! lists, sparse runs, run lists split over extension records. Not
//! recoverable (reported): compressed and EFS-encrypted data.

use crate::FsError;
use pr_model::source::{read_full_at, read_vec_at, ReadAt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;
/// Upper bounds that keep a damaged or hostile volume from exhausting memory.
const MAX_RECORDS: u64 = 64 * 1024 * 1024;
const MAX_BITMAP_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RUNS: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct NtfsGeometry {
    pub bytes_per_sector: u32,
    pub cluster_size: u64,
    pub total_clusters: u64,
    pub mft_lcn: u64,
    pub record_size: u32,
}

/// One data run: `lcn == None` is a sparse (unallocated, reads as zero) run.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub clusters: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum DataLocation {
    /// Data stored inside the MFT record itself (small files).
    Resident(Vec<u8>),
    NonResident {
        runs: Vec<Run>,
        size: u64,
    },
    /// No unnamed `$DATA` attribute was found.
    Missing,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NtfsEntry {
    pub record: u64,
    pub sequence: u16,
    pub path: String,
    pub name: String,
    pub deleted: bool,
    pub is_dir: bool,
    pub size: u64,
    /// RFC 3339 UTC timestamps from `$STANDARD_INFORMATION`.
    pub created: Option<String>,
    pub modified: Option<String>,
    pub data: DataLocation,
    pub compressed: bool,
    pub encrypted: bool,
    /// The parent directory record was reused, so the path is incomplete.
    pub orphan: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NtfsReadPlan {
    /// Volume-relative `(offset, length)` pieces; `None` offset = zeros (sparse).
    pub pieces: Vec<(Option<u64>, u64)>,
    pub clusters_total: u64,
    pub clusters_now_allocated: u64,
    pub first_cluster_allocated: bool,
    pub notes: Vec<String>,
}

pub struct NtfsVolume<'a> {
    src: &'a dyn ReadAt,
    pub geo: NtfsGeometry,
    mft_runs: Vec<Run>,
    mft_records: u64,
    bitmap: Vec<u8>,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Windows FILETIME (100 ns since 1601) → RFC 3339 UTC.
fn filetime(ft: u64) -> Option<String> {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    if ft == 0 {
        return None;
    }
    let secs = (ft / 10_000_000) as i64 - EPOCH_DIFF;
    let t = time::OffsetDateTime::from_unix_timestamp(secs).ok()?;
    t.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// Apply the update-sequence fixup to one MFT record in place.
fn apply_fixup(rec: &mut [u8], stride: usize) -> bool {
    if rec.len() < 48 || &rec[..4] != b"FILE" {
        return false;
    }
    let usa_off = le16(rec, 4) as usize;
    let usa_count = le16(rec, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() || (usa_count - 1) * stride > rec.len()
    {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * stride - 2;
        if rec[end..end + 2] != usn {
            return false; // torn write / not a valid record
        }
        rec[end] = rec[usa_off + 2 * i];
        rec[end + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

/// Decode an NTFS mapping-pairs run list starting at `vcn`.
pub fn decode_runs(b: &[u8], start_vcn: u64) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut i = 0;
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    while i < b.len() && b[i] != 0 {
        let h = b[i];
        let (ls, os) = ((h & 0x0F) as usize, (h >> 4) as usize);
        i += 1;
        if ls == 0 || ls > 8 || os > 8 || i + ls + os > b.len() {
            return None;
        }
        let mut len: u64 = 0;
        for k in 0..ls {
            len |= (b[i + k] as u64) << (8 * k);
        }
        i += ls;
        let run_lcn = if os == 0 {
            None
        } else {
            let mut d: i64 = 0;
            for k in 0..os {
                d |= (b[i + k] as i64) << (8 * k);
            }
            if b[i + os - 1] & 0x80 != 0 && os < 8 {
                d -= 1i64 << (8 * os); // sign-extend
            }
            i += os;
            lcn = lcn.checked_add(d)?;
            if lcn < 0 {
                return None;
            }
            Some(lcn as u64)
        };
        runs.push(Run {
            vcn,
            lcn: run_lcn,
            clusters: len,
        });
        vcn = vcn.checked_add(len)?;
        if runs.len() > MAX_RUNS {
            return None;
        }
    }
    Some(runs)
}

/// Unnamed `$DATA`: (resident bytes, runs, real size, start VCN).
type DataAttr = (Option<Vec<u8>>, Vec<Run>, u64, u64);

/// What one MFT record contributes.
#[derive(Default)]
struct RecordInfo {
    in_use: bool,
    is_dir: bool,
    sequence: u16,
    base: u64,
    name: Option<(String, u64, u16, u8)>, // name, parent record, parent seq, namespace
    size_from_name: u64,
    created: u64,
    modified: u64,
    data: Option<DataAttr>,
    compressed: bool,
    encrypted: bool,
}

fn parse_record(rec: &[u8]) -> Option<RecordInfo> {
    let mut info = RecordInfo {
        sequence: le16(rec, 16),
        ..Default::default()
    };
    let flags = le16(rec, 22);
    info.in_use = flags & 1 != 0;
    info.is_dir = flags & 2 != 0;
    info.base = le64(rec, 32) & 0x0000_FFFF_FFFF_FFFF;
    let mut off = le16(rec, 20) as usize;
    let used = (le32(rec, 24) as usize).min(rec.len());
    let mut guard = 0;
    while off + 16 <= used && guard < 256 {
        guard += 1;
        let ty = le32(rec, off);
        if ty == 0xFFFF_FFFF {
            break;
        }
        let len = le32(rec, off + 4) as usize;
        if len < 16 || off + len > used {
            break;
        }
        let a = &rec[off..off + len];
        let non_resident = a[8] != 0;
        let name_len = a[9];
        let aflags = le16(a, 12);
        match (ty, non_resident) {
            (0x10, false) if a.len() >= 24 => {
                let vo = le16(a, 20) as usize;
                if vo + 16 <= a.len() {
                    info.created = le64(a, vo);
                    info.modified = le64(a, vo + 8);
                }
            }
            (0x30, false) if a.len() >= 24 => {
                let vo = le16(a, 20) as usize;
                if vo + 66 <= a.len() {
                    let v = &a[vo..];
                    let parent = le64(v, 0);
                    let nlen = v[64] as usize;
                    let ns = v[65];
                    if 66 + nlen * 2 <= v.len() {
                        let units: Vec<u16> = v[66..66 + nlen * 2]
                            .chunks(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        let name = String::from_utf16_lossy(&units);
                        // Prefer Win32 / POSIX names over 8.3 DOS names.
                        let better = match &info.name {
                            None => true,
                            Some((_, _, _, old_ns)) => *old_ns == 2 && ns != 2,
                        };
                        if better {
                            info.name = Some((
                                name,
                                parent & 0x0000_FFFF_FFFF_FFFF,
                                (parent >> 48) as u16,
                                ns,
                            ));
                            info.size_from_name = le64(v, 48);
                        }
                    }
                }
            }
            (0x80, _) if name_len == 0 => {
                info.compressed |= aflags & 0x0001 != 0;
                info.encrypted |= aflags & 0x4000 != 0;
                if non_resident {
                    if a.len() < 64 {
                        break;
                    }
                    let start_vcn = le64(a, 16);
                    let ro = le16(a, 32) as usize;
                    let real = le64(a, 48);
                    if ro < a.len() {
                        if let Some(runs) = decode_runs(&a[ro..], start_vcn) {
                            info.data = Some((None, runs, real, start_vcn));
                        }
                    }
                } else {
                    let vl = le32(a, 16) as usize;
                    let vo = le16(a, 20) as usize;
                    if vo + vl <= a.len() {
                        info.data = Some((Some(a[vo..vo + vl].to_vec()), Vec::new(), vl as u64, 0));
                    }
                }
            }
            _ => {}
        }
        off += len;
    }
    Some(info)
}

impl<'a> NtfsVolume<'a> {
    pub fn open(src: &'a dyn ReadAt) -> Result<Self, FsError> {
        let bs = read_vec_at(src, 0, 512)?;
        if &bs[3..11] != b"NTFS    " {
            return Err(FsError::Unsupported("not an NTFS boot sector".into()));
        }
        let bps = le16(&bs, 11) as u32;
        let spc_raw = bs[13];
        let spc: u64 = if spc_raw > 0x80 {
            1u64 << (256 - spc_raw as u32).min(31)
        } else {
            spc_raw as u64
        };
        if !matches!(bps, 512 | 1024 | 2048 | 4096) || spc == 0 {
            return Err(FsError::Invalid("bad NTFS geometry".into()));
        }
        let cluster_size = bps as u64 * spc;
        let total_sectors = le64(&bs, 40);
        let mft_lcn = le64(&bs, 48);
        let cpr = bs[64] as i8;
        let record_size: u32 = if cpr < 0 {
            1u32 << (-(cpr as i32)).min(16)
        } else {
            (cpr as u64 * cluster_size).min(65536) as u32
        };
        if record_size < 512 || cluster_size > 2 * 1024 * 1024 {
            return Err(FsError::Invalid("bad MFT record or cluster size".into()));
        }
        let geo = NtfsGeometry {
            bytes_per_sector: bps,
            cluster_size,
            total_clusters: total_sectors * bps as u64 / cluster_size,
            mft_lcn,
            record_size,
        };
        // Bootstrap: record 0 ($MFT) at mft_lcn describes the whole MFT.
        let mut rec0 = read_vec_at(src, mft_lcn * cluster_size, record_size as usize)?;
        if !apply_fixup(&mut rec0, 512) {
            return Err(FsError::Invalid("$MFT record 0 is damaged".into()));
        }
        let info = parse_record(&rec0)
            .ok_or_else(|| FsError::Invalid("$MFT record 0 unreadable".into()))?;
        let Some((None, runs, size, _)) = info.data else {
            return Err(FsError::Invalid("$MFT has no data runs".into()));
        };
        let mut vol = NtfsVolume {
            src,
            geo,
            mft_runs: runs,
            mft_records: (size / record_size as u64).min(MAX_RECORDS),
            bitmap: Vec::new(),
        };
        // $Bitmap (record 6) tells which clusters are allocated now.
        if let Some(rec6) = vol.record(BITMAP_RECORD) {
            if let Some(RecordInfo {
                data: Some((res, runs, size, _)),
                ..
            }) = parse_record(&rec6)
            {
                if size <= MAX_BITMAP_BYTES {
                    vol.bitmap = match res {
                        Some(r) => r,
                        None => vol.read_runs(&runs, size).unwrap_or_default(),
                    };
                }
            }
        }
        Ok(vol)
    }

    /// Read and fix up MFT record `n`.
    fn record(&self, n: u64) -> Option<Vec<u8>> {
        let rs = self.geo.record_size as u64;
        let byte = n.checked_mul(rs)?;
        let cs = self.geo.cluster_size;
        let mut out = vec![0u8; rs as usize];
        let mut done = 0u64;
        while done < rs {
            let vbyte = byte + done;
            let vcn = vbyte / cs;
            let run = self
                .mft_runs
                .iter()
                .find(|r| vcn >= r.vcn && vcn < r.vcn + r.clusters)?;
            let lcn = run.lcn?;
            let in_run = (run.vcn + run.clusters) * cs - vbyte;
            let take = (rs - done).min(in_run);
            let phys = (lcn + (vcn - run.vcn)) * cs + vbyte % cs;
            if read_full_at(
                self.src,
                phys,
                &mut out[done as usize..(done + take) as usize],
            )
            .ok()? as u64
                != take
            {
                return None;
            }
            done += take;
        }
        apply_fixup(&mut out, 512).then_some(out)
    }

    fn read_runs(&self, runs: &[Run], size: u64) -> std::io::Result<Vec<u8>> {
        let plan = self.plan_runs(runs, size);
        self.read(&plan, size)
    }

    /// Is cluster `lcn` marked allocated in `$Bitmap` now?
    pub fn is_allocated(&self, lcn: u64) -> bool {
        let byte = (lcn / 8) as usize;
        self.bitmap
            .get(byte)
            .is_some_and(|b| b & (1 << (lcn % 8)) != 0)
    }

    pub fn has_bitmap(&self) -> bool {
        !self.bitmap.is_empty()
    }

    /// Volume-relative byte ranges of currently unallocated clusters.
    pub fn free_extents(&self) -> Vec<(u64, u64)> {
        let cs = self.geo.cluster_size;
        let mut out: Vec<(u64, u64)> = Vec::new();
        if self.bitmap.is_empty() {
            return out;
        }
        // Clusters beyond the bitmap are treated as allocated (unknown).
        let free = |c: u64| {
            self.bitmap
                .get((c / 8) as usize)
                .is_some_and(|b| b & (1 << (c % 8)) == 0)
        };
        let total = self.geo.total_clusters;
        let mut c = 0u64;
        while c < total {
            if c.is_multiple_of(8) && self.bitmap.get((c / 8) as usize).is_none_or(|&b| b == 0xFF) {
                c += 8;
                continue;
            }
            if free(c) {
                let start = c;
                while c < total && free(c) {
                    c += 1;
                }
                out.push((start * cs, c * cs));
            } else {
                c += 1;
            }
        }
        out
    }

    /// Byte ranges of the volume that are allocated now (complement of free).
    pub fn allocated_extents(&self) -> Vec<(u64, u64)> {
        let end = self.geo.total_clusters * self.geo.cluster_size;
        let mut out = Vec::new();
        let mut pos = 0;
        for (s, e) in self.free_extents() {
            if s > pos {
                out.push((pos, s));
            }
            pos = e;
        }
        if pos < end {
            out.push((pos, end));
        }
        out
    }

    fn plan_runs(&self, runs: &[Run], size: u64) -> NtfsReadPlan {
        let cs = self.geo.cluster_size;
        let mut plan = NtfsReadPlan {
            pieces: Vec::new(),
            clusters_total: 0,
            clusters_now_allocated: 0,
            first_cluster_allocated: false,
            notes: Vec::new(),
        };
        let mut remaining = size;
        let mut sorted = runs.to_vec();
        sorted.sort_by_key(|r| r.vcn);
        let mut expect_vcn = sorted.first().map_or(0, |r| r.vcn);
        if expect_vcn != 0 {
            plan.notes
                .push("run list does not start at VCN 0; leading data missing".into());
        }
        for (i, r) in sorted.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            if r.vcn != expect_vcn {
                plan.notes
                    .push(format!("gap in run list at VCN {expect_vcn}"));
            }
            expect_vcn = r.vcn + r.clusters;
            let bytes = (r.clusters * cs).min(remaining);
            let used_clusters = bytes.div_ceil(cs);
            match r.lcn {
                Some(lcn) => {
                    if lcn + r.clusters > self.geo.total_clusters {
                        plan.notes.push("run extends beyond the volume".into());
                    }
                    let alloc = (lcn..lcn + used_clusters)
                        .filter(|&c| self.is_allocated(c))
                        .count() as u64;
                    if i == 0 && self.is_allocated(lcn) {
                        plan.first_cluster_allocated = true;
                    }
                    plan.clusters_now_allocated += alloc;
                    plan.clusters_total += used_clusters;
                    plan.pieces.push((Some(lcn * cs), bytes));
                }
                None => plan.pieces.push((None, bytes)),
            }
            remaining -= bytes;
        }
        if remaining > 0 {
            plan.notes.push(format!(
                "run list covers {remaining} bytes less than the file size"
            ));
        }
        if sorted.len() > 1 {
            plan.notes.push(format!(
                "{} data runs (fragmented); run list followed",
                sorted.len()
            ));
        }
        plan
    }

    /// Plan how to read an entry's data.
    pub fn plan(&self, e: &NtfsEntry) -> NtfsReadPlan {
        match &e.data {
            DataLocation::NonResident { runs, size } => self.plan_runs(runs, *size),
            DataLocation::Resident(d) => NtfsReadPlan {
                pieces: Vec::new(),
                clusters_total: 0,
                clusters_now_allocated: 0,
                first_cluster_allocated: false,
                notes: vec![format!("{} bytes stored inside the MFT record", d.len())],
            },
            DataLocation::Missing => NtfsReadPlan {
                pieces: Vec::new(),
                clusters_total: 0,
                clusters_now_allocated: 0,
                first_cluster_allocated: false,
                notes: vec!["no data attribute".into()],
            },
        }
    }

    /// Read planned data (resident data is returned directly).
    pub fn read_entry(&self, e: &NtfsEntry, max: u64) -> std::io::Result<Vec<u8>> {
        match &e.data {
            DataLocation::Resident(d) => Ok(d.clone()),
            DataLocation::Missing => Ok(Vec::new()),
            DataLocation::NonResident { size, .. } => {
                if *size > max {
                    return Err(std::io::Error::other(format!(
                        "file of {size} bytes exceeds limit {max}"
                    )));
                }
                self.read(&self.plan(e), *size)
            }
        }
    }

    fn read(&self, plan: &NtfsReadPlan, size: u64) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(size as usize);
        for &(off, len) in &plan.pieces {
            match off {
                Some(o) => out.extend_from_slice(&read_vec_at(self.src, o, len as usize)?),
                None => out.resize(out.len() + len as usize, 0),
            }
        }
        Ok(out)
    }

    /// Head bytes of an entry's data (format sniffing without a full read).
    pub fn head(&self, e: &NtfsEntry, n: usize) -> Vec<u8> {
        match &e.data {
            DataLocation::Resident(d) => d[..d.len().min(n)].to_vec(),
            DataLocation::NonResident { runs, .. } => {
                let first = runs.iter().filter(|r| r.vcn == 0).find_map(|r| r.lcn);
                let mut b = vec![0u8; n];
                match first {
                    Some(lcn) => {
                        let k = read_full_at(self.src, lcn * self.geo.cluster_size, &mut b)
                            .unwrap_or(0);
                        b.truncate(k);
                        b
                    }
                    None => Vec::new(),
                }
            }
            DataLocation::Missing => Vec::new(),
        }
    }

    pub fn record_count(&self) -> u64 {
        self.mft_records
    }

    /// Walk the whole MFT. `want(name, deleted)` filters file entries
    /// (directories are always kept for path reconstruction), so memory
    /// stays proportional to the files of interest; `progress(done, total)`.
    pub fn walk(
        &self,
        want: &dyn Fn(&str, bool) -> bool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Vec<NtfsEntry> {
        // record → (name, parent, parent_seq, seq, in_use)
        let mut dirs: HashMap<u64, (String, u64, u16, u16)> = HashMap::new();
        let mut files: Vec<(u64, RecordInfo)> = Vec::new();
        let mut ext: HashMap<u64, Vec<(Vec<Run>, u64)>> = HashMap::new();
        for n in 0..self.mft_records {
            if n % 4096 == 0 {
                progress(n, self.mft_records);
            }
            let Some(rec) = self.record(n) else { continue };
            let Some(info) = parse_record(&rec) else {
                continue;
            };
            if info.base != 0 && info.base != n {
                // Extension record: carries more of the base record's $DATA runs.
                if let Some((None, runs, _, vcn)) = &info.data {
                    ext.entry(info.base).or_default().push((runs.clone(), *vcn));
                }
                continue;
            }
            let Some((name, parent, pseq, _)) = &info.name else {
                continue;
            };
            if info.is_dir {
                dirs.insert(n, (name.clone(), *parent, *pseq, info.sequence));
            } else if n >= 16 && want(name, !info.in_use) {
                files.push((n, info));
            }
        }
        progress(self.mft_records, self.mft_records);
        let path_of = |mut parent: u64, mut pseq: u16| -> (String, bool) {
            let mut parts = Vec::new();
            let mut orphan = false;
            for _ in 0..256 {
                if parent == ROOT_RECORD {
                    break;
                }
                match dirs.get(&parent) {
                    // A deleted record's sequence is incremented on reuse;
                    // allow the in-use sequence or the one before it.
                    Some((name, pp, ps, seq)) if *seq == pseq || seq.wrapping_sub(1) == pseq => {
                        parts.push(name.clone());
                        parent = *pp;
                        pseq = *ps;
                    }
                    _ => {
                        orphan = true;
                        parts.push(format!("$Orphan{parent}"));
                        break;
                    }
                }
            }
            parts.reverse();
            (format!("/{}", parts.join("/")), orphan)
        };
        files
            .into_iter()
            .map(|(n, info)| {
                let (name, parent, pseq, _) = info.name.clone().unwrap_or_default();
                let (dir, orphan) = path_of(parent, pseq);
                let path = if dir == "/" {
                    format!("/{name}")
                } else {
                    format!("{dir}/{name}")
                };
                let data = match info.data {
                    Some((Some(res), _, _, _)) => DataLocation::Resident(res),
                    Some((None, mut runs, size, _)) => {
                        if let Some(extra) = ext.get(&n) {
                            for (r, _) in extra {
                                runs.extend_from_slice(r);
                            }
                        }
                        DataLocation::NonResident { runs, size }
                    }
                    None => DataLocation::Missing,
                };
                let size = match &data {
                    DataLocation::Resident(d) => d.len() as u64,
                    DataLocation::NonResident { size, .. } => *size,
                    DataLocation::Missing => info.size_from_name,
                };
                NtfsEntry {
                    record: n,
                    sequence: info.sequence,
                    path,
                    name,
                    deleted: !info.in_use,
                    is_dir: false,
                    size,
                    created: filetime(info.created),
                    modified: filetime(info.modified),
                    data,
                    compressed: info.compressed,
                    encrypted: info.encrypted,
                    orphan,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_list_decoding() {
        // 0x21: len 1 byte, off 2 bytes. Run 1: 0x18 clusters at 0x5634;
        // run 2 (0x11): 0x10 clusters at -0x10 relative; then sparse 4.
        let b = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xF0, 0x01, 0x04, 0x00];
        let r = decode_runs(&b, 0).unwrap();
        assert_eq!(
            r[0],
            Run {
                vcn: 0,
                lcn: Some(0x5634),
                clusters: 0x18
            }
        );
        assert_eq!(
            r[1],
            Run {
                vcn: 0x18,
                lcn: Some(0x5634 - 0x10),
                clusters: 0x10
            }
        );
        assert_eq!(
            r[2],
            Run {
                vcn: 0x28,
                lcn: None,
                clusters: 4
            }
        );
        assert!(decode_runs(&[0x21, 0x18], 0).is_none());
    }

    #[test]
    fn fixup_detects_torn_records() {
        let mut r = vec![0u8; 1024];
        r[..4].copy_from_slice(b"FILE");
        r[4] = 48; // usa offset
        r[6] = 3; // usn + 2 entries
        r[48..50].copy_from_slice(&[0xAB, 0xCD]);
        r[50..52].copy_from_slice(&[1, 2]);
        r[52..54].copy_from_slice(&[3, 4]);
        r[510..512].copy_from_slice(&[0xAB, 0xCD]);
        r[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        let mut ok = r.clone();
        assert!(apply_fixup(&mut ok, 512));
        assert_eq!(&ok[510..512], &[1, 2]);
        assert_eq!(&ok[1022..1024], &[3, 4]);
        r[1023] = 0;
        assert!(!apply_fixup(&mut r, 512));
    }

    #[test]
    fn filetime_conversion() {
        // 2024-05-01T12:00:00Z
        let ft = (1_714_564_800u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(filetime(ft).as_deref(), Some("2024-05-01T12:00:00Z"));
    }
}
