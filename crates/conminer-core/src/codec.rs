//! Input codecs (§15.6, §15.7): LAVA job logs and kernel pstore dumps.
//!
//! Both are the same shape of problem — a container format wrapped around console
//! text — and both are solved the same way: unwrap to the *original bytes* and
//! feed the ordinary pipeline. No preprocessing scripts, and no second code path
//! that could drift from the live one.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};

/// A recognised container around console output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wrapper {
    /// Plain console capture.
    None,
    /// LAVA dispatcher YAML: a list of `{dt, lvl, msg}` records.
    Lava,
    /// `/sys/fs/pstore` dmesg fragment.
    Pstore,
}

impl Wrapper {
    pub fn as_str(self) -> &'static str {
        match self {
            Wrapper::None => "none",
            Wrapper::Lava => "lava",
            Wrapper::Pstore => "pstore",
        }
    }
}

/// Sniff the container from the first bytes of a stream.
pub fn detect(head: &[u8]) -> Wrapper {
    let text = String::from_utf8_lossy(&head[..head.len().min(4096)]);
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");

    // LAVA writes one YAML list entry per line: `- {"dt": …, "lvl": …, "msg": …}`
    // or the block form with `dt:`/`lvl:`/`msg:` keys.
    if first.trim_start().starts_with("- ")
        && (text.contains("\"lvl\"") || text.contains("lvl:"))
        && (text.contains("\"msg\"") || text.contains("msg:"))
    {
        return Wrapper::Lava;
    }
    // pstore fragments start with the ramoops header the kernel writes.
    if first.starts_with("Panic#") || first.starts_with("Oops#") || text.starts_with("====") {
        return Wrapper::Pstore;
    }
    Wrapper::None
}

/// One line lifted out of a container, with any stage hint the container carried.
#[derive(Debug, Clone, PartialEq)]
pub struct Unwrapped {
    pub text: String,
    /// LAVA action names map onto boot stages, which is free stage information
    /// the console text alone would not have.
    pub stage_hint: Option<String>,
    pub level: Option<String>,
}

/// Unwrap a whole buffer.
///
/// Returns the console bytes to feed the pipeline. For `Wrapper::None` this is
/// the identity function, which is what keeps the post-hoc path honest: an
/// ordinary capture is never rewritten on its way in.
pub fn unwrap_all(data: &[u8], wrapper: Wrapper) -> Result<Vec<u8>> {
    match wrapper {
        Wrapper::None => Ok(data.to_vec()),
        Wrapper::Lava => {
            let text = String::from_utf8_lossy(data);
            let mut out = String::with_capacity(text.len());
            for line in text.lines() {
                if let Some(u) = lava_line(line) {
                    out.push_str(&u.text);
                    out.push('\n');
                }
            }
            if out.is_empty() && !text.trim().is_empty() {
                return Err(ToolError::new(
                    ErrorCode::IngestFailed,
                    "input looked like a LAVA job log but no message lines were found",
                )
                .with_hint("pass the raw console capture, or check the log format"));
            }
            Ok(out.into_bytes())
        }
        Wrapper::Pstore => Ok(pstore_body(data)),
    }
}

/// Parse one LAVA log line.
///
/// LAVA's own writer emits JSON-ish entries; the parse is deliberately tolerant
/// because a job log that is 99% parseable is still worth mining, and a strict
/// parser that rejected the file would leave the agent with nothing.
pub fn lava_line(line: &str) -> Option<Unwrapped> {
    let body = line.trim().strip_prefix("- ")?;
    let msg = extract_field(body, "msg")?;
    let level = extract_field(body, "lvl");
    // `target` entries are the device's own console output; everything else is
    // dispatcher commentary, which is context rather than console text.
    let stage_hint = match level.as_deref() {
        Some("target") => None,
        Some("info") => extract_field(body, "msg")
            .filter(|m| m.starts_with("start: "))
            .map(|m| lava_action_to_stage(&m)),
        _ => None,
    };
    Some(Unwrapped {
        text: unescape(&msg),
        stage_hint: stage_hint.flatten(),
        level,
    })
}

