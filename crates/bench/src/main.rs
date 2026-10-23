//! Ground-truth benchmark harness (spec section 6).
//!
//! `pr-bench run` generates synthetic evidence with a manifest of what was
//! placed where, runs the real pipeline on it and measures:
//!
//! * byte-identical recovery rate (per scenario),
//! * valid-candidate precision and false-positive rate,
//! * partial-image yield for truncated/corrupted/fragmented files,
//! * scan throughput and peak memory (measured in a fresh child process so
//!   fixture generation does not pollute the numbers),
//! * FAT deleted-record recovery by deletion scenario.
//!
//! Accessible-but-failed and inaccessible data are reported separately and
//! never merged into one percentage.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pr_carve::scanner::{scan, Found, ScanConfig, ScanSink, ScanState};
use pr_model::evidence::EvidenceMode;
use pr_model::model::{Classification, RecoveryMethod, RecoveryResult};
use pr_model::source::FileSource;
use pr_testkit::disk::{scrub_signatures, standard_disk, Manifest, Scenario, StandardDiskParams};
use pr_testkit::fat::FatBuilder;
use pr_testkit::images::{self, JpegOpts};
use pr_testkit::{sha256_hex, Rng};
use pr_worker::case::Case;
use pr_worker::jobs::{self, image::ImageScanParams, JobParams};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "pr-bench", about = "PhotoRecovery ground-truth benchmarks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate fixtures, run the pipeline, and write bench.json / bench.md.
    Run {
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Number of intact JPEGs (other scenarios scale with it).
        #[arg(long, default_value_t = 24)]
        jpegs: usize,
        /// Megabytes of non-image filler appended (throughput measurement).
        #[arg(long, default_value_t = 256)]
        pad_mb: u64,
        #[arg(long, default_value_t = 8)]
        chunk_mib: u32,
        /// FAT scenario: files per deletion scenario.
        #[arg(long, default_value_t = 10)]
        fat_files: usize,
        #[arg(long, default_value = "bench-out")]
        out: PathBuf,
    },
    /// Write sample evidence (raw disk + FAT32 SD-card image) with manifests.
    Fixtures {
        #[arg(long, default_value = "fixtures")]
        out: PathBuf,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Internal: carve an image without exporting; print timing and memory.
    CarveOnly {
        #[arg(long)]
        image: PathBuf,
        #[arg(long, default_value_t = 8)]
        chunk_mib: u32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct CarveOnly {
    bytes: u64,
    seconds: f64,
    mb_per_s: f64,
    peak_rss_bytes: Option<u64>,
    candidates: u64,
    rejected_signatures: u64,
    chunk_mib: u32,
}

#[cfg(target_os = "linux")]
fn peak_rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(windows)]
fn peak_rss() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            Some(c.PeakWorkingSetSize as u64)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn peak_rss() -> Option<u64> {
    None
}

struct CountSink(u64);
impl ScanSink for CountSink {
    fn on_candidate(&mut self, _: Found) -> std::io::Result<()> {
        self.0 += 1;
        Ok(())
    }
    fn on_checkpoint(&mut self, _: &ScanState, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

fn carve_only(image: &Path, chunk_mib: u32) -> Result<CarveOnly> {
    let src = FileSource::open(image)?;
    let cfg = ScanConfig {
        chunk_size: (chunk_mib as usize) << 20,
        ..Default::default()
    };
    let mut st = ScanState::default();
    let mut sink = CountSink(0);
    let t = Instant::now();
    scan(&src, &cfg, &mut st, &pr_model::new_cancel_flag(), &mut sink)?;
    let secs = t.elapsed().as_secs_f64();
    let bytes = pr_model::source::ReadAt::len(&src);
    Ok(CarveOnly {
        bytes,
        seconds: secs,
        mb_per_s: bytes as f64 / 1e6 / secs.max(1e-9),
        peak_rss_bytes: peak_rss(),
        candidates: sink.0,
        rejected_signatures: st.stats.rejected,
        chunk_mib,
    })
}

#[derive(Serialize, Default, Debug)]
struct ScenarioStats {
    placed: usize,
    byte_identical: usize,
    partial: usize,
    other_result: usize,
    missed: usize,
}

#[derive(Serialize, Debug)]
struct CarveMetrics {
    by_scenario: BTreeMap<String, ScenarioStats>,
    byte_identical_rate_recoverable: f64,
    carved_image_results: usize,
    carved_image_correct: usize,
    valid_candidate_precision: f64,
    raw_results: usize,
    false_positive_results: usize,
    false_positive_rate: f64,
    false_positive_signatures_placed: usize,
    false_positive_signatures_accepted: usize,
    partial_yield: f64,
    embedded_thumbnails: usize,
    pipeline_seconds: f64,
}

fn carve_metrics(manifest: &Manifest, results: &[RecoveryResult], secs: f64) -> CarveMetrics {
    let originals: HashSet<&str> = manifest
        .entries
        .iter()
        .map(|e| e.original_sha256.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let raw: Vec<&RecoveryResult> = results
        .iter()
        .filter(|r| r.method == RecoveryMethod::RawCarve)
        .collect();
    let truth_offsets: HashSet<u64> = manifest
        .entries
        .iter()
        .filter(|e| e.scenario != Scenario::FalsePositive)
        .map(|e| e.offset)
        .collect();
    let mut by: BTreeMap<String, ScenarioStats> = BTreeMap::new();
    let (mut fp_placed, mut fp_accepted) = (0, 0);
    for e in &manifest.entries {
        let at: Vec<&&RecoveryResult> = raw
            .iter()
            .filter(|r| r.source_offset == Some(e.offset))
            .collect();
        let s = by
            .entry(format!("{:?}", e.scenario).to_lowercase())
            .or_default();
        s.placed += 1;
        if e.scenario == Scenario::FalsePositive {
            fp_placed += 1;
            if !at.is_empty() {
                fp_accepted += 1;
                s.other_result += 1;
            }
            continue;
        }
        if at
            .iter()
            .any(|r| r.sha256.as_deref() == Some(e.original_sha256.as_str()))
        {
            s.byte_identical += 1;
        } else if at
            .iter()
            .any(|r| r.classification == Classification::PartialImage)
        {
            s.partial += 1;
        } else if !at.is_empty() {
            s.other_result += 1;
        } else {
            s.missed += 1;
        }
    }
    let recoverable: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| e.scenario.fully_recoverable())
        .collect();
    let rec_ok = recoverable
        .iter()
        .filter(|e| {
            raw.iter().any(|r| {
                r.source_offset == Some(e.offset)
                    && r.sha256.as_deref() == Some(e.original_sha256.as_str())
            })
        })
        .count();
    let carved: Vec<_> = raw
        .iter()
        .filter(|r| r.classification == Classification::CarvedImage)
        .collect();
    let correct = carved
        .iter()
        .filter(|r| r.sha256.as_deref().is_some_and(|h| originals.contains(h)))
        .count();
    let fps = raw
        .iter()
        .filter(|r| !r.source_offset.is_some_and(|o| truth_offsets.contains(&o)))
        .count();
    let damaged: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.scenario,
                Scenario::Truncated | Scenario::Corrupted | Scenario::Fragmented
            )
        })
        .collect();
    let damaged_yield = damaged
        .iter()
        .filter(|e| {
            raw.iter().any(|r| {
                r.source_offset == Some(e.offset)
                    && r.classification == Classification::PartialImage
            })
        })
        .count();
    let ratio = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 / b as f64 };
    CarveMetrics {
        by_scenario: by,
        byte_identical_rate_recoverable: ratio(rec_ok, recoverable.len()),
        carved_image_results: carved.len(),
        carved_image_correct: correct,
        valid_candidate_precision: ratio(correct, carved.len()),
        raw_results: raw.len(),
        false_positive_results: fps,
        false_positive_rate: ratio(fps, raw.len()),
        false_positive_signatures_placed: fp_placed,
        false_positive_signatures_accepted: fp_accepted,
        partial_yield: ratio(damaged_yield, damaged.len()),
        embedded_thumbnails: results
            .iter()
            .filter(|r| r.method == RecoveryMethod::EmbeddedThumbnail)
            .count(),
        pipeline_seconds: secs,
    }
}

