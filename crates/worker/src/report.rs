//! Recovery and evidence-integrity reports (JSON + self-contained HTML).

use crate::case::Case;
use crate::finalize::{finalize, FinalizeSummary};
use crate::jobs::JobRecord;
use crate::util;
use anyhow::Result;
use pr_model::audit::AuditLog;
use pr_model::evidence::EvidenceRecord;
use pr_model::model::{Classification, RecoveryResult};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Serialize)]
pub struct Report {
    pub tool: String,
    pub tool_version: String,
    pub generated_at: String,
    pub case: crate::case::CaseInfo,
    pub summary: FinalizeSummary,
    pub evidence: Vec<EvidenceRecord>,
    pub jobs: Vec<JobRecord>,
    pub layouts: Vec<serde_json::Value>,
    pub by_source: BTreeMap<String, BTreeMap<String, usize>>,
    pub limitations: Vec<String>,
    pub statements: Vec<String>,
    pub audit_events: usize,
    /// `Ok(n)`: hash chain intact over n events; `Err(line)`: first broken line.
    pub audit_chain: Result<usize, usize>,
}

pub fn build(case: &Case) -> Result<(Report, Vec<RecoveryResult>)> {
    let (results, summary) = finalize(case)?;
    let mut jobs = Vec::new();
    let mut evidence = Vec::new();
    let mut layouts = Vec::new();
    let mut limitations = Vec::new();
    for id in case.list_job_ids() {
        let Ok(j) = JobRecord::load(case, &id) else {
            continue;
        };
        let d = j.dir(case);
        if let Ok(e) = util::read_json::<EvidenceRecord>(&d.join("evidence.json")) {
            evidence.push(e);
        }
        if let Ok(v) = util::read_json::<serde_json::Value>(&d.join("layout.json")) {
            layouts.push(v);
        }
        for l in &j.limitations {
            if !limitations.contains(l) {
                limitations.push(l.clone());
            }
        }
        jobs.push(j);
    }
    let mut by_source: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for r in &results {
        *by_source
            .entry(r.source_id.clone())
            .or_default()
            .entry(r.classification.label().to_string())
            .or_default() += 1;
    }
    let s = &summary.summary;
    let statements = vec![
        format!("{} result(s) in total.", s.total),
        format!("{} are files that still exist (existing originals or duplicates); they are copies, not recovered deleted data.", s.existing_files),
        format!("{} were recovered from deleted or unallocated data (deleted-file records, carved images, partial images).", s.deleted_data_recovered),
        format!("{} are thumbnails/previews and are not substitutes for originals.", s.thumbnails),
        format!("{} deleted records have no recoverable data (overwritten, wiped or inaccessible); they are listed but not counted as recovered.", s.unavailable),
        "Data that could not be read (encrypted, inaccessible or unsupported sources) is reported as a limitation and is never included in any recovery count.".into(),
        "No recovery percentage is given against unknown totals: the number of photos that once existed on the device is not knowable from the evidence.".into(),
    ];
    let audit_events = AuditLog::read_all(case.audit.path())
        .map(|v| v.len())
        .unwrap_or(0);
    let audit_chain = AuditLog::verify_chain(case.audit.path()).unwrap_or(Err(0));
    let report = Report {
        tool: pr_model::TOOL_NAME.into(),
        tool_version: pr_model::TOOL_VERSION.into(),
        generated_at: pr_model::now_rfc3339(),
        case: case.info.clone(),
        summary,
        evidence,
        jobs,
        layouts,
        by_source,
        limitations,
        statements,
        audit_events,
        audit_chain,
    };
    Ok((report, results))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn write(case: &Case) -> Result<(String, String)> {
    let (report, results) = build(case)?;
    let json_path = case.root.join("report.json");
    util::write_json(&json_path, &report)?;
    let html = render_html(&report, &results);
    let html_path = case.root.join("report.html");
    util::atomic_write(&html_path, html.as_bytes())?;
    case.audit(
        "report_generated",
        serde_json::json!({ "json": "report.json", "html": "report.html" }),
    );
    Ok((
        json_path.display().to_string(),
        html_path.display().to_string(),
    ))
}

pub fn render_html(r: &Report, results: &[RecoveryResult]) -> String {
    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<title>PhotoRecovery report</title><style>
body{{font:14px/1.45 system-ui,Segoe UI,sans-serif;margin:24px;color:#1b1f24;background:#fff}}
h1{{font-size:22px}}h2{{font-size:17px;margin-top:28px;border-bottom:1px solid #ddd;padding-bottom:4px}}
table{{border-collapse:collapse;width:100%;margin:8px 0}}td,th{{border:1px solid #ddd;padding:4px 6px;text-align:left;vertical-align:top;font-size:12.5px}}
th{{background:#f4f6f8}}code{{font-size:12px;word-break:break-all}}.ok{{color:#11692b}}.bad{{color:#b3261e;font-weight:600}}
.note{{background:#fff8e1;border:1px solid #f0d58c;padding:8px 10px;border-radius:4px}}
@media (prefers-color-scheme:dark){{body{{background:#14171a;color:#e6e8ea}}th{{background:#22272c}}td,th{{border-color:#333}}.note{{background:#2a2414;border-color:#5c4b1c}}}}
</style></head><body><h1>PhotoRecovery report</h1>
<p>Case <code>{}</code> ({}) · generated {} · {} {}</p>"#,
        esc(&r.case.case_id),
        esc(&r.case.name),
        esc(&r.generated_at),
        esc(&r.tool),
        esc(&r.tool_version)
    );
    h.push_str("<h2>Summary</h2><ul>");
    for s in &r.statements {
        let _ = write!(h, "<li>{}</li>", esc(s));
    }
    h.push_str("</ul><table><tr><th>Classification</th><th>Count</th></tr>");
    for c in Classification::ALL {
        let n = r
            .summary
            .summary
            .by_classification
            .get(c.label())
            .copied()
            .unwrap_or(0);
        let _ = write!(h, "<tr><td>{}</td><td>{n}</td></tr>", esc(c.label()));
    }
    let _ = write!(
        h,
        "</table><p>Exact duplicate groups: {} · visually similar pairs: {} · thumbnail↔original links: {}</p>",
        r.summary.exact_duplicate_groups, r.summary.similar_pairs, r.summary.thumbnail_links
    );

    h.push_str("<h2>Evidence integrity</h2>");
    if r.evidence.is_empty() {
        h.push_str("<p>No evidence images in this case.</p>");
    } else {
        h.push_str("<table><tr><th>Source</th><th>Original</th><th>Mode</th><th>Size</th><th>SHA-256 before</th><th>SHA-256 after</th><th>Result</th></tr>");
        for e in &r.evidence {
            let verdict = match e.unchanged {
                Some(true) => "<span class=ok>unchanged</span>",
                Some(false) => "<span class=bad>CHANGED</span>",
                None => "not verified (job incomplete)",
            };
            let _ = write!(
                h,
                "<tr><td>{}</td><td><code>{}</code></td><td>{:?}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{verdict}</td></tr>",
                esc(&e.source_id),
                esc(&e.original_path),
                e.mode,
                e.size,
                esc(&e.sha256_before),
                esc(e.sha256_after.as_deref().unwrap_or("—"))
            );
        }
        h.push_str("</table>");
    }

    h.push_str("<h2>Limitations</h2>");
    if r.limitations.is_empty() {
        h.push_str("<p>None recorded.</p>");
    } else {
        h.push_str("<div class=note><ul>");
        for l in &r.limitations {
            let _ = write!(h, "<li>{}</li>", esc(l));
        }
        h.push_str("</ul></div>");
    }

    h.push_str("<h2>Jobs</h2><table><tr><th>Job</th><th>Kind</th><th>Status</th><th>Runs</th><th>Created</th><th>Updated</th><th>Error</th></tr>");
    for j in &r.jobs {
        let _ = write!(
            h,
            "<tr><td><code>{}</code></td><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&j.id),
            j.params.kind(),
            j.status,
            j.runs,
            esc(&j.created_at),
            esc(&j.updated_at),
            esc(j.error.as_deref().unwrap_or(""))
        );
    }
    h.push_str("</table>");

    let _ = write!(h, "<h2>Results ({})</h2><table><tr><th>Classification</th><th>Format</th><th>Method</th><th>Source</th><th>Offset</th><th>Length</th><th>Size</th><th>Validation</th><th>SHA-256</th><th>Notes</th></tr>", results.len());
    for x in results.iter().take(20_000) {
        let loc = x.source_path.clone().unwrap_or_else(|| x.source_id.clone());
        let notes: Vec<String> = x
            .warnings
            .iter()
            .chain(x.limitations.iter())
            .cloned()
            .collect();
        let _ = write!(
            h,
            "<tr><td>{}</td><td>{}</td><td>{:?}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{:?}</td><td><code>{}</code></td><td>{}</td></tr>",
            esc(x.classification.label()),
            x.format.extension(),
            x.method,
            esc(&loc),
            x.source_offset.map(|o| o.to_string()).unwrap_or_default(),
            x.candidate_length.map(|o| o.to_string()).unwrap_or_default(),
            match (x.width, x.height) { (Some(w), Some(hh)) => format!("{w}×{hh}"), _ => String::new() },
            x.validation,
            esc(x.sha256.as_deref().unwrap_or("")),
            esc(&notes.join(" · "))
        );
    }
    if results.len() > 20_000 {
        h.push_str("<tr><td colspan=10>… truncated; see report.json / results.json for all results</td></tr>");
    }
    let chain = match r.audit_chain {
        Ok(n) => format!("<span class=ok>hash chain intact over {n} events</span>"),
        Err(line) => format!(
            "<span class=bad>HASH CHAIN BROKEN at line {line}</span> — the log was modified"
        ),
    };
    let _ = write!(
        h,
        "</table><h2>Audit log</h2><p><code>audit.jsonl</code>: {} events, {chain}. Verified when this report was generated; events after that (such as this report's generation) are appended later.</p></body></html>",
        r.audit_events
    );
    h
}
