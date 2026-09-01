//! Cross-process recovery requests for the ser2net supervisor.
//!
//! The supervisor watches ser2net's stderr, but the evidence that a console is
//! wedged does not live there. ser2net answers a failed device open by serving
//! its failure text TO THE CLIENT and keeps the accepter bound; it does not
//! reliably log anything, and it never retries. So the process that can see the
//! wedge is the one holding the connection -- minerd -- and it lives in a
//! different container from the supervisor.
//!
//! Both mount the same run dir, so a request file is the whole transport. It is
//! deliberately dumb: a request is a hint that something is wrong, the
//! supervisor decides whether to act, and the cap on restarts lives there.
//!
//! This is the SECOND trigger. The first (ser2net's own log line) only fires
//! when ser2net bothers to log, which on this rig it often did not: the RIDE's
//! AP console delivered 0 bytes through ser2net while the tty itself produced
//! 102190 bytes in the same window, and nothing appeared in ser2net's log at
//! all.

use std::path::{Path, PathBuf};

/// One recorded reason a console could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenRequest {
    pub device: String,
    pub reason: String,
}

fn requests_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("reopen.d")
}

/// Sanitize a device name into one path segment.
///
/// Device names are `/dev/serial/by-id/...` paths, so they cannot be used as
/// file names directly, and a half-escaped name would let one device's request
/// land on another's file.
fn stem(device: &str) -> String {
    device
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Ask the ser2net supervisor to reopen a device.
///
/// One file per device, overwritten rather than appended: a console that is
/// wedged reports it on every read attempt, and a growing spool would turn one
/// wedge into an unbounded restart queue.
pub fn request_reopen(run_dir: &Path, device: &str, reason: &str) {
    let dir = requests_dir(run_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{}.req", stem(device)));
    // Best effort throughout: a recovery hint must never be able to take down
    // the capture loop that noticed the problem.
    let _ = std::fs::write(path, format!("{device}\n{reason}\n"));
}

/// Take every pending request, removing them.
///
/// Consuming is what makes a request one-shot: the supervisor acts once per
/// report, and a console that is still wedged simply asks again.
pub fn take_reopen_requests(run_dir: &Path) -> Vec<ReopenRequest> {
    let dir = requests_dir(run_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("req") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut lines = text.lines();
            let device = lines.next().unwrap_or_default().to_string();
            let reason = lines.next().unwrap_or_default().to_string();
            if !device.is_empty() {
                out.push(ReopenRequest { device, reason });
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_survives_the_trip_between_processes() {
        let d = tempfile::tempdir().unwrap();
        request_reopen(
            d.path(),
            "/dev/serial/by-id/usb-FTDI_RIDE-if00-port0",
            "device open failure",
        );

        let got = take_reopen_requests(d.path());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].device, "/dev/serial/by-id/usb-FTDI_RIDE-if00-port0");
        assert_eq!(got[0].reason, "device open failure");
    }

    /// Taking must consume. Otherwise one wedge restarts ser2net on every tick
    /// forever -- which is precisely the phantom restart loop that took every
    /// console down for hours.
    #[test]
    fn requests_are_consumed_so_one_wedge_is_not_an_endless_restart_loop() {
        let d = tempfile::tempdir().unwrap();
        request_reopen(d.path(), "/dev/a", "wedged");
        assert_eq!(take_reopen_requests(d.path()).len(), 1);
        assert!(
            take_reopen_requests(d.path()).is_empty(),
            "a consumed request must not come back"
        );
    }

    /// A console reports its wedge on every read attempt, so repeats must
    /// collapse to one pending request per device.
    #[test]
    fn repeated_reports_for_one_device_collapse_to_a_single_request() {
        let d = tempfile::tempdir().unwrap();
        for _ in 0..50 {
            request_reopen(d.path(), "/dev/a", "wedged");
        }
        assert_eq!(take_reopen_requests(d.path()).len(), 1);
    }

    /// Device names are paths; two different devices must never share a file.
    #[test]
    fn different_devices_do_not_overwrite_each_other() {
        let d = tempfile::tempdir().unwrap();
        request_reopen(d.path(), "/dev/serial/by-id/usb-A-if00-port0", "x");
        request_reopen(d.path(), "/dev/serial/by-id/usb-B-if00-port0", "y");
        let got = take_reopen_requests(d.path());
        assert_eq!(
            got.len(),
            2,
            "distinct devices need distinct requests: {got:?}"
        );
    }

    #[test]
    fn a_missing_run_dir_is_not_an_error() {
        let missing = Path::new("/nonexistent/conminer-run-dir");
        assert!(take_reopen_requests(missing).is_empty());
        request_reopen(missing, "/dev/a", "x"); // must not panic
    }
}
