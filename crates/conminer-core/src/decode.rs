//! Silicon decode: turning the numbers a console prints into what they mean.
//!
//! Embedded consoles communicate in bare integers. `-110`, `INTID 61`,
//! `ESR 0x96000021`, `0x088e1000`: each is a fact, and each costs an agent a
//! detour to look up, every session, forever. The lookups are small, stable and
//! entirely mechanical, so they belong in the tool rather than in the agent's
//! context window.
//!
//! Three rules keep this honest:
//!
//! * **Decoding never rewrites anything.** It is a derived annotation beside the
//!   raw token, exactly like [`crate::values`] and symbolization. The `-110` in
//!   the record stays `-110`.
//! * **Ambiguity is reported, not resolved.** A bare `61` could be an errno, a
//!   GIC INTID or a line number. Every plausible reading is returned with the
//!   assumption it rests on, because picking one silently is how an agent ends
//!   up confidently chasing the wrong interrupt.
//! * **Nothing is invented.** A value with no known meaning decodes to nothing,
//!   not to a guess.

use crate::config::MemoryRegion;
use serde::Serialize;

/// One reading of a value.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reading {
    /// What kind of thing this interpretation assumes the value is.
    pub kind: &'static str,
    pub meaning: String,
    /// The assumption a reader must accept for this to be the right reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assuming: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl Reading {
    fn new(kind: &'static str, meaning: impl Into<String>) -> Self {
        Self {
            kind,
            meaning: meaning.into(),
            assuming: None,
            detail: None,
        }
    }
    fn assuming(mut self, s: impl Into<String>) -> Self {
        self.assuming = Some(s.into());
        self
    }
    fn detail(mut self, v: serde_json::Value) -> Self {
        self.detail = Some(v);
        self
    }
}

/// Every plausible meaning of one token.
#[derive(Debug, Clone, Serialize)]
pub struct Decoded {
    pub token: String,
    pub readings: Vec<Reading>,
}

// ------------------------------------------------------------------ errno ----

/// Linux errno numbers, as consoles print them: usually negative, from a driver
/// return path. Covers the generic set plus the ones that dominate bring-up.
const ERRNO: &[(i64, &str, &str)] = &[
    (1, "EPERM", "operation not permitted"),
    (2, "ENOENT", "no such file or directory"),
    (5, "EIO", "I/O error"),
    (11, "EAGAIN", "try again"),
    (12, "ENOMEM", "out of memory"),
    (13, "EACCES", "permission denied"),
    (14, "EFAULT", "bad address"),
    (16, "EBUSY", "device or resource busy"),
    (19, "ENODEV", "no such device"),
    (22, "EINVAL", "invalid argument"),
    (28, "ENOSPC", "no space left on device"),
    (34, "ERANGE", "result out of range"),
    (38, "ENOSYS", "function not implemented"),
    (52, "EBADE", "invalid exchange"),
    (60, "ETIME", "timer expired"),
    (61, "ENODATA", "no data available"),
    (62, "ETIME", "timer expired"),
    (70, "ECOMM", "communication error on send"),
    (71, "EPROTO", "protocol error"),
    (74, "EBADMSG", "bad message"),
    (75, "EOVERFLOW", "value too large"),
    (95, "EOPNOTSUPP", "operation not supported"),
    (110, "ETIMEDOUT", "connection timed out"),
    (114, "EALREADY", "operation already in progress"),
    (
        517,
        "EPROBE_DEFER",
        "probe deferred: a dependency is not ready yet",
    ),
    (
        512,
        "ERESTARTSYS",
        "interrupted syscall, should restart (should never reach userspace)",
    ),
];

/// PSCI return codes (§5.2 of the PSCI spec), which TF-A and the kernel both
/// print bare and which collide numerically with nothing else useful.
const PSCI: &[(i64, &str)] = &[
    (0, "SUCCESS"),
    (-1, "NOT_SUPPORTED"),
    (-2, "INVALID_PARAMETERS"),
    (-3, "DENIED"),
    (-4, "ALREADY_ON"),
    (-5, "ON_PENDING"),
    (-6, "INTERNAL_FAILURE"),
    (-7, "NOT_PRESENT"),
    (-8, "DISABLED"),
    (-9, "INVALID_ADDRESS"),
];

