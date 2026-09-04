//! One gate per round-4 finding.
//!
//! Round 3 shipped a gate per finding and round 4 still found three of them
//! open, because several of those gates asserted that the FIX WAS PRESENT in the
//! source rather than that the BEHAVIOUR was right. N5 is the cautionary tale:
//! the ordering it checked really had been fixed, and the field it was about
//! still lied on hardware, because the prompt was never in the data the ordering
//! reordered. So the gates here drive the real handler, the real pipeline and
//! the real state machine wherever the behaviour can be reached in-process.

use conminer_core::config::Config;
use conminer_core::console::{derive, ConsoleState, Observation};
use conminer_core::framer::ProfileSet;
use conminer_core::live::CaptureState;
use conminer_core::runner::{Prompt, Prompts};
use conminer_core::store::{DeviceStore, IdentityKind, Registry};
use conminer_core::usb::{watch_for_edl_with, Liveness, UsbDevice};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

fn tools_src() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs")
}

fn qdl(responsive: bool) -> UsbDevice {
    UsbDevice {
        vendor_id: 0x05c6,
        product_id: 0x9008,
        bus: 3,
        address: 11,
        port_path: None,
        liveness: if responsive {
            Liveness::Alive
        } else {
            Liveness::Dead
        },
    }
}

// ---------------------------------------------------------------------- R4 --

/// R4: a board re-entering EDL is invisible for several seconds, and one scan
/// taken in that gap must not be reported as proof the board is not in EDL.
///
/// Measured on the ADP: `off` while in EDL warm-reset the PBL, the QDL gadget
/// dropped, a scan at T+2s saw a clean bus, and the response stated as fact that
/// USB had been checked and the board was not in EDL. The gadget came back six
/// seconds later; the board had never left EDL.
#[test]
fn r4_the_edl_probe_waits_out_the_re_enumeration_window() {
    // The gadget comes back at T+6s, exactly the measured case.
    let elapsed = std::cell::Cell::new(0i64);
    let probe = watch_for_edl_with(
        || {
            if elapsed.get() >= 6_000 {
                vec![qdl(true)]
            } else {
                vec![]
            }
        },
        Duration::from_secs(8),
        Duration::from_millis(500),
        |d| elapsed.set(elapsed.get() + d.as_millis() as i64),
    );
    assert!(
        probe.in_edl,
        "a gadget that re-enumerates inside the window must be seen; \
         sampling once at T+2s is what produced the wrong claim"
    );
    assert!(
        probe.waited_ms >= 6_000,
        "it must actually have waited for it, not guessed: {probe:?}"
    );
}

#[test]
fn r4_a_clean_bus_only_excludes_edl_after_the_full_window() {
    let mut elapsed = 0i64;
    let probe = watch_for_edl_with(
        Vec::new,
        Duration::from_secs(8),
        Duration::from_millis(500),
        |d| elapsed += d.as_millis() as i64,
    );
    assert!(!probe.in_edl);
    assert!(
        probe.settled,
        "the full window elapsed, so this is a real claim"
    );
    assert!(probe.excludes_edl());
    assert_eq!(probe.waited_ms, 8_000);

    // ...and a probe that stopped early may NOT be stated as fact.
    let early = conminer_core::usb::EdlProbe {
        in_edl: false,
        waited_ms: 2_000,
        settled: false,
        stale_qdl: 0,
    };
    assert!(
        !early.excludes_edl(),
        "\"no gadget right now\" and \"not in EDL\" are different claims"
    );
}

/// A live gadget needs no waiting: only the board that really powered off pays
/// the window. This is what keeps R4's fix from becoming R5's latency finding.
#[test]
fn r4_a_live_gadget_answers_immediately() {
    let mut slept = 0i64;
    let probe = watch_for_edl_with(
        || vec![qdl(true)],
        Duration::from_secs(8),
        Duration::from_millis(500),
        |d| slept += d.as_millis() as i64,
    );
    assert!(probe.in_edl);
    assert_eq!(slept, 0, "a positive answer must not wait");
    assert_eq!(probe.waited_ms, 0);
}

/// The prose is the finding: `verified: false` was already correct, but the
/// `why` asserted a fact an agent would act on.
#[test]
fn r4_the_not_in_edl_claim_is_conditioned_on_a_settled_probe() {
    let src = tools_src();
    // Anchored on the CONDITION, not the layout: rustfmt moves this across
    // lines whenever a branch is added, and a gate that breaks on reformatting
    // teaches nothing.
    assert!(
        src.contains("if edl.excludes_edl()"),
        "the wording must branch on whether EDL was actually excluded"
    );
    assert!(
        src.contains("does NOT exclude EDL"),
        "the unsettled branch must say so rather than claiming the board is not in EDL"
    );
    assert!(
        !src.contains("found no live QDL gadget, so the board is not in EDL"),
        "the unconditional claim is the bug and must be gone"
    );
    // And the flag path must consult the watched probe, not a bare scan --
    // scoped to this board's ports since 2026-08-16.
    assert!(
        src.contains("conminer_core::usb::watch_for_edl_on_ports(edl_settle, &ports)"),
        "off must use the watching probe, on this board's ports"
    );
}

/// Found ON HARDWARE. `off` while the ADP sat in EDL left `05c6:9008`
/// enumerated as the SAME device number and no longer answering -- confirmed
/// with `lsusb -v`: "cannot read device status, Resource temporarily
/// unavailable". conminer correctly reported `in_edl: false` (a dead gadget is
/// not evidence of a live EDL) and then told the caller "no QDL gadget
/// appeared", which is simply not true: one was sitting on the bus. Same
/// wrong-fact class as R4 itself -- an agent would believe the bus was clean
/// and be surprised by a stale 9008 on its next flash.
#[test]
fn r4_a_listed_but_dead_gadget_is_reported_not_described_as_a_clean_bus() {
    let dead = UsbDevice {
        port_path: None,
        liveness: Liveness::Dead,
        ..qdl(true)
    };
    let probe = watch_for_edl_with(
        || vec![dead.clone()],
        Duration::from_secs(8),
        Duration::from_millis(500),
        |_| {},
    );
    assert!(!probe.in_edl, "a gadget that does not answer is not EDL");
    assert!(probe.settled);
    assert_eq!(
        probe.stale_qdl, 1,
        "...but what IS on the bus must be carried out of the probe, not dropped"
    );

    let src = tools_src();
    assert!(
        src.contains("edl.excludes_edl() && edl.stale_qdl > 0"),
        "the wording must have a branch for the listed-but-dead case"
    );
    assert!(
        src.contains("no LIVE QDL gadget answered"),
        "which must not claim the bus was clean"
    );
    assert!(
        src.contains("\"stale_qdl_entries\": edl.stale_qdl"),
        "and the count belongs in the response, not only in prose"
    );
    // Measured on the ADP: the sweep reported "cleared 0 of them" while lsusb
    // showed the entry gone. `clear_zombie` returns whether the RESET CALL
    // succeeded -- opening a zombie usually fails -- so that number counts
    // attempts, not outcomes, and the response contradicted the hardware. What
    // the caller needs is what is on the bus NOW.
    assert!(
        src.contains("let remaining = stale_qdl_on_bus();"),
        "the outcome must come from a fresh scan, not from the sweep's return value"
    );
    assert!(
        src.contains("\"stale_qdl_remaining\": stale_qdl_on_bus()"),
        "and that measured number belongs in the response"
    );
    assert!(
        !src.contains("conminer cleared {cleared} of them"),
        "counting reset() calls as clearances is the bug"
    );
}

