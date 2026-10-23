//! Resumable jobs. Each job persists its parameters (`job.json`), a
//! checkpoint (`checkpoint.json`) and its results (`results*.jsonl`) in its
//! own directory so it can be cancelled and resumed later — also after a
//! crash or reboot.

pub mod device;
pub mod drive;
pub mod image;
pub mod local;
pub mod partition;

use crate::case::Case;
use crate::util;
use crate::Progress;
use anyhow::{bail, Result};
use pr_model::model::RecoveryResult;
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobParams {
    ImageScan(image::ImageScanParams),
    DriveScan(drive::DriveScanParams),
    DeviceAcquire(device::DeviceParams),
    LocalScan(local::LocalScanParams),
    PartitionAcquire(partition::PartitionParams),
}

impl JobParams {
    pub fn kind(&self) -> &'static str {
        match self {
            JobParams::ImageScan(_) => "image_scan",
            JobParams::DriveScan(_) => "drive_scan",
            JobParams::DeviceAcquire(_) => "device_acquire",
            JobParams::LocalScan(_) => "local_scan",
            JobParams::PartitionAcquire(_) => "partition_acquire",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Created,
    Running,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub params: JobParams,
    pub status: JobStatus,
    pub created_at: String,
    pub updated_at: String,
    pub tool_version: String,
    pub runs: u32,
    pub error: Option<String>,
    /// Free-form statistics written by the job (counts, timings).
    pub stats: serde_json::Value,
    /// Limitations discovered while running (inaccessible paths, encryption...).
    pub limitations: Vec<String>,
}

impl JobRecord {
    pub fn dir(&self, case: &Case) -> PathBuf {
        case.job_dir(&self.id)
    }

    pub fn save(&mut self, case: &Case) -> Result<()> {
        self.updated_at = pr_model::now_rfc3339();
        util::write_json(&self.dir(case).join("job.json"), self)?;
        Ok(())
    }

    pub fn load(case: &Case, id: &str) -> Result<JobRecord> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("invalid job id");
        }
        Ok(util::read_json(&case.job_dir(id).join("job.json"))?)
    }

    /// All results this job has produced so far.
    pub fn results(&self, case: &Case) -> Vec<RecoveryResult> {
        let dir = self.dir(case);
        let mut out = Vec::new();
        for name in ["results.jsonl", "results_fs.jsonl", "results_carve.jsonl"] {
            out.extend(util::read_jsonl::<RecoveryResult>(&dir.join(name)).unwrap_or_default());
        }
        out
    }
}

pub fn create_job(case: &Case, params: JobParams) -> Result<JobRecord> {
    let now = pr_model::now_rfc3339();
    let ts = now
        .replace([':', '-'], "")
        .chars()
        .take(15)
        .collect::<String>();
    let id = format!(
        "{}-{}-{}",
        ts,
        params.kind().replace('_', ""),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let mut rec = JobRecord {
        id,
        params,
        status: JobStatus::Created,
        created_at: now.clone(),
        updated_at: now,
        tool_version: pr_model::TOOL_VERSION.to_string(),
        runs: 0,
        error: None,
        stats: serde_json::json!({}),
        limitations: vec![],
    };
    std::fs::create_dir_all(rec.dir(case))?;
    rec.save(case)?;
    case.audit(
        "job_created",
        serde_json::json!({ "job_id": rec.id, "params": rec.params }),
    );
    Ok(rec)
}

/// Run (or resume) a job to completion, cancellation or failure.
pub fn run_job(
    case: &Case,
    rec: &mut JobRecord,
    cancel: &CancelFlag,
    progress: &dyn Progress,
) -> Result<JobStatus> {
    if rec.status == JobStatus::Completed {
        return Ok(JobStatus::Completed);
    }
    rec.status = JobStatus::Running;
    rec.runs += 1;
    rec.error = None;
    rec.save(case)?;
    case.audit(
        "job_started",
        serde_json::json!({ "job_id": rec.id, "run": rec.runs }),
    );
    let params = rec.params.clone();
    let outcome = match &params {
        JobParams::ImageScan(p) => image::run(case, rec, p, cancel, progress),
        JobParams::DriveScan(p) => drive::run(case, rec, p, cancel, progress),
        JobParams::DeviceAcquire(p) => device::run(case, rec, p, cancel, progress),
        JobParams::LocalScan(p) => local::run(case, rec, p, cancel, progress),
        JobParams::PartitionAcquire(p) => partition::run(case, rec, p, cancel, progress),
    };
    match outcome {
        Ok(true) => rec.status = JobStatus::Completed,
        Ok(false) => rec.status = JobStatus::Cancelled,
        Err(e) => {
            rec.status = JobStatus::Failed;
            rec.error = Some(format!("{e:#}"));
        }
    }
    rec.save(case)?;
    case.audit(
        "job_finished",
        serde_json::json!({ "job_id": rec.id, "status": rec.status, "error": rec.error, "stats": rec.stats }),
    );
    Ok(rec.status)
}
