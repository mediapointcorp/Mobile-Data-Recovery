//! PC drives: listing, identifier validation, and the "never write to the
//! drive being scanned" rule.
//!
//! Writing anything to the drive you are recovering from can overwrite the
//! very data you want back, so a drive scan refuses to run when the case
//! folder lives on that drive.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DriveInfo {
    /// Identifier accepted by drive scans: `C:` / `PhysicalDrive1` on
    /// Windows, `sdb` / `nvme0n1` on Linux.
    pub id: String,
    pub device_path: String,
    /// `volume` (a drive letter / partition) or `disk` (whole physical disk).
    pub kind: String,
    pub label: Option<String>,
    pub filesystem: Option<String>,
    pub size: Option<u64>,
    pub free: Option<u64>,
    /// `fixed`, `removable`, `network`, `optical`, `unknown`.
    pub drive_type: String,
    /// `Some(true)` = solid-state (no seek penalty).
    pub ssd: Option<bool>,
    pub trim: Option<bool>,
    /// Holds the running operating system.
    pub is_system: bool,
    pub notes: Vec<String>,
}

impl DriveInfo {
    fn add_notes(&mut self) {
        if self.ssd == Some(true) {
            self.notes.push(if self.trim != Some(false) {
                "SSD with TRIM: Windows tells the SSD which blocks are free, and the SSD usually erases deleted data within seconds to minutes. Recovery chances are low; deleted files typically read back as zeros.".into()
            } else {
                "SSD without TRIM reported: deleted data may still be present.".into()
            });
        }
        if self.is_system {
            self.notes.push("This is the Windows system drive: it is written to constantly, which overwrites deleted data. Scan it as soon as possible and save results to a different drive.".into());
        }
        if self.drive_type == "network" || self.drive_type == "optical" {
            self.notes
                .push("Network and optical drives cannot be scanned at the block level.".into());
        }
    }
}

/// Validate a drive identifier and return its raw device path.
pub fn device_path(id: &str) -> Result<String> {
    if cfg!(windows) {
        let b = id.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return Ok(format!(r"\\.\{}:", (b[0] as char).to_ascii_uppercase()));
        }
        if let Some(n) = id.strip_prefix("PhysicalDrive") {
            if !n.is_empty() && n.len() <= 2 && n.chars().all(|c| c.is_ascii_digit()) {
                return Ok(format!(r"\\.\PhysicalDrive{n}"));
            }
        }
    } else if !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(format!("/dev/{id}"));
    }
    bail!("invalid drive identifier {id:?}")
}

