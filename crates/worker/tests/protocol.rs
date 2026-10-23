//! JSON-lines service contract.

use pr_testkit::images::{self, JpegOpts};
use pr_worker::server::Server;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn lines(&self) -> Vec<Value> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

#[test]
fn rejects_unknown_methods_fields_and_bad_json() {
    let buf = Buf::default();
    let srv = Server::new(Box::new(buf.clone()));
    let input =
        "not json\n{\"id\":1,\"method\":\"shell.exec\",\"params\":{\"cmd\":\"rm -rf /\"}}\n\
                 {\"id\":2,\"method\":\"case.open\",\"params\":{\"path\":\"/tmp/x\",\"evil\":1}}\n\
                 {\"id\":3,\"method\":\"results.list\"}\n\
                 {\"id\":4,\"method\":\"tool.version\"}\n{\"id\":5,\"method\":\"shutdown\"}\n";
    srv.serve(input.as_bytes()).unwrap();
    let out = buf.lines();
    assert_eq!(out[0]["event"], "ready");
    assert_eq!(out[1]["error"]["code"], "invalid_request");
    assert!(out[2]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown method"));
    assert!(out[3]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown field"));
    assert!(out[4]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no case is open"));
    assert_eq!(out[5]["result"]["protocol"], 1);
    assert_eq!(out[6]["result"], true);
}

#[test]
fn job_lifecycle_over_protocol() {
    let tmp = tempfile::tempdir().unwrap();
    let mut disk = vec![0x11u8; 5000];
    disk.extend_from_slice(&images::jpeg(400, 300, 1, &JpegOpts::default()));
    disk.extend_from_slice(&[0x22u8; 5000]);
    let img = tmp.path().join("disk.img");
    std::fs::write(&img, &disk).unwrap();

    let buf = Buf::default();
    let srv = Server::new(Box::new(buf.clone()));
    let case = tmp.path().join("case");
    srv.dispatch("case.open", json!({ "path": case })).unwrap();
    // Relative paths are refused.
    assert!(srv
        .dispatch("case.open", json!({ "path": "relative/case" }))
        .is_err());
    let r = srv
        .dispatch(
            "job.start",
            json!({ "params": { "kind": "image_scan", "image_path": img } }),
        )
        .unwrap();
    let job_id = r["job_id"].as_str().unwrap().to_string();
    let start = Instant::now();
    let finished = loop {
        if let Some(ev) = buf
            .lines()
            .into_iter()
            .find(|v| v["event"] == "job.finished")
        {
            break ev;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "job did not finish"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(finished["job_id"], job_id.as_str());
    assert_eq!(finished["status"], "completed");
    assert!(buf.lines().iter().any(|v| v["event"] == "job.progress"));
    let s = srv.dispatch("results.finalize", Value::Null).unwrap();
    assert_eq!(s["summary"]["total"], 1);
    let list = srv
        .dispatch("results.list", json!({ "classification": "carved_image" }))
        .unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["source_offset"], 5000);
    let jobs = srv.dispatch("job.list", Value::Null).unwrap();
    assert_eq!(jobs[0]["status"], "completed");
    assert!(srv
        .dispatch("job.resume", json!({ "job_id": job_id }))
        .is_err());
    assert!(srv
        .dispatch("job.resume", json!({ "job_id": "../../etc" }))
        .is_err());
    let rep = srv.dispatch("report.generate", Value::Null).unwrap();
    assert!(std::path::Path::new(rep["html"].as_str().unwrap()).exists());
}
