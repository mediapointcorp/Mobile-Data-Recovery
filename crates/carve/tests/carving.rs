//! End-to-end carving tests against synthetic images with ground truth.

use pr_carve::scanner::{scan, Found, ScanConfig, ScanOutcome, ScanSink, ScanState, VecSink};
use pr_carve::{EndStatus, FormatSet};
use pr_model::model::ImageFormat;
use pr_model::new_cancel_flag;
use pr_model::source::{MemSource, ReadAt};
use pr_testkit::disk::{standard_disk, DiskBuilder, Scenario, StandardDiskParams};
use pr_testkit::images::{self, JpegOpts};
use pr_testkit::sha256_hex;
use std::sync::atomic::Ordering;

fn run(data: &[u8], chunk: usize) -> Vec<Found> {
    let src = MemSource::new(data.to_vec());
    let cfg = ScanConfig {
        chunk_size: chunk,
        ..Default::default()
    };
    let mut st = ScanState::default();
    let mut sink = VecSink::default();
    assert_eq!(
        scan(&src, &cfg, &mut st, &new_cancel_flag(), &mut sink).unwrap(),
        ScanOutcome::Completed
    );
    sink.found
}

fn bytes_of(data: &[u8], f: &Found) -> Vec<u8> {
    data[f.candidate.offset as usize..f.candidate.end() as usize].to_vec()
}

#[test]
fn intact_jpeg_variants_are_byte_identical() {
    let variants = [
        JpegOpts::default(),
        JpegOpts {
            progressive: true,
            ..Default::default()
        },
        JpegOpts {
            restart_interval: Some(1),
            ..Default::default()
        },
        JpegOpts {
            exif_thumbnail: true,
            exif_datetime: Some("2023:01:02 03:04:05".into()),
            ..Default::default()
        },
        JpegOpts {
            quality: 100,
            progressive: true,
            restart_interval: Some(3),
            exif_thumbnail: true,
            ..Default::default()
        },
    ];
    let mut d = DiskBuilder::new(1);
    let mut originals = vec![];
    for (i, o) in variants.iter().enumerate() {
        d.random(777);
        let j = images::jpeg(320 + 8 * i as u32, 240, 100 + i as u64, o);
        d.place(Scenario::Intact, ImageFormat::Jpeg, &j, 320, 240);
        originals.push(j);
    }
    d.random(500);
    let (data, manifest) = d.finish();
    let found = run(&data, 1 << 20);
    for (e, orig) in manifest.entries.iter().zip(&originals) {
        let f = found
            .iter()
            .find(|f| f.candidate.offset == e.offset)
            .expect("found");
        assert_eq!(f.candidate.status, EndStatus::Complete);
        assert_eq!(bytes_of(&data, f), *orig);
        assert_eq!(f.candidate.width, Some(e.width + 8 * e.id as u32));
        assert_eq!(f.embedded_in, None);
    }
}

#[test]
fn embedded_exif_thumbnail_is_found_and_linked() {
    let mut d = DiskBuilder::new(2);
    d.random(100);
    let j = images::jpeg(
        400,
        300,
        7,
        &JpegOpts {
            exif_thumbnail: true,
            ..Default::default()
        },
    );
    d.place(Scenario::Intact, ImageFormat::Jpeg, &j, 400, 300);
    let (data, _) = d.finish();
    let found = run(&data, 1 << 20);
    assert_eq!(found.len(), 2, "{found:?}");
    let thumb = found.iter().find(|f| f.embedded_in.is_some()).unwrap();
    assert_eq!(thumb.embedded_in, Some(100));
    assert_eq!(
        bytes_of(&data, thumb),
        images::embedded_thumbnail(400, 300, 7)
    );
    assert_eq!(thumb.candidate.width, Some(160));
}

