//! FAT12/16/32 parser with deleted directory-entry recovery.
//!
//! Deleted FAT files keep their directory entry (first byte set to 0xE5),
//! start cluster and size, but the cluster chain in the FAT is zeroed. Data
//! can therefore only be read by *assuming* contiguous allocation from the
//! start cluster. Every recovered file states that assumption, reports how
//! many of the assumed clusters are now allocated to other files, and must
//! pass image validation before it is reported as recovered.

use crate::FsError;
use pr_model::source::{read_full_at, read_vec_at, ReadAt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Clone, Debug)]
pub struct FatGeometry {
    pub fat_type: FatType,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    pub fat_sectors: u32,
    pub root_entries: u32,
    pub root_cluster: u32,
    pub total_sectors: u32,
    pub cluster_count: u32,
}

impl FatGeometry {
    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }
    fn root_dir_offset(&self) -> u64 {
        (self.reserved_sectors + self.num_fats * self.fat_sectors) as u64
            * self.bytes_per_sector as u64
    }
    fn data_offset(&self) -> u64 {
        self.root_dir_offset()
            + (self.root_entries as u64 * 32).div_ceil(self.bytes_per_sector as u64)
                * self.bytes_per_sector as u64
    }
    pub fn cluster_offset(&self, c: u32) -> u64 {
        self.data_offset() + (c as u64 - 2) * self.cluster_size()
    }
    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c < self.cluster_count + 2
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FatEntry {
    /// Full path inside the volume, `/`-separated.
    pub path: String,
    pub size: u32,
    pub first_cluster: u32,
    pub deleted: bool,
    pub is_dir: bool,
    /// `YYYY-MM-DD HH:MM:SS` (local time as stored by FAT).
    pub modified: Option<String>,
    pub created: Option<String>,
    /// Byte offset of the short directory entry within the volume.
    pub entry_offset: u64,
    /// For deleted entries the first character of the short name is lost.
    pub name_reconstructed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReadPlan {
    /// Absolute (volume-relative) byte ranges to read, in order.
    pub ranges: Vec<(u64, u64)>,
    pub contiguous_assumed: bool,
    /// Assumed clusters that the FAT now marks as allocated (overwrite risk).
    pub clusters_now_allocated: u32,
    pub clusters_total: u32,
    pub notes: Vec<String>,
}

pub struct FatVolume<'a> {
    src: &'a dyn ReadAt,
    pub geo: FatGeometry,
    fat: Vec<u32>,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn fat_datetime(date: u16, time: u16) -> Option<String> {
    if date == 0 {
        return None;
    }
    let (y, m, d) = (1980 + (date >> 9), (date >> 5) & 15, date & 31);
    let (hh, mm, ss) = (time >> 11, (time >> 5) & 63, (time & 31) * 2);
    if !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"))
}

/// Upper bound on FAT size we load into memory (FAT32 max is 1 GiB of FAT
/// for 2^28 clusters; typical SD cards are far smaller).
const MAX_FAT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 2_000_000;
const MAX_DEPTH: usize = 32;

impl<'a> FatVolume<'a> {
    pub fn open(src: &'a dyn ReadAt) -> Result<Self, FsError> {
        let bs = read_vec_at(src, 0, 512)?;
        if bs[510] != 0x55 || bs[511] != 0xAA {
            return Err(FsError::Unsupported("missing boot signature".into()));
        }
        let bps = le16(&bs, 11) as u32;
        let spc = bs[13] as u32;
        let reserved = le16(&bs, 14) as u32;
        let nfats = bs[16] as u32;
        let root_entries = le16(&bs, 17) as u32;
        let total = if le16(&bs, 19) != 0 {
            le16(&bs, 19) as u32
        } else {
            le32(&bs, 32)
        };
        let fat_sectors = if le16(&bs, 22) != 0 {
            le16(&bs, 22) as u32
        } else {
            le32(&bs, 36)
        };
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || spc == 0
            || !spc.is_power_of_two()
            || nfats == 0
            || reserved == 0
            || fat_sectors == 0
        {
            return Err(FsError::Unsupported("not a FAT boot sector".into()));
        }
        let root_secs = (root_entries * 32).div_ceil(bps);
        let data_secs = total
            .checked_sub(reserved + nfats * fat_sectors + root_secs)
            .ok_or_else(|| FsError::Invalid("geometry exceeds volume".into()))?;
        let clusters = data_secs / spc;
        let fat_type = if clusters < 4085 {
            FatType::Fat12
        } else if clusters < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };
        let geo = FatGeometry {
            fat_type,
            bytes_per_sector: bps,
            sectors_per_cluster: spc,
            reserved_sectors: reserved,
            num_fats: nfats,
            fat_sectors,
            root_entries,
            root_cluster: if fat_type == FatType::Fat32 {
                le32(&bs, 44)
            } else {
                0
            },
            total_sectors: total,
            cluster_count: clusters,
        };
        let fat_bytes = fat_sectors as u64 * bps as u64;
        if fat_bytes > MAX_FAT_BYTES {
            return Err(FsError::Unsupported(format!(
                "FAT of {fat_bytes} bytes exceeds limit"
            )));
        }
        let raw = read_vec_at(src, reserved as u64 * bps as u64, fat_bytes as usize)?;
        let n = clusters as usize + 2;
        let fat: Vec<u32> = match fat_type {
            FatType::Fat32 => (0..n)
                .map(|i| {
                    raw.get(i * 4..i * 4 + 4)
                        .map_or(0, |b| le32(b, 0) & 0x0FFF_FFFF)
                })
                .collect(),
            FatType::Fat16 => (0..n)
                .map(|i| raw.get(i * 2..i * 2 + 2).map_or(0, |b| le16(b, 0) as u32))
                .collect(),
            FatType::Fat12 => (0..n)
                .map(|i| {
                    let o = i + i / 2;
                    let v = raw.get(o..o + 2).map_or(0, |b| le16(b, 0)) as u32;
                    if i & 1 == 0 {
                        v & 0xFFF
                    } else {
                        v >> 4
                    }
                })
                .collect(),
        };
        Ok(FatVolume { src, geo, fat })
    }

    fn is_eoc(&self, v: u32) -> bool {
        match self.geo.fat_type {
            FatType::Fat32 => v >= 0x0FFF_FFF8,
            FatType::Fat16 => v >= 0xFFF8,
            FatType::Fat12 => v >= 0xFF8,
        }
    }

    pub fn is_allocated(&self, c: u32) -> bool {
        self.fat.get(c as usize).is_some_and(|&v| v != 0)
    }

    /// Follow a live cluster chain (with loop and bounds protection).
    pub fn chain(&self, first: u32, max: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut c = first;
        while self.geo.valid_cluster(c) && seen.insert(c) && out.len() < max {
            out.push(c);
            let next = self.fat[c as usize];
            if next == 0 || self.is_eoc(next) {
                break;
            }
            c = next;
        }
        out
    }

    fn dir_bytes(&self, first_cluster: u32, deleted: bool) -> Vec<u8> {
        if first_cluster == 0 {
            // FAT12/16 fixed root directory.
            let len = self.geo.root_entries as usize * 32;
            return read_vec_at(self.src, self.geo.root_dir_offset(), len).unwrap_or_default();
        }
        let clusters = if deleted {
            // Chain is gone: read up to 64 contiguous clusters; parsing stops
            // at the first never-used entry.
            (first_cluster..first_cluster.saturating_add(64))
                .take_while(|c| self.geo.valid_cluster(*c))
                .collect()
        } else {
            self.chain(first_cluster, 1 << 16)
        };
        let cs = self.geo.cluster_size() as usize;
        let mut out = Vec::with_capacity(clusters.len() * cs);
        for c in clusters {
            let mut b = vec![0u8; cs];
            let n = read_full_at(self.src, self.geo.cluster_offset(c), &mut b).unwrap_or(0);
            b.truncate(n);
            out.extend_from_slice(&b);
            if n < cs {
                break;
            }
        }
        out
    }

    /// Walk the directory tree, including deleted entries (and deleted
    /// directories whose first cluster still holds a directory).
    pub fn walk(&self) -> Vec<FatEntry> {
        let mut out = Vec::new();
        let root = if self.geo.fat_type == FatType::Fat32 {
            self.geo.root_cluster
        } else {
            0
        };
        let mut visited = HashSet::new();
        self.walk_dir(root, "", false, 0, &mut visited, &mut out);
        out
    }

    fn walk_dir(
        &self,
        cluster: u32,
        prefix: &str,
        deleted_dir: bool,
        depth: usize,
        visited: &mut HashSet<u32>,
        out: &mut Vec<FatEntry>,
    ) {
        if depth > MAX_DEPTH
            || out.len() >= MAX_ENTRIES
            || (cluster != 0 && !visited.insert(cluster))
        {
            return;
        }
        let bytes = self.dir_bytes(cluster, deleted_dir);
        let base = if cluster == 0 {
            self.geo.root_dir_offset()
        } else {
            self.geo.cluster_offset(cluster)
        };
        let mut lfn: Vec<(u8, Vec<u16>)> = Vec::new();
        let mut subdirs = Vec::new();
        for (i, e) in bytes.as_chunks::<32>().0.iter().enumerate() {
            if e[0] == 0x00 {
                break; // never used: end of directory
            }
            let attr = e[11];
            let deleted = e[0] == 0xE5;
            if attr & 0x3F == 0x0F {
                let mut units = Vec::with_capacity(13);
                for p in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
                    units.push(le16(e, p));
                }
                lfn.push((e[13], units));
                continue;
            }
            let pending_lfn = std::mem::take(&mut lfn);
            if attr & 0x08 != 0 {
                continue; // volume label
            }
            if e[0] == b'.' {
                continue;
            }
            let cluster_hi = if self.geo.fat_type == FatType::Fat32 {
                le16(e, 20) as u32
            } else {
                0
            };
            let first = (cluster_hi << 16) | le16(e, 26) as u32;
            let size = le32(e, 28);
            let is_dir = attr & 0x10 != 0;
            // Long name: LFN entries are stored last-part-first.
            let long = (!pending_lfn.is_empty())
                .then(|| {
                    let sum = pending_lfn[0].0;
                    if pending_lfn.iter().any(|(s, _)| *s != sum) {
                        return None;
                    }
                    let units: Vec<u16> = pending_lfn
                        .iter()
                        .rev()
                        .flat_map(|(_, u)| u.iter().copied())
                        .take_while(|&u| u != 0 && u != 0xFFFF)
                        .collect();
                    let s = String::from_utf16_lossy(&units);
                    (!s.is_empty()).then_some(s)
                })
                .flatten();
            let short = {
                let mut n = e[..8].to_vec();
                if deleted {
                    n[0] = b'_';
                }
                let base = String::from_utf8_lossy(&n).trim_end().to_string();
                let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
                if ext.is_empty() {
                    base
                } else {
                    format!("{base}.{ext}")
                }
            };
            let name_reconstructed = deleted && long.is_none();
            let name = long.unwrap_or(short);
            if name.chars().any(|c| c.is_control()) {
                continue;
            }
            let path = format!("{prefix}/{name}");
            out.push(FatEntry {
                path: path.clone(),
                size,
                first_cluster: first,
                deleted,
                is_dir,
                modified: fat_datetime(le16(e, 24), le16(e, 22)),
                created: fat_datetime(le16(e, 16), le16(e, 14)),
                entry_offset: base + (i * 32) as u64,
                name_reconstructed,
            });
            if is_dir && self.geo.valid_cluster(first) {
                if deleted && !self.looks_like_dir(first) {
                    continue;
                }
                subdirs.push((first, path, deleted));
            }
        }
        for (c, p, d) in subdirs {
            self.walk_dir(c, &p, deleted_dir || d, depth + 1, visited, out);
        }
    }

    /// A directory cluster starts with "." and ".." entries.
    fn looks_like_dir(&self, c: u32) -> bool {
        let mut b = [0u8; 64];
        if read_full_at(self.src, self.geo.cluster_offset(c), &mut b).unwrap_or(0) < 64 {
            return false;
        }
        &b[..11] == b".          " && &b[32..43] == b"..         " && b[11] & 0x10 != 0
    }

    /// Plan how to read an entry's data.
    pub fn plan(&self, e: &FatEntry) -> ReadPlan {
        let cs = self.geo.cluster_size();
        let needed = (e.size as u64).div_ceil(cs).max(1) as u32;
        let mut plan = ReadPlan {
            ranges: Vec::new(),
            contiguous_assumed: e.deleted,
            clusters_now_allocated: 0,
            clusters_total: needed,
            notes: Vec::new(),
        };
        if e.size == 0 || !self.geo.valid_cluster(e.first_cluster) {
            plan.notes.push("no data clusters recorded".into());
            return plan;
        }
        let clusters: Vec<u32> = if e.deleted {
            if self.geo.fat_type == FatType::Fat32
                && e.first_cluster < 65536
                && self.geo.cluster_count > 65536
            {
                plan.notes.push("start cluster < 65536 on a large FAT32 volume: some systems zero the high word on delete, so the start cluster may be wrong".into());
            }
            plan.notes.push("cluster chain was freed on deletion; contiguous allocation from the recorded start cluster is assumed".into());
            (e.first_cluster..e.first_cluster.saturating_add(needed))
                .take_while(|c| self.geo.valid_cluster(*c))
                .collect()
        } else {
            self.chain(e.first_cluster, needed as usize + 1)
        };
        if e.deleted {
            plan.clusters_now_allocated =
                clusters.iter().filter(|&&c| self.is_allocated(c)).count() as u32;
            if plan.clusters_now_allocated > 0 {
                plan.notes.push(format!(
                    "{} of {} assumed clusters are now allocated to other data — content is likely overwritten",
                    plan.clusters_now_allocated, needed
                ));
            }
        } else if clusters.len() < needed as usize {
            plan.notes
                .push("cluster chain shorter than file size".into());
        }
        let mut remaining = e.size as u64;
        for c in clusters {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(cs);
            let off = self.geo.cluster_offset(c);
            match plan.ranges.last_mut() {
                Some(last) if last.0 + last.1 == off => last.1 += take,
                _ => plan.ranges.push((off, take)),
            }
            remaining -= take;
        }
        if remaining > 0 {
            plan.notes
                .push(format!("{remaining} bytes beyond the end of the volume"));
        }
        plan
    }

    /// Read planned data (bounded by `max_bytes`).
    pub fn read(&self, plan: &ReadPlan, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        let total: u64 = plan.ranges.iter().map(|r| r.1).sum();
        if total > max_bytes {
            return Err(std::io::Error::other(format!(
                "file of {total} bytes exceeds limit {max_bytes}"
            )));
        }
        let mut out = Vec::with_capacity(total as usize);
        for &(off, len) in &plan.ranges {
            out.extend_from_slice(&read_vec_at(self.src, off, len as usize)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::source::MemSource;
    use pr_testkit::fat::FatBuilder;
    use pr_testkit::images::{self, JpegOpts};

    #[test]
    fn live_deleted_overwritten_and_fragmented() {
        let mut b = FatBuilder::new(34, 1);
        b.mkdir("DCIM");
        b.mkdir("DCIM/Camera");
        let keep = images::jpeg(200, 150, 1, &JpegOpts::default());
        let gone = images::jpeg(220, 160, 2, &JpegOpts::default());
        let over = images::jpeg(180, 140, 3, &JpegOpts::default());
        let frag = images::jpeg(240, 180, 4, &JpegOpts::default());
        b.add_file("DCIM/Camera/IMG_20240501_120000.jpg", &keep);
        b.add_file("DCIM/Camera/IMG_20240501_120100.jpg", &gone);
        let f_over = b.add_file("DCIM/Camera/IMG_20240501_120200.jpg", &over);
        b.add_interleaved(
            "DCIM/Camera/IMG_20240501_120300.jpg",
            &frag,
            "DCIM/Camera/note.txt",
            &[b'x'; 3000],
        );
        b.delete("DCIM/Camera/IMG_20240501_120100.jpg");
        b.delete("DCIM/Camera/IMG_20240501_120200.jpg");
        b.delete("DCIM/Camera/IMG_20240501_120300.jpg");
        // Reuse the clusters of the deleted "over" file.
        b.add_file_clusters(
            "DCIM/Camera/new.bin",
            &vec![0xAB; over.len()],
            f_over.clusters.clone(),
        );
        let img = b.finish();
        let src = MemSource::new(img);
        let v = FatVolume::open(&src).unwrap();
        assert_eq!(v.geo.fat_type, FatType::Fat32);
        let entries = v.walk();
        let find = |p: &str| {
            entries
                .iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("{p} in {entries:#?}"))
        };

        let live = find("/DCIM/Camera/IMG_20240501_120000.jpg");
        assert!(!live.deleted);
        assert_eq!(live.modified.as_deref(), Some("2024-05-01 12:00:00"));
        assert_eq!(v.read(&v.plan(live), u64::MAX).unwrap(), keep);

        // Deleted, untouched: long name recovered, data identical.
        let d = find("/DCIM/Camera/IMG_20240501_120100.jpg");
        assert!(d.deleted && !d.name_reconstructed);
        let p = v.plan(d);
        assert!(p.contiguous_assumed);
        assert_eq!(p.clusters_now_allocated, 0);
        assert_eq!(v.read(&p, u64::MAX).unwrap(), gone);

        // Deleted then overwritten: clusters reported as reallocated.
        let o = find("/DCIM/Camera/IMG_20240501_120200.jpg");
        let p = v.plan(o);
        assert_eq!(p.clusters_now_allocated, p.clusters_total);
        assert_ne!(v.read(&p, u64::MAX).unwrap(), over);

        // Deleted fragmented: contiguous assumption yields wrong data.
        let f = find("/DCIM/Camera/IMG_20240501_120300.jpg");
        let p = v.plan(f);
        assert!(
            p.clusters_now_allocated > 0,
            "foreign clusters inside assumed range are allocated"
        );
        assert_ne!(v.read(&p, u64::MAX).unwrap(), frag);
    }

    #[test]
    fn rejects_non_fat() {
        let src = MemSource::new(vec![0u8; 4096]);
        assert!(FatVolume::open(&src).is_err());
    }
}
