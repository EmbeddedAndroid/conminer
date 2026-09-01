//! Suite `framer-freertos` (§13) — FreeRTOS and vendor dialects (§A.7).
//!
//! Core FreeRTOS prints nothing on failure, so this suite tests two things: the
//! ESP-IDF dialect in detail (it dominates in the wild), and that the generic
//! heuristic tier still catches a project that only prints "stack overflow".

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};

#[test]
fn banner_detect_via_the_reset_cause_line() {
    let r = frame_auto(&corpus_text("freertos/esp-panic.log"));
    assert!(
        r.stage_names().contains(&"freertos"),
        "{:?}",
        r.stage_names()
    );
}

#[test]
fn the_esp_idf_log_levels_classify() {
    for (line, want) in [
        ("E (2200) sensor: i2c read failed", Severity::Err),
        ("W (1200) wifi: exceed max band", Severity::Warn),
        ("I (299) cpu_start: Starting scheduler", Severity::Info),
    ] {
        let r = frame_text("freertos", &format!("{line}\n"));
        assert_eq!(r.records[0].severity, want, "{line:?}");
    }
}

#[test]
fn the_esp_tag_is_extracted_and_the_timestamp_left_out_of_the_key() {
    let r = frame_text(
        "freertos",
        "E (2200) sensor: i2c read failed: ESP_ERR_TIMEOUT\n",
    );
    assert_eq!(r.records[0].fields["esp_tag"], "sensor");
    assert_eq!(
        r.records[0].mine_key.as_deref(),
        Some("sensor: i2c read failed: ESP_ERR_TIMEOUT")
    );
}

#[test]
fn a_guru_meditation_carries_its_register_dump_and_backtrace() {
    let r = frame_text("freertos", &corpus_text("freertos/esp-panic.log"));
    r.assert_conserved();
    let c = r.record_containing("Guru Meditation");
    let lines = r.record_lines(c);
    assert!(lines.iter().any(|l| l.contains("register dump")));
    assert!(lines.iter().any(|l| l.contains("EXCCAUSE")));
    assert!(lines.iter().any(|l| l.starts_with("Backtrace:")));
    assert_eq!(c.severity, Severity::Emerg);
    assert_eq!(c.fields["closed_by"], "terminator");
    assert!(
        r.record_lines(c).iter().any(|l| l.contains("Rebooting...")),
        "the reboot line ends the dump and belongs inside it"
    );
}

#[test]
fn the_reset_cause_is_extracted_and_is_a_reset_marker() {
    let r = frame_text(
        "freertos",
        "rst:0xc (SW_CPU_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)\n",
    );
    assert_eq!(r.records[0].fields["reset_cause"], "SW_CPU_RESET");

    // Two boots in one capture must be seen as two.
    let auto = frame_auto(&corpus_text("freertos/esp-panic.log"));
    assert!(
        auto.stages.iter().any(|s| s.is_reset),
        "the second rst: line is a reset marker: {:?}",
        auto.stages
    );
}

#[test]
fn a_stack_overflow_hook_is_caught() {
    let r = frame_text(
        "freertos",
        "***ERROR*** A stack overflow in task sensor has been detected.\n",
    );
    assert_eq!(r.crashes().len(), 1);
    assert_eq!(r.crashes()[0].severity, Severity::Emerg);
}

#[test]
fn the_generic_heuristic_tier_catches_a_bare_project() {
    // No ESP-IDF, no vendor strings — just what a project's own hook printed.
    for line in [
        "FATAL: stack overflow in task 'net'",
        "assert failed: xQueueSend queue.c:412 (pxQueue)",
        "malloc failed, heap exhausted",
    ] {
        let r = frame_text("freertos", &format!("{line}\n"));
        assert!(
            r.records[0].severity <= Severity::Crit,
            "{line:?} → {:?}",
            r.records[0].severity
        );
    }
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "freertos",
        "Guru Meditation Error: Core  0 panic'ed (LoadProhibited).\nCore  0 register dump:\n",
    );
    assert!(r.crashes()[0].truncated);
}
