//! One gate per round-3 finding.
//!
//! These exist because a previous round was reported as fixed and re-testing on
//! hardware found most of it unfixed. A claim of "fixed" with no test behind it
//! is worth nothing, so every finding here fails loudly if it regresses.

use conminer_core::config::Config;
use conminer_core::store::{IdentityKind, Registry};
use conminer_core::usb::{in_edl, zombies, Liveness, UsbDevice};

fn tools_src() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs")
}

/// R1 (CRITICAL): a silent console must never outrank the USB probe.
///
/// Measured: with `qcom_scm.download_mode=1`, a long RESIN press ~15s into boot
/// WARM-RESETS INTO DOWNLOAD MODE instead of powering off. The console goes
/// quiet exactly as it would if the board died, so byte-counting returned
/// `verified: true` while the board sat in EDL with a live QDL gadget. An agent
/// then believes a board is off and walks away from a powered, flashable board.
#[test]
fn r1_off_is_not_verified_while_a_live_qdl_gadget_answers() {
    let src = tools_src();
    // The probe moved from a bare scan to a WATCHED one (round 4, R4): a single
    // sample cannot tell "not in EDL" from "in EDL, mid-re-enumeration". What
    // this gate cares about is unchanged and stronger -- the probe is consulted
    // before success is claimed.
    let probe = src
        .find("ok && action == \"off\" && edl.in_edl")
        .expect("off-verification must consult the EDL probe BEFORE claiming success");
    // Anchored on the CALL: the window moved from a constant to a config knob
    // (`[hooks] edl_settle_s`) so a faster bench need not wait out ours, which
    // changes nothing about the property this gate asserts.
    // ...and scoped to THIS board's ports (2026-08-16): a bench-mate sitting in
    // download mode must not decide what happens to a board that powered down.
    assert!(
        src.contains("conminer_core::usb::watch_for_edl_on_ports(edl_settle, &ports)"),
        "and it must be the watching probe, on this board's ports, not one sample taken in the gap"
    );
    let success = src
        .find("json!({\"verified\": true, \"action\": action, \"escalated\": false}),")
        .expect("the success return must exist");
    assert!(
        probe < success,
        "the EDL check must come BEFORE the success return, or silence still wins"
    );
    assert!(
        src.contains("went into EDL, not off"),
        "the caller must be told which of the two happened"
    );

    // And the underlying predicate must actually distinguish them.
    let live = UsbDevice {
        vendor_id: 0x05c6,
        product_id: 0x9008,
        bus: 3,
        address: 9,
        port_path: None,
        liveness: Liveness::Alive,
    };
    assert!(
        in_edl(std::slice::from_ref(&live)),
        "a live QDL gadget is EDL"
    );
    assert!(
        !in_edl(&[UsbDevice {
            port_path: None,
            liveness: Liveness::Dead,
            ..live
        }]),
        "a stale one is not"
    );
}

/// R2: deliberate EDL work must not be "recovered" from.
///
/// A reset with the strap set is normal EDL entry: the console goes silent by
/// design. Byte-counting waited out the full window (~70s), escalated to a power
/// cycle NOBODY ASKED FOR, and reported "it may need physical attention". An
/// unrequested power cycle destroys state on someone else's rig.
#[test]
fn r2_a_board_in_edl_is_not_escalated_against() {
    let src = tools_src();
    let edl_guard = src
        .find("A BOARD IN EDL IS NOT A FAILURE")
        .expect("the guard must exist");
    let escalation = src
        .find("power action reported success but the console disagrees; escalating once")
        .expect("the escalation site must exist");
    assert!(
        edl_guard < escalation,
        "the EDL check must short-circuit BEFORE any escalation runs"
    );
    assert!(
        src.contains("not a failure -- so nothing was escalated"),
        "the caller must be told nothing was escalated"
    );
}

