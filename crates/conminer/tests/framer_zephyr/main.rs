//! Suite `framer-zephyr` (§13) — Zephyr (§A.6).
//!
//! The profile that proves **retro-attachment**: the Cortex-M fault decode
//! arrives *before* the FATAL banner, so the framer must pull the preceding
//! lines into the record rather than emitting two half-crashes.

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};

#[test]
fn banner_detect_and_build_extraction() {
    let r = frame_text(
        "zephyr",
        "*** Booting Zephyr OS build v4.0.0-1234-gabcdef ***\n",
    );
    assert_eq!(r.records[0].fields["zephyr_build"], "v4.0.0-1234-gabcdef");
    let auto = frame_auto("*** Booting Zephyr OS build v4.0.0 ***\n");
    assert_eq!(auto.stage_names(), vec!["zephyr"]);
}

#[test]
fn the_fault_decode_preceding_the_fatal_banner_is_one_record() {
    let r = frame_text("zephyr", &corpus_text("zephyr/fatal.log"));
    r.assert_conserved();

    let c = r.crashes();
    assert_eq!(c.len(), 1, "a fault plus its banner is one crash, not two");
    let lines = r.record_lines(c[0]);
    assert!(lines[0].contains("HARD FAULT"), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("Stacking error")));
    assert!(lines.iter().any(|l| l.contains("ZEPHYR FATAL ERROR")));
    assert!(lines.iter().any(|l| l.contains("Current thread")));
    assert!(lines.iter().any(|l| l.contains("xpsr")));
    assert_eq!(c[0].severity, Severity::Emerg);

    // The template names the real event, not the preamble.
    assert!(
        c[0].mine_key
            .as_deref()
            .unwrap()
            .contains("ZEPHYR FATAL ERROR"),
        "{:?}",
        c[0].mine_key
    );
    assert_eq!(c[0].fields["fatal_code"], "0");
}

#[test]
fn a_fatal_banner_with_no_preamble_still_frames() {
    let r = frame_text(
        "zephyr",
        ">>> ZEPHYR FATAL ERROR 3: Kernel oops on CPU 0\nCurrent thread: 0x20001a40 (main)\nHalting system\n",
    );
    r.assert_conserved();
    assert_eq!(r.crashes().len(), 1);
    assert_eq!(r.crashes()[0].line_count, 3);
}

#[test]
fn the_log_backend_levels_classify() {
    for (line, want) in [
        ("<err> can: bus off, restarting", Severity::Err),
        ("<wrn> thermal: sensor 2 above 70C", Severity::Warn),
        ("<inf> board: online", Severity::Info),
        ("<dbg> spi: xfer", Severity::Debug),
        ("E: minimal mode error", Severity::Err),
    ] {
        let r = frame_text("zephyr", &format!("{line}\n"));
        assert_eq!(r.records[0].severity, want, "{line:?}");
    }
}

#[test]
fn the_module_name_is_extracted() {
    let r = frame_text("zephyr", "<err> spi_nor: erase failed\n");
    assert_eq!(r.records[0].fields["module"], "spi_nor");
}

#[test]
fn the_zephyr_timestamp_is_stripped_from_the_mining_key_only() {
    let r = frame_text(
        "zephyr",
        "[00:00:01.100,000] <wrn> thermal: sensor 2 above 70C\n",
    );
    assert!(r.records[0].text.starts_with("[00:00:01.100,000]"));
    assert_eq!(
        r.records[0].mine_key.as_deref(),
        Some("<wrn> thermal: sensor 2 above 70C")
    );
}

#[test]
fn the_shell_prompt_is_declared_as_an_rtos_shell() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let z = profiles.get("zephyr").unwrap();
    let p = z.prompt_match("uart:~$ ").expect("uart:~$ must be known");
    assert_eq!(
        p.kind,
        conminer_core::framer::profile::PromptKind::RtosShell
    );
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "zephyr",
        ">>> ZEPHYR FATAL ERROR 0: CPU exception on CPU 0\nCurrent thread: 0x1\n",
    );
    assert!(r.crashes()[0].truncated);
}