/// R5: a reset into EDL must not sit out the whole console watch. The console is
/// silent by design in EDL, so waiting for bytes waits for something that will
/// never come -- 37s to produce an answer available at ~5s.
#[test]
fn r5_the_watch_loop_stops_when_the_gadget_appears() {
    let src = tools_src();
    let loop_start = src
        .find("while Instant::now() < deadline")
        .expect("watch loop");
    let edl_break = src
        .find("edl_during_watch = true;")
        .expect("the watch loop must also watch for EDL");
    let loop_end = src
        .find("let edl = if edl_during_watch")
        .expect("probe decision");
    assert!(
        loop_start < edl_break && edl_break < loop_end,
        "the EDL check has to be INSIDE the watch loop to shorten it"
    );
}

// ---------------------------------------------------------------------- N3 --

fn registry_with_target(dir: &std::path::Path) -> Registry {
    let mut reg = Registry::open(dir).unwrap();
    let console = reg
        .upsert_device(
            "/dev/serial/by-id/board-if00-port0",
            None,
            IdentityKind::ById,
            None,
            0,
        )
        .unwrap();
    let ctrl = reg
        .upsert_device(
            "/dev/serial/by-id/bantam-if00",
            None,
            IdentityKind::ById,
            None,
            0,
        )
        .unwrap();
    reg.set_target(console.id, Some("iq10")).unwrap();
    reg.set_target(ctrl.id, Some("iq10")).unwrap();
    // A controller is ignored: nothing captures it, it has no epochs, no lines.
    reg.set_ignored(ctrl.id, true).unwrap();
    reg
}

/// N3: leasing every console of a board and still being told `LEASE_REQUIRED`,
/// because the target's membership includes its controller -- a pseudo-device
/// that captures nothing and cannot usefully be leased.
#[test]
fn n3_target_members_for_actuation_exclude_the_controller() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry_with_target(dir.path());

    let all = conminer_core::target::members(&reg, "iq10").unwrap();
    assert_eq!(all.len(), 2, "membership itself still knows the controller");

    let (consoles, skipped) = conminer_core::target::console_members(&reg, "iq10").unwrap();
    assert_eq!(consoles.len(), 1, "only the console can hold an epoch");
    assert!(consoles[0].canonical.contains("board"));
    assert_eq!(
        skipped.len(),
        1,
        "and the exclusion is reported, not silent"
    );
    assert!(skipped[0].contains("bantam"));
}

/// N3, second half: the error must name the device whose lease is missing.
/// Reproduced holding four leases of five and being told only that a lease was
/// required.
#[test]
fn n3_lease_required_names_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::open(dir.path()).unwrap();
    let d = reg
        .upsert_device(
            "/dev/serial/by-id/usb-Nord_AP-if02-port0",
            None,
            IdentityKind::ById,
            None,
            0,
        )
        .unwrap();

    let e = reg.require_lease(d.id, "agent-1", 1_000).unwrap_err();
    assert_eq!(e.code, conminer_core::ErrorCode::LeaseRequired);
    assert!(
        e.message.contains("usb-Nord_AP-if02-port0"),
        "the caller must be told WHICH device: {}",
        e.message
    );
    assert!(
        e.hint.contains("acquire("),
        "and how to fix it: {:?}",
        e.hint
    );

    // A lease held by somebody else must name the device too.
    reg.acquire_lease(d.id, "agent-2", 1_000, 600, 3_600, false)
        .unwrap();
    let held = reg.require_lease(d.id, "agent-1", 2_000).unwrap_err();
    assert!(
        held.message.contains("usb-Nord_AP-if02-port0"),
        "{}",
        held.message
    );
    assert!(held.message.contains("agent-2"));
}

// ---------------------------------------------------------------------- N5 --

fn prompts() -> Prompts {
    Prompts(vec![Prompt {
        re: regex::Regex::new(r"root@[\w.-]+:[^\s]*[#$]\s*$").unwrap(),
        raw: r"root@.*[#$] $".into(),
        kind: conminer_core::framer::profile::PromptKind::Shell,
    }])
}

/// N5, the actual root cause: A PROMPT HAS NO TERMINATOR.
///
/// The console sits on `root@iq10:~#` with the cursor after it, so that text is
/// not a line and never reaches `raw_lines` until enter is pressed or ten
/// seconds of dead air close the record. `console_state` classified from stored
/// lines only, found kernel chatter, and answered `unstable, commandable: false`
/// at a healthy idle shell -- through three rounds of "fixed".
#[test]
fn n5_a_console_idle_at_an_unterminated_prompt_is_at_prompt_and_commandable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut store = DeviceStore::open(&path, "/dev/ttyUSB0", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    // Terminated lines: ordinary kernel output, none of it a prompt.
    for (i, text) in [
        "[    5.112233] usb 1-1: new high-speed USB device",
        "[    5.998877] systemd[1]: Reached target Multi-User System.",
    ]
    .iter()
    .enumerate()
    {
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: i as i64,
                    ts_wall: 1_000 + i as i64,
                    bytes: text.as_bytes(),
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
    }

    let obs = Observation {
        capture: CaptureState::Listening,
        now_ms: 6_700, // 5.7s after the last line, exactly the reported case
        hung_after_ms: 30_000,
        loop_min_epochs: 3,
        active_txn: None,
    };

    // Before the fix's input exists, the prompt is invisible and the answer is
    // whatever the epoch chain says -- never `at_prompt`.
    let blind = derive(&store, &prompts(), &obs).unwrap();
    assert!(
        !blind.commandable(),
        "sanity: with no pending tail there is genuinely no prompt to see"
    );

    // The capture loop publishes the unterminated line it is holding.
    store.set_pending_tail("root@iq10:~# ", 1_002).unwrap();

    let state = derive(&store, &prompts(), &obs).unwrap();
    assert!(
        matches!(state, ConsoleState::AtPrompt { .. }),
        "an idle root shell is at a prompt, not `unstable`: {state:?}"
    );
    assert!(
        state.commandable(),
        "run_command drives this console fine; console_state must agree"
    );
}

