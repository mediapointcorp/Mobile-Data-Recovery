//! Partition-table parsing, filesystem identification and encryption hints.

use pr_model::source::{read_full_at, ReadAt};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKind {
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ntfs,
    Ext4,
    F2fs,
    Luks,
    AndroidSparse,
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    /// Live and deleted directory records are parsed and recovered.
    DeletedRecordRecovery,
    /// Identified and reported; space is covered by raw carving only.
    IdentificationOnly,
    /// Cannot be analysed (encrypted, sparse container, unknown).
    Unsupported,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionState {
    NotDetected,
    /// Filesystem advertises per-file encryption (Android FBE).
    FileBasedCapable,
    /// Container is a known encrypted volume format.
    FullVolume,
    /// No filesystem found and data looks random.
    LikelyEncryptedOrRandom,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VolumeReport {
    pub offset: u64,
    pub length: u64,
    pub partition_name: Option<String>,
    pub kind: VolumeKind,
    pub support: SupportLevel,
    pub encryption: EncryptionState,
    pub label: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PartitionScheme {
    Gpt { sector_size: u32 },
    Mbr,
    /// The image is a single volume (no partition table).
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ImageLayout {
    pub scheme: PartitionScheme,
    pub volumes: Vec<VolumeReport>,
    /// Mean Shannon entropy (bits/byte) of sampled non-zero blocks.
    pub sampled_entropy: f64,
    /// Fraction of sampled non-zero blocks with entropy > 7.9 bits/byte.
    pub high_entropy_fraction: f64,
    pub notes: Vec<String>,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read(src: &dyn ReadAt, off: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let n = read_full_at(src, off, &mut v).unwrap_or(0);
    v.truncate(n);
    v
}

/// Is this sector a FAT/exFAT/NTFS boot sector (as opposed to an MBR)?
fn boot_sector_kind(bs: &[u8]) -> Option<VolumeKind> {
    if bs.len() < 512 || bs[510] != 0x55 || bs[511] != 0xAA {
        return None;
    }
    if &bs[3..11] == b"EXFAT   " {
        return Some(VolumeKind::ExFat);
    }
    if &bs[3..11] == b"NTFS    " {
        return Some(VolumeKind::Ntfs);
    }
    let bps = le16(bs, 11);
    let spc = bs[13];
    let reserved = le16(bs, 14);
    let nfats = bs[16];
    let jump_ok = bs[0] == 0xEB || bs[0] == 0xE9;
    if !jump_ok || !matches!(bps, 512 | 1024 | 2048 | 4096) || spc == 0 || !spc.is_power_of_two() || reserved == 0 || nfats == 0 {
        return None;
    }
    let root_entries = le16(bs, 17) as u32;
    let total = if le16(bs, 19) != 0 { le16(bs, 19) as u32 } else { le32(bs, 32) };
    let fat_sz = if le16(bs, 22) != 0 { le16(bs, 22) as u32 } else { le32(bs, 36) };
    let root_secs = (root_entries * 32).div_ceil(bps as u32);
    let data = total.checked_sub(reserved as u32 + nfats as u32 * fat_sz + root_secs)?;
    let clusters = data / spc as u32;
    Some(if clusters < 4085 {
        VolumeKind::Fat12
    } else if clusters < 65525 {
        VolumeKind::Fat16
    } else {
        VolumeKind::Fat32
    })
}

/// Identify a single volume starting at `off`.
pub fn identify_volume(src: &dyn ReadAt, off: u64, length: u64, name: Option<String>) -> VolumeReport {
    let head = read(src, off, 4096);
    let mut v = VolumeReport {
        offset: off,
        length,
        partition_name: name,
        kind: VolumeKind::Unknown,
        support: SupportLevel::Unsupported,
        encryption: EncryptionState::NotDetected,
        label: None,
        notes: Vec::new(),
    };
    if head.len() >= 6 && head[..6] == *b"LUKS\xBA\xBE" {
        v.kind = VolumeKind::Luks;
        v.encryption = EncryptionState::FullVolume;
        v.notes.push("LUKS encrypted volume: contents cannot be analysed without the key; this tool does not attempt decryption".into());
        return v;
    }
    if head.len() >= 4 && le32(&head, 0) == 0xED26_FF3A {
        v.kind = VolumeKind::AndroidSparse;
        v.notes.push("Android sparse image: convert to a raw image first (e.g. `simg2img`) — raw offsets inside a sparse file are not disk offsets".into());
        return v;
    }
    if let Some(k) = boot_sector_kind(&head) {
        v.kind = k;
        match k {
            VolumeKind::Fat12 | VolumeKind::Fat16 | VolumeKind::Fat32 => {
                v.support = SupportLevel::DeletedRecordRecovery;
                let lab = if k == VolumeKind::Fat32 { &head[71..82] } else { &head[43..54] };
                let lab = String::from_utf8_lossy(lab).trim().to_string();
                if !lab.is_empty() && lab != "NO NAME" {
                    v.label = Some(lab);
                }
                v.notes.push("FAT: live and deleted directory entries are parsed; deleted files are read assuming contiguous allocation and validated".into());
            }
            VolumeKind::ExFat => {
                v.support = SupportLevel::IdentificationOnly;
                v.notes.push("exFAT (common on SD cards > 32 GB): identified; deleted-record parsing not yet implemented — covered by raw carving".into());
            }
            _ => {
                v.support = SupportLevel::IdentificationOnly;
                v.notes.push("NTFS: identified only — covered by raw carving".into());
            }
        }
        return v;
    }
    if head.len() >= 1024 + 0x68 && le16(&head, 1024 + 0x38) == 0xEF53 {
        let sb = &head[1024..];
        v.kind = VolumeKind::Ext4;
        v.support = SupportLevel::IdentificationOnly;
        let incompat = le32(sb, 0x60);
        let log_bs = le32(sb, 0x18);
        let lab = String::from_utf8_lossy(&sb[0x78..0x88]).trim_matches(char::from(0)).trim().to_string();
        if !lab.is_empty() {
            v.label = Some(lab);
        }
        v.notes.push(format!("ext2/3/4 superblock, block size {} bytes, incompat features 0x{incompat:08X}", 1024u64 << log_bs.min(6)));
        v.notes.push("ext4 clears extent mappings when files are deleted, so deleted-record recovery is not offered; unallocated space is covered by raw carving".into());
        if incompat & 0x10000 != 0 {
            v.encryption = EncryptionState::FileBasedCapable;
            v.notes.push("encryption feature enabled (Android file-based encryption): file contents in encrypted directories are ciphertext on disk; carving cannot recover them without keys, which this tool does not have".into());
        }
        return v;
    }
    if head.len() >= 1024 + 2184 && le32(&head, 1024) == 0xF2F5_2010 {
        let sb = &head[1024..];
        v.kind = VolumeKind::F2fs;
        v.support = SupportLevel::IdentificationOnly;
        let features = le32(sb, 2180);
        v.notes.push(format!("F2FS superblock v{}.{}, features 0x{features:08X} (best-effort parse)", le16(sb, 4), le16(sb, 6)));
        v.notes.push("F2FS writes out of place and invalidates node blocks on delete; deleted-record recovery is not offered; covered by raw carving".into());
        if features & 0x1 != 0 {
            v.encryption = EncryptionState::FileBasedCapable;
            v.notes.push("encryption feature enabled (Android file-based encryption): user file contents are ciphertext on disk".into());
        }
        return v;
    }
    v.notes.push("no recognised filesystem signature".into());
    v
}

fn parse_gpt(src: &dyn ReadAt) -> Option<(u32, Vec<(u64, u64, String)>)> {
    for ss in [512u32, 4096] {
        let hdr = read(src, ss as u64, 92);
        if hdr.len() < 92 || &hdr[..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = le64(&hdr, 72);
        let count = le32(&hdr, 80).min(1024);
        let esize = le32(&hdr, 84) as usize;
        if !(128..=4096).contains(&esize) {
            continue;
        }
        let table = read(src, entries_lba * ss as u64, count as usize * esize);
        let mut parts = Vec::new();
        for e in table.chunks_exact(esize) {
            if e[..16].iter().all(|&b| b == 0) {
                continue;
            }
            let first = le64(e, 32);
            let last = le64(e, 40);
            if last < first {
                continue;
            }
            let name: Vec<u16> = e[56..128].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            parts.push((first * ss as u64, (last - first + 1) * ss as u64, String::from_utf16_lossy(&name)));
        }
        return Some((ss, parts));
    }
    None
}

fn parse_mbr(sector: &[u8]) -> Option<Vec<(u64, u64, u8)>> {
    if sector.len() < 512 || sector[510] != 0x55 || sector[511] != 0xAA || boot_sector_kind(sector).is_some() {
        return None;
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &sector[446 + 16 * i..446 + 16 * (i + 1)];
        if e[0] != 0x00 && e[0] != 0x80 {
            return None; // invalid boot indicator: not an MBR
        }
        let ty = e[4];
        let start = le32(e, 8) as u64;
        let count = le32(e, 12) as u64;
        if ty != 0 && count > 0 {
            parts.push((start * 512, count * 512, ty));
        }
    }
    (!parts.is_empty()).then_some(parts)
}

/// Shannon entropy sampling across the source.
fn sample_entropy(src: &dyn ReadAt) -> (f64, f64) {
    let len = src.len();
    if len == 0 {
        return (0.0, 0.0);
    }
    let samples = 64u64;
    let mut sum = 0.0;
    let mut high = 0usize;
    let mut n = 0usize;
    for i in 0..samples {
        let off = (len / samples) * i;
        let b = read(src, off, 4096);
        if b.is_empty() || b.iter().all(|&x| x == 0) {
            continue;
        }
        let mut hist = [0u32; 256];
        for &x in &b {
            hist[x as usize] += 1;
        }
        let total = b.len() as f64;
        let h: f64 = hist.iter().filter(|&&c| c > 0).map(|&c| {
            let p = c as f64 / total;
            -p * p.log2()
        }).sum();
        sum += h;
        if h > 7.9 {
            high += 1;
        }
        n += 1;
    }
    if n == 0 {
        (0.0, 0.0)
    } else {
        (sum / n as f64, high as f64 / n as f64)
    }
}

/// Identify the image layout and every volume in it.
pub fn analyze_layout(src: &dyn ReadAt) -> ImageLayout {
    let (entropy, high) = sample_entropy(src);
    let mut layout = ImageLayout {
        scheme: PartitionScheme::None,
        volumes: Vec::new(),
        sampled_entropy: entropy,
        high_entropy_fraction: high,
        notes: Vec::new(),
    };
    let first = read(src, 0, 512);
    if let Some((ss, parts)) = parse_gpt(src) {
        layout.scheme = PartitionScheme::Gpt { sector_size: ss };
        for (off, len, name) in parts {
            layout.volumes.push(identify_volume(src, off, len, Some(name)));
        }
    } else if let Some(parts) = parse_mbr(&first) {
        layout.scheme = PartitionScheme::Mbr;
        for (off, len, ty) in parts {
            if ty == 0xEE {
                layout.notes.push("protective MBR without readable GPT".into());
                continue;
            }
            layout.volumes.push(identify_volume(src, off, len, Some(format!("MBR type 0x{ty:02X}"))));
        }
    } else {
        layout.volumes.push(identify_volume(src, 0, src.len(), None));
    }
    let all_unknown = layout.volumes.iter().all(|v| v.kind == VolumeKind::Unknown);
    if all_unknown && high > 0.9 {
        for v in &mut layout.volumes {
            v.encryption = EncryptionState::LikelyEncryptedOrRandom;
        }
        layout.notes.push(format!(
            "{:.0}% of sampled blocks are near-random ({entropy:.2} bits/byte) and no filesystem was found: the source is most likely encrypted (e.g. Android full-disk or file-based encryption). Carving is still attempted but cannot recover images from ciphertext.",
            high * 100.0
        ));
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::source::MemSource;
    use pr_testkit::Rng;

    #[test]
    fn identifies_fat32() {
        let mut b = pr_testkit::fat::FatBuilder::new(34, 1);
        b.add_file("a.jpg", &[1, 2, 3]);
        let l = analyze_layout(&MemSource::new(b.finish()));
        assert_eq!(l.scheme, PartitionScheme::None);
        assert_eq!(l.volumes[0].kind, VolumeKind::Fat32);
        assert_eq!(l.volumes[0].support, SupportLevel::DeletedRecordRecovery);
        assert_eq!(l.volumes[0].label.as_deref(), Some("PHOTOTEST"));
    }

    #[test]
    fn identifies_ext4_with_encryption_flag() {
        let mut d = vec![0u8; 8192];
        d[1024 + 0x38..1024 + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        d[1024 + 0x18] = 2;
        d[1024 + 0x60..1024 + 0x64].copy_from_slice(&(0x10000u32 | 0x40).to_le_bytes());
        let l = analyze_layout(&MemSource::new(d));
        let v = &l.volumes[0];
        assert_eq!(v.kind, VolumeKind::Ext4);
        assert_eq!(v.support, SupportLevel::IdentificationOnly);
        assert_eq!(v.encryption, EncryptionState::FileBasedCapable);
    }

    #[test]
    fn identifies_f2fs_luks_sparse() {
        let mut d = vec![0u8; 8192];
        d[1024..1028].copy_from_slice(&0xF2F5_2010u32.to_le_bytes());
        assert_eq!(analyze_layout(&MemSource::new(d)).volumes[0].kind, VolumeKind::F2fs);
        let mut d = vec![0u8; 8192];
        d[..6].copy_from_slice(b"LUKS\xBA\xBE");
        let l = analyze_layout(&MemSource::new(d));
        assert_eq!(l.volumes[0].encryption, EncryptionState::FullVolume);
        let mut d = vec![0u8; 8192];
        d[..4].copy_from_slice(&0xED26_FF3Au32.to_le_bytes());
        assert_eq!(analyze_layout(&MemSource::new(d)).volumes[0].kind, VolumeKind::AndroidSparse);
    }

    #[test]
    fn random_data_flagged_as_likely_encrypted() {
        let d = Rng::new(9).bytes(4 << 20);
        let l = analyze_layout(&MemSource::new(d));
        assert_eq!(l.volumes[0].kind, VolumeKind::Unknown);
        assert_eq!(l.volumes[0].encryption, EncryptionState::LikelyEncryptedOrRandom);
        assert!(l.sampled_entropy > 7.9);
    }

    #[test]
    fn gpt_with_named_partitions() {
        let ss = 4096usize;
        let mut d = vec![0u8; ss * 64];
        // protective MBR
        d[446 + 4] = 0xEE;
        d[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        d[446 + 12..446 + 16].copy_from_slice(&63u32.to_le_bytes());
        d[510] = 0x55;
        d[511] = 0xAA;
        let h = ss;
        d[h..h + 8].copy_from_slice(b"EFI PART");
        d[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes());
        d[h + 80..h + 84].copy_from_slice(&1u32.to_le_bytes());
        d[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
        let e = 2 * ss;
        d[e] = 1; // type guid non-zero
        d[e + 32..e + 40].copy_from_slice(&8u64.to_le_bytes());
        d[e + 40..e + 48].copy_from_slice(&63u64.to_le_bytes());
        for (i, u) in "userdata".encode_utf16().enumerate() {
            d[e + 56 + 2 * i..e + 58 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        // ext4 superblock in the partition
        let p = 8 * ss + 1024;
        d[p + 0x38..p + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let l = analyze_layout(&MemSource::new(d));
        assert_eq!(l.scheme, PartitionScheme::Gpt { sector_size: 4096 });
        assert_eq!(l.volumes.len(), 1);
        assert_eq!(l.volumes[0].partition_name.as_deref(), Some("userdata"));
        assert_eq!(l.volumes[0].offset, 8 * 4096);
        assert_eq!(l.volumes[0].kind, VolumeKind::Ext4);
    }
}
