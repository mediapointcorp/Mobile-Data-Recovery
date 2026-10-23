//! JSON-lines service contract.

use pr_worker::server::Server;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

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

#[test]
fn rejects_unknown_methods_fields_drives_and_bad_json() {
    let buf = Buf::default();
    let srv = Server::new(Box::new(buf.clone()));
    let input = "not json\n\
        {\"id\":1,\"method\":\"shell.exec\",\"params\":{\"cmd\":\"format c:\"}}\n\
        {\"id\":2,\"method\":\"scan.results\",\"params\":{\"evil\":1}}\n\
        {\"id\":3,\"method\":\"scan.start\",\"params\":{\"drives\":[\"C:\"],\"category\":\"pictures\"}}\n\
        {\"id\":4,\"method\":\"scan.results\"}\n\
        {\"id\":5,\"method\":\"tool.version\"}\n{\"id\":6,\"method\":\"shutdown\"}\n";
    srv.serve(input.as_bytes()).unwrap();
    let out: Vec<Value> = String::from_utf8_lossy(&buf.0.lock().unwrap())
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
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
    // Drives must come from drives.list.
    assert!(out[4]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown drive"));
    assert_eq!(out[5]["result"]["total"], 0);
    assert_eq!(out[6]["result"]["protocol"], 2);
    assert_eq!(out[7]["result"], true);
}

#[test]
fn recover_requires_a_selection() {
    let srv = Server::new(Box::new(Buf::default()));
    let e = srv
        .dispatch("scan.recover", json!({ "ids": [1], "dest": "/tmp/x" }))
        .unwrap_err();
    assert!(e.to_string().contains("select"));
    assert!(srv.dispatch("scan.preview", json!({ "id": 9 })).is_err());
}
