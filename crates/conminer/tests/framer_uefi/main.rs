//! Suite `framer-uefi` (§13) — the `uefi` profile.
//!
//! Edge cases: ANSI-heavy output · ASSERT with context dump · progress spinner
//! overwrite. This is also the profile that proves the timeout-close path, since
//! EDK2 dead-loops instead of printing a terminator (§A.3).

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_text, frame_then_idle};

#[test]
fn banner_detect() {
    let r = conminer_testkit::frame::frame_auto(&corpus_text("uefi/assert-deadloop.log"));
    assert!(r.stage_names().contains(&"uefi"), "{:?}", r.stage_names());
}

#[test]
fn an_assert_with_its_context_dump_is_one_record() {
    let r = frame_text("uefi", &corpus_text("uefi/assert-deadloop.log"));
    r.assert_conserved();
    let a = r.record_containing("ASSERT_EFI_ERROR");
    let lines = r.record_lines(a);
    assert!(lines.iter().any(|l| l.contains("ExceptionData")));
    assert!(lines.iter().any(|l| l.contains("ESR :")));
    assert!(lines.iter().any(|l| l.contains("FAR :")));
    assert_eq!(a.severity, Severity::Crit);
}

#[test]
fn ansi_is_stripped_from_the_view_and_kept_in_the_bytes() {
    let text = corpus_text("uefi/ansi-exception.log");
    assert!(
        text.contains('\u{1b}'),
        "the corpus must actually contain ANSI"
    );
    let r = frame_text("uefi", &text);
    let exc = r.record_containing("X64 Exception Type");
    assert!(
        !exc.text.contains('\u{1b}'),
        "the display view strips ANSI: {:?}",
        exc.text
    );
    assert_eq!(exc.severity, Severity::Emerg);
    // The record still spans its register dump.
    assert!(exc.line_count >= 3, "{}", exc.line_count);
}

#[test]
fn a_progress_spinner_overwrites_in_place_and_is_one_record() {
    // A real EDK2 spinner rewrites one line with CR. The splitter must not treat
    // those CRs as line breaks, so eleven frames are one record that renders as
    // its final state — and one template, not eleven.
    let frames: String = (0..=10)
        .map(|i| format!("Progress |{}{}|\r", "=".repeat(i), " ".repeat(10 - i)))
        .collect::<String>()
        + "\nDone\n";
    let r = frame_text("uefi", &frames);
    assert_eq!(r.records.len(), 2, "the spinner line plus `Done`");
    assert_eq!(
        conminer_core::linesplit::render_overwrites(&r.lines[0]),
        "Progress |==========|"
    );
}

#[test]
fn a_dead_looping_fault_closes_on_silence_not_on_a_terminator() {
    // EDK2's CpuDeadLoop prints nothing further; DEAD_AIR is the only close.
    let r = frame_then_idle(
        "uefi",
        "Synchronous Exception at 0x00000000FFEE0000\n\
         X0 : 0x0000000000000000  X1 : 0x00000000FFFFFFFF\n\
         ESR : 0x0000000096000010\n",
        60_000,
    );
    let c = r.crashes();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].line_count, 3);
    assert_eq!(c[0].fields["closed_by"], "dead_air");
}

#[test]
fn the_uefi_record_timeout_is_raised_above_the_global_default() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let uefi = profiles.get("uefi").unwrap();
    let default = conminer_core::config::FramerConfig::default().record_timeout_s;
    assert!(
        uefi.record_timeout_s.unwrap() > default,
        "a dead-looping profile needs longer than the global {default}s"
    );
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "uefi",
        "Synchronous Exception at 0x00000000FFEE0000\nX0 : 0x0\n",
    );
    assert!(r.crashes()[0].truncated);
}
