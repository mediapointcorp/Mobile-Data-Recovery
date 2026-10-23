//! One file listing for every supported filesystem (NTFS, FAT, exFAT):
//! where each file's data is, and how much of it has been reused since it
//! was deleted.

use crate::detect::VolumeKind;
use crate::exfat::ExfatVolume;
use crate::fat::FatVolume;
use crate::ntfs::{DataLocation, NtfsVolume};
use crate::FsError;
use pr_model::source::{read_full_at, ReadAt};

#[derive(Clone, Debug, PartialEq)]
pub struct FsFile {
    /// `/`-separated path inside the volume.
    pub path: String,
    pub name: String,
    pub deleted: bool,
    pub size: u64,
    /// Local (FAT/exFAT) or UTC RFC 3339 (NTFS) timestamps.
    pub modified: Option<String>,
    pub created: Option<String>,
    /// Volume-relative `(offset, length)`; `None` offset = zeros (sparse).
    pub pieces: Vec<(Option<u64>, u64)>,
    /// Data stored inside the file record (small NTFS files).
    pub resident: Option<Vec<u8>>,
    pub clusters_total: u64,
    pub clusters_reused: u64,
    pub first_cluster_reused: bool,
    /// Location known exactly (vs. assumed contiguous after deletion).
    pub exact: bool,
    /// Why the data cannot be read at all (encrypted, compressed).
    pub unreadable: Option<String>,
    pub notes: Vec<String>,
}

pub struct FsListing {
    pub filesystem: &'static str,
    pub files: Vec<FsFile>,
    /// Volume-relative ranges allocated now.
    pub allocated: Vec<(u64, u64)>,
}

impl FsFile {
    /// First bytes of the file's data (for type and zero-fill checks).
    pub fn head(&self, vol: &dyn ReadAt, n: usize) -> Vec<u8> {
        if let Some(r) = &self.resident {
            return r[..r.len().min(n)].to_vec();
        }
        match self.pieces.first() {
            Some((Some(off), len)) => {
                let mut b = vec![0u8; n.min(*len as usize)];
                let k = read_full_at(vol, *off, &mut b).unwrap_or(0);
                b.truncate(k);
                b
            }
            Some((None, len)) => vec![0u8; n.min(*len as usize)],
            None => Vec::new(),
        }
    }

    /// Read the whole file (bounded by `max`).
    pub fn read(&self, vol: &dyn ReadAt, max: u64) -> std::io::Result<Vec<u8>> {
        if let Some(r) = &self.resident {
            return Ok(r.clone());
        }
        let total: u64 = self.pieces.iter().map(|p| p.1).sum();
        if total > max {
            return Err(std::io::Error::other(format!(
                "{total} bytes exceeds the {max} byte limit"
            )));
        }
        let mut out = Vec::with_capacity(total as usize);
        for (off, len) in &self.pieces {
            match off {
                Some(o) => {
                    let start = out.len();
                    out.resize(start + *len as usize, 0);
                    let n = read_full_at(vol, *o, &mut out[start..])?;
                    if n as u64 != *len {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "data runs past the end of the volume",
                        ));
                    }
                }
                None => out.resize(out.len() + *len as usize, 0),
            }
        }
        Ok(out)
    }
}

/// List the files of one volume. `want(name, deleted)` selects files.
pub fn list_files(
    vol: &dyn ReadAt,
    kind: VolumeKind,
    want: &dyn Fn(&str, bool) -> bool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<FsListing, FsError> {
    match kind {
        VolumeKind::Ntfs => {
            let nv = NtfsVolume::open(vol)?;
            let entries = nv.walk(want, progress);
            let files = entries
                .iter()
                .map(|e| {
                    let plan = nv.plan(e);
                    let unreadable = if e.encrypted {
                        Some("EFS-encrypted file: its data cannot be decrypted without the owner's certificate.".to_string())
                    } else if e.compressed {
                        Some("NTFS-compressed file: deleted compressed data cannot be decompressed by this tool.".to_string())
                    } else {
                        None
                    };
                    FsFile {
                        path: e.path.clone(),
                        name: e.name.clone(),
                        deleted: e.deleted,
                        size: e.size,
                        modified: e.modified.clone(),
                        created: e.created.clone(),
                        pieces: plan.pieces.clone(),
                        resident: match &e.data {
                            DataLocation::Resident(d) => Some(d.clone()),
                            _ => None,
                        },
                        clusters_total: plan.clusters_total,
                        clusters_reused: if e.deleted { plan.clusters_now_allocated } else { 0 },
                        first_cluster_reused: e.deleted && plan.first_cluster_allocated,
                        exact: true,
                        unreadable,
                        notes: plan.notes,
                    }
                })
                .collect();
            Ok(FsListing {
                filesystem: "NTFS",
                files,
                allocated: if nv.has_bitmap() {
                    nv.allocated_extents()
                } else {
                    Vec::new()
                },
            })
        }
        VolumeKind::Fat12 | VolumeKind::Fat16 | VolumeKind::Fat32 => {
            let fv = FatVolume::open(vol)?;
            progress(0, 1);
            let files = fv
                .walk()
                .into_iter()
                .filter(|e| !e.is_dir)
                .filter(|e| want(e.path.rsplit('/').next().unwrap_or(""), e.deleted))
                .map(|e| {
                    let plan = fv.plan(&e);
                    let mut notes = plan.notes.clone();
                    if e.name_reconstructed {
                        notes.push(
                            "the first letter of this deleted short file name is lost".into(),
                        );
                    }
                    FsFile {
                        name: e.path.rsplit('/').next().unwrap_or("").to_string(),
                        path: e.path.clone(),
                        deleted: e.deleted,
                        size: e.size as u64,
                        modified: e.modified.clone(),
                        created: e.created.clone(),
                        pieces: plan.ranges.iter().map(|&(o, l)| (Some(o), l)).collect(),
                        resident: None,
                        clusters_total: plan.clusters_total as u64,
                        clusters_reused: if e.deleted {
                            plan.clusters_now_allocated as u64
                        } else {
                            0
                        },
                        first_cluster_reused: e.deleted && fv.is_allocated(e.first_cluster),
                        exact: !e.deleted,
                        unreadable: None,
                        notes,
                    }
                })
                .collect();
            progress(1, 1);
            Ok(FsListing {
                filesystem: "FAT",
                files,
                allocated: fv.allocated_extents(),
            })
        }
        VolumeKind::ExFat => {
            let xv = ExfatVolume::open(vol)?;
            progress(0, 1);
            let files = xv
                .walk()
                .into_iter()
                .filter(|e| !e.is_dir && want(&e.name, e.deleted))
                .map(|e| {
                    let plan = xv.plan(&e);
                    FsFile {
                        path: e.path.clone(),
                        name: e.name.clone(),
                        deleted: e.deleted,
                        size: e.size,
                        modified: e.modified.clone(),
                        created: e.created.clone(),
                        pieces: plan.pieces.iter().map(|&(o, l)| (Some(o), l)).collect(),
                        resident: None,
                        clusters_total: plan.clusters_total,
                        clusters_reused: plan.clusters_now_allocated,
                        first_cluster_reused: plan.first_cluster_allocated,
                        exact: plan.exact,
                        unreadable: None,
                        notes: plan.notes,
                    }
                })
                .collect();
            progress(1, 1);
            Ok(FsListing {
                filesystem: "exFAT",
                files,
                allocated: if xv.has_bitmap() {
                    xv.allocated_extents()
                } else {
                    Vec::new()
                },
            })
        }
        other => Err(FsError::Unsupported(format!("{other:?}"))),
    }
}
