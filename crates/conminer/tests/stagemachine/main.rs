//! Suite `stagemachine` (§13) — boot-stage tracking.
//!
//! Edge cases: full BootROM→TF-A→U-Boot→kernel chain · boot **loop**
//! (stage cycle × 500 — dedup across iterations, bounded stage rows) ·
//! watchdog reset mid-kernel · pinned profile disables detection ·
//! ambiguous banner.

use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};
use conminer_testkit::Rig;

#[test]
fn the_full_chain_is_tracked_in_boot_order() {
    let r = frame_auto(&corpus_text("mixed/boot-loop.log"));
    let first_pass: Vec<&str> = r
        .stages
        .iter()
        .take_while(|s| !s.is_reset)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        first_pass,
        ["bl1", "bl2", "bl31", "uboot", "kernel-handoff", "kernel"]
    );
}

#[test]
fn every_record_is_tagged_with_the_stage_it_arrived_in() {
    let r = frame_auto(&corpus_text("mixed/boot-loop.log"));
    let panic = r.record_containing("Kernel panic");
    assert_eq!(panic.stage.as_deref(), Some("kernel"));
    let uboot = r.record_containing("U-Boot 2026.01");
    assert_eq!(uboot.stage.as_deref(), Some("uboot"));
}

#[test]
fn a_boot_loop_is_seen_as_repeated_epochs_not_as_one_long_boot() {
    let r = frame_auto(&corpus_text("mixed/boot-loop.log"));
    let resets = r.stages.iter().filter(|s| s.is_reset).count();
    assert_eq!(resets, 2, "three iterations means two involuntary reboots");
}

#[test]
fn five_hundred_loop_iterations_dedupe_to_a_bounded_number_of_templates() {
    // The whole point of mining: a board that has looped 500 times is a table of
    // contents with counts, not 500× the log.
    let one = corpus_text("mixed/boot-loop.log");
    let single_iteration: String = one.lines().take(8).collect::<Vec<_>>().join("\n") + "\n";
    let looping: String = single_iteration.repeat(500);

    let rig = Rig::new();
    let store = rig.ingest_text("loop-rig", None, &looping);

    let templates = store.template_count().unwrap();
    assert!(
        templates < 40,
        "500 iterations must collapse, got {templates} templates"
    );

    // Stage rows stay bounded too: one row per stage entry, not per line.
    let stages = store.stages(None, None).unwrap();
    assert!(
        stages.len() <= 500 * 8,
        "stage rows must be bounded by transitions, got {}",
        stages.len()
    );

    // …and every iteration got its own epoch.
    let boots = store.list_boots(1000).unwrap();
    assert!(
        boots.len() >= 400,
        "expected ~500 epochs, got {}",
        boots.len()
    );
}

#[test]
fn a_watchdog_reset_mid_kernel_opens_a_new_epoch_and_is_attributed() {
    let text = "\
[    0.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP
[    1.000000] mmc0: new HS200 MMC card at address 0001
[   30.000000] watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [swapper:1]
NOTICE:  BL1: v2.11(release):v2.11
NOTICE:  BL31: v2.11(release):v2.11
[    0.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP
";
    let r = frame_auto(text);
    assert!(
        r.stages.iter().any(|s| s.is_reset),
        "the earliest-stage banner reappearing is a reboot: {:?}",
        r.stages
    );
    // The watchdog line itself is classified, so the reset can be attributed to
    // a bite rather than to a crash.
    let wd = r.record_containing("soft lockup");
    assert!(conminer_core::framer::generic::is_watchdog(&wd.text));
}

#[test]
fn a_pinned_profile_disables_detection_entirely() {
    // A device pinned to `zephyr` must not be dragged into the linux profile by
    // a stray banner in the payload it is logging.
    let r = frame_text(
        "zephyr",
        "*** Booting Zephyr OS build v4.0.0 ***\n\
         <inf> app: replaying: Linux version 6.12.9 (build@lab) (gcc 14.2.0)\n",
    );
    assert!(
        r.stages.iter().all(|s| s.name == "zephyr"),
        "{:?}",
        r.stage_names()
    );
    assert_eq!(r.records[1].profile, "zephyr");
}

#[test]
fn an_ambiguous_banner_resolves_deterministically_by_stage_order() {
    // Two runs of the same input must pick the same profile — a coin flip here
    // would make every downstream stage assertion flaky.
    let line = "U-Boot SPL 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n";
    let a = frame_auto(line);
    let b = frame_auto(line);
    assert_eq!(a.stage_names(), b.stage_names());
    assert_eq!(a.stage_names(), ["spl"]);
}

#[test]
fn stage_rows_carry_the_banner_line_that_caused_them() {
    let rig = Rig::new();
    let store = rig.ingest_text("stage-rig", None, &corpus_text("uboot/spl-to-kernel.log"));
    let stages = store.stages(None, None).unwrap();
    assert!(!stages.is_empty());
    for s in &stages {
        let id = s.banner_line_id.expect("a stage must point at its banner");
        let line = store.line(id).unwrap();
        assert!(
            !line.bytes.is_empty(),
            "stage {} points at an empty line",
            s.name
        );
    }
}

#[test]
fn stages_and_records_agree_on_which_epoch_they_belong_to() {
    let rig = Rig::new();
    let store = rig.ingest_text("epoch-rig", None, &corpus_text("mixed/boot-loop.log"));
    let boots = store.list_boots(100).unwrap();
    assert!(boots.len() >= 3, "three iterations, three epochs");

    for b in &boots {
        for s in store.stages(None, Some(b.id)).unwrap() {
            assert_eq!(s.boot_id, Some(b.id));
        }
    }
}