/// N1: a nickname must never reach a hook.
///
/// `{device}` was substituted with `display_name()`, which is the nickname once
/// set. The bughopper hook resolves an FTDI by its by-id path, so it matched
/// four devices and failed DEVICE_GONE -- naming a board permanently broke its
/// power control.
#[test]
fn n1_hooks_receive_the_canonical_path_not_a_nickname() {
    let src = tools_src();
    assert!(
        !src.contains("let name = d.display_name().to_string();"),
        "a hook argument must never be built from the display name"
    );
    assert!(
        src.contains("HOOKS GET THE CANONICAL PATH"),
        "the rule must be stated where it is enforced"
    );
    // Every `{device}` substitution must come from the canonical path.
    let subs = src.matches("(\"device\", &name)").count();
    assert!(
        subs >= 4,
        "expected several hook substitutions, found {subs}"
    );
    assert_eq!(
        src.matches("let name = d.canonical.clone();").count(),
        6,
        "every hook site must take the canonical path"
    );
}

/// N2: naming a device must not be a one-way door.
#[test]
fn n2_a_nickname_can_be_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::open(dir.path()).unwrap();
    let row = reg
        .upsert_device(
            "/dev/serial/by-id/usb-FTDI_Nick_AAAA-if00-port0",
            None,
            IdentityKind::ById,
            None,
            1_000,
        )
        .unwrap();

    reg.set_nickname(row.id, "adp-ventuno").unwrap();
    let named = reg.all_devices().unwrap();
    assert_eq!(
        named
            .iter()
            .find(|d| d.id == row.id)
            .unwrap()
            .nickname
            .as_deref(),
        Some("adp-ventuno")
    );

    // The whole finding: this used to fail with "nickname must not be empty",
    // leaving the operator stuck with a name they could not withdraw.
    reg.set_nickname(row.id, "")
        .expect("an empty nickname must CLEAR it");
    let cleared = reg.all_devices().unwrap();
    assert_eq!(
        cleared.iter().find(|d| d.id == row.id).unwrap().nickname,
        None,
        "the nickname must be gone, not blank"
    );
}

/// N7: the power lamp must not lag half a minute behind reality.
///
/// The event-driven refresh only fires for actions taken through dashd; an agent
/// powering a board over MCP notifies nobody. Measured: four consoles still
/// reporting "on" 25s after an off.
#[test]
fn n7_the_power_sweep_is_fast_enough_to_trust() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs"))
        .expect("dash.rs");
    assert!(
        src.contains("from_secs(5)) => {}"),
        "the safety-net sweep must be seconds, not half a minute"
    );
    assert!(
        !src.contains("from_secs(30)) => {}"),
        "the 30s sweep left the lamp stale for actions taken outside dashd"
    );
}

/// N8: a zombie must be detected by ASKING the device, not by opening it.
///
/// `open().is_ok()` succeeds on a stale entry -- the kernel still holds cached
/// descriptors -- so a dead 18d1:d002 reported `usb_zombies: 0` twice while
/// lsusb showed the ghost.
#[test]
fn n8_liveness_requires_an_answer_not_merely_an_open() {
    let usb = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/usb.rs"
    ))
    .expect("usb.rs");
    assert!(
        !usb.contains("let responsive = d.open().is_ok();"),
        "opening a zombie succeeds; it proves nothing"
    );
    assert!(
        usb.contains("fn device_answers") && usb.contains("GET_DESCRIPTOR"),
        "liveness must be a real request the hardware has to answer"
    );

    // And a non-QDL zombie must be counted: the finding was that only QDL
    // devices were ever noticed.
    let ghost = UsbDevice {
        vendor_id: 0x18d1,
        product_id: 0xd002,
        bus: 3,
        address: 114,
        port_path: None,
        liveness: Liveness::Dead,
    };
    assert_eq!(
        zombies(&[ghost]).len(),
        1,
        "any dead device is a zombie, not just Qualcomm's"
    );
}

