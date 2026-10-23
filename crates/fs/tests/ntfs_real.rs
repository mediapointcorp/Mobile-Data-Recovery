//! NTFS recovery against a real volume created by mkntfs and modified by
//! the ntfs-3g driver (see fixtures/make_ntfs_fixture.sh). Ground truth is
//! in fixtures/ntfs-deleted.manifest.json; `ntfsundelete` (an independent
//! implementation) reports IMG_0003..0006 as 100% recoverable.

use pr_fs::ntfs::{DataLocation, NtfsVolume};
use pr_model::source::MemSource;
use sha2::{Digest, Sha256};
use std::io::Read;

fn fixture() -> (MemSource, Vec<serde_json::Value>) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(format!("{dir}ntfs-deleted.img.gz")).unwrap())
        .read_to_end(&mut img)
        .unwrap();
    let manifest: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(format!("{dir}ntfs-deleted.manifest.json")).unwrap())
            .unwrap();
    (MemSource::new(img), manifest)
}

#[test]
fn recovers_deleted_and_fragmented_files_from_real_ntfs() {
    let (src, manifest) = fixture();
    let vol = NtfsVolume::open(&src).unwrap();
    assert_eq!(vol.geo.cluster_size, 4096);
    assert!(vol.has_bitmap());
    let entries = vol.walk(&|_, _| true, &mut |_, _| {});
    for m in &manifest {
        let path = m["path"].as_str().unwrap();
        let sha = m["sha256"].as_str().unwrap();
        let scenario = m["scenario"].as_str().unwrap();
        let found: Vec<_> = entries.iter().filter(|e| e.path == path).collect();
        match scenario {
            "live" | "deleted" | "deleted_fragmented" => {
                assert_eq!(found.len(), 1, "{path}: {found:?}");
                let e = found[0];
                assert_eq!(e.deleted, scenario != "live", "{path}");
                assert!(!e.orphan);
                assert!(e.modified.is_some());
                let plan = vol.plan(e);
                assert_eq!(
                    plan.clusters_now_allocated,
                    if scenario == "live" {
                        plan.clusters_total
                    } else {
                        0
                    },
                    "{path} {plan:?}"
                );
                if scenario == "deleted_fragmented" {
                    let DataLocation::NonResident { runs, .. } = &e.data else {
                        panic!()
                    };
                    assert!(runs.len() > 1, "fixture must be fragmented");
                }
                let data = vol.read_entry(e, u64::MAX).unwrap();
                assert_eq!(hex::encode(Sha256::digest(&data)), sha, "{path}");
            }
            "deleted_overwritten" => {
                // Its record was reused by a new file; nothing may claim it.
                for e in found {
                    let data = vol.read_entry(e, u64::MAX).unwrap();
                    assert_ne!(hex::encode(Sha256::digest(&data)), sha);
                }
            }
            other => panic!("unknown scenario {other}"),
        }
    }
    // Small deleted files from the fragmentation setup are found too.
    assert!(entries
        .iter()
        .any(|e| e.deleted && e.path == "/Documents/f001.bin"));
}

#[test]
fn free_extents_cover_deleted_data_only() {
    let (src, manifest) = fixture();
    let vol = NtfsVolume::open(&src).unwrap();
    let free = vol.free_extents();
    let free_bytes: u64 = free.iter().map(|(s, e)| e - s).sum();
    let alloc_bytes: u64 = vol.allocated_extents().iter().map(|(s, e)| e - s).sum();
    assert_eq!(
        free_bytes + alloc_bytes,
        vol.geo.total_clusters * vol.geo.cluster_size
    );
    // The disk was filled; free space is roughly the deleted photos.
    let entries = vol.walk(&|n, _| n.ends_with(".jpg"), &mut |_, _| {});
    for m in manifest.iter().filter(|m| {
        m["scenario"].as_str().unwrap().starts_with("deleted")
            && m["scenario"] != "deleted_overwritten"
    }) {
        let e = entries
            .iter()
            .find(|e| e.path == m["path"].as_str().unwrap())
            .unwrap();
        let DataLocation::NonResident { runs, .. } = &e.data else {
            panic!()
        };
        for r in runs {
            let start = r.lcn.unwrap() * vol.geo.cluster_size;
            assert!(
                free.iter().any(|(s, e)| start >= *s && start < *e),
                "{} run not in free space",
                e.path
            );
        }
    }
    assert!(
        free_bytes < 2 * 1024 * 1024,
        "only deleted data should be free, got {free_bytes}"
    );
}
