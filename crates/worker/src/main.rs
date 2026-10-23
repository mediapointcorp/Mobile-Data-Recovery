//! `photorecovery-worker` — local service for the desktop UI and a CLI for
//! headless use.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use pr_model::evidence::EvidenceMode;
use pr_model::model::Classification;
use pr_worker::case::Case;
use pr_worker::jobs::{
    self, device::DeviceParams, image::ImageScanParams, local::LocalScanParams, JobParams,
    JobRecord, JobStatus,
};
use pr_worker::{finalize, report, server, Progress};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Parser)]
#[command(
    name = "photorecovery-worker",
    version,
    about = "PhotoRecovery engine: carving, filesystem and Android acquisition"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// JSON-lines service on stdin/stdout (used by the desktop app).
    Serve,
    /// Scan a raw evidence image (filesystem analysis + carving).
    ScanImage {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        image: PathBuf,
        /// Analyse a hash-verified working copy instead of the original.
        #[arg(long)]
        working_copy: bool,
        /// Comma-separated: jpeg,png,webp,heic
        #[arg(long)]
        formats: Option<String>,
        #[arg(long)]
        no_fs: bool,
        #[arg(long)]
        no_carve: bool,
        #[arg(long)]
        chunk_mib: Option<u32>,
    },
    /// List PC drives and disks that can be scanned.
    Drives,
    /// Scan a PC drive in place for deleted photos (needs administrator).
    ScanDrive {
        #[arg(long)]
        case: PathBuf,
        /// `D:`, `PhysicalDrive1` (Windows) or `sdb` (Linux).
        #[arg(long)]
        drive: String,
        /// Carve the whole drive instead of only free space.
        #[arg(long)]
        all_space: bool,
        /// Also list photos that still exist.
        #[arg(long)]
        include_existing: bool,
        #[arg(long)]
        formats: Option<String>,
    },
    /// Search an existing local folder (backups, copies) for images.
    ScanDir {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        dir: PathBuf,
    },
    /// List connected Android devices.
    Devices {
        #[arg(long)]
        adb: Option<String>,
    },
    /// Show device identification and storage capabilities.
    DeviceInfo {
        #[arg(long)]
        serial: String,
        #[arg(long)]
        adb: Option<String>,
    },
    /// Copy accessible images from a device (read-only) and classify them.
    AcquireDevice {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        serial: String,
        /// Device roots to search (repeatable). Default: /sdcard/ and SD cards.
        #[arg(long)]
        root: Vec<String>,
        #[arg(long)]
        adb: Option<String>,
    },
    /// Resume an interrupted or cancelled job.
    Resume {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        job: String,
    },
    /// List jobs in a case.
    Jobs {
        #[arg(long)]
        case: PathBuf,
    },
    /// Merge results and write report.json / report.html.
    Report {
        #[arg(long)]
        case: PathBuf,
    },
    /// Export result files to a separate destination with a manifest.
    Export {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        dest: PathBuf,
        /// Classifications (snake_case, repeatable). Default: all with files.
        #[arg(long)]
        class: Vec<String>,
        #[arg(long)]
        include_duplicates: bool,
    },
}

struct CliProgress;

impl Progress for CliProgress {
    fn report(&self, _job: &str, phase: &str, done: u64, total: u64, message: &str) {
        let pct = if total > 0 {
            format!("{:5.1}%", done as f64 * 100.0 / total as f64)
        } else {
            format!("{done}")
        };
        let mut e = std::io::stderr();
        let _ = write!(e, "\r[{phase}] {pct} {message:<70.70}");
        let _ = e.flush();
    }
}

fn abs(p: PathBuf) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p
    } else {
        std::env::current_dir()?.join(p)
    })
}

fn run_new(case: &Case, params: JobParams) -> Result<()> {
    let mut rec = jobs::create_job(case, params)?;
    run_existing(case, &mut rec)
}

fn run_existing(case: &Case, rec: &mut JobRecord) -> Result<()> {
    let cancel = pr_model::new_cancel_flag();
    let c2 = cancel.clone();
    let _ = ctrlc_flag(c2);
    eprintln!("job {}", rec.id);
    let status = jobs::run_job(case, rec, &cancel, &CliProgress)?;
    eprintln!();
    for l in &rec.limitations {
        eprintln!("limitation: {l}");
    }
    match status {
        JobStatus::Completed => {
            let (_, s) = finalize::finalize(case)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
            Ok(())
        }
        JobStatus::Cancelled => {
            eprintln!(
                "cancelled; resume with: photorecovery-worker resume --case {} --job {}",
                case.root.display(),
                rec.id
            );
            Ok(())
        }
        _ => bail!("job failed: {}", rec.error.clone().unwrap_or_default()),
    }
}