#[test]
fn signatures_split_across_every_chunk_boundary_position() {
    let j = images::jpeg(64, 48, 9, &JpegOpts::default());
    let chunk = 4096usize;
    for k in 0..40usize {
        let mut data = vec![0x11u8; chunk - 20 + k];
        let off = data.len();
        data.extend_from_slice(&j);
        data.extend_from_slice(&[0x22; 300]);
        let found = run(&data, chunk);
        assert_eq!(found.len(), 1, "boundary offset {k}");
        assert_eq!(found[0].candidate.offset as usize, off);
        assert_eq!(bytes_of(&data, &found[0]), j);
    }
}

#[test]
fn tiny_chunks_give_identical_results() {
    let (data, _) = standard_disk(&StandardDiskParams {
        jpegs: 6,
        ..Default::default()
    });
    let a = run(&data, 8 << 20);
    let b = run(&data, 64);
    let c = run(&data, 4093);
    let key = |v: &Vec<Found>| {
        v.iter()
            .map(|f| (f.candidate.offset, f.candidate.length, f.candidate.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&a), key(&b));
    assert_eq!(key(&a), key(&c));
}

#[test]
fn standard_disk_ground_truth() {
    let params = StandardDiskParams::default();
    let (data, manifest) = standard_disk(&params);
    let found = run(&data, params.chunk_size as usize);
    let at = |off: u64| {
        found
            .iter()
            .find(|f| f.candidate.offset == off && f.embedded_in.is_none())
    };

    for e in &manifest.entries {
        let f = at(e.offset);
        match e.scenario {
            s if s.fully_recoverable() => {
                let f = f.unwrap_or_else(|| panic!("missing {e:?}"));
                assert_eq!(f.candidate.status, EndStatus::Complete, "{e:?}");
                assert_eq!(sha256_hex(&bytes_of(&data, f)), e.original_sha256, "{e:?}");
                assert_eq!(f.candidate.format, e.format);
                assert_eq!(
                    (f.candidate.width, f.candidate.height),
                    (Some(e.width), Some(e.height)),
                    "{e:?}"
                );
            }
            Scenario::Truncated => {
                let f = f.expect("partial for truncated");
                assert_ne!(f.candidate.status, EndStatus::Complete);
                assert!(f.candidate.length <= e.stored_len, "{e:?} {f:?}");
                assert!(f.candidate.length > e.stored_len / 2);
            }
            Scenario::Corrupted => {
                let f = f.expect("partial for corrupted");
                assert_ne!(f.candidate.status, EndStatus::Complete, "{e:?} {f:?}");
                assert!(f.candidate.length < e.original_len);
            }
            Scenario::Fragmented => {
                let f = f.expect("partial for fragmented");
                assert_ne!(sha256_hex(&bytes_of(&data, f)), e.original_sha256);
                assert_ne!(f.candidate.status, EndStatus::Complete, "{e:?} {f:?}");
                assert!(
                    f.candidate.length <= e.stored_len + 16 * 1024,
                    "{e:?} {f:?}"
                );
            }
            Scenario::FalsePositive => assert!(f.is_none(), "false positive accepted: {f:?}"),
            _ => unreachable!(),
        }
    }
    // Nothing reported that is not in the ground truth (except embedded thumbnails).
    for f in found.iter().filter(|f| f.embedded_in.is_none()) {
        assert!(
            manifest
                .entries
                .iter()
                .any(|e| e.offset == f.candidate.offset),
            "unexpected {f:?}"
        );
    }
}

/// A sink that raises the cancel flag after `n` checkpoints.
struct CancelAfter {
    inner: VecSink,
    n: u64,
    flag: pr_model::CancelFlag,
    saved: Option<ScanState>,
}

impl ScanSink for CancelAfter {
    fn on_candidate(&mut self, f: Found) -> std::io::Result<()> {
        self.inner.on_candidate(f)
    }
    fn on_checkpoint(&mut self, st: &ScanState, total: u64) -> std::io::Result<()> {
        self.inner.on_checkpoint(st, total)?;
        self.saved = Some(st.clone());
        if self.inner.checkpoints >= self.n {
            self.flag.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[test]
fn interrupted_scan_resumes_to_identical_results() {
    let (data, _) = standard_disk(&StandardDiskParams {
        jpegs: 8,
        ..Default::default()
    });
    let src = MemSource::new(data.clone());
    let cfg = ScanConfig {
        chunk_size: 64 * 1024,
        ..Default::default()
    };
    let full = run(&data, cfg.chunk_size);

    let mut collected: Vec<Found> = vec![];
    let mut state = ScanState::default();
    let mut rounds = 0;
    loop {
        rounds += 1;
        let flag = new_cancel_flag();
        let mut sink = CancelAfter {
            inner: VecSink::default(),
            n: 3,
            flag: flag.clone(),
            saved: None,
        };
        // Resume from the last persisted checkpoint (serialised round-trip).
        let json = serde_json::to_string(&state).unwrap();
        let mut st: ScanState = serde_json::from_str(&json).unwrap();
        let outcome = scan(&src, &cfg, &mut st, &flag, &mut sink).unwrap();
        // Keep only candidates found before the checkpoint we resume from.
        let resume_at = sink
            .saved
            .as_ref()
            .map(|s| s.next_offset)
            .unwrap_or(st.next_offset);
        collected.extend(
            sink.inner
                .found
                .into_iter()
                .filter(|f| f.candidate.offset < resume_at),
        );
        state = sink.saved.unwrap_or(st);
        if outcome == ScanOutcome::Completed {
            break;
        }
        assert!(rounds < 1000);
    }
    assert!(rounds > 2, "test should actually interrupt");
    let key = |v: &Vec<Found>| {
        v.iter()
            .map(|f| (f.candidate.offset, f.candidate.length))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&collected), key(&full));
}

#[test]
fn format_selection_and_other_formats() {
    let mut d = DiskBuilder::new(5);
    d.random(10);
    let p = images::png(50, 40, 1);
    d.place(Scenario::Intact, ImageFormat::Png, &p, 50, 40);
    d.random(10);
    let w = images::webp_lossless(30, 20, 2);
    d.place(Scenario::Intact, ImageFormat::Webp, &w, 30, 20);
    d.random(10);
    let h = images::heif_like(4032, 3024, 5000, 3);
    d.place(Scenario::Intact, ImageFormat::Heic, &h, 4032, 3024);
    d.random(10);
    let (data, _) = d.finish();
    let all = run(&data, 1 << 20);
    assert_eq!(all.len(), 3);
    assert_eq!(bytes_of(&data, &all[0]), p);
    assert_eq!(bytes_of(&data, &all[1]), w);
    assert_eq!(bytes_of(&data, &all[2]), h);
    assert_eq!(
        (all[2].candidate.width, all[2].candidate.height),
        (Some(4032), Some(3024))
    );

    let src = MemSource::new(data.clone());
    let cfg = ScanConfig {
        formats: FormatSet::jpeg_only(),
        ..Default::default()
    };
    let mut sink = VecSink::default();
    scan(
        &src,
        &cfg,
        &mut ScanState::default(),
        &new_cancel_flag(),
        &mut sink,
    )
    .unwrap();
    assert!(sink.found.is_empty());
    assert!(src.len() > 0);
}

#[test]
fn truncated_heif_and_png_are_partial() {
    let h = images::heif_like(100, 100, 50_000, 1);
    let mut data = h[..h.len() - 1000].to_vec();
    data.extend_from_slice(&[0u8; 100]);
    let f = run(&data, 1 << 20);
    assert_eq!(f.len(), 1);
    assert_ne!(f[0].candidate.status, EndStatus::Complete);

    let p = images::png(300, 300, 4);
    let data = p[..p.len() / 2].to_vec();
    let f = run(&data, 1 << 20);
    assert!(f.iter().all(|x| x.candidate.status != EndStatus::Complete));
}
