//! exFAT parser with deleted-file recovery (memory cards, USB sticks > 32 GB).
//!
//! Deleting a file on exFAT clears the "in use" bit of its directory entry
//! set (0x85 → 0x05, 0xC0 → 0x40, 0xC1 → 0x41) and its clusters in the
//! allocation bitmap, but the name, size and first cluster stay. Files
//! written in one contiguous run carry the `NoFatChain` flag, so their
//! exact location is known and they can be read back precisely. Fragmented
//! files depend on the FAT chain, which may have been cleared; the reader
//! follows it when still present and otherwise assumes contiguity.

use crate::FsError;
use pr_model::source::{read_full_at, read_vec_at, ReadAt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_ENTRIES: usize = 4_000_000;
const MAX_DEPTH: usize = 32;
const MAX_DIR_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BITMAP: u64 = 512 * 1024 * 1024;
const MAX_FAT: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ExfatGeometry {
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub fat_offset: u64,
    pub heap_offset: u64,
    pub cluster_count: u32,
    pub root_cluster: u32,
}

impl ExfatGeometry {
    pub fn cluster_offset(&self, c: u32) -> u64 {
        self.heap_offset + (c as u64 - 2) * self.cluster_size
    }
    fn valid(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.cluster_count as u64 + 2
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExfatEntry {
    pub path: String,
    pub name: String,
    pub deleted: bool,
    pub is_dir: bool,
    pub size: u64,
    pub first_cluster: u32,
    /// `NoFatChain`: the data is one contiguous run (exact location known).
    pub contiguous: bool,
    pub modified: Option<String>,
    pub created: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExfatPlan {
    /// Volume-relative `(offset, length)` pieces.
    pub pieces: Vec<(u64, u64)>,
    pub clusters_total: u64,
    pub clusters_now_allocated: u64,
    pub first_cluster_allocated: bool,
    /// The cluster list is known exactly (NoFatChain or intact FAT chain).
    pub exact: bool,
    pub notes: Vec<String>,
}

pub struct ExfatVolume<'a> {
    src: &'a dyn ReadAt,
    pub geo: ExfatGeometry,
    fat: Vec<u32>,
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

/// exFAT timestamp → `YYYY-MM-DD HH:MM:SS` (local time as recorded).
fn ts(t: u32) -> Option<String> {
    if t == 0 {
        return None;
    }
    let (sec, min, hour) = ((t & 31) * 2, (t >> 5) & 63, (t >> 11) & 31);
    let (day, month, year) = ((t >> 16) & 31, (t >> 21) & 15, 1980 + (t >> 25));
    if !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}"
    ))
}

impl<'a> ExfatVolume<'a> {
    pub fn open(src: &'a dyn ReadAt) -> Result<Self, FsError> {
        let bs = read_vec_at(src, 0, 512)?;
        if &bs[3..11] != b"EXFAT   " {
            return Err(FsError::Unsupported("not an exFAT boot sector".into()));
        }
        let bps_shift = bs[108] as u32;
        let spc_shift = bs[109] as u32;
        if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
            return Err(FsError::Invalid("bad exFAT geometry".into()));
        }
        let bps = 1u64 << bps_shift;
        let geo = ExfatGeometry {
            bytes_per_sector: bps,
            cluster_size: bps << spc_shift,
            fat_offset: le32(&bs, 80) as u64 * bps,
            heap_offset: le32(&bs, 88) as u64 * bps,
            cluster_count: le32(&bs, 92),
            root_cluster: le32(&bs, 96),
        };
        let fat_bytes = (geo.cluster_count as u64 + 2) * 4;
        let fat = if fat_bytes <= MAX_FAT {
            read_vec_at(src, geo.fat_offset, fat_bytes as usize)
                .map(|b| {
                    b.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes(*c))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut vol = ExfatVolume {
            src,
            geo,
            fat,
            bitmap: Vec::new(),
        };
        // The allocation bitmap is described by a 0x81 entry in the root directory.
        let root = vol.dir_bytes(vol.geo.root_cluster, None, false);
        for e in root.as_chunks::<32>().0 {
            if e[0] == 0x81 && e[1] & 1 == 0 {
                let first = le32(e, 20);
                let len = le64(e, 24);
                if len <= MAX_BITMAP && vol.geo.valid(first) {
                    vol.bitmap = vol.read_contiguous(first, len).unwrap_or_default();
                }
                break;
            }
        }
        Ok(vol)
    }

    fn read_contiguous(&self, first: u32, len: u64) -> std::io::Result<Vec<u8>> {
        read_vec_at(self.src, self.geo.cluster_offset(first), len as usize)
    }

    pub fn has_bitmap(&self) -> bool {
        !self.bitmap.is_empty()
    }

    pub fn is_allocated(&self, c: u32) -> bool {
        if c < 2 {
            return true;
        }
        let i = (c - 2) as usize;
        self.bitmap
            .get(i / 8)
            .is_some_and(|b| b & (1 << (i % 8)) != 0)
    }

    /// FAT chain from `first`, if it is intact for `needed` clusters.
    fn chain(&self, first: u32, needed: u64) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut c = first;
        while (out.len() as u64) < needed {
            if !self.geo.valid(c) || !seen.insert(c) {
                return None;
            }
            out.push(c);
            let next = *self.fat.get(c as usize)?;
            if out.len() as u64 == needed {
                break;
            }
            if !(2..0xFFFF_FFF7).contains(&next) {
                return None;
            }
            c = next;
        }
        Some(out)
    }

    /// Clusters of a file or directory.
    fn clusters(&self, first: u32, len: u64, contiguous: bool) -> (Vec<u32>, bool) {
        let needed = len.div_ceil(self.geo.cluster_size).max(1);
        if contiguous {
            return (
                (first..first.saturating_add(needed as u32))
                    .take_while(|c| self.geo.valid(*c))
                    .collect(),
                true,
            );
        }
        match self.chain(first, needed) {
            Some(c) => (c, true),
            None => (
                (first..first.saturating_add(needed as u32))
                    .take_while(|c| self.geo.valid(*c))
                    .collect(),
                false,
            ),
        }
    }

    fn dir_bytes(&self, first: u32, len: Option<u64>, contiguous: bool) -> Vec<u8> {
        let clusters = match len {
            Some(l) => self.clusters(first, l.min(MAX_DIR_BYTES), contiguous).0,
            // Root directory: follow the FAT chain to its end.
            None => {
                let mut v = Vec::new();
                let mut seen = HashSet::new();
                let mut c = first;
                while self.geo.valid(c)
                    && seen.insert(c)
                    && (v.len() as u64) * self.geo.cluster_size < MAX_DIR_BYTES
                {
                    v.push(c);
                    match self.fat.get(c as usize) {
                        Some(&n) if (2..0xFFFF_FFF7).contains(&n) => c = n,
                        _ => break,
                    }
                }
                v
            }
        };
        let cs = self.geo.cluster_size as usize;
        let mut out = Vec::with_capacity(clusters.len() * cs);
        for c in clusters {
            let mut b = vec![0u8; cs];
            let n = read_full_at(self.src, self.geo.cluster_offset(c), &mut b).unwrap_or(0);
            out.extend_from_slice(&b[..n]);
        }
        out
    }

    /// Walk the directory tree including deleted entry sets.
    pub fn walk(&self) -> Vec<ExfatEntry> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let root = self.dir_bytes(self.geo.root_cluster, None, false);
        self.walk_dir(&root, "", 0, &mut visited, &mut out);
        out
    }

    fn walk_dir(
        &self,
        bytes: &[u8],
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<u32>,
        out: &mut Vec<ExfatEntry>,
    ) {
        if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
            return;
        }
        let entries: Vec<&[u8]> = bytes
            .as_chunks::<32>()
            .0
            .iter()
            .map(|c| c.as_slice())
            .collect();
        let mut i = 0;
        let mut subdirs = Vec::new();
        while i < entries.len() {
            let e = entries[i];
            let ty = e[0];
            if ty == 0x00 {
                break; // end of directory
            }
            // File directory entry, in use (0x85) or deleted (0x05).
            if ty & 0x7F != 0x05 {
                i += 1;
                continue;
            }
            let deleted = ty & 0x80 == 0;
            let secondary = e[1] as usize;
            if secondary < 2 || i + secondary > entries.len() {
                i += 1;
                continue;
            }
            let stream = entries.get(i + 1).copied();
            let Some(s) = stream.filter(|s| s[0] & 0x7F == 0x40 && (s[0] & 0x80 != 0) != deleted)
            else {
                i += 1;
                continue;
            };
            let name_len = s[3] as usize;
            let mut units: Vec<u16> = Vec::with_capacity(name_len);
            for k in 0..secondary - 1 {
                let Some(n) = entries.get(i + 2 + k) else {
                    break;
                };
                if n[0] & 0x7F != 0x41 {
                    break;
                }
                for j in 0..15 {
                    units.push(le16(n, 2 + 2 * j));
                }
            }
            units.truncate(name_len);
            let name = String::from_utf16_lossy(&units);
            let attrs = le16(e, 4);
            let entry = ExfatEntry {
                path: format!("{prefix}/{name}"),
                name,
                deleted,
                is_dir: attrs & 0x10 != 0,
                size: le64(s, 24),
                first_cluster: le32(s, 20),
                contiguous: s[1] & 2 != 0,
                modified: ts(le32(e, 12)),
                created: ts(le32(e, 8)),
            };
            if entry.is_dir
                && self.geo.valid(entry.first_cluster)
                && !entry.name.is_empty()
                && !entry.name.contains(['/', '\0'])
            {
                subdirs.push(entry.clone());
            }
            if !entry.name.is_empty() {
                out.push(entry);
            }
            i += 1 + secondary;
        }
        for d in subdirs {
            if !visited.insert(d.first_cluster) {
                continue;
            }
            // A deleted directory's clusters may have been reused.
            if d.deleted && self.is_allocated(d.first_cluster) {
                continue;
            }
            let b = self.dir_bytes(
                d.first_cluster,
                Some(d.size.max(self.geo.cluster_size)),
                d.contiguous,
            );
            self.walk_dir(&b, &d.path, depth + 1, visited, out);
        }
    }

    pub fn plan(&self, e: &ExfatEntry) -> ExfatPlan {
        let cs = self.geo.cluster_size;
        let mut plan = ExfatPlan {
            pieces: Vec::new(),
            clusters_total: 0,
            clusters_now_allocated: 0,
            first_cluster_allocated: false,
            exact: true,
            notes: Vec::new(),
        };
        if e.size == 0 || !self.geo.valid(e.first_cluster) {
            plan.notes.push("no data clusters recorded".into());
            return plan;
        }
        let (clusters, exact) = self.clusters(e.first_cluster, e.size, e.contiguous);
        plan.exact = exact;
        if !exact {
            plan.notes.push("cluster chain no longer available; contiguous allocation from the first cluster is assumed".into());
        } else if e.contiguous {
            plan.notes
                .push("contiguous file (NoFatChain): exact location known".into());
        }
        plan.clusters_total = clusters.len() as u64;
        if e.deleted {
            plan.clusters_now_allocated =
                clusters.iter().filter(|&&c| self.is_allocated(c)).count() as u64;
            plan.first_cluster_allocated = self.is_allocated(e.first_cluster);
        }
        let mut remaining = e.size;
        for c in clusters {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(cs);
            let off = self.geo.cluster_offset(c);
            match plan.pieces.last_mut() {
                Some(last) if last.0 + last.1 == off => last.1 += take,
                _ => plan.pieces.push((off, take)),
            }
            remaining -= take;
        }
        plan
    }

    /// Volume-relative ranges allocated now (metadata area + used clusters).
    pub fn allocated_extents(&self) -> Vec<(u64, u64)> {
        let mut out = vec![(0, self.geo.heap_offset)];
        let end = self.geo.cluster_count + 2;
        let mut c = 2u32;
        while c < end {
            if self.is_allocated(c) {
                let s = c;
                while c < end && self.is_allocated(c) {
                    c += 1;
                }
                out.push((self.geo.cluster_offset(s), self.geo.cluster_offset(c)));
            } else {
                c += 1;
            }
        }
        out
    }
}