/// Cancel on stdin "q\n" (portable; no signal handling dependency).
fn ctrlc_flag(flag: pr_model::CancelFlag) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin()
            .read_line(&mut line)
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            if line.trim() == "q" {
                flag.store(true, Ordering::Relaxed);
                return;
            }
            line.clear();
        }
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve => {
            let srv = server::Server::new(Box::new(std::io::stdout()));
            srv.serve(std::io::stdin().lock())
        }
        Cmd::ScanImage {
            case,
            image,
            working_copy,
            formats,
            no_fs,
            no_carve,
            chunk_mib,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            run_new(
                &case,
                JobParams::ImageScan(ImageScanParams {
                    image_path: abs(image)?.display().to_string(),
                    evidence_mode: if working_copy {
                        EvidenceMode::WorkingCopy
                    } else {
                        EvidenceMode::ReadOnlyOriginal
                    },
                    formats,
                    filesystem_analysis: !no_fs,
                    carve: !no_carve,
                    chunk_size_mib: chunk_mib,
                    carve_scope: Default::default(),
                    include_existing: true,
                }),
            )
        }
        Cmd::Drives => {
            let v = serde_json::json!({ "elevated": pr_worker::drives::is_elevated(), "drives": pr_worker::drives::list() });
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::ScanDrive {
            case,
            drive,
            all_space,
            include_existing,
            formats,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            run_new(
                &case,
                JobParams::DriveScan(pr_worker::jobs::drive::DriveScanParams {
                    drive,
                    formats,
                    carve_scope: if all_space {
                        pr_worker::jobs::image::CarveScope::All
                    } else {
                        pr_worker::jobs::image::CarveScope::Unallocated
                    },
                    include_existing,
                    chunk_size_mib: None,
                }),
            )
        }
        Cmd::ScanDir { case, dir } => {
            let case = Case::open_or_create(&abs(case)?)?;
            run_new(
                &case,
                JobParams::LocalScan(LocalScanParams {
                    dir: abs(dir)?.display().to_string(),
                    max_files: None,
                }),
            )
        }
        Cmd::Devices { adb } => {
            let c = jobs::device::client(adb.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&c.list_devices()?)?);
            Ok(())
        }
        Cmd::DeviceInfo { serial, adb } => {
            let c = jobs::device::client(adb.as_deref())?;
            let v = serde_json::json!({ "info": c.device_info(&serial)?, "capabilities": c.capabilities(&serial)? });
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::AcquireDevice {
            case,
            serial,
            root,
            adb,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            run_new(
                &case,
                JobParams::DeviceAcquire(DeviceParams {
                    serial,
                    roots: (!root.is_empty()).then_some(root),
                    include_external_storage: true,
                    adb_path: adb,
                    max_files: None,
                }),
            )
        }
        Cmd::Resume { case, job } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let mut rec = JobRecord::load(&case, &job)?;
            run_existing(&case, &mut rec)
        }
        Cmd::Jobs { case } => {
            let case = Case::open_or_create(&abs(case)?)?;
            for id in case.list_job_ids() {
                let j = JobRecord::load(&case, &id)?;
                println!(
                    "{}  {:<18} {:?}  runs={}  results={}",
                    j.id,
                    j.params.kind(),
                    j.status,
                    j.runs,
                    j.results(&case).len()
                );
            }
            Ok(())
        }
        Cmd::Report { case } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let (j, h) = report::write(&case)?;
            println!("{j}\n{h}");
            Ok(())
        }
        Cmd::Export {
            case,
            dest,
            class,
            include_duplicates,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let classes: BTreeSet<Classification> = if class.is_empty() {
                Classification::ALL
                    .into_iter()
                    .filter(|c| *c != Classification::UnavailableSourceData)
                    .collect()
            } else {
                class
                    .iter()
                    .map(|c| serde_json::from_value(serde_json::json!(c)))
                    .collect::<Result<_, _>>()?
            };
            finalize::finalize(&case)?;
            let s = finalize::export(&case, &abs(dest)?, &classes, include_duplicates)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
            Ok(())
        }
    }
}