#[cfg(windows)]
mod win {
    use super::DriveInfo;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, StorageDeviceTrimProperty,
        DEVICE_SEEK_PENALTY_DESCRIPTOR, DEVICE_TRIM_DESCRIPTOR, DISK_EXTENT,
        GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_PROPERTY_QUERY, VOLUME_DISK_EXTENTS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// CTL_CODE(IOCTL_VOLUME_BASE = 0x56, 0, METHOD_BUFFERED, FILE_ANY_ACCESS).
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(b: &[u16]) -> String {
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..n])
    }

    /// Open a device for IOCTLs only (no read access; works without admin).
    fn open_query(path: &str) -> Option<std::fs::File> {
        std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .ok()
    }

    fn ioctl<T>(
        f: &std::fs::File,
        code: u32,
        input: Option<&STORAGE_PROPERTY_QUERY>,
        out: &mut T,
        out_size: usize,
    ) -> bool {
        let mut ret = 0u32;
        let (ip, il) = match input {
            Some(q) => (
                q as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            ),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: valid handle; buffers live for the call and sizes match.
        unsafe {
            DeviceIoControl(
                f.as_raw_handle() as _,
                code,
                ip,
                il,
                out as *mut T as *mut _,
                out_size as u32,
                &mut ret,
                std::ptr::null_mut(),
            ) != 0
        }
    }

    fn storage_flags(f: &std::fs::File) -> (Option<bool>, Option<bool>) {
        let mut q: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
        q.QueryType = PropertyStandardQuery;
        q.PropertyId = StorageDeviceSeekPenaltyProperty;
        let mut sp: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let ssd = ioctl(
            f,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q),
            &mut sp,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>(),
        )
        .then_some(!sp.IncursSeekPenalty);
        q.PropertyId = StorageDeviceTrimProperty;
        let mut tr: DEVICE_TRIM_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let trim = ioctl(
            f,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q),
            &mut tr,
            std::mem::size_of::<DEVICE_TRIM_DESCRIPTOR>(),
        )
        .then_some(tr.TrimEnabled);
        (ssd, trim)
    }

    /// Physical disk numbers a volume (e.g. `\\.\C:`) lives on.
    pub fn volume_disks(volume_device: &str) -> Vec<u32> {
        let Some(f) = open_query(volume_device) else {
            return vec![];
        };
        #[repr(C)]
        struct Buf {
            head: VOLUME_DISK_EXTENTS,
            more: [DISK_EXTENT; 31],
        }
        let mut b: Buf = unsafe { std::mem::zeroed() };
        if !ioctl(
            &f,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            &mut b,
            std::mem::size_of::<Buf>(),
        ) {
            return vec![];
        }
        let n = (b.head.NumberOfDiskExtents as usize).min(32);
        // SAFETY: the extents array is laid out contiguously in `Buf`.
        let ext = unsafe { std::slice::from_raw_parts(b.head.Extents.as_ptr(), n) };
        ext.iter().map(|e| e.DiskNumber).collect()
    }

    pub fn list() -> Vec<DriveInfo> {
        let mut out = Vec::new();
        let sys = std::env::var("SystemDrive")
            .unwrap_or_else(|_| "C:".into())
            .to_ascii_uppercase();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = wide(&format!("{letter}:\\"));
            let dtype = unsafe { GetDriveTypeW(root.as_ptr()) };
            let drive_type = match dtype {
                2 => "removable",
                3 => "fixed",
                4 => "network",
                5 => "optical",
                _ => "unknown",
            };
            let mut name = [0u16; 261];
            let mut fs = [0u16; 261];
            let (mut serial, mut maxc, mut flags) = (0u32, 0u32, 0u32);
            let have_vol = unsafe {
                GetVolumeInformationW(
                    root.as_ptr(),
                    name.as_mut_ptr(),
                    261,
                    &mut serial,
                    &mut maxc,
                    &mut flags,
                    fs.as_mut_ptr(),
                    261,
                )
            } != 0;
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            let have_space =
                unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) }
                    != 0;
            let dev = format!(r"\\.\{letter}:");
            let (ssd, trim) = open_query(&dev)
                .map(|f| storage_flags(&f))
                .unwrap_or((None, None));
            let mut d = DriveInfo {
                id: format!("{letter}:"),
                device_path: dev,
                kind: "volume".into(),
                label: have_vol.then(|| from_wide(&name)).filter(|s| !s.is_empty()),
                filesystem: have_vol.then(|| from_wide(&fs)).filter(|s| !s.is_empty()),
                size: have_space.then_some(total),
                free: have_space.then_some(free),
                drive_type: drive_type.into(),
                ssd,
                trim,
                is_system: format!("{letter}:") == sys,
                notes: vec![],
            };
            if d.filesystem.is_none() && drive_type != "optical" {
                d.notes.push("Windows does not recognise a filesystem here (RAW / damaged / unformatted) — a block-level scan can still carve photos.".into());
            }
            d.add_notes();
            out.push(d);
        }
        let sys_disks = volume_disks(&format!(r"\\.\{sys}"));
        for n in 0..32u32 {
            let path = format!(r"\\.\PhysicalDrive{n}");
            let Some(f) = open_query(&path) else { continue };
            let mut len = GET_LENGTH_INFORMATION { Length: 0 };
            let size = ioctl(
                &f,
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                &mut len,
                std::mem::size_of::<GET_LENGTH_INFORMATION>(),
            )
            .then_some(len.Length as u64);
            let (ssd, trim) = storage_flags(&f);
            let mut d = DriveInfo {
                id: format!("PhysicalDrive{n}"),
                device_path: path,
                kind: "disk".into(),
                label: None,
                filesystem: None,
                size,
                free: None,
                drive_type: "disk".into(),
                ssd,
                trim,
                is_system: sys_disks.contains(&n),
                notes: vec!["Whole disk: scans all partitions and unpartitioned space (use this for deleted or damaged partitions).".into()],
            };
            d.add_notes();
            out.push(d);
        }
        out
    }

    pub fn is_elevated() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut e as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && e.TokenIsElevated != 0
        }
    }
}