/// Map a LAVA action name onto a boot stage where the correspondence is real.
fn lava_action_to_stage(msg: &str) -> Option<String> {
    let m = msg.to_ascii_lowercase();
    if m.contains("bootloader") || m.contains("u-boot") || m.contains("uboot") {
        Some("uboot".into())
    } else if m.contains("boot-kernel") || m.contains("auto-login") {
        Some("kernel".into())
    } else if m.contains("login-action") || m.contains("expect-shell") {
        Some("userspace".into())
    } else {
        None
    }
}

/// Pull `"key": "value"` or `key: value` out of a LAVA entry.
fn extract_field(body: &str, key: &str) -> Option<String> {
    for form in [format!("\"{key}\":"), format!("{key}:")] {
        if let Some(i) = body.find(&form) {
            let rest = body[i + form.len()..].trim_start();
            return Some(match rest.strip_prefix('"') {
                Some(quoted) => {
                    // Take up to the closing quote, honouring backslash escapes.
                    let mut out = String::new();
                    let mut chars = quoted.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => {
                                out.push('\\');
                                if let Some(n) = chars.next() {
                                    out.push(n);
                                }
                            }
                            '"' => break,
                            c => out.push(c),
                        }
                    }
                    out
                }
                None => {
                    // An unquoted value ends at the next separator, not at the
                    // end of the entry — otherwise `lvl` would swallow `msg`.
                    let end = rest.find(&[',', '}', ']'][..]).unwrap_or(rest.len());
                    rest[..end].trim().to_string()
                }
            });
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    // A LAVA `msg` is one console line; embedded newlines would fragment it into
    // records the device never emitted as separate lines.
    out.replace('\n', " ").trim_end().to_string()
}

/// Strip the ramoops/pstore header, leaving the preserved dmesg.
///
/// The console can miss a crash the kernel preserved: after a reboot,
/// `/sys/fs/pstore` still holds the previous oops. What is inside is ordinary
/// kernel output, so it belongs in the *previous* epoch, mined by the ordinary
/// `linux` profile.
pub fn pstore_body(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);
    let mut lines = text.lines().peekable();
    let mut skipped = 0;
    while let Some(l) = lines.peek() {
        let t = l.trim();
        if t.starts_with("Panic#")
            || t.starts_with("Oops#")
            || t.starts_with("====")
            || t.is_empty() && skipped == 0
        {
            lines.next();
            skipped += 1;
        } else {
            break;
        }
    }
    let mut out: String = lines.collect::<Vec<_>>().join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAVA: &str = r#"- {"dt": "2026-01-04T12:00:00", "lvl": "info", "msg": "start: 2 uboot-action"}
