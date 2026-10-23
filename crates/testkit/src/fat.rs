//! Minimal FAT32 image builder with controlled deletion scenarios.
//!
//! Produces spec-shaped FAT32 volumes (boot sector, FSInfo, two FATs, LFN
//! directory entries) so the filesystem engine can be tested against known
//! ground truth: intact files, deleted files with untouched data, deleted
//! files whose clusters were reused, and deleted fragmented files.

use std::collections::BTreeMap;

pub const SECTOR: usize = 512;
const EOC: u32 = 0x0FFF_FFFF;
const DIR_CLUSTERS: u32 = 8;

pub struct FatBuilder {
    img: Vec<u8>,
    spc: u32,
    reserved: u32,
    fat_sectors: u32,
    clusters: u32,
    fat: Vec<u32>,
    next_free: u32,
    /// directory path ("" = root) -> first cluster
    dirs: BTreeMap<String, u32>,
    /// file path -> (dir path, entry offsets in image, clusters)
    files: BTreeMap<String, (Vec<usize>, Vec<u32>)>,
}

/// Ground truth for one file written to the image.
#[derive(Clone, Debug)]
pub struct FatFile {
    pub path: String,
    pub clusters: Vec<u32>,
    pub size: u32,
}

impl FatBuilder {
    /// `size_mb` must be large enough for >= 65525 clusters (FAT32 by spec).
    pub fn new(size_mb: u32, sectors_per_cluster: u32) -> Self {
        let total_sectors = size_mb * 1024 * 1024 / SECTOR as u32;
        let reserved = 32;
        // Solve for FAT size (iterate; small error is fine).
        let mut fat_sectors = 1;
        let clusters = loop {
            let data = total_sectors - reserved - 2 * fat_sectors;
            let cl = data / sectors_per_cluster;
            let need = ((cl + 2) * 4).div_ceil(SECTOR as u32);
            if need <= fat_sectors {
                break cl;
            }
            fat_sectors = need;
        };
        assert!(clusters >= 65525, "volume too small for FAT32");
        let mut b = FatBuilder {
            img: vec![0u8; total_sectors as usize * SECTOR],
            spc: sectors_per_cluster,
            reserved,
            fat_sectors,
            clusters,
            fat: vec![0u32; clusters as usize + 2],
            next_free: 2,
            dirs: BTreeMap::new(),
            files: BTreeMap::new(),
        };
        b.fat[0] = 0x0FFF_FFF8;
        b.fat[1] = EOC;
        let root = b.alloc_contiguous(DIR_CLUSTERS);
        b.chain(&root);
        b.dirs.insert(String::new(), root[0]);
        b.write_boot(total_sectors);
        b
    }

    pub fn cluster_size(&self) -> usize {
        self.spc as usize * SECTOR
    }

