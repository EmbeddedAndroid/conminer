//! Virtual serial ports (§12.3).
//!
//! `testkit::replay` streams corpus bytes through **real** ptys so the identical
//! live pipeline is exercised without hardware. A mock that skipped the kernel's
//! tty layer would also skip the behaviours that break in the lab: partial
//! reads, EAGAIN, and the fact that a tty is not a pipe.

use std::ffi::CStr;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

/// A pty pair: the master end conminer's peer writes to, and the slave device
/// path a consumer opens as if it were `/dev/ttyUSB0`.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    slave_path: PathBuf,
}

impl Pty {
    pub fn open() -> io::Result<Self> {
        // SAFETY: posix_openpt/grantpt/unlockpt/ptsname is the documented
        // sequence; every fd is checked and wrapped in OwnedFd immediately.
        unsafe {
            let fd: RawFd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let master = OwnedFd::from_raw_fd(fd);
            if libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            let name = libc::ptsname(fd);
            if name.is_null() {
                return Err(io::Error::last_os_error());
            }
            let slave_path = PathBuf::from(CStr::from_ptr(name).to_string_lossy().into_owned());
            Ok(Self { master, slave_path })
        }
    }

    /// Path a consumer opens, e.g. what a generated ser2net config points at.
    pub fn slave_path(&self) -> &std::path::Path {
        &self.slave_path
    }

    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Write bytes into the pty as if the target had printed them.
    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        // SAFETY: `master` is a live fd owned by self; `data` is a valid slice.
        let n = unsafe {
            libc::write(
                self.master.as_raw_fd(),
                data.as_ptr() as *const libc::c_void,
                data.len(),
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Write everything, retrying short writes — a tty's buffer is small, and a
    /// replay that silently dropped the tail would invalidate every assertion
    /// downstream of it.
    pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            match self.write(data) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pty accepted no bytes",
                    ))
                }
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
                    std::thread::yield_now();
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Is a pty even available? Some CI sandboxes have no `/dev/ptmx`; a test that
/// needs one should say so rather than fail obscurely.
pub fn available() -> bool {
    Pty::open().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_pty_round_trips_bytes() {
        let Ok(pty) = Pty::open() else {
            eprintln!("no /dev/ptmx in this environment; skipping");
            return;
        };
        let mut slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pty.slave_path())
            .expect("open slave");

        pty.write_all(b"NOTICE:  BL31: v2.11\r\n").expect("write");
        let mut buf = [0u8; 64];
        let n = slave.read(&mut buf).expect("read");
        assert!(String::from_utf8_lossy(&buf[..n]).contains("BL31"));
    }
}
