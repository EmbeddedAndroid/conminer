//! Suite `framer-uboot` (§13) — the `uboot` profile.
//!
//! Edge cases: SPL→U-Boot handoff · exception dump · autoboot countdown ·
//! env dump · interrupted boot (keypress).

use conminer_core::store::{RecordKind, Severity};
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_auto, frame_text};

#[test]
fn banner_detect_spl_then_uboot_then_kernel_handoff() {
    let r = frame_auto(&corpus_text("uboot/spl-to-kernel.log"));
    let stages = r.stage_names();
    assert!(stages.contains(&"spl"), "{stages:?}");
    assert!(stages.contains(&"uboot"), "{stages:?}");
    assert!(stages.contains(&"kernel-handoff"), "{stages:?}");
    // SPL must be seen before U-Boot proper.
    let spl = stages.iter().position(|s| *s == "spl").unwrap();
    let ub = stages.iter().position(|s| *s == "uboot").unwrap();
    assert!(spl < ub);
    r.assert_conserved();
}

#[test]
fn the_uboot_version_is_extracted() {
    let r = frame_text("uboot", "U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n");
    assert_eq!(r.records[0].fields["uboot_version"], "2026.01");
}

#[test]
fn a_data_abort_dump_is_one_record() {
    let r = frame_text(
        "uboot",
        "data abort\n\
         pc : [<00000000801020a0>]          lr : [<0000000080102000>]\n\
         sp : 000000007ef00000 ip : 0000000000000000 fp : 0000000000000000\n\
         Code: d10083ff a9017bfd 910043fd f9000fe0\n\
         resetting ...\n",
    );
    r.assert_conserved();
    let crashes = r.crashes();
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].line_count, 5);
    assert_eq!(crashes[0].severity, Severity::Emerg);
    assert_eq!(crashes[0].fields["closed_by"], "terminator");
}

#[test]
fn an_autoboot_countdown_is_a_single_record_rendering_its_final_state() {
    // The bytes carry every rewrite; the display view carries the final text.
    let raw = "Hit any key to stop autoboot:  2 \u{8}\u{8}\u{8} 1 \u{8}\u{8}\u{8} 0 \n";
    let r = frame_text("uboot", raw);
    assert_eq!(r.records.len(), 1);
    assert_eq!(
        conminer_core::linesplit::render_overwrites(&r.lines[0]),
        "Hit any key to stop autoboot:  0 "
    );
}

#[test]
fn an_env_dump_collapses_to_few_templates_via_the_key_value_rule() {
    use conminer_core::drain::{mine_all, DrainConfig};
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let uboot = profiles.get("uboot").unwrap();
    assert!(
        uboot.tokenizer.split_key_value,
        "the uboot profile must enable the KEY_VALUE_RUN tokenizer rule (§6)"
    );

    let lines: Vec<String> = (0..20)
        .map(|i| format!("bootargs=console=ttyS0 bootdelay={i} baudrate=115200 ipaddr=10.0.0.{i}"))
        .collect();
    let with = mine_all(DrainConfig::default(), uboot.tokenizer.clone(), &lines);
    assert_eq!(
        with.len(),
        1,
        "the rule is what keeps env dumps to one template"
    );
}

#[test]
fn an_interrupted_boot_leaves_the_prompt_as_ordinary_traffic() {
    let r = frame_text(
        "uboot",
        "Hit any key to stop autoboot:  0 \n=> \n=> printenv baudrate\nbaudrate=115200\n",
    );
    r.assert_conserved();
    assert!(r.crashes().is_empty(), "a keypress is not a crash");
    assert!(r.records.iter().all(|x| x.kind == RecordKind::Line));
}

#[test]
fn the_prompt_is_declared_as_a_bootloader_prompt() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let uboot = profiles.get("uboot").unwrap();
    let p = uboot
        .prompt_match("=> ")
        .expect("=> must be a known prompt");
    assert_eq!(
        p.kind,
        conminer_core::framer::profile::PromptKind::Bootloader
    );
    assert!(p.kind.is_commandable());
}

#[test]
fn mid_record_stream_loss_is_flagged_truncated() {
    let r = frame_text(
        "uboot",
        "data abort\npc : [<00000000801020a0>]          lr : [<0000000080102000>]\n",
    );
    assert_eq!(r.crashes().len(), 1);
    assert!(r.crashes()[0].truncated);
}
