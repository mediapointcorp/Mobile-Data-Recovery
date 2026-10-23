//! Rooted test devices only: acquire a block partition image, read-only,
//! into `evidence/`. Requires the exact confirmation phrase.

use crate::case::Case;
use crate::jobs::device::client;
use crate::jobs::JobRecord;
use crate::util;
use crate::Progress;
use anyhow::Result;
use pr_adb::validate::{validate_partition_name, validate_serial};
use pr_adb::AdbError;
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PartitionParams {
    pub serial: String,
    pub partition: String,
    /// Must equal `READ-ONLY ROOT ACQUISITION <serial>`.
    pub confirmation: String,
    #[serde(default)]
    pub adb_path: Option<String>,
}

pub fn run(
    case: &Case,
    rec: &mut JobRecord,
    p: &PartitionParams,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<bool> {
    validate_serial(&p.serial)?;
    validate_partition_name(&p.partition)?;
    let adb = client(p.adb_path.as_deref())?;
    let dest = case.root.join("evidence").join(format!(
        "{}_{}_{}.img",
        p.serial.replace(':', "_"),
        p.partition,
        rec.id
    ));
    if dest.exists() {
        // An earlier interrupted run: streamed acquisition cannot resume
        // mid-stream, so the incomplete image is kept aside and restarted.
        let aside = dest.with_extension(format!("incomplete-{}", rec.runs));
        std::fs::rename(&dest, &aside)?;
        rec.limitations.push(format!(
            "Previous incomplete acquisition kept as {}",
            util::rel(&case.root, &aside)
        ));
    }
    case.audit(
        "partition_acquisition_started",
        serde_json::json!({ "job_id": rec.id, "serial": p.serial, "partition": p.partition }),
    );
    let id = rec.id.clone();
    let mut prog = |n: u64| progress.report(&id, "acquire", n, 0, "reading block device");
    let img = match adb.acquire_partition(
        &p.serial,
        &p.partition,
        &dest,
        &p.confirmation,
        cancel,
        &mut prog,
    ) {
        Err(AdbError::Cancelled) => return Ok(false),
        other => other?,
    };
    util::write_json(&rec.dir(case).join("partition.json"), &img)?;
    case.audit("partition_acquired", serde_json::to_value(&img)?);
    rec.limitations.extend(img.notes.iter().cloned());
    rec.stats = serde_json::json!({ "image": util::rel(&case.root, &img.local_path), "size": img.size, "sha256": img.sha256 });
    Ok(true)
}