#[derive(Serialize, Debug, Default)]
struct FatRow {
    files: usize,
    byte_identical: usize,
    partial: usize,
    unavailable: usize,
    missed: usize,
}

fn run_job(
    tmp: &Path,
    name: &str,
    image: &Path,
) -> Result<(Vec<RecoveryResult>, Vec<String>, f64)> {
    let case = Case::open_or_create(&tmp.join(name))?;
    let mut rec = jobs::create_job(
        &case,
        JobParams::ImageScan(ImageScanParams {
            image_path: image.display().to_string(),
            evidence_mode: EvidenceMode::ReadOnlyOriginal,
            formats: None,
            filesystem_analysis: true,
            carve: true,
            chunk_size_mib: None,
        }),
    )?;
    let t = Instant::now();
    jobs::run_job(
        &case,
        &mut rec,
        &pr_model::new_cancel_flag(),
        &pr_worker::NoProgress,
    )?;
    let secs = t.elapsed().as_secs_f64();
    let (rs, _) = pr_worker::finalize::finalize(&case)?;
    Ok((rs, rec.limitations.clone(), secs))
}

fn fat_benchmark(tmp: &Path, n: usize, seed: u64) -> Result<BTreeMap<String, FatRow>> {
    let mut b = FatBuilder::new(192, 4);
    b.mkdir("DCIM");
    b.mkdir("DCIM/Camera");
    let mut truth: Vec<(String, String, String)> = Vec::new(); // (scenario, path, sha)
    let mut rng = Rng::new(seed);
    let mut s = seed * 7919;
    let mut next = |rng: &mut Rng| {
        s += 1;
        let w = rng.range(320, 1024) as u32 & !7;
        let h = rng.range(240, 768) as u32 & !7;
        images::jpeg(
            w,
            h,
            s,
            &JpegOpts {
                exif_thumbnail: false,
                ..Default::default()
            },
        )
    };
    let mut overwrite = Vec::new();
    for i in 0..n {
        let live = next(&mut rng);
        let p = format!("DCIM/Camera/IMG_LIVE_{i:04}.jpg");
        b.add_file(&p, &live);
        truth.push(("live".into(), format!("/{p}"), sha256_hex(&live)));

        let del = next(&mut rng);
        let p = format!("DCIM/Camera/IMG_DEL_{i:04}.jpg");
        b.add_file(&p, &del);
        b.delete(&p);
        truth.push((
            "deleted_contiguous".into(),
            format!("/{p}"),
            sha256_hex(&del),
        ));

        let ow = next(&mut rng);
        let p = format!("DCIM/Camera/IMG_OVER_{i:04}.jpg");
        let f = b.add_file(&p, &ow);
        b.delete(&p);
        overwrite.push((f.clusters.clone(), ow.len()));
        truth.push((
            "deleted_overwritten".into(),
            format!("/{p}"),
            sha256_hex(&ow),
        ));

        let fr = next(&mut rng);
        let p = format!("DCIM/Camera/IMG_FRAG_{i:04}.jpg");
        let mut other = rng.bytes(20_000);
        scrub_signatures(&mut other);
        b.add_interleaved(&p, &fr, &format!("DCIM/Camera/other_{i:04}.bin"), &other);
        b.delete(&p);
        truth.push((
            "deleted_fragmented".into(),
            format!("/{p}"),
            sha256_hex(&fr),
        ));
    }
    for (i, (cl, len)) in overwrite.into_iter().enumerate() {
        b.add_file_clusters(&format!("DCIM/Camera/new_{i:04}.bin"), &vec![0x33; len], cl);
    }
    let img = tmp.join("fat32.img");
    std::fs::write(&img, b.finish())?;
    let (rs, _, _) = run_job(tmp, "case-fat", &img)?;
    let mut rows: BTreeMap<String, FatRow> = BTreeMap::new();
    for (scen, path, sha) in truth {
        let row = rows.entry(scen).or_default();
        row.files += 1;
        let fs: Vec<_> = rs
            .iter()
            .filter(|r| {
                matches!(
                    r.method,
                    RecoveryMethod::FilesystemDeletedEntry | RecoveryMethod::FilesystemLiveEntry
                ) && r.source_path.as_deref() == Some(path.as_str())
            })
            .collect();
        if fs.iter().any(|r| {
            r.sha256.as_deref() == Some(sha.as_str())
                && r.classification != Classification::PartialImage
        }) {
            row.byte_identical += 1;
        } else if fs
            .iter()
            .any(|r| r.classification == Classification::PartialImage)
        {
            row.partial += 1;
        } else if fs
            .iter()
            .any(|r| r.classification == Classification::UnavailableSourceData)
        {
            row.unavailable += 1;
        } else {
            row.missed += 1;
        }
    }
    Ok(rows)
}

