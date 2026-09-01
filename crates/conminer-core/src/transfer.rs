//! File transfer over a console (§15.10), and auto-baud recovery (§15.9).
//!
//! Console-only boards still need artifacts moved. Rather than assume `lrzsz` is
//! present, transfer is a set of pluggable strategies with a fallback that works
//! on any Unix userspace: `base64` piped into a file, verified end to end with a
//! checksum. Integrity is checked *by the target*, because a transfer that only
//! the host believes in is not a transfer.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};

/// How to move bytes over a line-oriented console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// `rz`/`sz` when the target has lrzsz. Fast, but needs the binary and an
    /// exclusive claim on the port.
    Zmodem,
    /// `base64 -d > file` — works on any Unix userspace, no extra binaries.
    Base64,
    /// U-Boot `loady`.
    Loady,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Zmodem => "zmodem",
            Strategy::Base64 => "base64",
            Strategy::Loady => "loady",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "zmodem" => Strategy::Zmodem,
            "base64" => Strategy::Base64,
            "loady" => Strategy::Loady,
            other => {
                return Err(ToolError::invalid_arg(format!(
                    "transfer strategy must be zmodem|base64|loady, got {other:?}"
                )))
            }
        })
    }

    /// Does this strategy need the port handed over to a binary protocol?
    pub fn needs_exclusive(self) -> bool {
        matches!(self, Strategy::Zmodem | Strategy::Loady)
    }
}

/// A push, broken into the commands that perform it.
#[derive(Debug, Clone, Serialize)]
pub struct PushPlan {
    pub strategy: Strategy,
    pub remote_path: String,
    pub bytes: u64,
    pub sha256: String,
    /// Commands to run in order through the transaction runner.
    pub commands: Vec<String>,
    /// The command whose output must contain `sha256` for the push to count.
    pub verify_command: String,
    pub chunks: usize,
}

/// Maximum encoded payload per command line.
///
/// A console line is not a pipe: too long a line overruns the target's tty input
/// buffer, and the failure looks like corruption rather than backpressure.
pub const CHUNK_BYTES: usize = 512;

/// Plan a base64 push. Pure and inspectable, so the §13 suite can assert the
/// exact command sequence without a board.
pub fn plan_push(data: &[u8], remote_path: &str, max_bytes: u64) -> Result<PushPlan> {
    if data.len() as u64 > max_bytes {
        return Err(ToolError::new(
            ErrorCode::IngestTooLarge,
            format!(
                "{} bytes exceeds the {max_bytes}-byte console-transfer limit",
                data.len()
            ),
        )
        .with_hint("move large artifacts over the network, or raise the limit"));
    }
    if remote_path.contains('\'') || remote_path.contains('\n') {
        return Err(ToolError::invalid_arg(
            "remote path must not contain quotes or newlines",
        ));
    }

    let encoded = base64_encode(data);
    let sha256 = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    };

    let mut commands = vec![format!(": > '{remote_path}.b64'")];
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(CHUNK_BYTES)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    for c in &chunks {
        // `printf %s` rather than `echo`, because echo's handling of backslashes
        // and leading dashes varies between shells.
        commands.push(format!("printf %s '{c}' >> '{remote_path}.b64'"));
    }
    commands.push(format!(
        "base64 -d '{remote_path}.b64' > '{remote_path}' && rm -f '{remote_path}.b64'"
    ));

    Ok(PushPlan {
        strategy: Strategy::Base64,
        remote_path: remote_path.to_string(),
        bytes: data.len() as u64,
        sha256,
        verify_command: format!("sha256sum '{remote_path}' 2>/dev/null || sha256 '{remote_path}'"),
        commands,
        chunks: chunks.len(),
    })
}

/// Check a target's own checksum output against what we sent.
///
/// The target has to agree: a transfer the host alone believes in has not been
/// verified at all.
pub fn verify_push(plan: &PushPlan, target_output: &str) -> Result<()> {
    if target_output.to_ascii_lowercase().contains(&plan.sha256) {
        return Ok(());
    }
    Err(ToolError::new(
        ErrorCode::IngestFailed,
        "the target's checksum does not match what was sent",
    )
    .with_hint("the console may be dropping characters; lower runner.char_delay_ms")
    .with_detail(serde_json::json!({
        "expected_sha256": plan.sha256,
        "target_said": target_output.trim(),
    })))
}

/// Plan a pull: read the file back as base64 and decode it here.
pub fn plan_pull(remote_path: &str) -> Result<Vec<String>> {
    if remote_path.contains('\'') || remote_path.contains('\n') {
        return Err(ToolError::invalid_arg(
            "remote path must not contain quotes or newlines",
        ));
    }
    Ok(vec![
        format!("sha256sum '{remote_path}' 2>/dev/null || sha256 '{remote_path}'"),
        format!("base64 '{remote_path}'"),
    ])
}

