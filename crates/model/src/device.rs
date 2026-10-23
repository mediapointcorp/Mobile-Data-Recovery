//! Raw, read-only access to physical disks and volumes.
//!
//! Windows only accepts reads of a raw device (`\\.\C:`,
//! `\\.\PhysicalDrive1`) at sector-aligned offsets and lengths, so
//! [`AlignedSource`] turns arbitrary reads into aligned ones.

use crate::source::ReadAt;
use std::fs::File;
use std::io;

/// Alignment used for every raw-device read (covers 512 B and 4 KiB sectors).
pub const DEVICE_ALIGN: u64 = 4096;

/// Wraps a source that only supports aligned reads.
pub struct AlignedSource<T: ReadAt> {
    inner: T,
    align: u64,
}

impl<T: ReadAt> AlignedSource<T> {
    pub fn new(inner: T, align: u64) -> Self {
        assert!(align.is_power_of_two());
        AlignedSource { inner, align }
    }
}

impl<T: ReadAt> ReadAt for AlignedSource<T> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.len();
        if offset >= len || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(len - offset);
        let a = self.align;
        if offset.is_multiple_of(a) && want.is_multiple_of(a) {
            return self.inner.read_at(offset, &mut buf[..want as usize]);
        }
        let start = offset & !(a - 1);
        let end = (offset + want).div_ceil(a) * a;
        let mut tmp = vec![0u8; (end - start) as usize];
        let mut got = 0usize;
        while got < tmp.len() {
            match self.inner.read_at(start + got as u64, &mut tmp[got..])? {
                0 => break,
                n => got += n,
            }
        }
        let skip = (offset - start) as usize;
        if got <= skip {
            return Ok(0);
        }
        let n = (got - skip).min(want as usize);
        buf[..n].copy_from_slice(&tmp[skip..skip + n]);
        Ok(n)
    }
}

/// A raw device handle opened read-only (shared with the running system).
pub struct RawDevice {
    file: File,
    len: u64,
}

impl RawDevice {
    #[cfg(windows)]
    pub fn open(path: &str) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        use windows_sys::Win32::System::Ioctl::{
            GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        // Read access only; other processes (Windows itself) keep writing.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)?;
        let mut info = GET_LENGTH_INFORMATION { Length: 0 };
        let mut returned = 0u32;
        // SAFETY: valid handle, correctly sized output buffer, no input buffer.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // Not a disk or volume (e.g. an image file): use its file size.
            return match file.metadata() {
                Ok(m) if m.is_file() => Ok(RawDevice { len: m.len(), file }),
                _ => Err(err),
            };
        }
        Ok(RawDevice {
            file,
            len: info.Length as u64,
        })
    }

    #[cfg(not(windows))]
    pub fn open(path: &str) -> io::Result<Self> {
        use std::io::{Seek, SeekFrom};
        let mut file = File::open(path)?;
        // Block devices report size 0 in metadata; seeking to the end works.
        let len = file.seek(SeekFrom::End(0))?;
        Ok(RawDevice { file, len })
    }
}

impl ReadAt for RawDevice {
    fn len(&self) -> u64 {
        self.len
    }

    #[cfg(windows)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        if offset >= self.len {
            return Ok(0);
        }
        self.file.seek_read(buf, offset)
    }

    #[cfg(unix)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        if offset >= self.len {
            return Ok(0);
        }
        self.file.read_at(buf, offset)
    }
}

/// Open a raw device for aligned, read-only access.
pub fn open_device(path: &str) -> io::Result<AlignedSource<RawDevice>> {
    Ok(AlignedSource::new(RawDevice::open(path)?, DEVICE_ALIGN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{read_full_at, MemSource};

    /// Fails any read that is not 4 KiB aligned, like a Windows volume handle.
    struct Strict(MemSource);
    impl ReadAt for Strict {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            if !offset.is_multiple_of(4096) || !buf.len().is_multiple_of(4096) {
                return Err(io::Error::from_raw_os_error(87)); // ERROR_INVALID_PARAMETER
            }
            self.0.read_at(offset, buf)
        }
    }

    #[test]
    fn unaligned_reads_are_served_through_aligned_ones() {
        let data: Vec<u8> = (0..40_960u32).map(|i| (i * 7 % 251) as u8).collect();
        let s = AlignedSource::new(Strict(MemSource::new(data.clone())), 4096);
        for (off, n) in [
            (0u64, 10usize),
            (4095, 2),
            (5000, 9000),
            (40_950, 100),
            (8192, 4096),
            (1, 40_000),
        ] {
            let mut b = vec![0u8; n];
            let got = read_full_at(&s, off, &mut b).unwrap();
            let end = (off as usize + n).min(data.len());
            assert_eq!(got, end - off as usize);
            assert_eq!(&b[..got], &data[off as usize..end], "off {off} n {n}");
        }
        assert!(Strict(MemSource::new(data))
            .read_at(1, &mut [0u8; 4096])
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn raw_device_on_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.img");
        std::fs::write(&p, vec![9u8; 10_000]).unwrap();
        let d = open_device(p.to_str().unwrap()).unwrap();
        assert_eq!(d.len(), 10_000);
        let mut b = [0u8; 3];
        assert_eq!(d.read_at(9_998, &mut b).unwrap(), 2);
    }
}