/// The pipeline must actually publish it, or the fix above is unreachable in
/// production -- mcpd reads the store, it cannot see minerd's memory.
#[test]
fn n5_the_capture_loop_publishes_the_unterminated_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let store = DeviceStore::open(&dir.path().join("p.db"), "/dev/ttyUSB1", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSB1",
        None,
        Arc::new(conminer_core::clock::StepClock::default()),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();

    // Exactly what the live loop does: absorb bytes, then tick on the commit
    // interval. Publishing on the tick rather than per chunk keeps a busy
    // console from paying a transaction per read.
    pipe.feed(b"[    1.000000] booting\nroot@iq10:~# ").unwrap();
    // Two ticks: the partial must survive one unchanged before it is written.
    // See the busy-console test below for why that rule exists.
    pipe.tick().unwrap();
    pipe.tick().unwrap();
    let pending = pipe.store().pending_tail().unwrap();
    let (text, _) = pending.expect("the prompt must be published, it is in no line");
    assert_eq!(text, "root@iq10:~# ");

    // Pressing enter makes it a real line, and the pending copy must go: a stale
    // prompt would have console_state reporting one at a console that moved on.
    pipe.feed(b"\nuname -a\n").unwrap();
    pipe.tick().unwrap();
    assert!(
        pipe.store().pending_tail().unwrap().is_none(),
        "a completed line must clear the pending tail"
    );
}

/// The cost of publishing, pinned: a console under load must not pay for it.
///
/// A prompt is a partial line that STAYS. A console mid-firehose also has a
/// partial on every tick, but a different one each time, and writing that down
/// would spend a database transaction per tick on precisely the devices that can
/// least afford one. The rule is therefore "unchanged for one tick", which costs
/// an idle console 250 ms and a busy console nothing.
#[test]
fn n5_a_busy_console_never_pays_for_the_pending_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let store = DeviceStore::open(&dir.path().join("busy.db"), "/dev/ttyUSB9", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSB9",
        None,
        Arc::new(conminer_core::clock::StepClock::default()),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();

    // Output still arriving at every tick: the partial is never the same twice.
    for i in 0..20 {
        pipe.feed(format!("[  {i:>5}.000000] still going, partial-{i}").as_bytes())
            .unwrap();
        pipe.tick().unwrap();
        assert!(
            pipe.store().pending_tail().unwrap().is_none(),
            "a console that is still talking must not write a pending tail (tick {i})"
        );
        pipe.feed(b"\n").unwrap();
    }

    // ...and the instant it stops, the partial lands.
    pipe.feed(b"root@iq10:~# ").unwrap();
    pipe.tick().unwrap();
    pipe.tick().unwrap();
    assert_eq!(
        pipe.store().pending_tail().unwrap().map(|(t, _)| t),
        Some("root@iq10:~# ".to_string()),
        "a console that STOPPED at a partial is exactly the case this exists for"
    );
}

/// Found ON HARDWARE, immediately after deploying the fix above: redeploying the
/// stack put the IQ10 -- sitting untouched at its root prompt -- straight back
/// to `unstable, commandable: false`.
///
/// The prompt lives in the capture process's partial buffer, so a NEW minerd
/// starts with an empty one and cleared the stored copy on its first tick, while
/// the board (silent, unchanged, still at that prompt) had no reason to speak
/// again. Restarting conminer must not blind conminer. The observation survives;
/// what invalidates it is something that actually changes the screen.
#[test]
fn n5_restarting_conminer_does_not_blind_a_board_idle_at_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.db");

    // First process: sees the prompt and publishes it.
    {
        let store = DeviceStore::open(&path, "/dev/ttyUSB5", true).unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let mut pipe = conminer_core::pipeline::Pipeline::new(
            store,
            Arc::new(ProfileSet::builtin().unwrap()),
            cfg,
            "/dev/ttyUSB5",
            None,
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
            .unwrap();
        pipe.feed(b"[    2.0] systemd: Startup finished.\nroot@iq10:~# ")
            .unwrap();
        pipe.tick().unwrap();
        pipe.tick().unwrap();
        assert!(
            pipe.store().pending_tail().unwrap().is_some(),
            "precondition"
        );
    }

    // Second process: same store, empty buffer, board still silent.
    let store = DeviceStore::open(&path, "/dev/ttyUSB5", true).unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSB5",
        None,
        Arc::new(conminer_core::clock::StepClock::default()),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    pipe.tick().unwrap();
    pipe.tick().unwrap();
    assert!(
        pipe.store().pending_tail().unwrap().is_some(),
        "a restart must not erase what the previous process observed"
    );

    let state = derive(
        pipe.store(),
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 9_000_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        state.commandable(),
        "the board never moved; a deploy must not make conminer forget: {state:?}"
    );
}

/// The other side of that rule: an observation from before a POWER event is
/// worthless, because power is exactly what changes the screen.
#[test]
fn n5_a_power_cycle_invalidates_the_carried_over_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("inv.db"), "/dev/ttyUSB6", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store
        .open_boot("session", None, 1_000, Some(session))
        .unwrap();
    store
        .append_lines(
            session,
            Some(boot.id),
            &[conminer_core::store::PendingLine {
                stage_id: None,
                ts_mono: 0,
                ts_wall: 1_000,
                bytes: b"[    2.0] systemd: Startup finished.",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
            }],
        )
        .unwrap();
    store.set_pending_tail("root@iq10:~# ", 1_100).unwrap();

    let at_prompt = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 5_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(at_prompt.commandable(), "precondition: {at_prompt:?}");

    // Someone cut the power. Whatever was on that screen is gone.
    store
        .open_boot("power", None, 2_000, Some(session))
        .unwrap();
    let after = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 5_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        !after.commandable(),
        "a prompt observed before a power cycle is not evidence after it: {after:?}"
    );
}