/// Decode a pull, verifying the target's checksum first.
pub fn finish_pull(checksum_output: &str, b64_output: &str) -> Result<Vec<u8>> {
    let cleaned: String = b64_output.chars().filter(|c| !c.is_whitespace()).collect();
    let data = base64_decode(&cleaned)?;
    let got = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&data))
    };
    if !checksum_output.to_ascii_lowercase().contains(&got) {
        return Err(ToolError::new(
            ErrorCode::IngestFailed,
            "the pulled bytes do not match the checksum the target reported",
        )
        .with_detail(serde_json::json!({
            "computed_sha256": got,
            "target_said": checksum_output.trim(),
        })));
    }
    Ok(data)
}

// -------------------------------------------------------------- auto-baud ---

/// A candidate rate and how plausible the output looked at it.
#[derive(Debug, Clone, Serialize)]
pub struct BaudProbe {
    pub baud: u32,
    pub printable_ratio: f64,
    pub sample: String,
}

/// Pick the best rate from a set of probes.
///
/// Auto-baud is off by default because it perturbs the port — it has to actually
/// change the line rate to test one — so this only decides; the caller owns the
/// disruption.
pub fn best_rate(probes: &[BaudProbe], min_ratio: f64) -> Option<&BaudProbe> {
    probes
        .iter()
        .filter(|p| p.printable_ratio >= min_ratio)
        .max_by(|a, b| a.printable_ratio.total_cmp(&b.printable_ratio))
}

/// Fraction of a sample that reads as plausible console text.
pub fn printable_ratio(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut d = crate::framer::generic::GarbageDetector::new(bytes.len().max(1), 1.0);
    d.push(bytes);
    1.0 - d.ratio()
}

// ------------------------------------------------------------------ base64 ---

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                return Err(ToolError::invalid_arg(format!(
                    "invalid base64 byte {:?}",
                    c as char
                )))
            }
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

// -------------------------------------------------------- zmodem pull (F10) --

/// POSIX `cksum` of a buffer — the checksum the board can compute for itself.
///
/// Verification has to be something the TARGET can produce independently.
/// Comparing a local hash against a local hash proves the file arrived
/// somewhere, not that it arrived intact.
pub fn cksum(data: &[u8]) -> u32 {
    // CRC-32/CKSUM: the POSIX variant, MSB-first with the length appended.
    const POLY: u32 = 0x04c1_1db7;
    let mut crc: u32 = 0;
    let mut feed = |b: u8| {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
        }
    };
    for b in data {
        feed(*b);
    }
    let mut len = data.len();
    while len > 0 {
        feed((len & 0xff) as u8);
        len >>= 8;
    }
    !crc
}