/// N12: a diff across mismatched windows must say so.
#[test]
fn n12_a_diff_reports_each_epochs_window() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/report.rs"
    ))
    .expect("report.rs");
    for field in ["window_a_ms", "window_b_ms", "opened_by_a", "opened_by_b"] {
        assert!(
            src.contains(field),
            "a diff must report `{field}`, or window asymmetry reads as regression"
        );
    }
}

/// T3/R3: the invalid-mode error must point at the way OUT, not only the ways in.
#[test]
fn t3_the_boot_mode_error_says_how_to_clear_a_strap() {
    let src = tools_src();
    assert!(
        src.contains("pass \\\"clear\\\" to release every"),
        "every listed mode enters something; the error must name the escape"
    );
    assert!(
        src.contains("\"also_accepted\": [\"clear\", \"none\", \"normal\"]"),
        "the accepted-values detail must include the clear verbs"
    );
}

/// S9: a bare filename must land somewhere the host can reach.
#[test]
fn s9_exports_resolve_into_the_shared_directory() {
    let src = tools_src();
    assert!(src.contains("fn shared_path"), "the resolver must exist");
    assert!(
        src.contains(r#"const EXPORT_DIR: &str = "/exports";"#),
        "bare filenames must resolve into the bind-mounted export directory"
    );
    let compose = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker-compose.yaml"
    ))
    .expect("docker-compose.yaml");
    assert!(
        compose.contains("./exports:/exports"),
        "the export directory must actually be mounted, or the path is a lie"
    );
}

/// The rig-wide config must still be coherent after all of the above.
#[test]
fn the_shipped_configuration_still_holds_together() {
    let cfg = Config::default();
    assert!(
        cfg.api.full_toolset,
        "the full tool surface must stay advertised (S1)"
    );
    assert!(
        !cfg.memory_map.is_empty(),
        "the rig memory map must survive (S6)"
    );
    let bug = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bughopper")
        .unwrap();
    assert!(
        bug.power_timeout_s.is_some_and(|t| t >= 45),
        "the slow controller keeps its timeout (T1)"
    );
}

/// N6: a boot that reached userspace must not be called unstable because
/// EARLIER boots differed.
///
/// The history verdicts were evaluated BEFORE this epoch's own result, so
/// `outcome: unstable -- 20 distinct fingerprints across recent epochs` fired on
/// healthy boots. An outcome must describe the epoch it is attached to.
///
/// §K5a finished this by DELETING the verdict rather than merely ordering it
/// last: `unstable` counted distinct fingerprints across recent epochs, which is
/// a property of the bench's past and never of this boot. So the check here is
/// now the stronger one -- it cannot appear as an outcome at all -- while the
/// measurement itself stays in `history` and `distinct_fingerprints_recent`.
#[test]
fn n6_history_never_overwrites_this_epochs_outcome() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/report.rs"
    ))
    .expect("report.rs");

    // The outcome ladder runs from `let (outcome, why) = ` to the end of the
    // chain; `unstable` must not be produced anywhere inside it.
    let ladder_start = src
        .find("let (outcome, why) = if b.bytes == 0 {")
        .expect("the outcome ladder");
    let ladder_end = src[ladder_start..]
        .find("// Templates novel to this epoch")
        .expect("the end of the ladder")
        + ladder_start;
    let ladder = &src[ladder_start..ladder_end];
    assert!(
        !ladder.contains("\"unstable\""),
        "a history statistic must never be this epoch's outcome"
    );
    assert!(
        !ladder.contains("distinct fingerprints across recent epochs"),
        "nor its explanation"
    );

    // History must still be reported, just not as the outcome.
    assert!(
        src.contains(r#""history": if looping {"#),
        "history must survive as its own field"
    );
    assert!(
        src.contains("distinct_fingerprints_recent"),
        "the churn count must stay visible"
    );
}

