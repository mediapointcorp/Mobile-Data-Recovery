//! The Recuva-style engine on real NTFS and exFAT volumes made with the
//! real filesystem drivers (crates/fs/tests/fixtures).

use pr_model::source::{MemSource, ReadAt};
use pr_worker::recover::{recover, scan_source, Category, FoundFile, ScanRequest, State};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;

fn fixture(name: &str) -> (Arc<dyn ReadAt>, Vec<serde_json::Value>) {
    let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/../fs/tests/fixtures/");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(format!("{fx}{name}.img.gz")).unwrap())
        .read_to_end(&mut img)
        .unwrap();
    let m = serde_json::from_slice(&std::fs::read(format!("{fx}{name}.manifest.json")).unwrap())
        .unwrap();
    (Arc::new(MemSource::new(img)), m)
}

fn run(src: &Arc<dyn ReadAt>, req: &ScanRequest) -> Vec<FoundFile> {
    let (mut out, mut notes, mut id) = (Vec::new(), Vec::new(), 0);
    scan_source(
        "E:",
        src.clone(),
        req,
        &mut id,
        &pr_model::new_cancel_flag(),
        &|_, _, _, _, _| {},
        &mut out,
        &mut notes,
    )
    .unwrap();
    out
}

fn req(cat: Category, deep: bool) -> ScanRequest {
    ScanRequest {
        drives: vec!["E:".into()],
        category: cat,
        deep_scan: deep,
        folder: None,
        recycle_bin_only: false,
    }
}

fn sha(d: &[u8]) -> String {
    hex::encode(Sha256::digest(d))
}

/// Recover everything to a temp folder; return name → sha256.
fn recover_all(
    src: &Arc<dyn ReadAt>,
    found: &[FoundFile],
    keep: bool,
) -> (Vec<(String, String)>, std::path::PathBuf, tempfile::TempDir) {
    let t = tempfile::tempdir().unwrap();
    let dest = t.path().join("Recovered");
    let refs: Vec<&FoundFile> = found.iter().collect();
    let s = src.clone();
    let summary = recover(
        &refs,
        &dest,
        keep,
        &move |_| Ok(s.clone()),
        &|_, _| false,
        &pr_model::new_cancel_flag(),
        &|_, _| {},
    )
    .unwrap();
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    let mut files = Vec::new();
    for e in walk(&dest) {
        files.push((
            e.file_name().unwrap().to_string_lossy().to_string(),
            sha(&std::fs::read(&e).unwrap()),
        ));
    }
    (files, dest, t)
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(p).unwrap().flatten() {
        if e.path().is_dir() {
            v.extend(walk(&e.path()));
        } else {
            v.push(e.path());
        }
    }
    v
}

#[test]
fn recycle_bin_shift_delete_and_emptied_bin() {
    let (src, m) = fixture("ntfs-recycle");
    let found = run(&src, &req(Category::Pictures, false));
    let (files, dest, _t) = recover_all(&src, &found, false);
    for e in &m {
        let name = e["name"].as_str().unwrap();
        let hit = files.iter().find(|(n, _)| n == name);
        if e["scenario"] == "not_deleted" {
            assert!(hit.is_none(), "{name} still exists and must not be listed");
        } else {
            assert_eq!(
                hit.map(|h| h.1.as_str()),
                e["sha256"].as_str(),
                "{name} recovered byte-identical under its original name in {}",
                dest.display()
            );
            let f = found.iter().find(|f| f.name == name).unwrap();
            assert_eq!(f.state, State::Excellent);
            if e["scenario"] != "shift_delete" {
                assert_eq!(f.path, e["original_path"].as_str().unwrap());
            }
        }
    }
    let bin_only = run(
        &src,
        &ScanRequest {
            recycle_bin_only: true,
            ..req(Category::Pictures, false)
        },
    );
    assert_eq!(bin_only.len(), 3);
    assert_eq!(
        bin_only
            .iter()
            .filter(|f| f.origin == "recycle_bin")
            .count(),
        2
    );
    // Folder filter.
    let folder = run(
        &src,
        &ScanRequest {
            folder: Some(r"E:\Pictures\Holiday".into()),
            ..req(Category::Pictures, false)
        },
    );
    assert_eq!(folder.len(), 5);
    assert!(run(
        &src,
        &ScanRequest {
            folder: Some(r"E:\Documents".into()),
            ..req(Category::Pictures, false)
        }
    )
    .is_empty());
    assert!(run(&src, &req(Category::Music, false)).is_empty());
}

#[test]
fn ntfs_fragmented_and_overwritten() {
    let (src, m) = fixture("ntfs-deleted");
    let found = run(&src, &req(Category::Pictures, false));
    let (files, _, _t) = recover_all(&src, &found, true);
    for e in &m {
        let name = e["path"].as_str().unwrap().rsplit('/').next().unwrap();
        let hit = files.iter().find(|(n, _)| n == name);
        match e["scenario"].as_str().unwrap() {
            "live" => assert!(hit.is_none(), "{name} still exists and must not be listed"),
            "deleted_overwritten" => assert!(
                hit.is_none_or(|h| h.1 != e["sha256"].as_str().unwrap()),
                "{name}"
            ),
            _ => assert_eq!(hit.map(|h| h.1.as_str()), e["sha256"].as_str(), "{name}"),
        }
    }
    // Keep-folder-structure layout.
    assert!(found.iter().all(|f| f.path.starts_with(r"E:\")));
}

#[test]
fn exfat_card_all_files_and_deep_scan() {
    let (src, m) = fixture("exfat-card");
    let found = run(&src, &req(Category::All, false));
    let (files, _, _t) = recover_all(&src, &found, false);
    for e in &m {
        let name = e["path"].as_str().unwrap().rsplit('/').next().unwrap();
        let f = found.iter().find(|f| f.name == name);
        match e["scenario"].as_str().unwrap() {
            "live" => assert!(f.is_none(), "{name}"),
            "deleted" => {
                assert_eq!(f.unwrap().state, State::Excellent, "{name}");
                assert!(
                    files
                        .iter()
                        .any(|(n, h)| n == name && h == e["sha256"].as_str().unwrap()),
                    "{name}"
                );
            }
            // Its FAT chain was cleared and half its clusters reused: never "Excellent".
            "deleted_fragmented" => {
                assert_ne!(f.unwrap().state, State::Excellent, "{name}: {:?}", f)
            }
            _ => unreachable!(),
        }
    }
    let docs = run(&src, &req(Category::Documents, false));
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "report.pdf");
    // Deep scan adds nothing new on a card whose deleted photos were all
    // found through their records (no duplicates are listed).
    let deep = run(&src, &req(Category::Pictures, true));
    let fs = run(&src, &req(Category::Pictures, false));
    assert!(deep.iter().filter(|f| f.origin == "deep_scan").all(|d| fs
        .iter()
        .all(|f| f.size != d.size || f.state != State::Excellent || d.state != State::Excellent)));
}

#[test]
fn refuses_recovering_onto_the_scanned_drive() {
    let (src, _) = fixture("ntfs-recycle");
    let found = run(&src, &req(Category::Pictures, false));
    let refs: Vec<&FoundFile> = found.iter().collect();
    let t = tempfile::tempdir().unwrap();
    let s = src.clone();
    let err = recover(
        &refs,
        &t.path().join("x"),
        false,
        &move |_| Ok(s.clone()),
        &|_, d| d == "E:",
        &pr_model::new_cancel_flag(),
        &|_, _| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("overwrite"));
    assert!(!t.path().join("x").exists());
}
