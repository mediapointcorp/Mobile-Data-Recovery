//! PC drive job: scan a mounted drive (`D:`) or whole disk
//! (`PhysicalDrive1`) in place for deleted photos.

use crate::case::Case;
use crate::drives;
use crate::jobs::image::{self, CarveScope, ImageScanParams, Target};
use crate::jobs::JobRecord;
use crate::Progress;
use anyhow::{bail, Result};
use pr_model::evidence::EvidenceMode;
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};

fn unallocated() -> CarveScope {
    CarveScope::Unallocated
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DriveScanParams {
    /// `C:`, `D:`, `PhysicalDrive1` (Windows) or `sdb` (Linux).
    pub drive: String,
    #[serde(default)]
    pub formats: Option<String>,
    /// Default: only free space, where deleted data lives.
    #[serde(default = "unallocated")]
    pub carve_scope: CarveScope,
    /// Also list photos that still exist on the drive (default: deleted only).
    #[serde(default)]
    pub include_existing: bool,
    #[serde(default)]
    pub chunk_size_mib: Option<u32>,
}

pub fn run(
    case: &Case,
    rec: &mut JobRecord,
    p: &DriveScanParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    let path = drives::device_path(&p.drive)?;
    if drives::path_on_drive(&case.root, &p.drive) {
        bail!(
            "The case folder {} is on drive {}. Writing recovered files to the drive you are recovering from can overwrite the deleted photos. Choose a case folder on a different drive (for example a USB disk) and start again.",
            case.root.display(),
            p.drive
        );
    }
    let params = ImageScanParams {
        image_path: path.clone(),
        evidence_mode: EvidenceMode::LiveDevice,
        formats: p.formats.clone(),
        filesystem_analysis: true,
        carve: true,
        chunk_size_mib: p.chunk_size_mib,
        carve_scope: p.carve_scope,
        include_existing: p.include_existing,
    };
    image::run_target(
        case,
        rec,
        Target::Device {
            id: p.drive.clone(),
            path,
        },
        &params,
        cancel,
        progress,
    )
}
