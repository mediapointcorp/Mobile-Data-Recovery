//! Process execution for adb with timeouts. No shell is involved on the PC
//! side: arguments are passed as an argv vector.

use crate::AdbError;
use pr_model::{is_cancelled, CancelFlag};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl CmdOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).replace("\r\n", "\n")
    }
}

/// Abstraction over running adb, so tests can inject canned responses.
pub trait CommandRunner: Send + Sync {
    fn run(&self, args: &[String], timeout: Duration) -> Result<CmdOutput, AdbError>;

    /// Stream stdout into `sink` (used for `exec-out` acquisitions).
    /// Returns the exit code and stderr.
    fn run_streaming(
        &self,
        args: &[String],
        sink: &mut dyn Write,
        cancel: &CancelFlag,
        progress: &mut dyn FnMut(u64),
    ) -> Result<CmdOutput, AdbError>;
}

pub struct ProcessRunner {
    pub adb_path: PathBuf,
}

impl ProcessRunner {
    pub fn new(adb_path: PathBuf) -> Self {
        ProcessRunner { adb_path }
    }

    /// Locate adb: explicit path, `PATH`, then common Windows SDK locations.
    pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, AdbError> {
        if let Some(p) = explicit {
            return if p.is_file() { Ok(p.to_path_buf()) } else { Err(AdbError::AdbNotFound) };
        }
        let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
        let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect())
            .unwrap_or_default();
        for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
            if let Some(d) = std::env::var_os(var) {
                candidates.push(PathBuf::from(d).join("platform-tools").join(exe));
            }
        }
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(d).join("Android").join("Sdk").join("platform-tools").join(exe));
        }
        candidates.into_iter().find(|p| p.is_file()).ok_or(AdbError::AdbNotFound)
    }

    fn command(&self, args: &[String]) -> Command {
        let mut c = Command::new(&self.adb_path);
        c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        c
    }
}

fn drain<R: Read + Send + 'static>(r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = r.take(64 * 1024 * 1024).read_to_end(&mut v);
        v
    })
}

impl CommandRunner for ProcessRunner {
    fn run(&self, args: &[String], timeout: Duration) -> Result<CmdOutput, AdbError> {
        let mut child = self.command(args).spawn().map_err(AdbError::Spawn)?;
        let out = drain(child.stdout.take().expect("piped"));
        let err = drain(child.stderr.take().expect("piped"));
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AdbError::Timeout(timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Ok(CmdOutput {
            code: status.code(),
            stdout: out.join().unwrap_or_default(),
            stderr: String::from_utf8_lossy(&err.join().unwrap_or_default()).to_string(),
        })
    }

    fn run_streaming(
        &self,
        args: &[String],
        sink: &mut dyn Write,
        cancel: &CancelFlag,
        progress: &mut dyn FnMut(u64),
    ) -> Result<CmdOutput, AdbError> {
        let mut child = self.command(args).spawn().map_err(AdbError::Spawn)?;
        let mut stdout = child.stdout.take().expect("piped");
        let err = drain(child.stderr.take().expect("piped"));
        let mut buf = vec![0u8; 1024 * 1024];
        let mut total = 0u64;
        loop {
            if is_cancelled(cancel) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AdbError::Cancelled);
            }
            let n = stdout.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n])?;
            total += n as u64;
            progress(total);
        }
        let status = child.wait()?;
        Ok(CmdOutput {
            code: status.code(),
            stdout: Vec::new(),
            stderr: String::from_utf8_lossy(&err.join().unwrap_or_default()).to_string(),
        })
    }
}
