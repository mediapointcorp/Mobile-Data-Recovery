//! exFAT recovery against a real card image made by mkfs.exfat and the
//! exfat-fuse driver (fixtures/make_exfat_fixture.sh).

use pr_fs::exfat::ExfatVolume;
use pr_model::source::{read_vec_at, MemSource};
use sha2::{Digest, Sha256};
use std::io::Read;

#[test]
fn recovers_deleted_files_from_real_exfat() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(format!("{dir}exfat-card.img.gz")).unwrap())
        .read_to_end(&mut img)
        .unwrap();
    let manifest: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(format!("{dir}exfat-card.manifest.json")).unwrap())
            .unwrap();
    let src = MemSource::new(img);
    let vol = ExfatVolume::open(&src).unwrap();
    assert!(vol.has_bitmap());
    let entries = vol.walk();
    for m in &manifest {
        let path = m["path"].as_str().unwrap();
        let e = entries.iter().find(|e| e.path == path).unwrap_or_else(|| {
            panic!(
                "{path} not found in {:?}",
                entries.iter().map(|e| &e.path).take(20).collect::<Vec<_>>()
            )
        });
        let scen = m["scenario"].as_str().unwrap();
        assert_eq!(e.deleted, scen != "live", "{path}");
        assert!(e.modified.is_some());
        let plan = vol.plan(e);
        let mut data = Vec::new();
        for (off, len) in &plan.pieces {
            data.extend(read_vec_at(&src, *off, *len as usize).unwrap());
        }
        let ok = hex::encode(Sha256::digest(&data)) == m["sha256"].as_str().unwrap();
        eprintln!(
            "{path}: {scen}, exact={}, pieces={}, reused={}/{}, identical={ok}",
            plan.exact,
            plan.pieces.len(),
            plan.clusters_now_allocated,
            plan.clusters_total
        );
        if scen == "deleted_fragmented" {
            // Recovered exactly only if the driver left the FAT chain; never claimed exact otherwise.
            assert!(
                ok || !plan.exact,
                "{path}: exact plan must give identical data"
            );
        } else {
            assert!(ok, "{path}");
            if scen != "live" {
                assert_eq!(plan.clusters_now_allocated, 0);
            }
        }
    }
}
