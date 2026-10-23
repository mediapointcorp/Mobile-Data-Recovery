//! JSON-lines service over stdin/stdout.
//!
//! The desktop UI spawns the worker as a child process and is its only
//! client; there is no socket or port. Each request is one line:
//! `{"id": 1, "method": "job.start", "params": {...}}`. Responses carry the
//! same `id` with `result` or `error`; asynchronous notifications carry
//! `event` instead of `id`. Only the methods matched in [`Server::dispatch`]
//! exist; parameters are strictly typed (`deny_unknown_fields`).

use crate::case::Case;
use crate::finalize;
use crate::jobs::{self, JobParams, JobRecord, JobStatus};
use crate::report;
use crate::Progress;
use anyhow::{anyhow, bail, Result};
use pr_model::model::Classification;
use pr_model::{new_cancel_flag, CancelFlag};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_LINE: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathParam {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdbParam {
    #[serde(default)]
    adb_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SerialParam {
    serial: String,
    #[serde(default)]
    adb_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootParam {
    serial: String,
    confirmation: String,
    #[serde(default)]
    adb_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobStart {
    params: JobParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobId {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultsList {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    classification: Option<Classification>,
}

fn default_limit() -> usize {
    500
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    dest: String,
    classifications: Vec<Classification>,
    #[serde(default)]
    include_duplicates: bool,
}

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn emit(out: &Out, v: &Value) {
    let mut w = out.lock().unwrap_or_else(|p| p.into_inner());
    let _ = serde_json::to_writer(&mut *w, v);
    let _ = w.write_all(b"\n");
    let _ = w.flush();
}

struct EventProgress {
    out: Out,
    last: Mutex<Instant>,
}

impl Progress for EventProgress {
    fn report(&self, job_id: &str, phase: &str, done: u64, total: u64, message: &str) {
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if last.elapsed() < Duration::from_millis(150) && done < total {
            return;
        }
        *last = Instant::now();
        emit(
            &self.out,
            &json!({ "event": "job.progress", "job_id": job_id, "phase": phase, "done": done, "total": total, "message": message }),
        );
    }
}

struct Running {
    cancel: CancelFlag,
    handle: std::thread::JoinHandle<()>,
}

pub struct Server {
    out: Out,
    case: Mutex<Option<Case>>,
    running: Mutex<HashMap<String, Running>>,
}

impl Server {
    pub fn new(out: Box<dyn Write + Send>) -> Arc<Server> {
        Arc::new(Server {
            out: Arc::new(Mutex::new(out)),
            case: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
        })
    }

    /// Serve until stdin closes or `shutdown` is received.
    pub fn serve(self: &Arc<Self>, input: impl BufRead) -> Result<()> {
        emit(
            &self.out,
            &json!({ "event": "ready", "tool": pr_model::TOOL_NAME, "version": pr_model::TOOL_VERSION, "protocol": PROTOCOL_VERSION }),
        );
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if line.len() > MAX_LINE {
                emit(
                    &self.out,
                    &json!({ "id": null, "error": { "code": "invalid_request", "message": "request too large" } }),
                );
                continue;
            }
            let req: Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    emit(
                        &self.out,
                        &json!({ "id": null, "error": { "code": "invalid_request", "message": e.to_string() } }),
                    );
                    continue;
                }
            };
            if req.method == "shutdown" {
                self.cancel_all();
                emit(&self.out, &json!({ "id": req.id, "result": true }));
                break;
            }
            let resp = match self.dispatch(&req.method, req.params) {
                Ok(v) => json!({ "id": req.id, "result": v }),
                Err(e) => {
                    json!({ "id": req.id, "error": { "code": "failed", "message": format!("{e:#}") } })
                }
            };
            emit(&self.out, &resp);
        }
        self.cancel_all();
        Ok(())
    }

    fn cancel_all(&self) {
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        for r in running.values() {
            r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for (_, r) in running.drain() {
            let _ = r.handle.join();
        }
    }

    fn case(&self) -> Result<Case> {
        self.case
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no case is open"))
    }

    fn p<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
        let v = if v.is_null() { json!({}) } else { v };
        Ok(serde_json::from_value(v)?)
    }

    pub fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "tool.version" => Ok(
                json!({ "tool": pr_model::TOOL_NAME, "version": pr_model::TOOL_VERSION, "protocol": PROTOCOL_VERSION }),
            ),
            "case.open" => {
                let p: PathParam = Self::p(params)?;
                {
                    let mut running = self.running.lock().unwrap();
                    running.retain(|_, r| !r.handle.is_finished());
                    if !running.is_empty() {
                        bail!("cannot switch cases while a job is running");
                    }
                }
                let c = Case::open_or_create(&PathBuf::from(p.path))?;
                let v = json!({ "root": c.root.display().to_string(), "info": c.info });
                *self.case.lock().unwrap() = Some(c);
                Ok(v)
            }
            "drives.list" => {
                let root = self.case.lock().unwrap().as_ref().map(|c| c.root.clone());
                let list: Vec<Value> = crate::drives::list()
                    .into_iter()
                    .map(|d| {
                        let holds_case = root
                            .as_ref()
                            .is_some_and(|r| crate::drives::path_on_drive(r, &d.id));
                        let mut v = serde_json::to_value(&d).unwrap_or_default();
                        v["holds_case"] = json!(holds_case);
                        v
                    })
                    .collect();
                Ok(json!({ "elevated": crate::drives::is_elevated(), "drives": list }))
            }
            "adb.status" => {
                let p: AdbParam = Self::p(params)?;
                match jobs::device::client(p.adb_path.as_deref()) {
                    Ok(c) => Ok(json!({ "found": true, "version": c.version().ok() })),
                    Err(e) => Ok(json!({ "found": false, "message": e.to_string() })),
                }
            }
            "adb.devices" => {
                let p: AdbParam = Self::p(params)?;
                Ok(serde_json::to_value(
                    jobs::device::client(p.adb_path.as_deref())?.list_devices()?,
                )?)
            }
            "adb.deviceInfo" => {
                let p: SerialParam = Self::p(params)?;
                let c = jobs::device::client(p.adb_path.as_deref())?;
                let info = c.device_info(&p.serial)?;
                let caps = c.capabilities(&p.serial)?;
                Ok(json!({ "info": info, "capabilities": caps }))
            }
            "adb.listPartitions" => {
                let p: RootParam = Self::p(params)?;
                let c = jobs::device::client(p.adb_path.as_deref())?;
                if !c.root_check(&p.serial, &p.confirmation)? {
                    bail!("root (uid 0) is not available on this device");
                }
                let parts = c.list_partitions(&p.serial, &p.confirmation)?;
                if let Ok(case) = self.case() {
                    case.audit(
                        "root_partitions_listed",
                        json!({ "serial": p.serial, "partitions": parts }),
                    );
                }
                Ok(json!(parts))
            }
            "job.start" => {
                let p: JobStart = Self::p(params)?;
                let case = self.case()?;
                let rec = jobs::create_job(&case, p.params)?;
                self.spawn(case, rec)
            }
            "job.resume" => {
                let p: JobId = Self::p(params)?;
                let case = self.case()?;
                let rec = JobRecord::load(&case, &p.job_id)?;
                if rec.status == JobStatus::Completed {
                    bail!("job already completed");
                }
                self.spawn(case, rec)
            }
            "job.cancel" => {
                let p: JobId = Self::p(params)?;
                let running = self.running.lock().unwrap();
                let r = running
                    .get(&p.job_id)
                    .ok_or_else(|| anyhow!("job is not running"))?;
                r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(json!(true))
            }
            "job.list" => {
                let case = self.case()?;
                let running = self.running.lock().unwrap();
                let jobs: Vec<Value> = case
                    .list_job_ids()
                    .iter()
                    .filter_map(|id| JobRecord::load(&case, id).ok())
                    .map(|j| {
                        let n = j.results(&case).len();
                        let mut v = serde_json::to_value(&j).unwrap_or_default();
                        v["running"] = json!(running.contains_key(&j.id));
                        v["result_count"] = json!(n);
                        v
                    })
                    .collect();
                Ok(json!(jobs))
            }
            "results.finalize" => {
                let case = self.case()?;
                Ok(serde_json::to_value(finalize::finalize(&case)?.1)?)
            }
            "results.list" => {
                let p: ResultsList = Self::p(params)?;
                let case = self.case()?;
                let all = finalize::load_results(&case);
                let filtered: Vec<_> = all
                    .iter()
                    .filter(|r| p.classification.is_none_or(|c| r.classification == c))
                    .collect();
                let items: Vec<_> = filtered
                    .iter()
                    .skip(p.offset)
                    .take(p.limit.min(5000))
                    .collect();
                Ok(json!({ "total": filtered.len(), "offset": p.offset, "items": items }))
            }
            "export.run" => {
                let p: ExportParams = Self::p(params)?;
                let case = self.case()?;
                let classes: BTreeSet<_> = p.classifications.into_iter().collect();
                Ok(serde_json::to_value(finalize::export(
                    &case,
                    &PathBuf::from(p.dest),
                    &classes,
                    p.include_duplicates,
                )?)?)
            }
            "report.generate" => {
                let case = self.case()?;
                let (j, h) = report::write(&case)?;
                Ok(json!({ "json": j, "html": h }))
            }
            other => bail!("unknown method '{other}'"),
        }
    }

    fn spawn(self: &Arc<Self>, case: Case, mut rec: JobRecord) -> Result<Value> {
        let mut running = self.running.lock().unwrap();
        running.retain(|_, r| !r.handle.is_finished());
        if !running.is_empty() {
            bail!("another job is running; cancel it or wait for it to finish");
        }
        let cancel = new_cancel_flag();
        let id = rec.id.clone();
        let out = self.out.clone();
        let flag = cancel.clone();
        let me = Arc::downgrade(self);
        let handle = std::thread::Builder::new().name(format!("job-{id}")).spawn(move || {
            let progress = EventProgress { out: out.clone(), last: Mutex::new(Instant::now() - Duration::from_secs(1)) };
            let status = jobs::run_job(&case, &mut rec, &flag, &progress);
            let (status, error) = match status {
                Ok(s) => (s, rec.error.clone()),
                Err(e) => (JobStatus::Failed, Some(format!("{e:#}"))),
            };
            emit(&out, &json!({ "event": "job.finished", "job_id": rec.id, "status": status, "error": error }));
            if let Some(s) = me.upgrade() {
                if let Ok(mut r) = s.running.try_lock() {
                    r.remove(&rec.id);
                }
            }
        })?;
        running.insert(id.clone(), Running { cancel, handle });
        Ok(json!({ "job_id": id }))
    }
}
