//! End-to-end pipeline tests on synthetic evidence with known ground truth.

use pr_model::evidence::EvidenceMode;
use pr_model::model::{Classification, RecoveryResult, Relation, ValidationStatus};
use pr_testkit::fat::FatBuilder;
use pr_testkit::images::{self, JpegOpts};
use pr_testkit::{sha256_hex, Rng};
use pr_worker::case::Case;
use pr_worker::finalize;
use pr_worker::jobs::{self, image::ImageScanParams, JobParams, JobRecord, JobStatus};
use pr_worker::{report, NoProgress, Progress};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    keep: Vec<u8>,
    gone: Vec<u8>,
    over: Vec<u8>,
    frag: Vec<u8>,
    thumb_only: Vec<u8>,
}

/// FAT32 volume: live photo, deleted photo, deleted+overwritten photo,
/// deleted fragmented photo, and a deleted small preview.
fn fat_image() -> (Vec<u8>, Fixture) {
    let mut b = FatBuilder::new(34, 1);
    b.mkdir("DCIM");
    b.mkdir("DCIM/Camera");
    b.mkdir("DCIM/.thumbnails");
    let fx = Fixture {
        keep: images::jpeg(
            480,
            360,
            1,
            &JpegOpts {
                exif_thumbnail: true,
                exif_datetime: Some("2024:05:01 12:00:00".into()),
                ..Default::default()
            },
        ),
        gone: images::jpeg(
            512,
            384,
            2,
            &JpegOpts {
                exif_datetime: Some("2024:05:01 12:01:00".into()),
                ..Default::default()
            },
        ),
        over: images::jpeg(400, 300, 3, &JpegOpts::default()),
        frag: images::jpeg(640, 480, 4, &JpegOpts::default()),
        thumb_only: images::jpeg(160, 120, 5, &JpegOpts::default()),
    };
    b.add_file("DCIM/Camera/IMG_20240501_120000.jpg", &fx.keep);
    b.add_file("DCIM/Camera/IMG_20240501_120100.jpg", &fx.gone);
    let o = b.add_file("DCIM/Camera/IMG_20240501_120200.jpg", &fx.over);
    b.add_interleaved(
        "DCIM/Camera/IMG_20240501_120300.jpg",
        &fx.frag,
        "DCIM/Camera/notes.txt",
        &Rng::new(7).bytes(4000),
    );
    b.add_file("DCIM/.thumbnails/1714564800.jpg", &fx.thumb_only);
    b.delete("DCIM/Camera/IMG_20240501_120100.jpg");
    b.delete("DCIM/Camera/IMG_20240501_120200.jpg");
    b.delete("DCIM/Camera/IMG_20240501_120300.jpg");
    b.delete("DCIM/.thumbnails/1714564800.jpg");
    b.add_file_clusters(
        "DCIM/Camera/overwriter.bin",
        &vec![0x5A; fx.over.len()],
        o.clusters,
    );
    (b.finish(), fx)
}

fn setup(tmp: &Path, img: &[u8]) -> (Case, std::path::PathBuf) {
    let ev = tmp.join("evidence").join("sdcard.img");
    std::fs::create_dir_all(ev.parent().unwrap()).unwrap();
    std::fs::write(&ev, img).unwrap();
    let case = Case::open_or_create(&tmp.join("case")).unwrap();
    (case, ev)
}

fn scan_params(ev: &Path, mode: EvidenceMode) -> JobParams {
    JobParams::ImageScan(ImageScanParams {
        image_path: ev.display().to_string(),
        evidence_mode: mode,
        formats: None,
        filesystem_analysis: true,
        carve: true,
        chunk_size_mib: Some(1),
        carve_scope: Default::default(),
        include_existing: true,
    })
}

fn by_sha<'a>(rs: &'a [RecoveryResult], data: &[u8]) -> Vec<&'a RecoveryResult> {
    let h = sha256_hex(data);
    rs.iter()
        .filter(|r| r.sha256.as_deref() == Some(h.as_str()))
        .collect()
}