/// N11: the freshness envelope rides on every response, so it must carry only
/// what a caller branches on.
///
/// Measured at 250-400 bytes on calls whose own answer was smaller than the
/// envelope wrapping it. `server_now`/`last_rx_ts` existed only to compute
/// `idle_ms` (already present); `boot_seq` tracks `boot_id`; `line` changes
/// about once a year.
#[test]
fn n11_the_freshness_envelope_carries_only_decisions() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/state.rs"
    ))
    .expect("state.rs");
    // Just the emitted object: the surrounding doc comments legitimately
    // MENTION the removed fields to explain why they went, and matching those
    // would make this test fail on its own explanation.
    let after = src
        .split("pub fn freshness")
        .nth(1)
        .expect("the freshness builder");
    let start = after.find("Ok(json!({").expect("the emitted envelope");
    let end = after[start..].find("}))").expect("end of the envelope") + start;
    let env = &after[start..end];
    for gone in [
        "\"server_now\"",
        "\"last_rx_ts\"",
        "\"boot_seq\"",
        "\"line\"",
    ] {
        assert!(
            !env.contains(gone),
            "{gone} rides on every response and changes no decision"
        );
    }
    for kept in [
        "\"idle_ms\"",
        "\"boot_id\"",
        "\"capture_state\"",
        "\"cursor\"",
    ] {
        assert!(
            env.contains(kept),
            "{kept} is what an agent actually branches on"
        );
    }
}

/// N5: a board sitting at a login prompt must be reported as `login_wait`,
/// even when recent epochs have been churning.
///
/// `loop_state` (epoch-chain history) ran BEFORE the tail was classified, and on
/// a rig where boards are power-cycled all day it fires on almost every call --
/// so `classify` never ran and `login_wait` was UNREACHABLE. A prompt taught
/// with `classify_prompt` could never appear in the state it was taught for,
/// which made the teaching pointless.
///
/// The distinction is an operator's next action: "this board is broken" versus
/// "this board wants a password".
#[test]
fn n5_a_login_prompt_is_reported_even_when_recent_epochs_churned() {
    // EXECUTED, not grepped. This gate used to search console.rs for the literal
    // `prompts.classify(&tail)` and check it appeared before `loop_state`. That
    // is a claim about where a line of code sits, and it broke the moment the
    // call moved into a helper -- while the behaviour it protects was intact.
    // Worse, the reverse is also true: the text could sit in the right order and
    // the console still answer wrongly. So ask the console.
    use conminer_core::console::{derive, ConsoleState, Observation};
    use conminer_core::framer::profile::PromptKind;
    use conminer_core::live::CaptureState;
    use conminer_core::runner::{Prompt, Prompts};

    let rig = conminer_testkit::Rig::new();
    let mut p = rig.pipeline("n5", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    // Churn: epochs opening one after another, which is what a rig looks like
    // when boards are power-cycled all day...
    for _ in 0..4 {
        p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\n[    0.0] Linux version 6.12.9 (b@h)\n")
            .unwrap();
    }
    // ...and the board is now sitting at a credential gate. Flushed, because an
    // unterminated partial is only published once the capture loop calls it
    // stable, and this gate is about ORDERING -- the tail before the history --
    // not about the partial-line path (which `state` covers).
    p.feed(b"debian login: ").unwrap();
    p.finish().unwrap();
    let store = p.into_store();

    let prompts = Prompts(vec![Prompt {
        re: regex::Regex::new(r"(^|\n)[\w.-]* ?login: *$").unwrap(),
        raw: "login: ".into(),
        kind: PromptKind::CredentialGate,
    }]);
    let state = derive(
        &store,
        &prompts,
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 10_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();

    assert!(
        matches!(state, ConsoleState::LoginWait { .. }),
        "a board at a login gate wants a password; epoch churn must not bury that: {state:?}"
    );
    assert!(
        !state.commandable(),
        "up is not the same as commandable: {state:?}"
    );
}