/// AArch64 `ESR_ELx.EC` exception classes: the field that says *what kind* of
/// fault this was, and the first thing anyone reads off a crash dump.
const ESR_EC: &[(u64, &str)] = &[
    (0x00, "unknown reason"),
    (0x01, "trapped WFI/WFE"),
    (0x03, "trapped MCR/MRC (CP15)"),
    (0x07, "trapped SVE/SIMD/FP access"),
    (0x0e, "illegal execution state"),
    (0x11, "SVC (AArch32)"),
    (0x15, "SVC (AArch64)"),
    (0x16, "HVC (AArch64)"),
    (0x17, "SMC (AArch64)"),
    (0x18, "trapped MSR/MRS/system instruction"),
    (0x20, "instruction abort, lower EL"),
    (0x21, "instruction abort, same EL"),
    (0x22, "PC alignment fault"),
    (0x24, "data abort, lower EL"),
    (0x25, "data abort, same EL"),
    (0x26, "SP alignment fault"),
    (0x2c, "trapped FP exception"),
    (0x2f, "SError interrupt"),
    (0x30, "breakpoint, lower EL"),
    (0x31, "breakpoint, same EL"),
    (0x3c, "BRK instruction"),
];

/// `ESR_ELx.ISS.DFSC` / `IFSC` fault status codes for aborts.
const FSC: &[(u64, &str)] = &[
    (0x00, "address size fault, level 0"),
    (0x04, "translation fault, level 0"),
    (0x05, "translation fault, level 1"),
    (0x06, "translation fault, level 2"),
    (0x07, "translation fault, level 3"),
    (0x09, "access flag fault, level 1"),
    (0x0a, "access flag fault, level 2"),
    (0x0b, "access flag fault, level 3"),
    (0x0d, "permission fault, level 1"),
    (0x0e, "permission fault, level 2"),
    (0x0f, "permission fault, level 3"),
    (0x10, "synchronous external abort"),
    (0x11, "synchronous tag check fault"),
    (
        0x15,
        "synchronous external abort on translation table walk, level 1",
    ),
    (
        0x16,
        "synchronous external abort on translation table walk, level 2",
    ),
    (0x21, "alignment fault"),
    (0x30, "TLB conflict abort"),
];

// --------------------------------------------------------------- decoding ----

/// Decode one token, returning every plausible reading.
pub fn decode_token(token: &str, regions: &[MemoryRegion]) -> Decoded {
    let t = token.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | ')' | ']' | '.' | '"'));
    let mut readings = Vec::new();

    if let Some(v) = parse_int(t) {
        readings.extend(decode_int(v, t, regions));
    }
    Decoded {
        token: token.to_string(),
        readings,
    }
}

/// Decode every number in a line. This is what an agent actually wants: hand it
/// a crash line, get back what each field means.
pub fn decode_line(line: &str, regions: &[MemoryRegion]) -> Vec<Decoded> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    // Field-aware readings first: a labelled `ESR 0x…` is unambiguous in a way a
    // bare hex number never is, so the label is used when it is there.
    for (label, value) in labelled_fields(line) {
        if let Some(v) = parse_int(&value) {
            let readings = match label.as_str() {
                "esr" => decode_esr(v as u64),
                "intid" | "irq" | "hwirq" => vec![gic_reading(v)],
                "err" | "errno" | "ret" | "rc" | "status" => errno_readings(v),
                _ => continue,
            };
            if !readings.is_empty() && seen.insert(value.clone()) {
                out.push(Decoded {
                    token: format!("{label} {value}"),
                    readings,
                });
            }
        }
    }

    for tok in line.split(|c: char| c.is_whitespace() || c == '=' || c == '(') {
        let d = decode_token(tok, regions);
        if !d.readings.is_empty() && seen.insert(d.token.clone()) {
            out.push(d);
        }
    }
    out
}

/// `KEY 0xVALUE` / `KEY=VALUE` / `KEY: VALUE` pairs, lowercased.
fn labelled_fields(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let toks: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|s| !s.is_empty())
        .collect();
    for (i, t) in toks.iter().enumerate() {
        if let Some((k, v)) = t.split_once('=') {
            if !v.is_empty() {
                out.push((clean_key(k), v.to_string()));
            }
            continue;
        }
        let key = clean_key(t);
        if key.is_empty() {
            continue;
        }
        if let Some(next) = toks.get(i + 1) {
            out.push((key, (*next).to_string()));
        }
    }
    out
}

fn clean_key(k: &str) -> String {
    k.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .to_ascii_lowercase()
}

