//! Suite `framer-optee` (§13) — OP-TEE (§A.5).
//!
//! This is the profile that proves **interleaved-dialect handling**: OP-TEE
//! shares the UART with the normal world, so its lines appear *inside* Linux
//! output. The stage machine must not switch away from the kernel, and the
//! interleaved lines must still be attributed to OP-TEE rather than to Linux.

use conminer_core::store::Severity;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};

#[test]
fn banner_detect() {
    let r = frame_text(
        "optee",
        "I/TC: OP-TEE version: 4.5.0 (gcc version 14.2.0)\n",
    );
    assert_eq!(r.records[0].severity, Severity::Info);
}

#[test]
fn optee_lines_interleave_inside_linux_without_stealing_the_stage() {
    let r = frame_auto(&corpus_text("optee/interleaved.log"));
    r.assert_conserved();

    assert_eq!(
        r.stage_names(),
        vec!["kernel"],
        "an overlay dialect must not become the active stage"
    );

    // …but the OP-TEE lines are attributed to OP-TEE, not to the kernel.
    let tee = r.record_containing("Core data-abort");
    assert_eq!(tee.profile, "optee");
    let kernel = r.record_containing("optee: probing for conduit method");
    assert_eq!(kernel.profile, "linux");
}

#[test]
fn the_prefix_encodes_both_severity_and_origin() {
    for (line, sev, origin) in [
        (
            "E/TC:0 0 tee_ta_init_pseudo_ta_session:283 failed",
            Severity::Err,
            "TC",
        ),
        ("E/LD:  Can't find ELF cff7d191", Severity::Err, "LD"),
        ("I/TC: Primary CPU initializing", Severity::Info, "TC"),
        ("D/TC:0 0 tee_ta_init", Severity::Debug, "TC"),
    ] {
        let r = frame_text("optee", &format!("{line}\n"));
        assert_eq!(r.records[0].severity, sev, "{line:?}");
        assert_eq!(r.records[0].fields["origin"], origin, "{line:?}");
    }
}

#[test]
fn an_abort_carries_its_call_stack_and_the_relocation_hint() {
    let r = frame_text("optee", &corpus_text("optee/interleaved.log"));
    r.assert_conserved();
    let c = r.record_containing("Core data-abort");
    let lines = r.record_lines(c);
    assert!(lines.iter().any(|l| l.contains("esr 0x96000006")));
    assert!(lines.iter().any(|l| l.contains("Call stack:")));
    assert!(
        lines.iter().any(|l| l.contains("TEE load address")),
        "the relocation hint a human needs to symbolize must stay in the record"
    );
}

#[test]
fn a_ta_panic_is_a_crash_record() {
    let r = frame_text("optee", "E/TC:0 0 TA panicked with code 0xffff0006\n");
    assert_eq!(r.crashes().len(), 1);
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "optee",
        "E/TC:0 0 Core data-abort at address 0x0000000000000010\nE/TC:0 0  esr 0x96000006\n",
    );
    assert!(r.crashes()[0].truncated);
}