/// The REAL reason N5 kept coming back, caught by leaving the IQ10 idle for half
/// an hour after the first fix was verified. This is the line it was sitting on:
///
///   ESC[?2004hroot@debian-trixie-arm64:~# [  862.282959] phy phy-fc3a00.phy.0: ...
///
/// The shell is at its prompt. A printk landed ON THAT LINE, because the kernel
/// writes wherever the cursor happens to be. Prompt patterns are anchored at
/// end-of-line -- they have to be, or the word "root@host" inside a log message
/// would match -- so nothing matched, and console_state answered `unstable,
/// commandable: false` at a shell that run_command drives without trouble.
/// Round 3 taught the classifier to look past kernel LINES; this is the same
/// fact one level down, within a line.
#[test]
fn n5_a_printk_landing_on_the_prompts_own_line_does_not_hide_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("printk.db"), "/dev/ttyUSB8", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    // Verbatim from the board, escapes and all.
    let observed = "\x1b[?2004hroot@debian-trixie-arm64:~#                     [  862.282959] phy phy-fc3a00.phy.0: phy_power_on";
    store
        .append_lines(
            session,
            Some(boot.id),
            &[conminer_core::store::PendingLine {
                stage_id: None,
                ts_mono: 0,
                ts_wall: 1_000,
                bytes: observed.as_bytes(),
                terminator: conminer_core::linesplit::Terminator::None,
                truncated: false,
                continuation: false,
            }],
        )
        .unwrap();

    let debian = Prompts(vec![Prompt {
        re: regex::Regex::new(r"root@[\w.-]+:[^\s]*[#$]\s*$").unwrap(),
        raw: r"root@.*[#$] $".into(),
        kind: conminer_core::framer::profile::PromptKind::Shell,
    }]);
    let state = derive(
        &store,
        &debian,
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 25_000, // long idle: the console really has stopped
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        state.commandable(),
        "a printk scribbling across the prompt line does not move the shell off          its prompt: {state:?}"
    );
}

/// ...and the narrowness that keeps it honest: a line that IS a kernel message
/// must not be mistaken for a prompt with noise on it.
#[test]
fn n5_a_kernel_line_is_still_a_kernel_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("kern.db"), "/dev/ttyUSBa", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();
    store
        .append_lines(
            session,
            Some(boot.id),
            &[conminer_core::store::PendingLine {
                stage_id: None,
                ts_mono: 0,
                ts_wall: 1_000,
                bytes: b"[  862.282959] some subsystem mentions root@host:~# in passing",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
            }],
        )
        .unwrap();
    let state = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 25_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        !state.commandable(),
        "a kernel message that quotes a prompt is not a prompt: {state:?}"
    );
}

/// The panic this fix caused, and the reason it reached hardware: console bytes
/// are not always valid UTF-8.
///
/// Deployed, then the very first `console_state` call came back
/// `handler panicked: start byte index 1 is not a char boundary; it is inside
/// '\u{fffd}' (bytes 0..3 of string)`. A baud mismatch, a truncated multi-byte
/// sequence, or line noise decodes to U+FFFD, which is THREE bytes -- and the
/// first version of the printk-stripper began its scan at byte 1, inside it.
/// Every console_state call on such a line died.
///
/// Byte arithmetic on a `&str` is the bug; char boundaries are the fix.
#[test]
fn n5_a_line_that_is_not_valid_utf8_does_not_panic_the_state_machine() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("utf8.db"), "/dev/ttyUSBb", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    // Raw bytes off a real wire: a lone 0xFF (invalid UTF-8, decodes to U+FFFD),
    // then a prompt, then a printk that landed on the same line.
    let mut bytes: Vec<u8> = vec![0xFF];
    bytes.extend_from_slice(b"root@iq10:~# [  862.282959] phy phy-fc3a00.phy.0: phy_power_on");

    // ...and a few more shapes that have all appeared on these boards.
    for raw in [
        bytes.clone(),
        vec![0xFF, 0xFE, 0xFD],
        b"\xc3root@iq10:~# ".to_vec(),
        vec![0x1b, b'[', b'?', b'2', b'0', b'0', b'4', b'h', 0xFF],
    ] {
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: 0,
                    ts_wall: 1_000,
                    bytes: &raw,
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();

        // The assertion is that this RETURNS AT ALL.
        let state = derive(
            &store,
            &prompts(),
            &Observation {
                capture: CaptureState::Listening,
                now_ms: 25_000,
                hung_after_ms: 30_000,
                loop_min_epochs: 3,
                active_txn: None,
            },
        )
        .expect("deriving state from non-UTF-8 console output must not fail");
        assert!(!state.name().is_empty());
    }

    // The pending-tail path takes the same text, so it must survive it too.
    store
        .set_pending_tail("\u{fffd}root@iq10:~# ", 1_100)
        .unwrap();
    let state = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 25_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .expect("a pending tail with a replacement character must not panic");
    assert!(!state.name().is_empty());
}

/// Found ON HARDWARE, on the redeploy itself: reopening the FTDI put two lines
/// of UART noise on top of a live shell prompt --
///
///   \u{fffd}\u{fffd}\x03\u{fffd}\u{fffd}\x03\u{fffd}\u{fffd}\x01...
///
/// -- and because the classifier takes the LAST line, it considered the noise
/// and never looked at the prompt underneath. `unstable, commandable: false` at
/// a shell that was sitting there the whole time. Same shape as the printk case:
/// something that is not what the console is waiting at, taken as what the
/// console is waiting at.
#[test]
fn n5_reconnect_noise_on_top_of_a_prompt_does_not_hide_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("noise.db"), "/dev/ttyUSBc", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    // The prompt, then the exact noise the reconnect produced (raw bytes).
    let rows: Vec<Vec<u8>> = vec![
        b"root@iq10:~# ".to_vec(),
        vec![0xFF, 0xFE, 0x03, 0xFF, 0xFE, 0x03, 0xFF, 0xFE, 0x01],
        vec![0xFF, 0xFE, 0x03, 0xFF, 0xFE, 0x03, 0xFF, 0xFE, 0x00],
    ];
    for (i, raw) in rows.iter().enumerate() {
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: i as i64,
                    ts_wall: 1_000 + i as i64,
                    bytes: raw,
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
    }

    let state = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 25_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        state.commandable(),
        "line noise from a reconnect is not where the console is waiting: {state:?}"
    );
}

