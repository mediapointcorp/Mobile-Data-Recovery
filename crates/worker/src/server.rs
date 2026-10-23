//! JSON-lines service over stdin/stdout, used by the desktop app.
//!
//! The app spawns the worker as a child process and is its only client;
//! there is no socket or port. Each request is one line:
//! `{"id": 1, "method": "scan.start", "params": {...}}`. Responses carry
//! the same `id` with `result` or `error`; notifications carry `event`.
//! Only the methods in [`Server::dispatch`] exist, and parameters are
//! strictly typed (`deny_unknown_fields`).

use crate::drives;
use crate::recover::{self, FoundFile, ScanRequest, ScanSummary, State};
use anyhow::{anyhow, bail, Result};
use pr_model::source::ReadAt;
use pr_model::{new_cancel_flag, CancelFlag};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 2;
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
struct ResultsQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    /// `name`, `path`, `size`, `modified`, `state`.
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    descending: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Option<Vec<State>>,
}

fn default_limit() -> usize {
    1000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParam {
    id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverParams {
    ids: Vec<u64>,
    dest: String,
    #[serde(default)]
    keep_folders: bool,
}

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn emit(out: &Out, v: &Value) {
    let mut w = out.lock().unwrap_or_else(|p| p.into_inner());
    let _ = serde_json::to_writer(&mut *w, v);
    let _ = w.write_all(b"\n");
    let _ = w.flush();
}

#[derive(Default)]
struct Session {
    files: Vec<FoundFile>,
    summary: Option<ScanSummary>,
}

struct Running {
    cancel: CancelFlag,
    handle: std::thread::JoinHandle<()>,
}

pub struct Server {
    out: Out,
    session: Arc<Mutex<Session>>,
    running: Mutex<Option<Running>>,
    /// Drives listed by `drives.list` — the only drives a scan may open.
    listed: Mutex<HashSet<String>>,
}

impl Server {
    pub fn new(out: Box<dyn Write + Send>) -> Arc<Server> {
        Arc::new(Server {
            out: Arc::new(Mutex::new(out)),
            session: Arc::new(Mutex::new(Session::default())),
            running: Mutex::new(None),
            listed: Mutex::new(HashSet::new()),
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
                self.stop_scan();
                emit(&self.out, &json!({ "id": req.id, "result": true }));
                return Ok(());
            }
            let resp = match self.dispatch(&req.method, req.params) {
                Ok(v) => json!({ "id": req.id, "result": v }),
                Err(e) => {
                    json!({ "id": req.id, "error": { "code": "failed", "message": format!("{e:#}") } })
                }
            };
            emit(&self.out, &resp);
        }
        self.stop_scan();
        Ok(())
    }

    fn stop_scan(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = r.handle.join();
        }
    }

    fn p<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
        Ok(serde_json::from_value(if v.is_null() {
            json!({})
        } else {
            v
        })?)
    }

    fn open_drive(id: &str) -> Result<Arc<dyn ReadAt>> {
        let path = drives::device_path(id)?;
        Ok(Arc::new(
            pr_model::device::open_device(&path).map_err(|e| anyhow!("cannot open {id}: {e}"))?,
        ))
    }

    pub fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "tool.version" => Ok(
                json!({ "tool": pr_model::TOOL_NAME, "version": pr_model::TOOL_VERSION, "protocol": PROTOCOL_VERSION }),
            ),
            "drives.list" => {
                let list = drives::list();
                *self.listed.lock().unwrap() = list.iter().map(|d| d.id.clone()).collect();
                Ok(json!({ "elevated": drives::is_elevated(), "drives": list }))
            }
            "scan.start" => {
                let req: ScanRequest = Self::p(params)?;
                {
                    let listed = self.listed.lock().unwrap();
                    if let Some(d) = req.drives.iter().find(|d| !listed.contains(*d)) {
                        bail!("unknown drive {d}");
                    }
                }
                let mut running = self.running.lock().unwrap();
                if running.as_ref().is_some_and(|r| !r.handle.is_finished()) {
                    bail!("a scan is already running");
                }
                *self.session.lock().unwrap() = Session::default();
                let cancel = new_cancel_flag();
                let (out, session, flag) = (self.out.clone(), self.session.clone(), cancel.clone());
                let handle = std::thread::Builder::new().name("scan".into()).spawn(move || {
                    let last = Mutex::new(Instant::now() - Duration::from_secs(1));
                    let progress = |drive: &str, phase: &str, done: u64, total: u64, msg: &str| {
                        let mut l = last.lock().unwrap();
                        if l.elapsed() >= Duration::from_millis(200) || done >= total {
                            *l = Instant::now();
                            emit(&out, &json!({ "event": "scan.progress", "drive": drive, "phase": phase, "done": done, "total": total, "message": msg }));
                        }
                    };
                    let result = recover::scan_drives(&req, &flag, &progress);
                    let cancelled = pr_model::is_cancelled(&flag);
                    match result {
                        Ok((files, summary)) => {
                            let ev = json!({ "event": "scan.finished", "cancelled": cancelled, "summary": summary });
                            *session.lock().unwrap() = Session { files, summary: Some(summary) };
                            emit(&out, &ev);
                        }
                        Err(e) => emit(&out, &json!({ "event": "scan.finished", "error": format!("{e:#}") })),
                    }
                })?;
                *running = Some(Running { cancel, handle });
                Ok(json!(true))
            }
            "scan.cancel" => {
                if let Some(r) = self.running.lock().unwrap().as_ref() {
                    r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(json!(true))
            }
            "scan.results" => {
                let q: ResultsQuery = Self::p(params)?;
                let s = self.session.lock().unwrap();
                let text = q
                    .text
                    .as_deref()
                    .map(str::to_lowercase)
                    .filter(|t| !t.is_empty());
                let mut v: Vec<&FoundFile> = s
                    .files
                    .iter()
                    .filter(|f| q.states.as_ref().is_none_or(|st| st.contains(&f.state)))
                    .filter(|f| {
                        text.as_ref()
                            .is_none_or(|t| f.path.to_lowercase().contains(t))
                    })
                    .collect();
                match q.sort.as_deref().unwrap_or("state") {
                    "name" => v.sort_by_key(|f| f.name.to_lowercase()),
                    "path" => v.sort_by_key(|f| f.path.to_lowercase()),
                    "size" => v.sort_by_key(|f| f.size),
                    "modified" => v.sort_by(|a, b| a.modified.cmp(&b.modified)),
                    _ => v.sort_by_key(|f| (f.state, f.path.to_lowercase())),
                }
                if q.descending {
                    v.reverse();
                }
                let items: Vec<&FoundFile> = v
                    .iter()
                    .skip(q.offset)
                    .take(q.limit.min(5000))
                    .copied()
                    .collect();
                Ok(
                    json!({ "total": v.len(), "all": s.files.len(), "items": items, "summary": s.summary }),
                )
            }
            "scan.preview" => {
                let p: IdParam = Self::p(params)?;
                let f = self
                    .session
                    .lock()
                    .unwrap()
                    .files
                    .iter()
                    .find(|f| f.id == p.id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown file"))?;
                let src = Self::open_drive(&f.drive)?;
                let jpeg = recover::preview(&f, src.as_ref())?;
                Ok(json!({ "jpeg_base64": base64(&jpeg) }))
            }
            "scan.recover" => {
                let p: RecoverParams = Self::p(params)?;
                let s = self.session.lock().unwrap();
                let wanted: HashSet<u64> = p.ids.into_iter().collect();
                let files: Vec<&FoundFile> =
                    s.files.iter().filter(|f| wanted.contains(&f.id)).collect();
                if files.is_empty() {
                    bail!("select the files to recover");
                }
                let out = self.out.clone();
                let open = |d: &str| Self::open_drive(d);
                let summary = recover::recover(
                    &files,
                    &PathBuf::from(&p.dest),
                    p.keep_folders,
                    &open,
                    &|dest, drive| drives::path_on_drive(dest, drive),
                    &new_cancel_flag(),
                    &|done, total| {
                        emit(
                            &out,
                            &json!({ "event": "recover.progress", "done": done, "total": total }),
                        )
                    },
                )?;
                Ok(serde_json::to_value(summary)?)
            }
            other => bail!("unknown method '{other}'"),
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_encoding() {
        assert_eq!(super::base64(b"Man"), "TWFu");
        assert_eq!(super::base64(b"Ma"), "TWE=");
        assert_eq!(super::base64(b"M"), "TQ==");
    }
}
