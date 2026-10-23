//! Merge job results, deduplicate, link thumbnails, and export.

use crate::case::Case;
use crate::jobs::JobRecord;
use crate::util;
use anyhow::{bail, Result};
use pr_model::model::{Classification, RecoveryResult, Relation, ResultSummary};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// dHash distance for "visually similar" (exact banding requires <= 7).
pub const SIMILAR_DISTANCE: u32 = 6;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FinalizeSummary {
    pub summary: ResultSummary,
    pub exact_duplicate_groups: usize,
    pub similar_pairs: usize,
    pub thumbnail_links: usize,
}

/// Merge all jobs' results into `results.json`.
pub fn finalize(case: &Case) -> Result<(Vec<RecoveryResult>, FinalizeSummary)> {
    let mut all = Vec::new();
    for id in case.list_job_ids() {
        if let Ok(job) = JobRecord::load(case, &id) {
            let mut rs = job.results(case);
            for r in &mut rs {
                r.metadata.insert("job_id".into(), id.clone());
            }
            all.extend(rs);
        }
    }
    // Reverse links for embedded thumbnails.
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for r in &all {
        for a in r
            .associations
            .iter()
            .filter(|a| a.relation == Relation::EmbeddedIn)
        {
            reverse
                .entry(a.result_id.clone())
                .or_default()
                .push(r.id.clone());
        }
    }
    for r in &mut all {
        if let Some(children) = reverse.get(&r.id) {
            for c in children {
                r.associate(c, Relation::HasThumbnail, "embedded EXIF thumbnail");
            }
        }
    }
    let d = pr_validate::dedupe(&mut all, SIMILAR_DISTANCE);
    let links = pr_validate::match_thumbnails(&mut all);
    all.sort_by(|a, b| {
        (
            a.classification,
            &a.source_id,
            a.source_offset,
            &a.source_path,
        )
            .cmp(&(
                b.classification,
                &b.source_id,
                b.source_offset,
                &b.source_path,
            ))
    });
    util::write_json(&case.root.join("results.json"), &all)?;
    let s = FinalizeSummary {
        summary: ResultSummary::from_results(&all),
        exact_duplicate_groups: d.exact_groups,
        similar_pairs: d.similar_pairs,
        thumbnail_links: links,
    };
    case.audit("results_finalized", serde_json::to_value(&s)?);
    Ok((all, s))
}

pub fn load_results(case: &Case) -> Vec<RecoveryResult> {
    util::read_json(&case.root.join("results.json")).unwrap_or_default()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExportSummary {
    pub destination: String,
    pub exported: usize,
    pub skipped_no_file: usize,
    pub verification_failures: usize,
}

fn csv(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Copy result files of the selected classifications into `dest` (which must
/// be a new or empty folder outside the case and outside evidence), verify
/// each copy's SHA-256, and write `manifest.csv` + `export.json`.
pub fn export(
    case: &Case,
    dest: &Path,
    classes: &BTreeSet<Classification>,
    include_duplicates: bool,
) -> Result<ExportSummary> {
    if !dest.is_absolute() {
        bail!("export destination must be an absolute path");
    }
    if dest.exists() && std::fs::read_dir(dest)?.next().is_some() {
        bail!("export destination must be a new or empty folder");
    }
    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize()?;
    if dest.starts_with(&case.root) {
        bail!("export destination must be outside the case folder");
    }
    let results = load_results(case);
    let mut summary = ExportSummary {
        destination: dest.display().to_string(),
        exported: 0,
        skipped_no_file: 0,
        verification_failures: 0,
    };
    let mut manifest = String::from("id,classification,method,format,source_id,source_path,source_offset,length,width,height,validation,sha256,exported_path,warnings\n");
    let mut exported = Vec::new();
    for r in &results {
        if !classes.contains(&r.classification) {
            continue;
        }
        if !include_duplicates
            && r.associations
                .iter()
                .any(|a| a.relation == Relation::DuplicateOf)
        {
            continue;
        }
        let Some(src) = r.output_path.as_deref().and_then(|p| case.resolve(p)) else {
            summary.skipped_no_file += 1;
            continue;
        };
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let out = dest
            .join(r.classification.dir_name())
            .join(format!("{}_{}", &r.id[..8], name));
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::fs::copy(&src, &out)?;
        let (h, _) = pr_model::evidence::sha256_file(&out, None, |_| {})?;
        if Some(&h) != r.sha256.as_ref() {
            summary.verification_failures += 1;
        }
        summary.exported += 1;
        let rel = util::rel(&dest, &out);
        manifest.push_str(&format!(
            "{},{},{:?},{},{},{},{},{},{},{},{:?},{},{},{}\n",
            r.id,
            csv(r.classification.label()),
            r.method,
            r.format.extension(),
            csv(&r.source_id),
            csv(r.source_path.as_deref().unwrap_or("")),
            r.source_offset.map(|o| o.to_string()).unwrap_or_default(),
            r.candidate_length
                .map(|o| o.to_string())
                .unwrap_or_default(),
            r.width.map(|o| o.to_string()).unwrap_or_default(),
            r.height.map(|o| o.to_string()).unwrap_or_default(),
            r.validation,
            r.sha256.as_deref().unwrap_or(""),
            csv(&rel),
            csv(&r.warnings.join(" | ")),
        ));
        exported.push(serde_json::json!({ "id": r.id, "path": rel, "sha256": h }));
    }
    util::atomic_write(&dest.join("manifest.csv"), manifest.as_bytes())?;
    util::write_json(
        &dest.join("export.json"),
        &serde_json::json!({
            "tool": pr_model::TOOL_NAME, "tool_version": pr_model::TOOL_VERSION, "case_id": case.info.case_id,
            "exported_at": pr_model::now_rfc3339(), "summary": summary, "files": exported,
        }),
    )?;
    case.audit("export_completed", serde_json::to_value(&summary)?);
    Ok(summary)
}