/// Pull a file with zmodem, bridging the board's `sz` to a host-side `rz`.
///
/// conminer does not implement the protocol: `lrzsz` is what every board's `sz`
/// expects to be talking to, and a hand-rolled receiver would be a new source of
/// corruption on the one path where corruption is silent. What conminer owns is
/// the part that goes wrong in practice -- claiming the port, bridging the two
/// streams, bounding the time, and always releasing.
pub fn zmodem_pull(
    endpoint: &str,
    remote_path: &str,
    local_path: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<u64> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let dir = local_path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir).ok();

    let mut sock = std::net::TcpStream::connect(endpoint.trim_start_matches("tcp://"))
        .map_err(|e| ToolError::new(ErrorCode::Internal, format!("connect {endpoint}: {e}")))?;
    sock.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .ok();

    // Ask the board to send, then hand the stream to rz.
    writeln!(sock, "sz -b {remote_path}")
        .map_err(|e| ToolError::new(ErrorCode::Internal, format!("write: {e}")))?;

    let mut rz = Command::new("rz")
        .args(["-b", "-y"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ToolError::new(
                ErrorCode::HookNotConfigured,
                format!("no host-side `rz` (lrzsz): {e}"),
            )
            .with_hint("lrzsz is installed in the conminer image; this build may predate it")
        })?;
    let mut to_rz = rz.stdin.take().expect("piped");
    let mut from_rz = rz.stdout.take().expect("piped");
    let mut sock_w = sock
        .try_clone()
        .map_err(|e| ToolError::new(ErrorCode::Internal, format!("clone: {e}")))?;

    // rz → board.
    let pump = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = from_rz.read(&mut buf) {
            if n == 0 || sock_w.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });

    // board → rz, bounded: a board that resets mid-stream must not park a thread
    // forever.
    let deadline = std::time::Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if std::time::Instant::now() > deadline {
            let _ = rz.kill();
            let _ = std::fs::remove_file(local_path);
            return Err(ToolError::new(
                ErrorCode::HookTimeout,
                format!("zmodem pull of {remote_path} timed out"),
            )
            .with_hint("the partial file was removed; the port has been released"));
        }
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to_rz.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if let Ok(Some(_)) = rz.try_wait() {
            break;
        }
    }
    drop(to_rz);
    let status = rz.wait().ok();
    let _ = pump.join();

    let landed = dir.join(
        std::path::Path::new(remote_path)
            .file_name()
            .unwrap_or_default(),
    );
    if landed != local_path && landed.exists() {
        std::fs::rename(&landed, local_path).ok();
    }
    let size = std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err(ToolError::new(
            ErrorCode::Internal,
            format!("zmodem pull produced no bytes (rz exit {status:?})"),
        ));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_arbitrary_bytes() {
        for data in [
            &b""[..],
            &b"a"[..],
            &b"ab"[..],
            &b"abc"[..],
            &b"hello world"[..],
            &[0u8, 255, 128, 1, 2, 3][..],
        ] {
            let e = base64_encode(data);
            assert_eq!(base64_decode(&e).unwrap(), data, "{e}");
        }
    }

    #[test]
    fn our_encoding_matches_the_canonical_one() {
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn a_push_is_chunked_so_it_cannot_overrun_the_target_input_buffer() {
        let data = vec![b'x'; 4096];
        let plan = plan_push(&data, "/tmp/blob.bin", 1 << 20).unwrap();
        assert_eq!(plan.strategy, Strategy::Base64);
        assert!(plan.chunks > 1);
        for c in &plan.commands {
            assert!(
                c.len() < CHUNK_BYTES + 200,
                "a {}-byte command line would overrun the tty buffer",
                c.len()
            );
        }
        // Truncate, append, decode, clean up.
        assert!(plan.commands.first().unwrap().starts_with(": >"));
        assert!(plan.commands.last().unwrap().contains("base64 -d"));
    }

    #[test]
    fn a_push_is_only_complete_when_the_target_agrees_on_the_checksum() {
        let plan = plan_push(b"hello world", "/tmp/hi.txt", 1 << 20).unwrap();
        let good = format!("{}  /tmp/hi.txt", plan.sha256);
        verify_push(&plan, &good).unwrap();

        let err = verify_push(&plan, "0000  /tmp/hi.txt").unwrap_err();
        assert_eq!(err.code, ErrorCode::IngestFailed);
        assert!(err.hint.contains("char_delay_ms"));
    }

    #[test]
    fn a_pull_verifies_before_it_returns_bytes() {
        let data = b"the quick brown fox";
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(data))
        };
        let b64 = base64_encode(data);
        assert_eq!(
            finish_pull(&format!("{sha}  /tmp/f"), &b64).unwrap(),
            data,
            "verified pull returns the bytes"
        );
        assert!(
            finish_pull("deadbeef  /tmp/f", &b64).is_err(),
            "a mismatched checksum must not yield bytes"
        );
    }

    #[test]
    fn a_pull_tolerates_the_line_wrapping_a_console_adds() {
        let data = vec![7u8; 300];
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&data))
        };
        let wrapped = base64_encode(&data)
            .as_bytes()
            .chunks(76)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("\r\n");
        assert_eq!(finish_pull(&format!("{sha} f"), &wrapped).unwrap(), data);
    }

    #[test]
    fn a_hostile_remote_path_is_refused_rather_than_quoted_into_a_shell() {
        assert!(plan_push(b"x", "/tmp/a'; rm -rf /; '", 1 << 20).is_err());
        assert!(plan_pull("/tmp/a\nrm -rf /").is_err());
    }

    #[test]
    fn an_oversized_transfer_is_refused_with_a_hint() {
        let err = plan_push(&[0u8; 5000], "/tmp/x", 1024).unwrap_err();
        assert_eq!(err.code, ErrorCode::IngestTooLarge);
        assert!(err.hint.contains("network"));
    }

    #[test]
    fn strategies_that_take_over_the_port_declare_it() {
        assert!(Strategy::Zmodem.needs_exclusive());
        assert!(Strategy::Loady.needs_exclusive());
        assert!(
            !Strategy::Base64.needs_exclusive(),
            "base64 is line-oriented, so framing keeps working throughout"
        );
        assert!(Strategy::parse("nope").is_err());
    }

    #[test]
    fn auto_baud_picks_the_rate_whose_output_reads_as_text() {
        let probes = vec![
            BaudProbe {
                baud: 9600,
                printable_ratio: printable_ratio(&[0x80u8; 256]),
                sample: String::new(),
            },
            BaudProbe {
                baud: 115_200,
                printable_ratio: printable_ratio(
                    b"U-Boot 2026.01 (Jan 04 2026)\r\nDRAM: 8 GiB\r\n",
                ),
                sample: "U-Boot 2026.01".into(),
            },
        ];
        let best = best_rate(&probes, 0.7).expect("one rate reads as text");
        assert_eq!(best.baud, 115_200);
        // …and nothing is picked when nothing looks like text.
        let junk = vec![BaudProbe {
            baud: 9600,
            printable_ratio: printable_ratio(&[0x80u8; 256]),
            sample: String::new(),
        }];
        assert!(best_rate(&junk, 0.7).is_none());
    }
}
