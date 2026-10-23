//! The fixed set of ADB operations exposed to the rest of the system.

use crate::discovery::{is_candidate, IMAGE_EXTENSIONS};
use crate::parse::{self, ListedDevice, ListedFile};
use crate::runner::CommandRunner;
use crate::validate::{local_relative_path, sh_quote, validate_device_path, validate_partition_name, validate_serial};
use crate::AdbError;
use pr_model::CancelFlag;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SHORT: Duration = Duration::from_secs(20);
const LISTING: Duration = Duration::from_secs(600);
const PULL: Duration = Duration::from_secs(1800);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub serial: String,
    pub state: String,
    pub manufacturer: Option<String>,
    pub brand: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub android_version: Option<String>,
    pub sdk: Option<u32>,
    pub security_patch: Option<String>,
    pub is_realme: bool,
    /// realme UI / ColorOS version properties, if present.
    pub vendor_ui: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceCapabilities {
    pub usb_debugging_authorized: bool,
    /// `ro.crypto.state` (encrypted / unencrypted / unsupported).
    pub crypto_state: Option<String>,
    /// `ro.crypto.type` (file / block / none).
    pub crypto_type: Option<String>,
    pub build_type: Option<String>,
    pub verified_boot_state: Option<String>,
    pub bootloader_locked: Option<bool>,
    /// `su` binary is present on PATH (not executed unless root mode).
    pub su_present: bool,
    pub shared_storage_total_kib: Option<u64>,
    pub shared_storage_free_kib: Option<u64>,
    pub external_volumes: Vec<String>,
    pub scoped_storage: bool,
    /// Human-readable statements about what is and is not possible.
    pub limitations: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct DiscoveryResult {
    pub files: Vec<ListedFile>,
    /// Errors such as "Permission denied" for directories we could not read.
    pub inaccessible: Vec<String>,
    pub roots: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PulledFile {
    pub device_path: String,
    pub local_path: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// Hash computed on the device before transfer, if `sha256sum` exists.
    pub device_sha256: Option<String>,
    /// `Some(true)` if device and local hashes match.
    pub transfer_verified: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PartitionImage {
    pub partition: String,
    pub local_path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub notes: Vec<String>,
}

pub struct AdbClient {
    runner: Arc<dyn CommandRunner>,
}

fn s(v: &str) -> String {
    v.to_string()
}

impl AdbClient {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        AdbClient { runner }
    }

    fn run_ok(&self, args: Vec<String>, timeout: Duration) -> Result<String, AdbError> {
        let out = self.runner.run(&args, timeout)?;
        if out.code != Some(0) {
            return Err(AdbError::Failed {
                code: out.code,
                stderr: out.stderr.trim().to_string(),
            });
        }
        Ok(out.stdout_text())
    }

    /// `adb shell <cmd>` where `cmd` is built only from fixed templates and
    /// quoted, validated arguments.
    fn shell(&self, serial: &str, cmd: String, timeout: Duration) -> Result<crate::runner::CmdOutput, AdbError> {
        validate_serial(serial)?;
        self.runner.run(&[s("-s"), s(serial), s("shell"), cmd], timeout)
    }

    pub fn version(&self) -> Result<String, AdbError> {
        Ok(self.run_ok(vec![s("version")], SHORT)?.lines().next().unwrap_or("").to_string())
    }

    pub fn list_devices(&self) -> Result<Vec<ListedDevice>, AdbError> {
        Ok(parse::parse_devices(&self.run_ok(vec![s("devices"), s("-l")], SHORT)?))
    }

    fn require_ready(&self, serial: &str) -> Result<ListedDevice, AdbError> {
        validate_serial(serial)?;
        let dev = self
            .list_devices()?
            .into_iter()
            .find(|d| d.serial == serial)
            .ok_or_else(|| AdbError::DeviceState(format!("{serial} is not connected")))?;
        match dev.state.as_str() {
            "device" => Ok(dev),
            "unauthorized" => Err(AdbError::DeviceState(
                "USB debugging is not authorized: unlock the phone and accept the 'Allow USB debugging' prompt".into(),
            )),
            other => Err(AdbError::DeviceState(format!("device state is '{other}'"))),
        }
    }

    pub fn props(&self, serial: &str) -> Result<BTreeMap<String, String>, AdbError> {
        let out = self.shell(serial, s("getprop"), SHORT)?;
        Ok(parse::parse_getprop(&out.stdout_text()))
    }

    pub fn device_info(&self, serial: &str) -> Result<DeviceInfo, AdbError> {
        let dev = self.require_ready(serial)?;
        let p = self.props(serial)?;
        let get = |k: &str| p.get(k).filter(|v| !v.is_empty()).cloned();
        let brand = get("ro.product.brand");
        let manufacturer = get("ro.product.manufacturer");
        let is_realme = [&brand, &manufacturer].iter().any(|v| v.as_deref().is_some_and(|x| x.eq_ignore_ascii_case("realme")));
        let vendor_ui = ["ro.build.version.realmeui", "ro.build.version.oplusrom", "ro.build.version.opporom"]
            .iter()
            .find_map(|k| get(k).map(|v| format!("{k}={v}")));
        Ok(DeviceInfo {
            serial: dev.serial,
            state: dev.state,
            manufacturer,
            brand,
            model: get("ro.product.model").or(dev.model),
            device: get("ro.product.device"),
            android_version: get("ro.build.version.release"),
            sdk: get("ro.build.version.sdk").and_then(|v| v.parse().ok()),
            security_patch: get("ro.build.version.security_patch"),
            is_realme,
            vendor_ui,
        })
    }

    pub fn capabilities(&self, serial: &str) -> Result<DeviceCapabilities, AdbError> {
        let info = self.device_info(serial)?;
        let p = self.props(serial)?;
        let get = |k: &str| p.get(k).filter(|v| !v.is_empty()).cloned();
        let su_present = self
            .shell(serial, s("command -v su"), SHORT)
            .map(|o| o.code == Some(0) && !o.stdout_text().trim().is_empty())
            .unwrap_or(false);
        let df = self.shell(serial, s("df -k /sdcard/"), SHORT).ok().and_then(|o| parse::parse_df(&o.stdout_text()));
        let volumes = self
            .shell(serial, s("ls -1 /storage"), SHORT)
            .map(|o| parse::parse_storage_volumes(&o.stdout_text()))
            .unwrap_or_default();
        let sdk = info.sdk.unwrap_or(0);
        let crypto_type = get("ro.crypto.type");
        let mut lim = vec![
            "ADB without root can read shared storage (/sdcard) only; deleted files there cannot be undeleted over ADB.".to_string(),
            "Raw storage blocks (where deleted data might remain) are not readable without root.".to_string(),
        ];
        if crypto_type.as_deref() == Some("file") || sdk >= 29 {
            lim.push("This device uses file-based encryption: deleted file contents in /data are ciphertext; even a raw image cannot yield those photos without keys.".into());
        }
        if sdk >= 30 {
            lim.push("Android 11+ scoped storage: other apps' private folders under Android/data are typically not readable.".into());
        }
        if info.is_realme {
            lim.push("realme/ColorOS gallery 'Recently deleted' locations vary by version; they are searched heuristically and reported only if readable.".into());
        }
        Ok(DeviceCapabilities {
            usb_debugging_authorized: true,
            crypto_state: get("ro.crypto.state"),
            crypto_type,
            build_type: get("ro.build.type"),
            verified_boot_state: get("ro.boot.verifiedbootstate"),
            bootloader_locked: get("ro.boot.flash.locked").map(|v| v == "1"),
            su_present,
            shared_storage_total_kib: df.map(|d| d.0),
            shared_storage_free_kib: df.map(|d| d.1),
            external_volumes: volumes,
            scoped_storage: sdk >= 30,
            limitations: lim,
        })
    }

    /// List image files (and thumbnail containers) under the given roots.
    pub fn discover(&self, serial: &str, roots: &[String]) -> Result<DiscoveryResult, AdbError> {
        self.require_ready(serial)?;
        let mut res = DiscoveryResult { roots: roots.to_vec(), ..Default::default() };
        let mut name_tests: Vec<String> = IMAGE_EXTENSIONS.iter().map(|e| format!("-iname {}", sh_quote(&format!("*.{e}")))).collect();
        name_tests.push(format!("-name {}", sh_quote(".thumbdata*")));
        let expr = name_tests.join(" -o ");
        for root in roots {
            validate_device_path(root)?;
            let cmd = format!("find {} -type f \\( {expr} \\) -exec stat -c '%s:%Y:%n' {{}} +", sh_quote(root));
            let out = self.shell(serial, cmd, LISTING)?;
            let (files, other) = parse::parse_stat_lines(&out.stdout_text());
            res.files.extend(files.into_iter().filter(|f| is_candidate(&f.path) && validate_device_path(&f.path).is_ok()));
            res.inaccessible.extend(other);
            res.inaccessible.extend(out.stderr.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()));
        }
        res.files.sort_by(|a, b| a.path.cmp(&b.path));
        res.files.dedup_by(|a, b| a.path == b.path);
        Ok(res)
    }

    pub fn device_sha256(&self, serial: &str, path: &str) -> Option<String> {
        validate_device_path(path).ok()?;
        let out = self.shell(serial, format!("sha256sum {}", sh_quote(path)), LISTING).ok()?;
        (out.code == Some(0)).then(|| parse::parse_sha256sum(&out.stdout_text())).flatten()
    }

    /// Copy one accessible file to `dest_root/<sanitised device path>`.
    /// The device is only read. Existing local files are never overwritten.
    pub fn pull(&self, serial: &str, device_path: &str, dest_root: &Path) -> Result<PulledFile, AdbError> {
        validate_serial(serial)?;
        validate_device_path(device_path)?;
        let local = dest_root.join(local_relative_path(device_path));
        if local.exists() {
            return Err(AdbError::InvalidArgument(format!("destination exists: {}", local.display())));
        }
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let device_sha256 = self.device_sha256(serial, device_path);
        let tmp = local.with_extension("partial");
        let out = self.runner.run(
            &[s("-s"), s(serial), s("pull"), s("-a"), s(device_path), tmp.to_string_lossy().to_string()],
            PULL,
        )?;
        if out.code != Some(0) || !tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
            return Err(AdbError::Failed { code: out.code, stderr: out.stderr.trim().to_string() });
        }
        std::fs::rename(&tmp, &local)?;
        let (sha256, size) = pr_model::evidence::sha256_file(&local, None, |_| {})
            .map_err(|e| AdbError::Io(std::io::Error::other(e.to_string())))?;
        let transfer_verified = device_sha256.as_ref().map(|d| *d == sha256);
        Ok(PulledFile { device_path: device_path.to_string(), local_path: local, size, sha256, device_sha256, transfer_verified })
    }

    // ----- Root-only, explicitly confirmed, read-only operations -----

    fn require_root_confirmation(confirmation: &str, serial: &str) -> Result<(), AdbError> {
        let expected = format!("READ-ONLY ROOT ACQUISITION {serial}");
        if confirmation != expected {
            return Err(AdbError::ConfirmationRequired(format!("type exactly: {expected}")));
        }
        Ok(())
    }

    /// Check that `su` grants uid 0. Running `su` may show a root-manager
    /// prompt on the device and be logged there; that is why it is gated.
    pub fn root_check(&self, serial: &str, confirmation: &str) -> Result<bool, AdbError> {
        Self::require_root_confirmation(confirmation, serial)?;
        let out = self.shell(serial, s("su -c id"), SHORT)?;
        Ok(out.code == Some(0) && out.stdout_text().contains("uid=0"))
    }

    pub fn list_partitions(&self, serial: &str, confirmation: &str) -> Result<Vec<String>, AdbError> {
        Self::require_root_confirmation(confirmation, serial)?;
        let out = self.shell(serial, s("su -c 'ls -1 /dev/block/by-name/'"), SHORT)?;
        Ok(parse::parse_name_list(&out.stdout_text()))
    }

    /// Stream a block device to a local image with `dd` (read-only on the
    /// device), hashing while writing.
    pub fn acquire_partition(
        &self,
        serial: &str,
        partition: &str,
        dest: &Path,
        confirmation: &str,
        cancel: &CancelFlag,
        progress: &mut dyn FnMut(u64),
    ) -> Result<PartitionImage, AdbError> {
        Self::require_root_confirmation(confirmation, serial)?;
        validate_partition_name(partition)?;
        if !self.list_partitions(serial, confirmation)?.iter().any(|p| p == partition) {
            return Err(AdbError::InvalidArgument(format!("partition {partition} not found on device")));
        }
        if dest.exists() {
            return Err(AdbError::InvalidArgument(format!("destination exists: {}", dest.display())));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        struct HashWriter<W: Write> {
            inner: W,
            h: Sha256,
            n: u64,
        }
        impl<W: Write> Write for HashWriter<W> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                let n = self.inner.write(b)?;
                self.h.update(&b[..n]);
                self.n += n as u64;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.inner.flush()
            }
        }
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
        let mut w = HashWriter { inner: std::io::BufWriter::new(file), h: Sha256::new(), n: 0 };
        let cmd = format!("su -c {}", sh_quote(&format!("dd if=/dev/block/by-name/{partition} bs=4194304 2>/dev/null")));
        let out = self.runner.run_streaming(&[s("-s"), s(serial), s("exec-out"), cmd], &mut w, cancel, progress)?;
        w.flush()?;
        if out.code != Some(0) {
            return Err(AdbError::Failed { code: out.code, stderr: out.stderr });
        }
        let mut notes = vec![
            "Acquired with dd over adb exec-out; the device was only read.".to_string(),
            "If the partition was mounted during acquisition its contents may have changed while reading; the image is not a static snapshot.".to_string(),
        ];
        if partition == "userdata" {
            notes.push("userdata on modern Android is file-based encrypted: user file contents in this image are ciphertext unless the device is unencrypted.".into());
        }
        Ok(PartitionImage {
            partition: partition.to_string(),
            local_path: dest.to_path_buf(),
            size: w.n,
            sha256: hex::encode(w.h.finalize()),
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CmdOutput;
    use std::sync::Mutex;

    /// Canned-response runner that records every argv it receives.
    struct Fake {
        calls: Mutex<Vec<Vec<String>>>,
        respond: Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>,
    }

    impl CommandRunner for Fake {
        fn run(&self, args: &[String], _: Duration) -> Result<CmdOutput, AdbError> {
            self.calls.lock().unwrap().push(args.to_vec());
            Ok((self.respond)(args))
        }
        fn run_streaming(&self, args: &[String], sink: &mut dyn Write, _: &CancelFlag, p: &mut dyn FnMut(u64)) -> Result<CmdOutput, AdbError> {
            self.calls.lock().unwrap().push(args.to_vec());
            sink.write_all(b"BLOCKDATA")?;
            p(9);
            Ok(CmdOutput { code: Some(0), stdout: vec![], stderr: String::new() })
        }
    }

    fn ok(s: &str) -> CmdOutput {
        CmdOutput { code: Some(0), stdout: s.as_bytes().to_vec(), stderr: String::new() }
    }

    fn fake() -> (Arc<Fake>, AdbClient) {
        let f = Arc::new(Fake {
            calls: Mutex::new(vec![]),
            respond: Box::new(|a: &[String]| {
                let last = a.last().unwrap().as_str();
                if a[0] == "devices" {
                    ok("List of devices attached\nABC123 device product:RMX3363 model:RMX3363 transport_id:1\nUNAUTH unauthorized\n")
                } else if last == "getprop" {
                    ok("[ro.product.brand]: [realme]\n[ro.product.model]: [RMX3363]\n[ro.build.version.release]: [13]\n[ro.build.version.sdk]: [33]\n[ro.crypto.type]: [file]\n[ro.build.version.realmeui]: [V4.0]\n")
                } else if last.starts_with("find ") {
                    ok("100:1700000000:/sdcard/DCIM/Camera/IMG_1.jpg\n50:1700000001:/sdcard/DCIM/.thumbnails/.thumbdata4--1\n5:1:/sdcard/x.txt\n")
                } else if last.starts_with("sha256sum") {
                    ok(&format!("{}  x", "0".repeat(64)))
                } else if last == "su -c 'ls -1 /dev/block/by-name/'" {
                    ok("boot\nuserdata\n")
                } else if a.get(2).map(String::as_str) == Some("pull") {
                    std::fs::write(&a[5], b"hello").unwrap();
                    ok("")
                } else {
                    ok("")
                }
            }),
        });
        (f.clone(), AdbClient::new(f))
    }

    #[test]
    fn device_info_detects_realme() {
        let (_, c) = fake();
        let i = c.device_info("ABC123").unwrap();
        assert!(i.is_realme);
        assert_eq!(i.sdk, Some(33));
        assert_eq!(i.vendor_ui.as_deref(), Some("ro.build.version.realmeui=V4.0"));
        let caps = c.capabilities("ABC123").unwrap();
        assert!(caps.scoped_storage);
        assert!(caps.limitations.iter().any(|l| l.contains("file-based encryption")));
        assert!(matches!(c.device_info("UNAUTH"), Err(AdbError::DeviceState(m)) if m.contains("not authorized")));
    }

    #[test]
    fn discover_builds_quoted_find_and_filters() {
        let (f, c) = fake();
        let r = c.discover("ABC123", &["/sdcard/".into()]).unwrap();
        assert_eq!(r.files.len(), 2);
        let calls = f.calls.lock().unwrap();
        let find = calls.iter().find(|a| a.last().unwrap().starts_with("find")).unwrap();
        assert_eq!(&find[..3], &["-s", "ABC123", "shell"]);
        assert!(find[3].starts_with("find '/sdcard/' -type f \\( -iname '*.jpg'"));
        drop(calls);
        assert!(c.discover("ABC123", &["/data/".into()]).is_err());
    }

    #[test]
    fn pull_never_overwrites_and_records_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let (_, c) = fake();
        let p = c.pull("ABC123", "/sdcard/DCIM/Camera/IMG_1.jpg", dir.path()).unwrap();
        assert_eq!(p.size, 5);
        assert_eq!(p.transfer_verified, Some(false)); // fake device hash is zeros
        assert!(p.local_path.starts_with(dir.path()));
        assert!(c.pull("ABC123", "/sdcard/DCIM/Camera/IMG_1.jpg", dir.path()).is_err());
        assert!(c.pull("ABC;reboot", "/sdcard/x.jpg", dir.path()).is_err());
    }

    #[test]
    fn root_operations_require_exact_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let (f, c) = fake();
        let cancel = pr_model::new_cancel_flag();
        let dest = dir.path().join("userdata.img");
        assert!(matches!(
            c.acquire_partition("ABC123", "userdata", &dest, "yes", &cancel, &mut |_| {}),
            Err(AdbError::ConfirmationRequired(_))
        ));
        assert!(f.calls.lock().unwrap().is_empty(), "nothing may run before confirmation");
        let conf = "READ-ONLY ROOT ACQUISITION ABC123";
        assert!(c.acquire_partition("ABC123", "system_x", &dest, conf, &cancel, &mut |_| {}).is_err());
        let img = c.acquire_partition("ABC123", "userdata", &dest, conf, &cancel, &mut |_| {}).unwrap();
        assert_eq!(img.size, 9);
        assert_eq!(std::fs::read(&dest).unwrap(), b"BLOCKDATA");
        assert!(img.notes.iter().any(|n| n.contains("encrypted")));
    }
}
