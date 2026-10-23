//! Case directory: all acquired copies, recovered files, job state, audit
//! log and reports for one investigation live under one folder.
//!
//! ```text
//! <case>/
//!   case.json            case metadata
//!   audit.jsonl          append-only audit log
//!   evidence/            verified working copies and acquired partition images
//!   acquired/            reserved for copied source files
//!   recovered/<class>/   carved and filesystem-recovered files
//!   jobs/<job-id>/       job.json, checkpoint.json, results*.jsonl
//!   results.json         merged, deduplicated results
//!   report.json / report.html
//! ```

use crate::util;
use anyhow::{bail, Context, Result};
use pr_model::audit::AuditLog;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CaseInfo {
    pub case_id: String,
    pub name: String,
    pub created_at: String,
    pub tool_version: String,
}

#[derive(Clone)]
pub struct Case {
    pub root: PathBuf,
    pub info: CaseInfo,
    pub audit: Arc<AuditLog>,
}

impl Case {
    /// Open an existing case or create a new one at `root`.
    pub fn open_or_create(root: &Path) -> Result<Case> {
        if !root.is_absolute() {
            bail!("case path must be absolute: {}", root.display());
        }
        let info_path = root.join("case.json");
        let created = !info_path.exists();
        if created {
            if root.exists() && std::fs::read_dir(root)?.next().is_some() {
                bail!(
                    "{} is not empty and is not a PhotoRecovery case",
                    root.display()
                );
            }
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating {}", root.display()))?;
            let info = CaseInfo {
                case_id: uuid::Uuid::new_v4().to_string(),
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                created_at: pr_model::now_rfc3339(),
                tool_version: pr_model::TOOL_VERSION.to_string(),
            };
            util::write_json(&info_path, &info)?;
        }
        let info: CaseInfo = util::read_json(&info_path).context("reading case.json")?;
        for d in ["evidence", "acquired", "recovered", "jobs"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        let audit = Arc::new(AuditLog::open(&root.join("audit.jsonl"))?);
        let case = Case {
            root: root.canonicalize()?,
            info,
            audit,
        };
        case.audit(
            if created { "case_created" } else { "case_opened" },
            serde_json::json!({ "case_id": case.info.case_id, "path": case.root.display().to_string() }),
        );
        Ok(case)
    }

    pub fn audit(&self, event: &str, details: serde_json::Value) {
        // Audit failures must not be silent: surface on stderr.
        if let Err(e) = self.audit.record(event, details) {
            eprintln!("audit log write failed: {e}");
        }
    }

    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.root.join("jobs").join(job_id)
    }

    pub fn recovered_dir(&self) -> PathBuf {
        self.root.join("recovered")
    }

    /// Resolve a case-relative path, refusing anything outside the case.
    pub fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let p = self.root.join(rel);
        let c = p.canonicalize().ok()?;
        c.starts_with(&self.root).then_some(c)
    }

    pub fn list_job_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root.join("jobs"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().join("job.json").exists())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}
