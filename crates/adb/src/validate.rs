//! Input validation and device-shell quoting.

use crate::AdbError;

/// ADB serials: USB serials, `host:port` for TCP, emulator names.
pub fn validate_serial(s: &str) -> Result<(), AdbError> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '[' | ']'));
    if ok {
        Ok(())
    } else {
        Err(AdbError::InvalidArgument(format!(
            "invalid device serial {s:?}"
        )))
    }
}

/// Roots under which device paths may be read without root.
pub const ALLOWED_ROOTS: [&str; 3] = ["/sdcard/", "/storage/emulated/0/", "/storage/"];

/// Validate a device path: absolute, under an allowed root, no `..`, no
/// control characters, bounded length.
pub fn validate_device_path(p: &str) -> Result<(), AdbError> {
    let bad = |why: &str| {
        Err(AdbError::InvalidArgument(format!(
            "device path {p:?}: {why}"
        )))
    };
    if p.len() > 4096 {
        return bad("too long");
    }
    if p.chars().any(|c| c.is_control()) {
        return bad("contains control characters");
    }
    if !ALLOWED_ROOTS
        .iter()
        .any(|r| p.starts_with(r) || p == r.trim_end_matches('/'))
    {
        return bad("outside allowed shared-storage roots");
    }
    if p.split('/').any(|seg| seg == "..") {
        return bad("parent references are not allowed");
    }
    Ok(())
}

/// Block-device partition names under `/dev/block/by-name/`.
pub fn validate_partition_name(n: &str) -> Result<(), AdbError> {
    if !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(())
    } else {
        Err(AdbError::InvalidArgument(format!(
            "invalid partition name {n:?}"
        )))
    }
}

/// Quote one argument for the device's POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Turn a device path into a safe relative local path (Windows-safe names,
/// no traversal).
pub fn local_relative_path(device_path: &str) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for seg in device_path
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
    {
        let mut clean: String = seg
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        // Windows rejects trailing dots/spaces and reserved device names.
        while clean.ends_with('.') || clean.ends_with(' ') {
            clean.pop();
            clean.push('_');
        }
        let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            clean.insert(0, '_');
        }
        if clean.chars().count() > 200 {
            clean = clean.chars().take(200).collect();
        }
        out.push(clean);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials() {
        assert!(validate_serial("R58M123ABC").is_ok());
        assert!(validate_serial("192.168.1.5:5555").is_ok());
        assert!(validate_serial("emulator-5554").is_ok());
        assert!(validate_serial("a;rm -rf /").is_err());
        assert!(validate_serial("").is_err());
    }

    #[test]
    fn paths() {
        assert!(validate_device_path("/sdcard/DCIM/Camera/IMG_1.jpg").is_ok());
        assert!(validate_device_path("/storage/1234-ABCD/DCIM").is_ok());
        assert!(validate_device_path("/data/data/x").is_err());
        assert!(validate_device_path("/sdcard/../data").is_err());
        assert!(validate_device_path("/sdcard/a\nb").is_err());
        assert!(validate_partition_name("userdata").is_ok());
        assert!(validate_partition_name("../x").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("$(reboot)"), "'$(reboot)'");
    }

    #[test]
    fn local_paths_are_safe() {
        let p = local_relative_path("/sdcard/../DCIM/a:b?.jpg");
        assert_eq!(
            p,
            std::path::PathBuf::from("sdcard")
                .join("DCIM")
                .join("a_b_.jpg")
        );
        assert_eq!(
            local_relative_path("/sdcard/CON.jpg"),
            std::path::PathBuf::from("sdcard").join("_CON.jpg")
        );
        assert_eq!(
            local_relative_path("/sdcard/x. "),
            std::path::PathBuf::from("sdcard").join("x._")
        );
    }
}