fn self_exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Fixtures { out, seed } => {
            std::fs::create_dir_all(&out)?;
            let (disk, manifest) = standard_disk(&StandardDiskParams {
                seed,
                ..Default::default()
            });
            std::fs::write(out.join("raw-disk.img"), &disk)?;
            std::fs::write(
                out.join("raw-disk.manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            let mut b = FatBuilder::new(40, 1);
            b.mkdir("DCIM");
            b.mkdir("DCIM/Camera");
            let mut truth = Vec::new();
            for i in 0..6u64 {
                let j = images::jpeg(
                    640,
                    480,
                    seed + i,
                    &JpegOpts {
                        exif_thumbnail: true,
                        exif_datetime: Some(format!("2024:06:0{} 09:00:00", i + 1)),
                        ..Default::default()
                    },
                );
                let p = format!("DCIM/Camera/IMG_2024060{}_090000.jpg", i + 1);
                b.add_file(&p, &j);
                let deleted = i % 2 == 1;
                if deleted {
                    b.delete(&p);
                }
                truth.push(serde_json::json!({ "path": format!("/{p}"), "deleted": deleted, "sha256": sha256_hex(&j) }));
            }
            std::fs::write(out.join("sdcard-fat32.img"), b.finish())?;
            std::fs::write(
                out.join("sdcard-fat32.manifest.json"),
                serde_json::to_vec_pretty(&truth)?,
            )?;
            println!("wrote fixtures to {}", out.display());
            Ok(())
        }
        Cmd::CarveOnly { image, chunk_mib } => {
            println!(
                "{}",
                serde_json::to_string(&carve_only(&image, chunk_mib)?)?
            );
            Ok(())
        }
        Cmd::Run {
            seed,
            jpegs,
            pad_mb,
            chunk_mib,
            fat_files,
            out,
        } => {
            std::fs::create_dir_all(&out)?;
            let tmp = tempfile::tempdir()?;
            let params = StandardDiskParams {
                seed,
                jpegs,
                chunk_size: (chunk_mib as u64) << 20,
                min_dim: 160,
                max_dim: 2048,
            };
            eprintln!("generating ground-truth disk ({jpegs} intact JPEGs + scenarios)...");
            let (disk, manifest) = standard_disk(&params);
            let image = tmp.path().join("disk.img");
            {
                let mut f = std::io::BufWriter::new(std::fs::File::create(&image)?);
                f.write_all(&disk)?;
                let mut rng = Rng::new(seed ^ 0xABCD);
                let mut block = vec![0u8; 1 << 20];
                for _ in 0..pad_mb {
                    rng.fill(&mut block);
                    scrub_signatures(&mut block);
                    f.write_all(&block)?;
                }
            }
            std::fs::write(
                out.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;

            eprintln!("measuring carve throughput and peak memory in a child process...");
            let child = std::process::Command::new(self_exe()?)
                .args([
                    "carve-only",
                    "--image",
                    &image.display().to_string(),
                    "--chunk-mib",
                    &chunk_mib.to_string(),
                ])
                .output()
                .context("running carve-only child")?;
            let carve: CarveOnly =
                serde_json::from_slice(&child.stdout).context("parsing carve-only output")?;

            eprintln!("running full pipeline (export + validation + classification)...");
            let (rs, _lim, secs) = run_job(tmp.path(), "case-disk", &image)?;
            let metrics = carve_metrics(&manifest, &rs, secs);

            eprintln!("running FAT32 deletion-scenario benchmark...");
            let fat = fat_benchmark(tmp.path(), fat_files, seed)?;

            eprintln!("running encrypted-source control...");
            let enc_img = tmp.path().join("encrypted.img");
            std::fs::write(&enc_img, Rng::new(seed).bytes(32 << 20))?;
            let (enc_rs, enc_lim, _) = run_job(tmp.path(), "case-enc", &enc_img)?;

            let doc = serde_json::json!({
                "tool_version": pr_model::TOOL_VERSION,
                "params": { "seed": seed, "jpegs": jpegs, "pad_mb": pad_mb, "chunk_mib": chunk_mib, "fat_files": fat_files },
                "disk_bytes": carve.bytes,
                "carve_only": carve,
                "carve_metrics": metrics,
                "fat_scenarios": fat,
                "encrypted_control": { "results": enc_rs.len(), "limitations": enc_lim },
                "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
            });
            std::fs::write(out.join("bench.json"), serde_json::to_vec_pretty(&doc)?)?;

            let mut md = String::new();
            md.push_str(&format!(
                "# Benchmark results\n\nTool {} · seed {seed} · {} · {}\n\n",
                pr_model::TOOL_VERSION,
                std::env::consts::OS,
                std::env::consts::ARCH
            ));
            md.push_str("## Carving (synthetic raw image)\n\n");
            md.push_str(&format!(
                "| Metric | Value |\n|---|---|\n| Image size | {:.1} MB |\n| Carve-only throughput | {:.0} MB/s ({:.2} s) |\n| Peak memory (carve-only process) | {} |\n| Full pipeline time (export + validate) | {:.2} s |\n| Byte-identical recovery (recoverable scenarios) | {} |\n| Valid-candidate precision (carved images matching an original) | {} ({}/{}) |\n| False-positive rate (raw results not at a placed file) | {} ({}/{}) |\n| Fake signatures accepted | {}/{} |\n| Partial-image yield (truncated/corrupted/fragmented) | {} |\n| Embedded EXIF thumbnails recovered | {} |\n\n",
                carve.bytes as f64 / 1e6,
                carve.mb_per_s,
                carve.seconds,
                carve.peak_rss_bytes.map(|b| format!("{:.1} MiB", b as f64 / 1048576.0)).unwrap_or_else(|| "n/a".into()),
                metrics.pipeline_seconds,
                pct(metrics.byte_identical_rate_recoverable),
                pct(metrics.valid_candidate_precision),
                metrics.carved_image_correct,
                metrics.carved_image_results,
                pct(metrics.false_positive_rate),
                metrics.false_positive_results,
                metrics.raw_results,
                metrics.false_positive_signatures_accepted,
                metrics.false_positive_signatures_placed,
                pct(metrics.partial_yield),
                metrics.embedded_thumbnails,
            ));
            md.push_str("### By scenario\n\n| Scenario | Placed | Byte-identical | Partial | Other result | Missed |\n|---|---|---|---|---|---|\n");
            for (k, v) in &metrics.by_scenario {
                md.push_str(&format!(
                    "| {k} | {} | {} | {} | {} | {} |\n",
                    v.placed, v.byte_identical, v.partial, v.other_result, v.missed
                ));
            }
            md.push_str("\n## FAT32 deleted-record recovery\n\n| Scenario | Files | Byte-identical | Partial | Reported unavailable | Missed |\n|---|---|---|---|---|---|\n");
            for (k, v) in &fat {
                md.push_str(&format!(
                    "| {k} | {} | {} | {} | {} | {} |\n",
                    v.files, v.byte_identical, v.partial, v.unavailable, v.missed
                ));
            }
            md.push_str(&format!(
                "\n## Encrypted-source control\n\n32 MB of random data (stand-in for ciphertext): {} results; limitation reported: {}.\n",
                enc_rs.len(),
                enc_lim.iter().any(|l| l.contains("encrypted"))
            ));
            std::fs::write(out.join("bench.md"), &md)?;
            println!("{md}");
            Ok(())
        }
    }
}