/// Prompts arrive decorated. A colourised shell prompt is `ESC[1;32mroot@...`
/// and matches no pattern an operator would write.
#[test]
fn n5_a_colourised_prompt_still_classifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("c.db"), "/dev/ttyUSB2", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();
    store
        .append_lines(
            session,
            Some(boot.id),
            &[conminer_core::store::PendingLine {
                stage_id: None,
                ts_mono: 0,
                ts_wall: 1_000,
                bytes: b"[    5.9] systemd[1]: Startup finished.",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
            }],
        )
        .unwrap();
    store
        .set_pending_tail("\x1b[1;32mroot@iq10\x1b[0m:~# ", 1_001)
        .unwrap();

    let state = derive(
        &store,
        &prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 3_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        state.commandable(),
        "escape sequences must not hide a prompt: {state:?}"
    );
}

/// Found ON HARDWARE while verifying the fix above: at t+48s the IQ10 was
/// printing 6 KB in 8 seconds and `console_state` answered `unstable`.
///
/// `unstable` is not an observation. It is what the epoch chain returns when
/// recent boots produced too many different fingerprints to conclude anything --
/// a statement about history. Reporting it as the state of a console that is
/// visibly emitting output tells an agent the board is flapping when the board
/// is simply talking. Same shape as N5: a history verdict outranking the fact in
/// front of it.
#[test]
fn n5_a_console_that_is_actively_printing_is_not_reported_as_unstable() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("t.db"), "/dev/ttyUSB7", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();

    // Enough divergent epochs that the fingerprint chain gives up: this is what
    // a bench that power-cycles boards all day looks like.
    for i in 0..12 {
        let boot = store
            .open_boot("power", None, i as i64 * 100, Some(session))
            .unwrap();
        let text = format!("[    0.00000{i}] boot number {i} says something different");
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: i as i64,
                    ts_wall: 10_000 + i as i64 * 100,
                    bytes: text.as_bytes(),
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
        // Distinct fingerprints are what make the chain give up.
        store
            .set_boot_summary(boot.id, Some(&format!("fingerprint-{i}")), None)
            .unwrap();
    }

    let at = |now_ms| {
        derive(
            &store,
            &prompts(),
            &Observation {
                capture: CaptureState::Listening,
                now_ms,
                hung_after_ms: 30_000,
                loop_min_epochs: 3,
                active_txn: None,
            },
        )
        .unwrap()
    };

    // Last line landed at 11_100. Half a second later the console is mid-output.
    let talking = at(11_600);
    assert!(
        !matches!(talking, ConsoleState::Unstable { .. }),
        "a console that produced output 500ms ago is talking, not flapping: {talking:?}"
    );

    // ...but once it has genuinely gone quiet, the epoch chain IS the best
    // available answer and must still be given. Softening that would trade one
    // wrong answer for another.
    //
    // "Genuinely quiet" is now the configured hung threshold rather than a
    // two-second constant: the IQ10 prints its GMU init line every 15 s, and a
    // board on that cadence was being called `unstable` between messages. So
    // this probes PAST hung_after_ms, which is where the history verdict
    // legitimately takes over.
    let quiet = at(11_100 + 35_000);
    assert!(
        matches!(
            quiet,
            ConsoleState::Unstable { .. } | ConsoleState::Hung { .. }
        ),
        "history is the right answer for a console with nothing to say: {quiet:?}"
    );
}

// --------------------------------------------------------------- N11 / N7 --

struct Rig {
    _dir: tempfile::TempDir,
    h: Handler,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let ctx = Context::open(
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        Self {
            _dir: dir,
            h: Handler::new(ctx),
        }
    }

