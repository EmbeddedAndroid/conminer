//! Per-device writer lock.
//!
//! A device has exactly one writer at a time. That is not a limitation bolted on
//! after the fact — it is the architecture of §3: minerd owns a device's live
//! pipeline and runs post-hoc ingests for it through a *job queue*. The lock is
//! how that invariant is enforced across threads and across processes, so the
//! CLI cannot quietly race minerd.
//!
//! Two writers on one device would not merely be slow: each holds its own
//! in-memory miner, so both would allocate the same next template id and one
//! would lose. Serialising is the correct answer, not a bigger `busy_timeout`.
//!
//! Readers never take the lock. Reads are always unrestricted (§15.1).

use crate::error::{ErrorCode, Result, ToolError};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Held for as long as a writer owns a device. Released on drop.
#[derive(Debug)]
pub struct DeviceLock {
    file: std::fs::File,
    path: PathBuf,
}

impl DeviceLock {
    fn lock_path(db_path: &Path) -> PathBuf {
        let mut p = db_path.as_os_str().to_os_string();
        p.push(".writer.lock");
        PathBuf::from(p)
    }

    fn open(db_path: &Path) -> Result<(std::fs::File, PathBuf)> {
        let path = Self::lock_path(db_path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        Ok((file, path))
    }

    /// Block until the device is ours.
    pub fn acquire(db_path: &Path) -> Result<Self> {
        let (file, path) = Self::open(db_path)?;
        // SAFETY: `file` is a live fd owned here for the call's duration.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!(
                    "cannot lock {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ),
            ));
        }
        Ok(Self { file, path })
    }

    /// Take the device only if it is free. Used where blocking would be wrong —
    /// a healthcheck, or a tool that would rather report `DEVICE_GONE` than hang.
    pub fn try_acquire(db_path: &Path) -> Result<Option<Self>> {
        let (file, path) = Self::open(db_path)?;
        // SAFETY: as above.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { file, path }));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(ToolError::new(
                ErrorCode::Internal,
                format!("cannot lock {}: {e}", path.display()),
            ))
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DeviceLock {
    fn drop(&mut self) {
        // SAFETY: the fd is still open until `file` drops immediately after.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("dev.db");

        let held = DeviceLock::acquire(&db).unwrap();
        assert!(
            DeviceLock::try_acquire(&db).unwrap().is_none(),
            "a second writer must not get in"
        );
        drop(held);
        assert!(DeviceLock::try_acquire(&db).unwrap().is_some());
    }

    #[test]
    fn different_devices_do_not_block_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let a = DeviceLock::acquire(&dir.path().join("a.db")).unwrap();
        let b = DeviceLock::try_acquire(&dir.path().join("b.db")).unwrap();
        assert!(b.is_some());
        drop(a);
    }

    #[test]
    fn concurrent_acquirers_serialise_rather_than_failing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(dir.path().join("busy.db"));
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let db = db.clone();
                let inside = inside.clone();
                let peak = peak.clone();
                std::thread::spawn(move || {
                    let _l = DeviceLock::acquire(&db).unwrap();
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::yield_now();
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "only one writer at a time");
    }
}
