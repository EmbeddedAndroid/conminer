//! Suite `framer-tfa` (§13) — Trusted Firmware-A (§A.4).
//!
//! Edge cases: banner detect; fatal/panic record with full dump;
//! profile-specific severities; mid-record stream loss.

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text, frame_then_idle};

#[test]
fn banner_detect_gives_each_bootloader_stage_its_own_name() {
    let r = frame_auto(&corpus_text("tfa/panic.log"));
    let s = r.stage_names();
    assert!(s.contains(&"bl1"), "{s:?}");
    assert!(s.contains(&"bl2"), "{s:?}");
    assert!(s.contains(&"bl31"), "{s:?}");
}

#[test]
fn the_fixed_width_severity_prefixes_classify_exactly() {
    for (line, want) in [
        ("NOTICE:  BL31: v2.11(release)", Severity::Notice),
        ("ERROR:   Failed to load BL33", Severity::Crit),
        ("WARNING: Firmware update not supported", Severity::Warn),
        ("INFO:    Boot bl33 from 0x60000000", Severity::Info),
        ("VERBOSE: bl31_setup", Severity::Debug),
    ] {
        let r = frame_text("tfa", &format!("{line}\n"));
        assert_eq!(r.records[0].severity, want, "{line:?}");
    }
}

#[test]
fn a_panic_carries_its_whole_register_file() {
    let r = frame_text("tfa", &corpus_text("tfa/panic.log"));
    r.assert_conserved();
    let c = r.crashes();
    assert_eq!(c.len(), 1);
    let lines = r.record_lines(c[0]);
    assert!(lines[0].contains("Unhandled Exception in EL3"));
    assert!(lines.iter().any(|l| l.contains("scr_el3")));
    assert!(lines.iter().any(|l| l.contains("esr_el3")));
    assert!(lines.iter().any(|l| l.contains("elr_el3")));
    assert_eq!(c[0].severity, Severity::Emerg);
}

#[test]
fn a_panic_with_no_terminator_closes_on_silence() {
    // TF-A spins after a panic; DEAD_AIR is the close (§A.4).
    let r = frame_then_idle(
        "tfa",
        "ERROR:   PANIC at PC : 0x0000000004001234\nERROR:   x0             = 0x0\n",
        60_000,
    );
    assert_eq!(r.crashes().len(), 1);
    assert_eq!(r.crashes()[0].fields["closed_by"], "dead_air");
}

#[test]
fn the_severity_prefix_is_stripped_from_the_mining_key_only() {
    let r = frame_text("tfa", "NOTICE:  BL31: v2.11(release):v2.11\n");
    assert!(r.records[0].text.starts_with("NOTICE:"));
    assert_eq!(
        r.records[0].mine_key.as_deref(),
        Some("BL31: v2.11(release):v2.11")
    );
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "tfa",
        "ERROR:   Unhandled Exception in EL3.\nERROR:   x30 = 0x4001234\n",
    );
    assert!(r.crashes()[0].truncated);
    assert_eq!(r.crashes()[0].fields["closed_by"], "stream_end");
}