    /// The whole tool result, errors included: what actually goes on the wire.
    fn raw(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        self.h.handle(req).expect("a reply").result.unwrap()
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args);
        assert_eq!(r["isError"], false, "{name} failed: {r}");
        r["structuredContent"].clone()
    }

    fn ingest(&self, text: &str) -> String {
        let path = self._dir.path().join("in.log");
        std::fs::write(&path, text).unwrap();
        self.call("ingest_file", json!({"path": path.display().to_string()}))["device"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// N11: the envelope rides on every response, and a caller polling in a loop
/// already knows the console state. There was no way to decline it.
#[test]
fn n11_freshness_false_omits_the_envelope() {
    let rig = Rig::new();
    let device = rig.ingest("[    0.1] hello\n[    0.2] world\n");

    let with = rig.call("get_recent", json!({"device": device, "lines": 1}));
    assert!(with["freshness"].is_object(), "the default is unchanged");

    let without = rig.call(
        "get_recent",
        json!({"device": device, "lines": 1, "freshness": false}),
    );
    assert!(
        without.get("freshness").is_none(),
        "the caller declined it: {without}"
    );
    assert_eq!(
        without["device"], with["device"],
        "but a response must still say which device it is about"
    );
    assert!(without["text"].is_string(), "and still answer the question");

    let a = serde_json::to_string(&with).unwrap().len();
    let b = serde_json::to_string(&without).unwrap().len();
    assert!(b < a, "declining it has to actually save bytes: {b} vs {a}");
}

/// N11, second half: ANSI escapes were shipped verbatim inside every text field.
/// They cost tokens, and they break equality -- two identical log lines that
/// differ only by a colour reset diff as changed.
#[test]
fn n11_ansi_is_stripped_from_text_fields_unless_asked_for() {
    let rig = Rig::new();
    let device = rig.ingest("\x1b[0;32mOK\x1b[0m  Started \x1b[1mThing\x1b[0m.\n");

    let clean = rig.call("get_recent", json!({"device": device, "lines": 1}));
    let text = clean["text"].as_str().unwrap();
    assert!(
        !text.contains('\x1b'),
        "escapes must not reach the response by default: {text:?}"
    );
    assert!(
        text.contains("Started"),
        "and the content must survive: {text:?}"
    );

    let raw = rig.call(
        "get_recent",
        json!({"device": device, "lines": 1, "ansi": "keep"}),
    );
    // Every text field, not just this one -- enforced at the response boundary
    // rather than per site, so a tool cannot forget. See
    // `n11_no_tool_may_ship_an_escape_sequence_by_default`.
    assert!(
        raw["text"].as_str().unwrap().contains('\x1b'),
        "a caller replaying into a terminal can still ask for the bytes"
    );
}

/// Both universal arguments must be accepted everywhere, and no tool may have to
/// declare them.
#[test]
fn n11_universal_arguments_are_accepted_by_tools_that_never_declared_them() {
    let rig = Rig::new();
    let device = rig.ingest("[    0.1] hello\n");
    for tool in ["stats", "list_templates", "get_recent"] {
        let v = rig.call(
            tool,
            json!({"device": device, "freshness": false, "ansi": "keep"}),
        );
        assert!(
            v.get("freshness").is_none(),
            "{tool} ignored the universal argument"
        );
    }
}

/// N7: the lamp lagged because the sweep probed once per CONSOLE, and a
/// six-console board therefore paid six identical ~2s controller queries.
#[test]
fn n7_power_is_probed_once_per_controller_not_once_per_console() {
    // Behavioural, not textual. This gate used to search dash.rs for the
    // grouping expression, and it broke the moment the grouping was extracted
    // into a function -- while the property it names stayed true the whole time.
    // That is the failure mode this suite's own header warns about, so ask the
    // rule directly instead.
    let six_consoles: Vec<conminer::dash::DashDevice> = (0..6)
        .map(|i| dash_console(&format!("usb-NordAU-if{i:02}-port0"), "/dev/ttyACM1"))
        .collect();
    let groups = conminer::dash::group_by_controller(&six_consoles);
    assert_eq!(
        groups.len(),
        1,
        "a six-console board is ONE controller query, not six ~2s queries: {groups:?}"
    );
    assert_eq!(groups.values().next().unwrap().len(), 6);
}

/// The other half of the same rule, and the one that was actually wrong on
/// hardware: grouping must not reach ACROSS boards. See the dash suite for the
/// full account -- the NordAU read "off" while powered on because both boards'
/// Bantams share the profile name "bantam".
#[test]
fn n7_grouping_never_reaches_across_two_boards() {
    let devices = vec![
        dash_console("usb-IQ10-if00-port0", "/dev/ttyACM0"),
        dash_console("usb-NordAU-if00-port0", "/dev/ttyACM1"),
    ];
    let groups = conminer::dash::group_by_controller(&devices);
    assert_eq!(
        groups.len(),
        2,
        "two controllers, two queries: sharing one answer means a board reports \
         another board's power: {groups:?}"
    );
}

/// A console as the power sweep sees it: same controller PROFILE, differing
/// only in which controller instance it resolves to.
fn dash_console(canonical: &str, controller_port: &str) -> conminer::dash::DashDevice {
    conminer::dash::DashDevice {
        // §P1: a local device, which is what every pre-fleet test means.
        node: None,
        node_host: None,
        // A local chassis is headed by its own key, so no override.
        adapter_label: None,
        // Plugged in: these fixtures model a bench with the cable in.
        present: true,
        device: canonical.into(),
        canonical: canonical.into(),
        nickname: None,
        target: None,
        adapter: None,
        port: None,
        boot_modes: Vec::new(),
        has_power_hook: true,
        power_sensed_at: None,
        power: None,
        controller: Some("bantam".into()),
        controller_port: Some(controller_port.into()),
        controller_label: None,
        controller_tags: Default::default(),
        is_file: false,
        is_controller: false,
        line: String::new(),
        state: "listening".into(),
        capture_state: None,
        ignored: false,
        observed: json!({}),
        tags: Default::default(),
        last_seen: 0,
        viewers: 0,
        attached: false,
    }
}

/// N7, the half that matters for automation: a reading with no age cannot be
/// told apart from one taken before the caller's own action.
#[test]
fn n7_the_api_publishes_when_power_was_last_sensed() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs")).unwrap();
    assert!(
        src.contains("pub power_sensed_at: Option<i64>"),
        "the device JSON must carry the age of its power reading"
    );
    assert!(
        src.contains("fn note_power_sensed") || src.contains("fn publish_power"),
        "and every path that senses power must stamp it"
    );
    // Behavioural: a published reading carries its time, and that time is what
    // decides whether it may still be served. Counting call sites only ever
    // proved that a name appeared the expected number of times.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let dash = conminer::dash::Dash::new(cfg, dir.path().to_path_buf());
    let dev = vec!["usb-NordAU-if00-port0".to_string()];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    dash.publish_power(&dev, Some("on".into()), now);
    assert_eq!(dash.fresh_power(&dev[0]).as_deref(), Some("on"));
    dash.publish_power(&dev, Some("on".into()), now - 300_000);
    assert_eq!(
        dash.fresh_power(&dev[0]),
        None,
        "a reading too old to trust must read as unknown; automation cannot \
         otherwise tell it from one taken after its own action"
    );
}

// ----------------------------------------------------------------- N12 / S9 --

/// N12: two epochs of very different length are not comparable, and the
/// difference reads as a regression. Reporting both durations was not enough --
/// the phantom `gone_from_b: 497` was read as a regression by someone who had
/// both numbers in front of them.
#[test]
fn n12_diff_boots_says_in_words_when_the_windows_are_not_comparable() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/report.rs"
    ))
    .unwrap();
    assert!(src.contains("\"duration_note\": duration_note(&ba, &bb)"));
    assert!(src.contains("fn duration_note"));
    assert!(
        src.contains("coverage, not regression"),
        "the note has to name the trap, not just print two numbers"
    );
    assert!(
        src.contains("still open"),
        "an unfinished epoch is a third case and must be said"
    );
}

/// S9: an export the caller cannot find is an export that needs `docker cp`.
#[test]
fn s9_exports_report_where_they_landed_on_the_host() {
    let src = tools_src();
    assert!(src.contains("\"host_path\": host"), "the response must say");
    assert!(src.contains("fn host_hint"));
    assert!(
        src.contains("./exports/"),
        "and it must be the host-side path, not the container's"
    );
}

// ----------------------------------------------------------------- round 5 --