#[cfg(not(windows))]
mod unix {
    use super::DriveInfo;

    pub fn list() -> Vec<DriveInfo> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir("/sys/block") else {
            return out;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
                continue;
            }
            let base = e.path();
            let read = |f: &str| {
                std::fs::read_to_string(base.join(f))
                    .ok()
                    .map(|s| s.trim().to_string())
            };
            let size = read("size")
                .and_then(|s| s.parse::<u64>().ok())
                .map(|s| s * 512);
            let rotational = read("queue/rotational").map(|s| s == "1");
            let removable = read("removable").is_some_and(|s| s == "1");
            let mut d = DriveInfo {
                id: name.clone(),
                device_path: format!("/dev/{name}"),
                kind: "disk".into(),
                label: None,
                filesystem: None,
                size,
                free: None,
                drive_type: if removable { "removable" } else { "fixed" }.into(),
                ssd: rotational.map(|r| !r),
                trim: None,
                is_system: false,
                notes: vec![],
            };
            d.add_notes();
            out.push(d);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

pub fn list() -> Vec<DriveInfo> {
    #[cfg(windows)]
    {
        win::list()
    }
    #[cfg(not(windows))]
    {
        unix::list()
    }
}

/// True when the process can open raw devices (Administrator on Windows).
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        win::is_elevated()
    }
    #[cfg(not(windows))]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        unsafe { geteuid() == 0 }
    }
}

/// Does `path` (the case folder) live on the drive `id`?
pub fn path_on_drive(path: &Path, id: &str) -> bool {
    #[cfg(windows)]
    {
        let canon = pr_model::evidence::canonicalize_lenient(path)
            .display()
            .to_string();
        let letter = canon
            .trim_start_matches(r"\\?\")
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase());
        if id.len() == 2 && id.ends_with(':') {
            return letter == id.chars().next().map(|c| c.to_ascii_uppercase());
        }
        if let (Some(n), Some(l)) = (
            id.strip_prefix("PhysicalDrive")
                .and_then(|n| n.parse::<u32>().ok()),
            letter,
        ) {
            let disks = win::volume_disks(&format!(r"\\.\{l}:"));
            // Unknown mapping → assume the worst.
            return disks.is_empty() || disks.contains(&n);
        }
        true
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = pr_model::evidence::canonicalize_lenient(path)
            .ancestors()
            .find_map(|a| std::fs::metadata(a).ok())
            .map(|m| m.dev());
        let Some(dev) = dev else { return true };
        let (maj, min) = (
            ((dev >> 8) & 0xfff) as u32,
            ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32,
        );
        let mine = format!("{maj}:{min}");
        // The disk itself or any of its partitions.
        let base = std::path::PathBuf::from("/sys/block").join(id);
        let mut devs = vec![std::fs::read_to_string(base.join("dev"))
            .unwrap_or_default()
            .trim()
            .to_string()];
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                if let Ok(d) = std::fs::read_to_string(e.path().join("dev")) {
                    devs.push(d.trim().to_string());
                }
            }
        }
        devs.contains(&mine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        if cfg!(windows) {
            assert_eq!(device_path("c:").unwrap(), r"\\.\C:");
            assert_eq!(
                device_path("PhysicalDrive2").unwrap(),
                r"\\.\PhysicalDrive2"
            );
            assert!(device_path("PhysicalDrive").is_err());
            assert!(device_path(r"C:\Windows").is_err());
        } else {
            assert_eq!(device_path("sdb").unwrap(), "/dev/sdb");
            assert!(device_path("../etc/passwd").is_err());
            assert!(device_path("sdb; rm").is_err());
        }
        assert!(device_path("").is_err());
    }

    #[test]
    fn listing_does_not_panic() {
        let _ = list();
        let _ = is_elevated();
    }
}