#[test]
fn fat_image_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (img, fx) = fat_image();
    let (case, ev) = setup(tmp.path(), &img);
    let before = sha256_hex(&img);
    let mut rec = jobs::create_job(&case, scan_params(&ev, EvidenceMode::WorkingCopy)).unwrap();
    assert_eq!(
        jobs::run_job(&case, &mut rec, &pr_model::new_cancel_flag(), &NoProgress).unwrap(),
        JobStatus::Completed,
        "{:?}",
        rec.error
    );
    let (rs, summary) = finalize::finalize(&case).unwrap();

    // Live file: existing original via filesystem, plus carved duplicate.
    let keep = by_sha(&rs, &fx.keep);
    assert!(keep
        .iter()
        .any(|r| r.classification == Classification::ExistingOriginal
            && r.source_path.as_deref() == Some("/DCIM/Camera/IMG_20240501_120000.jpg")));
    assert!(keep
        .iter()
        .any(|r| r.classification == Classification::CarvedImage
            && r.associations
                .iter()
                .any(|a| a.relation == Relation::DuplicateOf)));

    // Deleted intact file: recovered through the deleted record, byte-identical.
    let gone = by_sha(&rs, &fx.gone);
    let rec_gone = gone
        .iter()
        .find(|r| r.classification == Classification::DeletedFileRecord)
        .expect("deleted record");
    assert_eq!(
        rec_gone.source_path.as_deref(),
        Some("/DCIM/Camera/IMG_20240501_120100.jpg")
    );
    assert_eq!(rec_gone.validation, ValidationStatus::Valid);
    assert_eq!(
        rec_gone
            .metadata
            .get("exif_datetime_original")
            .map(String::as_str),
        Some("2024-05-01 12:01:00")
    );

    // Overwritten: listed as unavailable, never exported, never counted as recovered.
    let over = rs
        .iter()
        .find(|r| r.source_path.as_deref() == Some("/DCIM/Camera/IMG_20240501_120200.jpg"))
        .unwrap();
    assert_eq!(over.classification, Classification::UnavailableSourceData);
    assert!(over.output_path.is_none());
    assert!(by_sha(&rs, &fx.over).is_empty());

    // Fragmented deleted file: not reported as a complete original.
    assert!(
        by_sha(&rs, &fx.frag).is_empty(),
        "fragmented file must not be claimed byte-identical"
    );
    let frag = rs
        .iter()
        .find(|r| r.source_path.as_deref() == Some("/DCIM/Camera/IMG_20240501_120300.jpg"))
        .unwrap();
    assert!(
        matches!(
            frag.classification,
            Classification::UnavailableSourceData | Classification::PartialImage
        ),
        "{frag:?}"
    );

    // Embedded EXIF thumbnail of the live photo is a thumbnail linked to its parent.
    let thumb = by_sha(&rs, &images::embedded_thumbnail(480, 360, 1));
    assert!(thumb
        .iter()
        .all(|t| t.classification == Classification::Thumbnail));
    assert!(thumb.iter().any(|t| t
        .associations
        .iter()
        .any(|a| a.relation == Relation::EmbeddedIn)));

    // Thumbnail-only deleted preview stays a thumbnail-sized result, never upscaled.
    let t = by_sha(&rs, &fx.thumb_only);
    assert!(!t.is_empty());
    assert!(t.iter().all(|r| r.width == Some(160)));
    assert!(t
        .iter()
        .any(|r| r.classification == Classification::Thumbnail));

    // Summary keeps the categories apart.
    assert!(summary.summary.unavailable >= 1);
    assert!(summary.summary.existing_files >= 1);

    // Evidence integrity.
    assert_eq!(sha256_hex(&std::fs::read(&ev).unwrap()), before);
    let evr: pr_model::evidence::EvidenceRecord =
        pr_worker::util::read_json(&rec.dir(&case).join("evidence.json")).unwrap();
    assert_eq!(evr.sha256_before, before);
    assert_eq!(evr.unchanged, Some(true));
    assert_eq!(evr.mode, EvidenceMode::WorkingCopy);
    assert_ne!(evr.analysed_path, evr.original_path);

    // Reports and audit log.
    let (j, h) = report::write(&case).unwrap();
    let html = std::fs::read_to_string(h).unwrap();
    assert!(html.contains("unchanged") && html.contains(&before));
    assert!(std::fs::read_to_string(j)
        .unwrap()
        .contains("\"statements\""));
    let audit = pr_model::audit::AuditLog::read_all(&case.root.join("audit.jsonl")).unwrap();
    assert!(
        pr_model::audit::AuditLog::verify_chain(&case.root.join("audit.jsonl"))
            .unwrap()
            .is_ok()
    );
    assert!(html.contains("hash chain intact"));
    for ev in [
        "case_created",
        "evidence_registered",
        "layout_analyzed",
        "evidence_verified",
        "job_finished",
        "report_generated",
    ] {
        assert!(
            audit.iter().any(|a| a.event == ev),
            "missing audit event {ev}"
        );
    }

    // Export to a separate destination with verified hashes.
    let dest = tmp.path().join("export");
    let classes: BTreeSet<_> = [Classification::DeletedFileRecord, Classification::Thumbnail]
        .into_iter()
        .collect();
    let s = finalize::export(&case, &dest, &classes, false).unwrap();
    assert!(s.exported >= 2);
    assert_eq!(s.verification_failures, 0);
    assert!(dest.join("manifest.csv").exists());
    assert!(
        finalize::export(&case, &dest, &classes, false).is_err(),
        "non-empty destination refused"
    );
    assert!(
        finalize::export(&case, &case.root.join("x"), &classes, false).is_err(),
        "destination inside case refused"
    );
}

