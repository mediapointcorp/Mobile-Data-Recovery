//! Drive-scan mode (live device, free-space carving, deleted-only) on a real
//! NTFS volume built by mkntfs/ntfs-3g (crates/fs/tests/fixtures).

use pr_model::evidence::EvidenceMode;
use pr_model::model::{Classification, RecoveryMethod};
use pr_worker::case::Case;
use pr_worker::jobs::image::{run_target, CarveScope, ImageScanParams, Target};
use pr_worker::jobs::{self, JobParams, JobStatus};
use pr_worker::{finalize, NoProgress};
use std::io::Read;

fn ntfs_image(dir: &std::path::Path) -> (std::path::PathBuf, Vec<serde_json::Value>) {
    let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/../fs/tests/fixtures/");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(format!("{fx}ntfs-deleted.img.gz")).unwrap())
        .read_to_end(&mut img)
        .unwrap();
    let p = dir.join("ntfs.img");
    std::fs::write(&p, img).unwrap();
    (
        p,
        serde_json::from_slice(&std::fs::read(format!("{fx}ntfs-deleted.manifest.json")).unwrap())
            .unwrap(),
    )
}

#[test]
fn drive_mode_recovers_deleted_photos_only() {
    let t = tempfile::tempdir().unwrap();
    let (img, manifest) = ntfs_image(t.path());
    let case = Case::open_or_create(&t.path().join("case")).unwrap();
    let params = ImageScanParams {
        image_path: img.display().to_string(),
        evidence_mode: EvidenceMode::LiveDevice,
        formats: None,
        filesystem_analysis: true,
        carve: true,
        chunk_size_mib: Some(1),
        carve_scope: CarveScope::Unallocated,
        include_existing: false,
    };
    // Record the job as a drive scan, then run the device path directly
    // (a real drive id is validated against the system's device names).
    let mut rec = jobs::create_job(&case, JobParams::ImageScan(params.clone())).unwrap();
    let target = Target::Device {
        id: "test".into(),
        path: img.display().to_string(),
    };
    assert!(run_target(
        &case,
        &mut rec,
        target,
        &params,
        &pr_model::new_cancel_flag(),
        &NoProgress
    )
    .unwrap());
    let (rs, _) = finalize::finalize(&case).unwrap();

    for m in &manifest {
        let sha = m["sha256"].as_str().unwrap();
        let path = m["path"].as_str().unwrap();
        let hits: Vec<_> = rs
            .iter()
            .filter(|r| r.sha256.as_deref() == Some(sha))
            .collect();
        match m["scenario"].as_str().unwrap() {
            // Existing photos are in allocated space: not listed, not carved.
            "live" => assert!(hits.is_empty(), "{path} should not be reported: {hits:?}"),
            "deleted" | "deleted_fragmented" => {
                let r = hits
                    .iter()
                    .find(|r| r.method == RecoveryMethod::FilesystemDeletedEntry)
                    .unwrap_or_else(|| panic!("{path} not recovered"));
                assert_eq!(r.classification, Classification::DeletedFileRecord);
                assert_eq!(r.source_path.as_deref(), Some(path));
                assert!(r.metadata.contains_key("fs_modified"));
            }
            "deleted_overwritten" => assert!(hits.is_empty()),
            _ => unreachable!(),
        }
    }
    assert!(rs
        .iter()
        .all(|r| r.classification != Classification::ExistingOriginal));
    let job = &rec;
    assert!(job.limitations.iter().any(|l| l.contains("not hashed")));
    assert!(job
        .limitations
        .iter()
        .any(|l| l.contains("unallocated space")));
    let ev: pr_model::evidence::EvidenceRecord =
        pr_worker::util::read_json(&rec.dir(&case).join("evidence.json")).unwrap();
    assert_eq!(ev.mode, EvidenceMode::LiveDevice);
    assert_eq!(ev.unchanged, None);
}

#[test]
fn image_mode_lists_existing_too() {
    let t = tempfile::tempdir().unwrap();
    let (img, manifest) = ntfs_image(t.path());
    let case = Case::open_or_create(&t.path().join("case")).unwrap();
    let mut rec = jobs::create_job(
        &case,
        JobParams::ImageScan(ImageScanParams {
            image_path: img.display().to_string(),
            evidence_mode: EvidenceMode::ReadOnlyOriginal,
            formats: None,
            filesystem_analysis: true,
            carve: true,
            chunk_size_mib: None,
            carve_scope: CarveScope::All,
            include_existing: true,
        }),
    )
    .unwrap();
    assert_eq!(
        jobs::run_job(&case, &mut rec, &pr_model::new_cancel_flag(), &NoProgress).unwrap(),
        JobStatus::Completed,
        "{:?}",
        rec.error
    );
    let (rs, _) = finalize::finalize(&case).unwrap();
    for m in manifest.iter().filter(|m| m["scenario"] == "live") {
        assert!(rs
            .iter()
            .any(|r| r.classification == Classification::ExistingOriginal
                && r.sha256.as_deref() == m["sha256"].as_str()));
    }
}