/// N11's ANSI half, done mechanically instead of field by field.
///
/// Round 4 routed eight text-emitting sites through a strip helper and round 5
/// still found `[0;1;31m` in the response, because there are roughly forty such
/// sites -- record text, template examples, diffs, follow, timeline,
/// target_context -- and every new tool is another chance to forget one. This
/// gate does not check a list of tools; it checks that NOTHING gets out.
#[test]
fn n11_no_tool_may_ship_an_escape_sequence_by_default() {
    let rig = Rig::new();
    // Colourised systemd output, a bracketed-paste toggle, and a colour reset
    // mid-line: all three shapes seen on these boards.
    let device = rig.ingest(
        "\x1b[0;32m  OK  \x1b[0m Started \x1b[1mSome Service\x1b[0m.\n\
         \x1b[?2004h\x1b[0;1;31mtest failed\x1b[0m: nothing to do\n\
         [    1.234567] usb usb3-port1: config error\n\
         \x1b[0;32m  OK  \x1b[0m Reached target \x1b[1mMulti-User System\x1b[0m.\n",
    );

    // Every read-only tool that can carry console text, called the way an agent
    // would call it.
    let calls: Vec<(&str, Value)> = vec![
        ("get_recent", json!({"device": device, "lines": 20})),
        (
            "get_recent",
            json!({"device": device, "lines": 20, "format": "records"}),
        ),
        ("list_templates", json!({"device": device, "limit": 50})),
        ("stats", json!({"device": device})),
        ("console_state", json!({"device": device})),
        ("diagnose", json!({"device": device})),
        ("list_boots", json!({"device": device})),
        ("boot_report", json!({"device": device})),
        ("get_prompts", json!({"device": device})),
        ("list_sessions", json!({"device": device})),
        ("timeline", json!({"device": device})),
        ("list_verdicts", json!({"device": device})),
        ("identify", json!({"device": device})),
        ("provenance", json!({"device": device})),
    ];
    // ...plus the ones that hand back RAW RECORD TEXT rather than mined
    // templates. This is where round 5 still saw `[0;1;31m`: template text is
    // ANSI-stripped when it is mined, so a sweep that only touches template
    // views proves nothing. These carry the bytes verbatim.
    let mut calls = calls;
    let toc = rig.call("list_templates", json!({"device": device, "limit": 50}));
    if let Some(id) = toc["templates"][0]["id"].as_i64() {
        calls.push((
            "template_detail",
            json!({"device": device, "template_id": id, "examples": 3}),
        ));
        calls.push((
            "get_records",
            json!({"device": device, "template_id": id, "n": 5}),
        ));
    }
    let recs = rig.call(
        "get_recent",
        json!({"device": device, "lines": 20, "format": "records"}),
    );
    if let Some(line_id) = recs["lines"][0]["line_id"].as_i64() {
        calls.push((
            "get_context",
            json!({"device": device, "line_id": line_id, "before": 3, "after": 3}),
        ));
    }

    let mut checked = 0;
    for (tool, args) in calls {
        let v = rig.raw(tool, args.clone());
        let body = serde_json::to_string(&v).unwrap();
        assert!(
            !body.contains('\u{1b}'),
            "{tool} shipped an escape sequence: {}",
            &body[..body.len().min(400)]
        );
        checked += 1;
    }
    assert!(checked >= 17, "the sweep must actually have called them");

    // The content block (what a client without structuredContent reads) counts
    // as a response too.
    let v = rig.raw("get_recent", json!({"device": device, "lines": 20}));
    let text = v["content"][0]["text"].as_str().unwrap_or_default();
    assert!(!text.contains('\u{1b}'), "the text block must be clean too");
    // Content must SURVIVE the strip -- checked on the structured payload, since
    // the text block is a summary that points at it.
    let payload = serde_json::to_string(&v["structuredContent"]).unwrap();
    assert!(
        payload.contains("Started") && payload.contains("Multi-User System"),
        "stripping must remove decoration, not content: {payload:.300}"
    );

    // ...and `ansi: "keep"` still returns the bytes, for a caller replaying into
    // a terminal.
    let raw = rig.raw(
        "get_recent",
        json!({"device": device, "lines": 20, "ansi": "keep"}),
    );
    assert!(
        serde_json::to_string(&raw).unwrap().contains("\\u001b"),
        "ansi:keep must not be stripped"
    );
}

/// N5's hard case: a credential gate under continuous spam.
///
/// Measured on the ADP, which emits USB gadget errors several times a second
/// forever. Its `login:` prompt was pushed out of the eight-line classification
/// window within seconds, so console_state reported `booting/userspace` at a
/// board sitting at a login prompt -- and the credential-gate pattern an
/// operator had explicitly taught was unusable in the one case it was taught
/// for. Nothing consumed that prompt; printk just printed past it.
#[test]
fn n5_a_login_gate_under_continuous_spam_is_still_login_wait() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("spam.db"), "/dev/ttyUSBd", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    let mut push = |text: &str, ts: i64| {
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: ts,
                    ts_wall: 1_000 + ts,
                    bytes: text.as_bytes(),
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
    };

    push("adp-ventuno login: ", 0);
    // Then the spam, far more than the recent-tail window holds.
    for i in 1..=120 {
        push(
            &format!("[  {}.{:06}] usb usb3-port1: config error", 100 + i, i),
            i,
        );
    }

    let taught = Prompts(vec![Prompt {
        re: regex::Regex::new(r"(^|\n)[\w.-]+ login: *$").unwrap(),
        raw: "login: $".into(),
        kind: conminer_core::framer::profile::PromptKind::CredentialGate,
    }]);
    let state = derive(
        &store,
        &taught,
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 5_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        matches!(state, ConsoleState::LoginWait { .. }),
        "a login gate does not scroll away just because the kernel keeps talking: {state:?}"
    );
    assert!(
        !state.commandable(),
        "and login_wait is NOT commandable -- that is the point of the state"
    );
}

/// Detecting a mess and leaving it there is half a job.
///
/// Measured on the ADP at the end of the round-5 pass: powering off a board that
/// was idle at a login prompt takes the "console was already silent" path, which
/// reported `usb_zombies: 1` for the `18d1:d002` ADB gadget the board left
/// enumerated -- and then left it on the bus. The sweep only ran when the off
/// was CONFIRMED, or when the stale entry happened to be a QDL gadget. Every
/// off must clean up after itself, whatever the board left behind and whatever
/// the verifier could prove.
#[test]
fn r5_every_off_path_sweeps_the_entries_the_board_left_behind() {
    let src = tools_src();

    // The already-silent branch must sweep and REPORT what it measured.
    let branch = src
        .find("if action == \"off\" && !was_talking")
        .expect("the already-silent branch must exist");
    let sweep = src[branch..]
        .find("let swept = sweep_usb_zombies();")
        .expect("it must sweep");
    let report = src[branch..]
        .find("\"usb_zombies_remaining\"")
        .expect("and report what is left, measured rather than assumed");
    assert!(
        sweep < report,
        "sweep first, then measure what remains -- reporting a stale count is the bug"
    );

    // The confirmed-off path already swept; both must still be there.
    assert!(
        src.matches("sweep_usb_zombies()").count() >= 3,
        "confirmed-off, stale-QDL, and already-silent are three distinct paths"
    );

    // ...and when the sweep CANNOT win, say what does. Measured on the ADP:
    // a truly dead entry survives a USBDEVFS reset AND the kernel's own logical
    // disconnect (`echo 1 > /sys/.../remove` is accepted and changes nothing),
    // because that board's Type-C controller never signals detach. A count the
    // caller cannot act on is not an answer.
    assert!(
        src.contains("power-cycle the hub port with uhubctl, or replug the board"),
        "a zombie userspace cannot clear must come with the remedy that works"
    );
    let core = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/usb.rs"
    ))
    .unwrap();
    assert!(
        core.contains("MEASURED LIMIT"),
        "and the limit must be written down where the next attempt would start"
    );
}

