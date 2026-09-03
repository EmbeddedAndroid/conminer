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

/// §F2. The EDK2 SEC banner is a VERSION, not just the marker that opens a stage.
///
/// `ArmPlatformPkg/PeilessSec` writes "UEFI firmware (version %s built at %a on
/// %a)" straight to the serial port rather than through DEBUG, so it survives a
/// release build with debug output off, and the %s is
/// `PcdFirmwareVersionString`: the field a project stamps its build fingerprint
/// into. The profile already saw the line twice over, as a `banners` entry that
/// enters the uefi stage and as an `[extract]` field that becomes a template
/// slot, and neither of those reaches `epoch_versions`. So the one line that
/// says which BL33 is running never became a version.
///
/// An epoch that ENTERED the uefi stage off this very line still reported
/// `versions` = {kernel, machine}, so a BL33 build could not be verified
/// through conminer at all.
#[test]
fn the_edk2_firmware_banner_becomes_a_uefi_version() {
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text(
        "/dev/ttyUSBedk2",
        Some("uefi"),
        "UEFI firmware (version BUILDFP-260903-181219 built at 19:19:40 on Sep  3 2026)\n",
    );
    let boot = store.list_boots(5).unwrap()[0].id;
    let versions = store.versions_in_boot(boot).unwrap();
    let uefi = versions
        .iter()
        .find(|(c, _)| c == "uefi")
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("the banner must identify BL33: {versions:?}"));
    assert_eq!(
        uefi["version"], "BUILDFP-260903-181219",
        "the fingerprint is what PcdFirmwareVersionString carries: {uefi}"
    );
    // The date is the half that distinguishes two builds stamped alike, so it
    // has to survive as detail rather than be parsed away.
    assert_eq!(
        uefi["build_date"], "19:19:40 on Sep  3 2026",
        "the banner's build date belongs with it: {uefi}"
    );
}

/// The other half: this must identify a BL33, not decorate every uefi boot.
///
/// A version that appears when the board never printed one is worse than no
/// version, because provenance would then compare a claim against noise.
#[test]
fn uefi_output_without_that_banner_yields_no_uefi_version() {
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text(
        "/dev/ttyUSBedk2quiet",
        Some("uefi"),
        "[Bds] Entry...\n\
         BdsDxe: loading Boot0001 \"UEFI Shell\" from Fv\n\
         Shell> \n",
    );
    let boot = store.list_boots(5).unwrap()[0].id;
    let versions = store.versions_in_boot(boot).unwrap();
    assert!(
        !versions.iter().any(|(c, _)| c == "uefi"),
        "a uefi boot that printed no firmware banner has not said what it is: {versions:?}"
    );
}

/// A banner cut off by a reset or a garbled line is not a version either.
///
/// The pattern requires the closing parenthesis precisely so a half-arrived line
/// cannot be stored as the running build: `version BUILDFP-...` with the rest of
/// the line missing would otherwise read as a complete answer.
#[test]
fn a_truncated_firmware_banner_is_not_stored_as_a_version() {
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text(
        "/dev/ttyUSBedk2cut",
        Some("uefi"),
        "UEFI firmware (version BUILDFP-260903-181219 built at 19:19:4\n",
    );
    let boot = store.list_boots(5).unwrap()[0].id;
    let versions = store.versions_in_boot(boot).unwrap();
    assert!(
        !versions.iter().any(|(c, _)| c == "uefi"),
        "half a banner is not a build: {versions:?}"
    );
}

/// PcdFirmwareVersionString is whatever the platform DSC puts in it.
///
/// edk2's own tree ships `L"2.7"` and `L"$(FIRMWARE_VER)"`, a build macro, and
/// the three call sites that print this banner (ArmPlatformPkg/Sec,
/// ArmPlatformPkg/PeilessSec, ArmVirtPkg/PrePi) all pass that PCD straight
/// through. A project that stamps "1.0 RC1", or a date, into it is doing
/// nothing unusual, and a version capture of `\S+` matches NOTHING on such a
/// board: the banner is there on the console and conminer reports no BL33 at
/// all, which reads as "that stage printed nothing" rather than as a gap.
#[test]
fn a_firmware_version_containing_spaces_is_captured_whole() {
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text(
        "/dev/ttyUSBedk2spaced",
        Some("uefi"),
        "UEFI firmware (version 1.0 RC1 built at 19:19:40 on Sep  3 2026)\n",
    );
    let boot = store.list_boots(5).unwrap()[0].id;
    let versions = store.versions_in_boot(boot).unwrap();
    let uefi = versions
        .iter()
        .find(|(c, _)| c == "uefi")
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("a spaced version is still a version: {versions:?}"));
    assert_eq!(
        uefi["version"], "1.0 RC1",
        "the capture stops at the first ` built at `, not at the first space: {uefi}"
    );
    assert_eq!(uefi["build_date"], "19:19:40 on Sep  3 2026", "{uefi}");
}

/// The direction that loosening the capture could have broken.
///
/// An unset PCD prints `(version  built at ...)` with nothing between the
/// spaces. That must stay unclaimed: a board that did not say what it is
/// running has not said it, and inventing " " or "built" as its version would
/// put a false answer in front of the one check meant to catch a stale image.
#[test]
fn an_unset_firmware_version_claims_nothing() {
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text(
        "/dev/ttyUSBedk2unset",
        Some("uefi"),
        "UEFI firmware (version  built at 19:19:40 on Sep  3 2026)\n",
    );
    let boot = store.list_boots(5).unwrap()[0].id;
    let versions = store.versions_in_boot(boot).unwrap();
    assert!(
        !versions.iter().any(|(c, _)| c == "uefi"),
        "an empty version string is not a build: {versions:?}"
    );
}
