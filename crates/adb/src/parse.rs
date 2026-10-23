//! Parsers for adb / toybox output.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ListedDevice {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, `recovery`, `sideload`, ...
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub transport_id: Option<String>,
}

/// Parse `adb devices -l`.
pub fn parse_devices(out: &str) -> Vec<ListedDevice> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let mut d = ListedDevice { serial, state, model: None, product: None, transport_id: None };
            for kv in parts {
                if let Some((k, v)) = kv.split_once(':') {
                    match k {
                        "model" => d.model = Some(v.to_string()),
                        "product" => d.product = Some(v.to_string()),
                        "transport_id" => d.transport_id = Some(v.to_string()),
                        _ => {}
                    }
                }
            }
            Some(d)
        })
        .collect()
}

/// Parse `getprop` output: `[key]: [value]` (values may span lines).
pub fn parse_getprop(out: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for l in out.lines() {
        let l = l.trim();
        if let Some(rest) = l.strip_prefix('[') {
            if let Some((k, v)) = rest.split_once("]: [") {
                m.insert(k.to_string(), v.trim_end_matches(']').to_string());
            }
        }
    }
    m
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ListedFile {
    pub path: String,
    pub size: u64,
    /// Modification time, seconds since epoch.
    pub mtime: i64,
}

/// Parse lines produced by `stat -c '%s:%Y:%n'`.
/// Returns files plus lines that could not be parsed (e.g. errors).
pub fn parse_stat_lines(out: &str) -> (Vec<ListedFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut other = Vec::new();
    for l in out.lines() {
        let mut it = l.splitn(3, ':');
        let parsed = (|| {
            let size = it.next()?.trim().parse::<u64>().ok()?;
            let mtime = it.next()?.trim().parse::<i64>().ok()?;
            let path = it.next()?;
            path.starts_with('/').then(|| ListedFile { path: path.to_string(), size, mtime })
        })();
        match parsed {
            Some(f) => files.push(f),
            None if !l.trim().is_empty() => other.push(l.trim().to_string()),
            None => {}
        }
    }
    (files, other)
}

/// Parse `sha256sum` output (`<hex>  <path>`).
pub fn parse_sha256sum(out: &str) -> Option<String> {
    let h = out.split_whitespace().next()?;
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
}

/// Parse `df -k <path>` output: (total KiB, available KiB).
pub fn parse_df(out: &str) -> Option<(u64, u64)> {
    let line = out.lines().filter(|l| !l.starts_with("Filesystem")).find(|l| !l.trim().is_empty())?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    Some((cols[1].parse().ok()?, cols[3].parse().ok()?))
}

/// Parse `ls -1 /dev/block/by-name` style output into names.
pub fn parse_name_list(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && crate::validate::validate_partition_name(l).is_ok())
        .map(String::from)
        .collect()
}

/// External SD-card volume names under /storage (e.g. `1A2B-3C4D`).
pub fn parse_storage_volumes(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|n| {
            n.len() == 9
                && n.as_bytes()[4] == b'-'
                && n.chars().enumerate().all(|(i, c)| i == 4 || c.is_ascii_hexdigit())
        })
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices() {
        let out = "List of devices attached\n\
            R5CT1234ABC    device usb:1-1 product:RMX3363 model:RMX3363 device:RE54E2L1 transport_id:3\n\
            emulator-5554  unauthorized transport_id:1\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model.as_deref(), Some("RMX3363"));
        assert_eq!(d[1].state, "unauthorized");
    }

    #[test]
    fn getprop() {
        let m = parse_getprop("[ro.product.brand]: [realme]\n[ro.build.version.release]: [13]\n[empty]: []\n");
        assert_eq!(m["ro.product.brand"], "realme");
        assert_eq!(m["empty"], "");
    }

    #[test]
    fn stat_lines() {
        let (f, other) = parse_stat_lines("1234:1700000000:/sdcard/DCIM/Camera/IMG:1.jpg\nfind: /sdcard/Android/data: Permission denied\n");
        assert_eq!(f, vec![ListedFile { path: "/sdcard/DCIM/Camera/IMG:1.jpg".into(), size: 1234, mtime: 1_700_000_000 }]);
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn misc() {
        assert_eq!(parse_sha256sum(&format!("{}  /sdcard/x", "A".repeat(64))), Some("a".repeat(64)));
        assert_eq!(parse_sha256sum("sha256sum: not found"), None);
        assert_eq!(parse_df("Filesystem 1K-blocks Used Available Use% Mounted on\n/dev/fuse 115000000 60000000 55000000 53% /storage/emulated\n"), Some((115_000_000, 55_000_000)));
        assert_eq!(parse_storage_volumes("emulated\nself\n1A2B-3C4D\n"), vec!["1A2B-3C4D"]);
    }
}