fn decode_int(v: i64, raw: &str, regions: &[MemoryRegion]) -> Vec<Reading> {
    let is_hex = raw.trim_start_matches(['-', '+']).starts_with("0x")
        || raw.trim_start_matches(['-', '+']).starts_with("0X");
    let mut out = Vec::new();

    // A negative small integer on an embedded console is an errno far more often
    // than it is anything else, so it leads.
    if v < 0 {
        out.extend(errno_readings(v));
        if let Some((_, name)) = PSCI.iter().find(|(c, _)| *c == v) {
            out.push(
                Reading::new("psci", format!("PSCI {name}"))
                    .assuming("a PSCI return value".to_string()),
            );
        }
    }

    if is_hex {
        let u = v as u64;
        // A plausible ESR only if the EC field is one we know and the value has
        // the shape of a 32-bit syndrome register.
        if u > 0xffff && u <= 0xffff_ffff {
            let ec = (u >> 26) & 0x3f;
            if ESR_EC.iter().any(|(c, _)| *c == ec) {
                out.extend(
                    decode_esr(u)
                        .into_iter()
                        .map(|r| r.assuming("an AArch64 ESR_ELx value".to_string())),
                );
            }
        }
        if let Some(r) = region_reading(u, regions) {
            out.push(r);
        }
    } else if (0..=1020).contains(&v) {
        // Bare small positive integers: could be an interrupt id, could be an
        // errno printed without its sign. Both readings, both labelled.
        out.push(gic_reading(v));
        if let Some((_, name, desc)) = ERRNO.iter().find(|(n, _, _)| *n == v) {
            out.push(
                Reading::new("errno", format!("{name}: {desc}"))
                    .assuming("an errno printed without its sign".to_string()),
            );
        }
    }
    out
}

fn errno_readings(v: i64) -> Vec<Reading> {
    let n = v.abs();
    match ERRNO.iter().find(|(c, _, _)| *c == n) {
        Some((_, name, desc)) => vec![Reading::new("errno", format!("{name}: {desc}"))],
        None => Vec::new(),
    }
}

/// GIC interrupt-id arithmetic.
///
/// The trap this exists for: a device tree writes `GIC_SPI 436`, the GIC and
/// every register dump speak INTID 468, and the 32 between them has cost more
/// than one afternoon. Both numbers are always shown, whichever was given.
fn gic_reading(intid: i64) -> Reading {
    let (class, dt) = match intid {
        0..=15 => ("SGI", None),
        16..=31 => ("PPI", Some(intid - 16)),
        32..=1019 => ("SPI", Some(intid - 32)),
        1020..=1023 => ("special", None),
        _ => ("out of range", None),
    };
    let meaning = match dt {
        Some(d) => format!("GIC INTID {intid} = {class} {d} (device tree writes `{class} {d}`)"),
        None => format!("GIC INTID {intid} = {class}"),
    };
    Reading::new("gic", meaning)
        .assuming("a GIC interrupt id".to_string())
        .detail(serde_json::json!({
            "intid": intid, "class": class, "dt_number": dt,
            // The inverse, because a reader usually has one and wants the other.
            "if_dt_spi_then_intid": (0..=987).contains(&intid).then_some(intid + 32),
        }))
}

fn decode_esr(esr: u64) -> Vec<Reading> {
    let ec = (esr >> 26) & 0x3f;
    let il = (esr >> 25) & 1;
    let iss = esr & 0x01ff_ffff;
    let Some((_, what)) = ESR_EC.iter().find(|(c, _)| *c == ec) else {
        return Vec::new();
    };

    let mut meaning = format!("EC 0x{ec:02x}: {what}");
    let mut detail = serde_json::json!({
        "ec": ec, "il": il, "iss": iss, "class": what,
    });

    // For aborts the fault status code is the part that says *why*.
    if matches!(ec, 0x20 | 0x21 | 0x24 | 0x25) {
        let dfsc = iss & 0x3f;
        let wnr = (iss >> 6) & 1;
        if let Some((_, fs)) = FSC.iter().find(|(c, _)| *c == dfsc) {
            meaning = format!("EC 0x{ec:02x}: {what}, {fs}");
            detail["fault_status"] = serde_json::json!(fs);
        }
        if matches!(ec, 0x24 | 0x25) {
            detail["access"] = serde_json::json!(if wnr == 1 { "write" } else { "read" });
        }
        detail["dfsc"] = serde_json::json!(dfsc);
    }
    vec![Reading::new("esr", meaning).detail(detail)]
}

