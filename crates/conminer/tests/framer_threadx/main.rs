//! Suite `framer-threadx` (§13) — ThreadX / Azure RTOS (§A.8).
//!
//! ThreadX is silent on failure; the reliable trigger is the vendor HardFault
//! handler dumping Cortex-M `SCB` register names. This suite locks that in, and
//! locks in the expectation that per-project profiles are the norm here.

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};

#[test]
fn banner_detect() {
    let r = frame_auto("Azure RTOS ThreadX version 6.4.0\n");
    assert_eq!(r.stage_names(), vec!["threadx"]);
}

#[test]
fn a_hard_fault_pulls_in_the_scb_registers_that_preceded_it() {
    let r = frame_text("threadx", &corpus_text("threadx/hardfault.log"));
    r.assert_conserved();

    let c = r.crashes();
    assert_eq!(
        c.len(),
        1,
        "{:?}",
        c.iter().map(|x| x.line_count).collect::<Vec<_>>()
    );
    let lines = r.record_lines(c[0]);
    assert!(
        lines.iter().any(|l| l.starts_with("CFSR")),
        "the SCB dump printed *before* the fault line must retro-attach: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("hard fault on thread")));
    assert!(lines.iter().any(|l| l.contains("xpsr")));
    assert_eq!(c[0].severity, Severity::Emerg);
}

#[test]
fn threadx_error_codes_are_record_worthy() {
    let r = frame_text("threadx", "TX_STACK_ERROR on thread sensor\n");
    assert!(r.records[0].severity <= Severity::Crit);
}

#[test]
fn a_reset_is_detected() {
    let mut auto = frame_auto("Azure RTOS ThreadX version 6.4.0\nsystem reset\n");
    auto.records.clear();
    assert!(auto.stages.iter().any(|s| s.is_reset), "{:?}", auto.stages);
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "threadx",
        "hard fault on thread: sensor\nr0: 0x0  r1: 0x20001000\n",
    );
    assert!(r.crashes()[0].truncated);
}