// ------------------------------------------------ identity vs label (r5) ----

/// A NICKNAME IS A LABEL, NOT AN IDENTITY.
///
/// `display_name()` used to return the nickname when one was set, so naming a
/// board erased which physical port it was -- on the dashboard, in every tool
/// response, in every log line. "adp-ventuno" does not say which of four FTDI
/// interfaces is about to be powered off, and it can go stale when boards move;
/// `/dev/serial/by-id/...-if02-port0` can do neither. The label still rides
/// alongside, and still works as a selector.
#[test]
fn a_label_never_replaces_the_port_it_labels() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::open(dir.path()).unwrap();
    let port = "/dev/serial/by-id/usb-Arduino_Bughopper_DK0HDSRI-if00-port0";
    let d = reg
        .upsert_device(port, None, IdentityKind::ById, None, 0)
        .unwrap();
    reg.set_nickname(d.id, "adp-ventuno").unwrap();

    let named = reg.device(d.id).unwrap();
    assert_eq!(
        named.display_name(),
        port,
        "the port is the identity, whatever anyone calls it"
    );
    assert_eq!(named.label(), Some("adp-ventuno"), "and the label is kept");

    // Clearing the label leaves the identity untouched.
    reg.set_nickname(d.id, "").unwrap();
    let bare = reg.device(d.id).unwrap();
    assert_eq!(bare.display_name(), port);
    assert_eq!(bare.label(), None);
}

/// ...and the dashboard must not undo it in the page.
#[test]
fn the_dashboard_shows_the_port_and_hangs_the_label_beside_it() {
    let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dashboard.html"))
        .unwrap();
    assert!(
        !page.contains("d.nickname || d.device"),
        "the page must not substitute a label for the port either"
    );
    assert!(
        page.contains("THE PORT NAMES THE CARD"),
        "the card is named by its port"
    );
    // That the label renders as its OWN element beside the port is proven where
    // it can actually be seen -- in the browser suite, which counts `.label`
    // chips in a rendered DOM (`naming_from_the_controller_panel_renames_the_
    // controller_not_the_console`). The assertion that used to live here counted
    // a variable name in this file, and duly broke on a rename that changed
    // nothing an operator can see. Source text is not behaviour.
    // The riskiest surface: the thing you are about to actuate.
    assert!(
        page.contains("target.textContent = d.device.split(\"/\").pop()"),
        "the power bar must name the PORT it will actuate"
    );
}

/// Making the port the identity must not break config an operator keyed by the
/// name they use for the board.
///
/// Caught by the decode suite the moment `display_name()` stopped returning the
/// nickname: a memory map under `[devices."board-a"]` silently stopped applying
/// and decode resolved against the rig-wide map instead -- no error, just a
/// different answer. Config should not care which of the two names you used.
#[test]
fn config_matches_a_device_by_its_port_or_by_its_label() {
    use conminer_core::config::{DeviceOverride, MemoryRegion, StateOverride};

    let mut cfg = Config::default();
    cfg.devices.insert(
        "board-a".to_string(),
        DeviceOverride {
            memory_map: vec![MemoryRegion {
                name: "usb_dp_combo_phy".into(),
                base: 0x088e_1000,
                size: 0x1000,
                note: None,
            }],
            state: Some(StateOverride {
                hung_after_s: Some(99),
            }),
            ..Default::default()
        },
    );

    let port = "/dev/serial/by-id/usb-Vendor_Board_ABCD-if00-port0";
    // Keyed by the label, addressed by the port: must still apply.
    assert_eq!(cfg.memory_map_for(&[port, "board-a"]).len(), 1);
    assert_eq!(cfg.hung_after_s_for(&[port, "board-a"]), 99);

    // ...and keyed by the port, with no label at all.
    let mut by_port = Config::default();
    by_port.devices.insert(
        port.to_string(),
        DeviceOverride {
            state: Some(StateOverride {
                hung_after_s: Some(7),
            }),
            ..Default::default()
        },
    );
    assert_eq!(by_port.hung_after_s_for(&[port, ""]), 7);

    // An unrelated device gets the defaults, not somebody else's override.
    assert_eq!(
        cfg.hung_after_s_for(&["/dev/serial/by-id/other-if00-port0", ""]),
        Config::default().state.hung_after_s
    );
}

/// Report #20: the EDL settle-watch is bounded by WALL-CLOCK, not by how many
/// times it scanned.
///
/// `usb::scan()` is a libusb descriptor probe of every device on the bus, and on
/// a populated host it costs SECONDS (measured 6.6 s on bravo). The watch loop
/// accounted only the sleep `step`, so a scan that took 6.6 s counted as 0.5 s;
/// an 8 s window then ran ~16 iterations, ~113 s of real time, and a
/// `power off verify=poke` took ~131 s instead of the documented ~8-30 s.
///
/// The scan here sleeps a real, tiny amount to stand in for that cost. Under the
/// old accounting this test's watch ran (window/step) scans and blew the bound;
/// counting the scan's own elapsed keeps the whole watch near `window`.
#[test]
fn the_edl_watch_is_bounded_by_wall_clock_not_scan_count() {
    let scan_cost = Duration::from_millis(40);
    let window = Duration::from_millis(120);
    let step = Duration::from_millis(20);
    let t0 = Instant::now();
    let probe = watch_for_edl_with(
        || {
            std::thread::sleep(scan_cost);
            Vec::new()
        },
        window,
        step,
        std::thread::sleep,
    );
    let elapsed = t0.elapsed();
    assert!(
        !probe.in_edl && probe.settled,
        "a clean bus settles: {probe:?}"
    );
    // Old code: ~ window/step = 6 scans * 40ms + 120ms of sleeps = ~360ms.
    // New code: the scan cost counts toward the window, so the whole watch is
    // ~window plus at most one final scan.
    assert!(
        elapsed < window + scan_cost * 2,
        "the EDL watch must be bounded by wall-clock, not the scan count: {elapsed:?} \
         (window {window:?}, scan {scan_cost:?})"
    );
}