/// Cancels the job once carving has passed `at` bytes.
struct CancelAt {
    flag: pr_model::CancelFlag,
    at: u64,
    seen: AtomicU64,
}

impl Progress for CancelAt {
    fn report(&self, _: &str, phase: &str, done: u64, _: u64, _: &str) {
        if phase == "carve" && done >= self.at {
            self.seen.fetch_add(1, Ordering::Relaxed);
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

fn key(rs: &[RecoveryResult]) -> Vec<(Option<u64>, Option<String>, Classification)> {
    let mut v: Vec<_> = rs
        .iter()
        .map(|r| (r.source_offset, r.sha256.clone(), r.classification))
        .collect();
    v.sort();
    v
}

#[test]
fn interrupted_scan_resumes_with_identical_results() {
    let (disk, _) = pr_testkit::disk::standard_disk(&pr_testkit::disk::StandardDiskParams {
        jpegs: 9,
        ..Default::default()
    });
    // Reference: uninterrupted run.
    let t1 = tempfile::tempdir().unwrap();
    let (c1, ev1) = setup(t1.path(), &disk);
    let mut r1 = jobs::create_job(&c1, scan_params(&ev1, EvidenceMode::ReadOnlyOriginal)).unwrap();
    jobs::run_job(&c1, &mut r1, &pr_model::new_cancel_flag(), &NoProgress).unwrap();
    let full = r1.results(&c1);
    assert!(full.len() > 10);

    // Interrupted twice, then resumed to completion.
    let t2 = tempfile::tempdir().unwrap();
    let (c2, ev2) = setup(t2.path(), &disk);
    let mut r2 = jobs::create_job(&c2, scan_params(&ev2, EvidenceMode::ReadOnlyOriginal)).unwrap();
    for at in [1u64 << 20, 3 << 20] {
        let p = CancelAt {
            flag: pr_model::new_cancel_flag(),
            at,
            seen: AtomicU64::new(0),
        };
        let st = jobs::run_job(&c2, &mut r2, &p.flag.clone(), &p).unwrap();
        assert_eq!(st, JobStatus::Cancelled);
        assert!(p.seen.load(Ordering::Relaxed) > 0);
        r2 = JobRecord::load(&c2, &r2.id).unwrap();
    }
    assert_eq!(
        jobs::run_job(&c2, &mut r2, &pr_model::new_cancel_flag(), &NoProgress).unwrap(),
        JobStatus::Completed
    );
    assert_eq!(r2.runs, 3);
    assert_eq!(key(&r2.results(&c2)), key(&full));
    let ev: pr_model::evidence::EvidenceRecord =
        pr_worker::util::read_json(&r2.dir(&c2).join("evidence.json")).unwrap();
    assert_eq!(ev.unchanged, Some(true));
}

#[test]
fn encrypted_and_unsupported_sources_are_reported_not_fabricated() {
    // Random data: looks encrypted, nothing recovered.
    let t = tempfile::tempdir().unwrap();
    let (case, ev) = setup(t.path(), &Rng::new(1).bytes(3 << 20));
    let mut rec =
        jobs::create_job(&case, scan_params(&ev, EvidenceMode::ReadOnlyOriginal)).unwrap();
    assert_eq!(
        jobs::run_job(&case, &mut rec, &pr_model::new_cancel_flag(), &NoProgress).unwrap(),
        JobStatus::Completed
    );
    assert!(rec
        .results(&case)
        .iter()
        .all(|r| r.classification != Classification::CarvedImage));
    assert!(
        rec.limitations.iter().any(|l| l.contains("encrypted")),
        "{:?}",
        rec.limitations
    );

    // ext4 with the encryption feature: identification only, limitation recorded.
    let t = tempfile::tempdir().unwrap();
    let mut d = vec![0u8; 1 << 20];
    d[1024 + 0x38..1024 + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
    d[1024 + 0x60..1024 + 0x64].copy_from_slice(&0x10040u32.to_le_bytes());
    let (case, ev) = setup(t.path(), &d);
    let mut rec =
        jobs::create_job(&case, scan_params(&ev, EvidenceMode::ReadOnlyOriginal)).unwrap();
    jobs::run_job(&case, &mut rec, &pr_model::new_cancel_flag(), &NoProgress).unwrap();
    assert!(rec
        .limitations
        .iter()
        .any(|l| l.contains("ext4") && l.contains("not offered")));
    assert!(rec
        .limitations
        .iter()
        .any(|l| l.contains("file-based encryption")));
}