    fn write_boot(&mut self, total_sectors: u32) {
        let bs = &mut self.img[..SECTOR];
        bs[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        bs[3..11].copy_from_slice(b"MSWIN4.1");
        bs[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        bs[13] = self.spc as u8;
        bs[14..16].copy_from_slice(&(self.reserved as u16).to_le_bytes());
        bs[16] = 2;
        bs[21] = 0xF8;
        bs[24..26].copy_from_slice(&63u16.to_le_bytes());
        bs[26..28].copy_from_slice(&255u16.to_le_bytes());
        bs[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        bs[36..40].copy_from_slice(&self.fat_sectors.to_le_bytes());
        bs[44..48].copy_from_slice(&2u32.to_le_bytes());
        bs[48..50].copy_from_slice(&1u16.to_le_bytes());
        bs[50..52].copy_from_slice(&6u16.to_le_bytes());
        bs[64] = 0x80;
        bs[66] = 0x29;
        bs[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bs[71..82].copy_from_slice(b"PHOTOTEST  ");
        bs[82..90].copy_from_slice(b"FAT32   ");
        bs[510] = 0x55;
        bs[511] = 0xAA;
        // FSInfo
        let fsi = &mut self.img[SECTOR..2 * SECTOR];
        fsi[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        fsi[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        fsi[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        fsi[492..496].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        fsi[510] = 0x55;
        fsi[511] = 0xAA;
        let boot: Vec<u8> = self.img[..SECTOR].to_vec();
        self.img[6 * SECTOR..7 * SECTOR].copy_from_slice(&boot);
    }

    fn cluster_offset(&self, c: u32) -> usize {
        let first_data = self.reserved + 2 * self.fat_sectors;
        (first_data as usize + (c as usize - 2) * self.spc as usize) * SECTOR
    }

    pub fn alloc_contiguous(&mut self, n: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = self.next_free;
        while out.len() < n as usize {
            assert!(c < self.clusters + 2, "volume full");
            if self.fat[c as usize] == 0 {
                out.push(c);
            } else {
                out.clear();
            }
            c += 1;
        }
        self.next_free = c;
        out
    }

    fn chain(&mut self, clusters: &[u32]) {
        for w in clusters.windows(2) {
            self.fat[w[0] as usize] = w[1];
        }
        if let Some(&last) = clusters.last() {
            self.fat[last as usize] = EOC;
        }
    }

    fn write_data(&mut self, clusters: &[u32], data: &[u8]) {
        let cs = self.cluster_size();
        for (i, c) in clusters.iter().enumerate() {
            let off = self.cluster_offset(*c);
            let part = &data[(i * cs).min(data.len())..((i + 1) * cs).min(data.len())];
            self.img[off..off + part.len()].copy_from_slice(part);
            // Zero slack so slack never contains stale signatures from setup.
            self.img[off + part.len()..off + cs].fill(0);
        }
    }

    fn split(path: &str) -> (String, String) {
        match path.rfind('/') {
            Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
            None => (String::new(), path.to_string()),
        }
    }

    /// Create a directory (parents must exist).
    pub fn mkdir(&mut self, path: &str) {
        let (parent, name) = Self::split(path);
        let clusters = self.alloc_contiguous(DIR_CLUSTERS);
        self.chain(&clusters);
        let cs = self.cluster_size();
        for c in &clusters {
            let off = self.cluster_offset(*c);
            self.img[off..off + cs].fill(0);
        }
        let parent_cluster = self.dirs[&parent];
        // "." and ".."
        let off = self.cluster_offset(clusters[0]);
        let dot = short_entry(b".          ", 0x10, clusters[0], 0);
        let dotdot = short_entry(b"..         ", 0x10, if parent.is_empty() { 0 } else { parent_cluster }, 0);
        self.img[off..off + 32].copy_from_slice(&dot);
        self.img[off + 32..off + 64].copy_from_slice(&dotdot);
        self.add_entry(&parent, &name, 0x10, clusters[0], 0);
        self.dirs.insert(path.to_string(), clusters[0]);
    }

    fn free_dir_slots(&self, dir: &str, n: usize) -> Vec<usize> {
        let first = self.dirs[dir];
        let mut c = first;
        let mut slots = Vec::new();
        loop {
            let off = self.cluster_offset(c);
            for i in 0..self.cluster_size() / 32 {
                let e = off + i * 32;
                if self.img[e] == 0 {
                    slots.push(e);
                    if slots.len() == n {
                        return slots;
                    }
                } else {
                    slots.clear();
                }
            }
            let next = self.fat[c as usize];
            assert!(next < 0x0FFF_FFF8, "directory full");
            c = next;
        }
    }

    /// Write directory entries (LFN + short) and return their image offsets.
    fn add_entry(&mut self, dir: &str, name: &str, attr: u8, cluster: u32, size: u32) -> Vec<usize> {
        let short = short_name(name, self.files.len() + self.dirs.len());
        let lfn = lfn_entries(name, &short);
        let slots = self.free_dir_slots(dir, lfn.len() + 1);
        for (slot, e) in slots.iter().zip(lfn.iter()) {
            self.img[*slot..slot + 32].copy_from_slice(e);
        }
        let se = short_entry(&short, attr, cluster, size);
        let last = *slots.last().unwrap();
        self.img[last..last + 32].copy_from_slice(&se);
        slots
    }

    /// Add a file using the given clusters (contiguous or not).
    pub fn add_file_clusters(&mut self, path: &str, data: &[u8], clusters: Vec<u32>) -> FatFile {
        let (dir, name) = Self::split(path);
        assert!(clusters.len() * self.cluster_size() >= data.len());
        self.chain(&clusters);
        self.write_data(&clusters, data);
        let slots = self.add_entry(&dir, &name, 0x20, clusters[0], data.len() as u32);
        self.files.insert(path.to_string(), (slots, clusters.clone()));
        FatFile {
            path: path.to_string(),
            clusters,
            size: data.len() as u32,
        }
    }

    pub fn clusters_for(&self, len: usize) -> u32 {
        (len.div_ceil(self.cluster_size())).max(1) as u32
    }

    pub fn add_file(&mut self, path: &str, data: &[u8]) -> FatFile {
        let n = self.clusters_for(data.len());
        let cl = self.alloc_contiguous(n);
        self.add_file_clusters(path, data, cl)
    }

    /// Write `a` in two fragments with all of `b` in between.
    pub fn add_interleaved(&mut self, a_path: &str, a: &[u8], b_path: &str, b: &[u8]) -> (FatFile, FatFile) {
        let na = self.clusters_for(a.len());
        let first = self.alloc_contiguous(na / 2);
        let nb = self.clusters_for(b.len());
        let bcl = self.alloc_contiguous(nb);
        let second = self.alloc_contiguous(na - na / 2);
        let fb = self.add_file_clusters(b_path, b, bcl);
        let mut acl = first;
        acl.extend(second);
        let fa = self.add_file_clusters(a_path, a, acl);
        (fa, fb)
    }

    /// Delete like a FAT driver: mark entries 0xE5 and free the chain. The
    /// data clusters are left untouched.
    pub fn delete(&mut self, path: &str) {
        let (slots, clusters) = self.files.remove(path).expect("file exists");
        for s in slots {
            self.img[s] = 0xE5;
        }
        for c in clusters {
            self.fat[c as usize] = 0;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        let fat_bytes: Vec<u8> = self.fat.iter().flat_map(|v| v.to_le_bytes()).collect();
        for i in 0..2 {
            let off = (self.reserved + i * self.fat_sectors) as usize * SECTOR;
            let n = fat_bytes.len().min(self.fat_sectors as usize * SECTOR);
            self.img[off..off + n].copy_from_slice(&fat_bytes[..n]);
        }
        self.img
    }
}

fn short_entry(name11: &[u8], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(&name11[..11]);
    e[11] = attr;
    // 2024-05-01 12:00:00
    let date: u16 = ((2024 - 1980) << 9) | (5 << 5) | 1;
    let time: u16 = 12 << 11;
    e[14..16].copy_from_slice(&time.to_le_bytes());
    e[16..18].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes());
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

fn short_name(name: &str, uniq: usize) -> [u8; 11] {
    let mut out = [b' '; 11];
    let upper = name.to_ascii_uppercase();
    let (base, ext) = match upper.rfind('.') {
        Some(i) => (&upper[..i], &upper[i + 1..]),
        None => (upper.as_str(), ""),
    };
    let clean = |s: &str| s.bytes().filter(|b| b.is_ascii_alphanumeric()).collect::<Vec<u8>>();
    let b = clean(base);
    let tail = format!("~{}", uniq % 1000);
    let keep = 8 - tail.len();
    for (i, c) in b.iter().take(keep).enumerate() {
        out[i] = *c;
    }
    for (i, c) in tail.bytes().enumerate() {
        out[b.len().min(keep) + i] = c;
    }
    for (i, c) in clean(ext).iter().take(3).enumerate() {
        out[8 + i] = *c;
    }
    out
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0);
    while units.len() % 13 != 0 {
        units.push(0xFFFF);
    }
    let n = units.len() / 13;
    let sum = lfn_checksum(short);
    let mut out = Vec::new();
    for seq in (1..=n).rev() {
        let part = &units[(seq - 1) * 13..seq * 13];
        let mut e = [0u8; 32];
        e[0] = seq as u8 | if seq == n { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = sum;
        let pos = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (u, p) in part.iter().zip(pos) {
            e[p..p + 2].copy_from_slice(&u.to_le_bytes());
        }
        out.push(e);
    }
    out
}