- {"dt": "2026-01-04T12:00:01", "lvl": "target", "msg": "U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)"}
- {"dt": "2026-01-04T12:00:02", "lvl": "target", "msg": "Starting kernel ..."}
- {"dt": "2026-01-04T12:00:03", "lvl": "target", "msg": "[    0.000000] Linux version 6.12.9 (build@lab)"}
- {"dt": "2026-01-04T12:00:04", "lvl": "debug", "msg": "Waiting for prompt"}
"#;

    #[test]
    fn a_lava_job_log_is_recognised_and_unwrapped_to_console_text() {
        assert_eq!(detect(LAVA.as_bytes()), Wrapper::Lava);
        let out = String::from_utf8(unwrap_all(LAVA.as_bytes(), Wrapper::Lava).unwrap()).unwrap();
        assert!(out.contains("U-Boot 2026.01"));
        assert!(out.contains("[    0.000000] Linux version 6.12.9"));
        // The dispatcher's own framing is gone; the console text is intact.
        assert!(!out.contains("\"lvl\""));
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn lava_levels_are_preserved_so_target_output_is_distinguishable() {
        let target = lava_line(r#"- {"dt": "x", "lvl": "target", "msg": "mmc0: ready"}"#).unwrap();
        assert_eq!(target.level.as_deref(), Some("target"));
        assert_eq!(target.text, "mmc0: ready");
    }

    #[test]
    fn lava_action_names_become_stage_hints_where_the_mapping_is_real() {
        let u = lava_line(r#"- {"lvl": "info", "msg": "start: 2 uboot-action"}"#).unwrap();
        assert_eq!(u.stage_hint.as_deref(), Some("uboot"));
        let u = lava_line(r#"- {"lvl": "info", "msg": "start: 3 login-action"}"#).unwrap();
        assert_eq!(u.stage_hint.as_deref(), Some("userspace"));
        // No invented mapping for actions that are not boot stages.
        let u = lava_line(r#"- {"lvl": "info", "msg": "start: 1 deploy-action"}"#).unwrap();
        assert_eq!(u.stage_hint, None);
    }

    #[test]
    fn escapes_inside_a_lava_message_are_restored() {
        let u = lava_line(r#"- {"lvl": "target", "msg": "a \"quoted\" word\tand a tab"}"#).unwrap();
        assert!(u.text.contains('"'), "{:?}", u.text);
        assert!(u.text.contains('\t'));
    }

    #[test]
    fn the_block_form_of_a_lava_entry_also_parses() {
        let u = lava_line("- dt: 2026-01-04T12:00:00\n").is_none();
        assert!(u, "an entry with no msg is not a log line");
        let u = lava_line("- {dt: x, lvl: target, msg: mmc0 ready}").unwrap();
        assert_eq!(u.level.as_deref(), Some("target"));
        assert!(u.text.starts_with("mmc0 ready"));
    }

    #[test]
    fn a_file_that_is_not_lava_is_left_completely_alone() {
        let plain = b"[    0.000000] Linux version 6.12.9\n[    1.0] mmc0: ready\n";
        assert_eq!(detect(plain), Wrapper::None);
        assert_eq!(
            unwrap_all(plain, Wrapper::None).unwrap(),
            plain,
            "an ordinary capture must not be rewritten on its way in"
        );
    }

    #[test]
    fn something_that_looks_like_lava_but_has_no_messages_fails_loudly() {
        let fake = "- {\"dt\": \"x\", \"lvl\": \"info\"}\n- {\"dt\": \"y\", \"lvl\": \"info\"}\n";
        let err = unwrap_all(fake.as_bytes(), Wrapper::Lava).unwrap_err();
        assert_eq!(err.code, ErrorCode::IngestFailed);
    }

    #[test]
    fn a_pstore_fragment_is_recognised_and_its_header_stripped() {
        let dump = "Panic#1 Part1\n\
                    <4>[  123.456] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n\
                    <4>[  123.456] Modules linked in: foo\n";
        assert_eq!(detect(dump.as_bytes()), Wrapper::Pstore);
        let body = String::from_utf8(pstore_body(dump.as_bytes())).unwrap();
        assert!(!body.contains("Panic#1"));
        assert!(body.starts_with("<4>[  123.456] Internal error"));
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn a_pstore_dump_with_no_header_is_passed_through_unchanged() {
        let dump = "<4>[  1.0] Kernel panic - not syncing: x\n";
        assert_eq!(
            String::from_utf8(pstore_body(dump.as_bytes())).unwrap(),
            dump
        );
    }

    #[test]
    fn detection_never_panics_on_hostile_input() {
        for bad in [
            &b""[..],
            &[0xff, 0xfe, 0x00][..],
            &b"- "[..],
            &b"- {\"lvl\""[..],
        ] {
            let w = detect(bad);
            let _ = unwrap_all(bad, w);
        }
    }
}
