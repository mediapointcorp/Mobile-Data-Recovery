//! Windows only: read the real system volume through the raw-device path.
//! Runs where the test process is elevated (GitHub's Windows runners are);
//! otherwise it reports itself skipped.

#[cfg(windows)]
#[test]
fn reads_real_ntfs_system_volume() {
    use pr_model::source::ReadAt;
    use pr_worker::drives;
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let list = drives::list();
    let entry = list
        .iter()
        .find(|d| d.id.eq_ignore_ascii_case(&sys))
        .expect("system drive listed");
    assert!(entry.is_system);
    assert!(drives::path_on_drive(
        std::path::Path::new(&format!(r"{sys}\Windows")),
        &sys
    ));
    if !drives::is_elevated() {
        // On CI the raw read is the point of this test: a skip must not pass.
        assert!(
            std::env::var_os("CI").is_none(),
            "CI runner is not elevated; raw drive read was not tested"
        );
        eprintln!("skipped raw read: not running as administrator");
        return;
    }
    let dev = pr_model::device::open_device(&drives::device_path(&sys).unwrap()).unwrap();
    assert!(dev.len() > 1 << 30, "volume size {}", dev.len());
    assert_eq!(
        entry.filesystem.as_deref(),
        Some("NTFS"),
        "system drive filesystem"
    );
    let v =
        pr_fs::ntfs::NtfsVolume::open(&dev).expect("parse real NTFS boot sector, $MFT and $Bitmap");
    assert!(v.has_bitmap());
    assert!(v.record_count() > 1000);
    let free: u64 = v.free_extents().iter().map(|(s, e)| e - s).sum();
    let alloc: u64 = v.allocated_extents().iter().map(|(s, e)| e - s).sum();
    assert!(free > 0 && alloc > 0);
    assert_eq!(free + alloc, v.geo.total_clusters * v.geo.cluster_size);
    eprintln!(
        "system volume: {} bytes, {} MFT records, {:.1} GB free",
        dev.len(),
        v.record_count(),
        free as f64 / 1e9
    );
}
