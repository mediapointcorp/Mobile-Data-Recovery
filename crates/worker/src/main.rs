//! `photorecovery-worker` — the engine behind the PhotoRecovery app, and a
//! command-line tool. Run it from an administrator terminal to scan drives.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use pr_model::evidence::EvidenceMode;
use pr_model::model::Classification;
use pr_worker::case::Case;
use pr_worker::jobs::{self, image::ImageScanParams, JobParams, JobRecord, JobStatus};
use pr_worker::recover::{self, Category, ScanRequest, State};
use pr_worker::{drives, finalize, report, server, Progress};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "photorecovery-worker",
    version,
    about = "PhotoRecovery: recover deleted files from Windows drives"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum FileType {
    All,
    Pictures,
    Music,
    Documents,
    Video,
    Compressed,
    Emails,
}

impl From<FileType> for Category {
    fn from(t: FileType) -> Self {
        match t {
            FileType::All => Category::All,
            FileType::Pictures => Category::Pictures,
            FileType::Music => Category::Music,
            FileType::Documents => Category::Documents,
            FileType::Video => Category::Video,
            FileType::Compressed => Category::Compressed,
            FileType::Emails => Category::Emails,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Service for the desktop app (JSON lines on stdin/stdout).
    Serve,
    /// List drives that can be scanned.
    Drives,
    /// Scan drives for deleted files and optionally recover them.
    Scan {
        /// Drive letters, e.g. `--drive D: --drive E:`.
        #[arg(long, required = true)]
        drive: Vec<String>,
        #[arg(long, value_enum, default_value = "pictures")]
        r#type: FileType,
        /// Also search free space for pictures whose records are gone.
        #[arg(long)]
        deep: bool,
        /// Only files that were in this folder, e.g. `D:\Photos`.
        #[arg(long)]
        folder: Option<String>,
        /// Only files that were in the Recycle Bin.
        #[arg(long)]
        recycle_bin: bool,
        /// Recover files in Excellent/Poor state into this folder (on another drive).
        #[arg(long)]
        recover_to: Option<PathBuf>,
        /// Recreate the original folder structure under --recover-to.
        #[arg(long)]
        keep_folders: bool,
    },
    /// Advanced: analyse a raw disk image file with a forensic case folder
    /// (hashes, audit log, report).
    ScanImage {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        working_copy: bool,
        #[arg(long)]
        formats: Option<String>,
    },
    /// Advanced: resume an interrupted image scan.
    Resume {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        job: String,
    },
    /// Advanced: write report.json / report.html for a case.
    Report {
        #[arg(long)]
        case: PathBuf,
    },
    /// Advanced: export a case's results with a manifest.
    Export {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        dest: PathBuf,
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
            String::new()
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

fn run_job(case: &Case, rec: &mut JobRecord) -> Result<()> {
    let status = jobs::run_job(case, rec, &pr_model::new_cancel_flag(), &CliProgress)?;
    eprintln!();
    for l in &rec.limitations {
        eprintln!("note: {l}");
    }
    match status {
        JobStatus::Completed => {
            println!(
                "{}",
                serde_json::to_string_pretty(&finalize::finalize(case)?.1)?
            );
            Ok(())
        }
        JobStatus::Cancelled => Ok(()),
        _ => bail!("job failed: {}", rec.error.clone().unwrap_or_default()),
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve => {
            server::Server::new(Box::new(std::io::stdout())).serve(std::io::stdin().lock())
        }
        Cmd::Drives => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "administrator": drives::is_elevated(), "drives": drives::list() })
                )?
            );
            Ok(())
        }
        Cmd::Scan {
            drive,
            r#type,
            deep,
            folder,
            recycle_bin,
            recover_to,
            keep_folders,
        } => {
            let req = ScanRequest {
                drives: drive,
                category: r#type.into(),
                deep_scan: deep,
                folder,
                recycle_bin_only: recycle_bin,
            };
            let progress = |d: &str, phase: &str, done: u64, total: u64, msg: &str| {
                let pct = if total > 0 {
                    done as f64 * 100.0 / total as f64
                } else {
                    0.0
                };
                eprint!("\r{d} {phase} {pct:5.1}% {msg:<50.50}");
            };
            let (files, summary) =
                recover::scan_drives(&req, &pr_model::new_cancel_flag(), &progress)?;
            eprintln!();
            for f in &files {
                println!(
                    "{:<13} {:>12}  {:<19}  {}",
                    format!("{:?}", f.state),
                    f.size,
                    f.modified.clone().unwrap_or_default(),
                    f.path
                );
            }
            for n in &summary.notes {
                eprintln!("note: {n}");
            }
            eprintln!("{} files found in {:.1} s", files.len(), summary.seconds);
            if let Some(dest) = recover_to {
                let pick: Vec<_> = files
                    .iter()
                    .filter(|f| matches!(f.state, State::Excellent | State::Poor))
                    .collect();
                let open = |d: &str| -> Result<std::sync::Arc<dyn pr_model::source::ReadAt>> {
                    Ok(std::sync::Arc::new(pr_model::device::open_device(
                        &drives::device_path(d)?,
                    )?))
                };
                let s = recover::recover(
                    &pick,
                    &abs(dest)?,
                    keep_folders,
                    &open,
                    &|p, d| drives::path_on_drive(p, d),
                    &pr_model::new_cancel_flag(),
                    &|_, _| {},
                )?;
                println!("{}", serde_json::to_string_pretty(&s)?);
            }
            Ok(())
        }
        Cmd::ScanImage {
            case,
            image,
            working_copy,
            formats,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let mut rec = jobs::create_job(
                &case,
                JobParams::ImageScan(ImageScanParams {
                    image_path: abs(image)?.display().to_string(),
                    evidence_mode: if working_copy {
                        EvidenceMode::WorkingCopy
                    } else {
                        EvidenceMode::ReadOnlyOriginal
                    },
                    formats,
                    filesystem_analysis: true,
                    carve: true,
                    chunk_size_mib: None,
                    carve_scope: Default::default(),
                    include_existing: true,
                }),
            )?;
            run_job(&case, &mut rec)
        }
        Cmd::Resume { case, job } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let mut rec = JobRecord::load(&case, &job)?;
            run_job(&case, &mut rec)
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
            include_duplicates,
        } => {
            let case = Case::open_or_create(&abs(case)?)?;
            let classes: BTreeSet<Classification> = Classification::ALL
                .into_iter()
                .filter(|c| *c != Classification::UnavailableSourceData)
                .collect();
            finalize::finalize(&case)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&finalize::export(
                    &case,
                    &abs(dest)?,
                    &classes,
                    include_duplicates
                )?)?
            );
            Ok(())
        }
    }
}