fn region_reading(addr: u64, regions: &[MemoryRegion]) -> Option<Reading> {
    let r = regions
        .iter()
        .find(|r| addr >= r.base && addr < r.base.saturating_add(r.size))?;
    let offset = addr - r.base;
    Some(
        Reading::new("address", format!("{} + 0x{offset:x}", r.name))
            .assuming("an address in this device's memory map".to_string())
            .detail(serde_json::json!({
                "region": r.name, "base": format!("0x{:x}", r.base),
                "offset": format!("0x{offset:x}"),
                "note": r.note,
            })),
    )
}

fn parse_int(s: &str) -> Option<i64> {
    let neg = s.starts_with('-');
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()? as i64
    } else {
        if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        body.parse::<i64>().ok()?
    };
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions() -> Vec<MemoryRegion> {
        vec![MemoryRegion {
            name: "usb_dp_combo_phy".into(),
            base: 0x088e_1000,
            size: 0x1000,
            note: Some("DP0 combo PHY".into()),
        }]
    }

    #[test]
    fn a_negative_number_reads_as_an_errno() {
        let d = decode_token("-110", &[]);
        assert_eq!(d.readings[0].kind, "errno");
        assert!(d.readings[0].meaning.contains("ETIMEDOUT"));
    }

    #[test]
    fn the_gic_offset_of_thirty_two_is_stated_in_both_directions() {
        // The whole point: a DT `GIC_SPI 436` is INTID 468, and confusing them
        // sends an agent looking at the wrong interrupt.
        let r = gic_reading(468);
        assert!(r.meaning.contains("SPI 436"), "{}", r.meaning);
        assert_eq!(r.detail.as_ref().unwrap()["dt_number"], 436);
    }

    #[test]
    fn an_esr_decodes_to_its_class_and_fault_status() {
        // 0x96000021: EC 0x25 (data abort, same EL), IL=1, ISS 0x21 →
        // DFSC 0x21 (alignment fault) and WnR clear, so a read.
        let r = &decode_esr(0x9600_0021)[0];
        assert!(r.meaning.contains("data abort"), "{}", r.meaning);
        assert!(r.meaning.contains("alignment"), "{}", r.meaning);
        assert_eq!(r.detail.as_ref().unwrap()["access"], "read");

        // And a write, so the bit is actually being read rather than defaulted.
        let w = &decode_esr(0x9600_0061)[0];
        assert_eq!(w.detail.as_ref().unwrap()["access"], "write");
    }

    #[test]
    fn an_address_resolves_to_its_region_with_an_offset() {
        let d = decode_token("0x88e1004", &regions());
        let a = d
            .readings
            .iter()
            .find(|r| r.kind == "address")
            .expect("an address reading");
        assert_eq!(a.meaning, "usb_dp_combo_phy + 0x4");
    }

    #[test]
    fn an_ambiguous_bare_number_returns_every_reading_with_its_assumption() {
        // 61 is both a valid INTID and ENODATA. Picking one silently is how an
        // agent ends up chasing the wrong interrupt.
        let d = decode_token("61", &[]);
        let kinds: Vec<&str> = d.readings.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&"gic"), "{kinds:?}");
        assert!(kinds.contains(&"errno"), "{kinds:?}");
        assert!(d.readings.iter().all(|r| r.assuming.is_some()));
    }

    #[test]
    fn a_value_with_no_known_meaning_decodes_to_nothing() {
        assert!(decode_token("hello", &[]).readings.is_empty());
        assert!(decode_token("999999", &[]).readings.is_empty());
    }

    #[test]
    fn a_labelled_field_beats_guessing() {
        let out = decode_line("Unhandled fault: esr 0x96000021 far 0x0", &[]);
        let esr = out
            .iter()
            .find(|d| d.readings.iter().any(|r| r.kind == "esr"))
            .expect("the ESR was decoded");
        assert!(esr.readings[0].meaning.contains("data abort"));
    }

    #[test]
    fn integers_parse_in_the_forms_consoles_print() {
        assert_eq!(parse_int("0x1f"), Some(31));
        assert_eq!(parse_int("-110"), Some(-110));
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("0x"), None);
        assert_eq!(parse_int("1.5"), None);
        assert_eq!(parse_int("v2"), None);
    }
}
