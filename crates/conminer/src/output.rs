//! Output rendering. Every command speaks both human and JSON, because the
//! human form is what a person reads and the JSON form is what the tests and
//! scripts assert against.

use anyhow::Result;
use serde::Serialize;

pub struct Writer {
    json: bool,
}

impl Writer {
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    /// Emit a value as JSON, or run the human renderer.
    pub fn emit<T: Serialize>(&self, value: &T, human: impl FnOnce() -> Result<()>) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        } else {
            human()
        }
    }
}

/// Render bytes for a human without lying about what is stored: printable ASCII
/// passes through, everything else becomes a visible escape. The store still
/// holds the original bytes (§6).
pub fn escape_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        match c {
            0x20..=0x7e => s.push(c as char),
            b'\t' => s.push_str("\\t"),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            0x1b => s.push_str("\\e"),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s
}

/// Human-readable byte size.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Truncate for a table cell, marking that it was truncated.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_is_reversible_by_eye_and_never_silently_drops() {
        assert_eq!(escape_bytes(b"ok\n"), "ok\\n");
        assert_eq!(escape_bytes(&[0x00, 0xff]), "\\x00\\xff");
        assert_eq!(escape_bytes(b"\x1b[0m"), "\\e[0m");
    }

    #[test]
    fn human_bytes_reads_right() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(800 * 1024 * 1024), "800.0 MB");
    }

    #[test]
    fn ellipsize_marks_truncation() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("abcdefghij", 5), "abcd…");
    }
}
