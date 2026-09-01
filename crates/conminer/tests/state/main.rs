//! Suite `state` (§8.5, §13) — perceived console state and the loop taxonomy.
//!
//! Edge cases: every state reachable and correctly derived from a replayed
//! corpus · each loop-taxonomy class classified correctly (including
//! watchdog-vs-crash and flapping) · the default credential-gate set triggers
//! `login_wait`, a custom greeter only after teaching · an autologin corpus goes
//! straight to `at_prompt` · an unfamiliar idle line → `at_unknown_prompt` with
//! `prompt:true` withheld · teaching persists across a restart · no gate ever
//! satisfies `prompt:true` · state survives a restart because it is rebuilt from
//! the store, not from RAM.

use conminer_core::console::{derive, ConsoleState, LoopKind, Observation};
use conminer_core::framer::profile::PromptKind;
use conminer_core::live::CaptureState;
use conminer_core::runner::{Prompt, Prompts};
use conminer_core::store::DeviceStore;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::Rig;

fn prompts() -> Prompts {
    Prompts(vec![
        Prompt {
            re: regex::Regex::new(r"(^|\n)# $").unwrap(),
            raw: "# ".into(),
            kind: PromptKind::Shell,
        },
        Prompt {
            re: regex::Regex::new(r"(^|\n)=> $").unwrap(),
            raw: "=> ".into(),
            kind: PromptKind::Bootloader,
        },
        Prompt {
            re: regex::Regex::new(r"(^|\n)[\w.-]* ?login: *$").unwrap(),
            raw: "login: ".into(),
            kind: PromptKind::CredentialGate,
        },
    ])
}

fn obs(capture: CaptureState, now_ms: i64) -> Observation {
    Observation {
        capture,
        now_ms,
        hung_after_ms: 30_000,
        loop_min_epochs: 3,
        active_txn: None,
    }
}

fn state_of(store: &DeviceStore, capture: CaptureState, now_ms: i64) -> ConsoleState {
    derive(store, &prompts(), &obs(capture, now_ms)).unwrap()
}

// ------------------------------------------------------- attestation first ---

#[test]
fn without_capture_attestation_the_state_is_unknown_not_no_signal() {
    let rig = Rig::new();
    let store = rig.ingest_text("s1", None, "");
    assert_eq!(
        state_of(&store, CaptureState::NotListening, 0),
        ConsoleState::Unknown,
        "'I do not know' is a different answer from 'nothing arrived'"
    );
}

#[test]
fn no_signal_requires_a_live_listening_claim() {
    let rig = Rig::new();
    let store = rig.ingest_text("s2", None, "");
    assert_eq!(
        state_of(&store, CaptureState::Listening, 0),
        ConsoleState::NoSignal
    );
}

#[test]
fn a_baud_mismatch_is_named_garbage_rather_than_shown_as_noise() {
    let rig = Rig::new();
    let store = rig.ingest_text("s3", None, "some output\n");
    assert!(matches!(
        state_of(&store, CaptureState::Garbage, 0),
        ConsoleState::Garbage { .. }
    ));
}

// ------------------------------------------------------------- prompt states -

#[test]
fn a_shell_prompt_is_at_prompt_and_commandable() {
    let rig = Rig::new();
    let store = rig.ingest_text("s4", None, "[ 1.0] Run /sbin/init as init process\n# ");
    let s = state_of(&store, CaptureState::Listening, 0);
    assert!(matches!(s, ConsoleState::AtPrompt { .. }), "{s:?}");
    assert!(s.commandable());
}

#[test]
fn a_credential_gate_is_login_wait_and_never_commandable() {
    let rig = Rig::new();
    let store = rig.ingest_text(
        "s5",
        None,
        "[ 1.0] Run /sbin/init as init process\nboard login: ",
    );
    let s = state_of(&store, CaptureState::Listening, 0);
    assert!(matches!(s, ConsoleState::LoginWait { .. }), "{s:?}");
    assert!(
        !s.commandable(),
        "the board is up but not commandable; prompt:true must not fire"
    );
}

#[test]
fn an_autologin_image_goes_straight_to_at_prompt_with_no_gate_state() {
    let rig = Rig::new();
    let store = rig.ingest_text(
        "s6",
        None,
        "[ 1.0] Run /sbin/init as init process\n[ 2.0] autologin: root\n# ",
    );
    let s = state_of(&store, CaptureState::Listening, 0);
    assert!(matches!(s, ConsoleState::AtPrompt { .. }), "{s:?}");
}

#[test]
fn an_unfamiliar_idle_line_is_at_unknown_prompt_not_a_guess() {
    let rig = Rig::new();
    let store = rig.ingest_text("s7", None, "[ 1.0] NOTICE: BL31 handoff\nnucleus> ");
    // Long silence at a line matching neither a prompt nor a gate.
    let now = store.recent_lines(1).unwrap()[0].ts_wall + 60_000;
    let s = state_of(&store, CaptureState::Listening, now);
    match &s {
        ConsoleState::AtUnknownPrompt { observed_line } => {
            assert!(observed_line.contains("nucleus>"), "{observed_line}")
        }
        other => panic!("expected at_unknown_prompt, got {other:?}"),
    }
    assert!(
        !s.commandable(),
        "prompt:true is withheld for an unknown gate"
    );
}

#[test]
fn a_taught_prompt_is_honoured_and_survives_a_restart() {
    let rig = Rig::new();
    let dev = rig.device("s8");
    {
        let mut store = rig.store(&dev);
        store
            .learn_prompt("nucleus> ", "monitor", "learned", None, 1)
            .unwrap();
    }
    // Reopened from disk: the teaching is store state, not process memory.
    let store = rig.store(&dev);
    let learned = store.prompts(None).unwrap();
    assert_eq!(learned.len(), 1);
    assert_eq!(learned[0].kind, "monitor");
    assert_eq!(learned[0].provenance, "learned");
    assert!(PromptKind::parse(&learned[0].kind)
        .unwrap()
        .is_commandable());
}

#[test]
fn no_credential_gate_kind_is_ever_commandable() {
    assert!(!PromptKind::CredentialGate.is_commandable());
    for k in [
        PromptKind::Shell,
        PromptKind::Bootloader,
        PromptKind::RtosShell,
        PromptKind::Monitor,
    ] {
        assert!(k.is_commandable(), "{k:?}");
    }
    assert!(!PromptKind::Ignore.is_commandable());
}

// -------------------------------------------------------------- progression --

#[test]
fn a_boot_in_progress_reports_the_stage_it_is_in() {
    let rig = Rig::new();
    let store = rig.ingest_text(
        "s9",
        None,
        "NOTICE:  BL31: v2.11(release):v2.11\nERROR: x\n",
    );
    match state_of(&store, CaptureState::Listening, 0) {
        ConsoleState::Booting { stage } => assert_eq!(stage, "bl31"),
        other => panic!("expected booting, got {other:?}"),
    }
}

#[test]
fn a_silent_incomplete_boot_is_hung_with_its_deepest_stage() {
    let rig = Rig::new();
    let store = rig.ingest_text("s10", None, "NOTICE:  BL31: v2.11(release):v2.11\n");
    let now = store.recent_lines(1).unwrap()[0].ts_wall + 120_000;
    match state_of(&store, CaptureState::Listening, now) {
        ConsoleState::Hung { stage, silent_ms } => {
            assert_eq!(stage, "bl31");
            assert!(silent_ms >= 120_000);
        }
        // A trailing line the profile does not know could equally be something
        // waiting for input; either answer is honest, a guess would not be.
        ConsoleState::AtUnknownPrompt { .. } => {}
        other => panic!("expected hung or at_unknown_prompt, got {other:?}"),
    }
}

// ------------------------------------------------------------ loop taxonomy --

fn loop_corpus(iterations: usize, body: &str) -> String {
    let mut s = String::new();
    for _ in 0..iterations {
        s.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        s.push_str("U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n");
        s.push_str("[    0.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n");
        s.push_str(body);
    }
    s
}

#[test]
fn a_stable_loop_is_classified_with_its_count_and_fingerprint() {
    let rig = Rig::new();
    let store = rig.ingest_text("s11", None, &loop_corpus(6, "[    1.0] mmc0: card ready\n"));
    match state_of(&store, CaptureState::Listening, 0) {
        ConsoleState::BootLooping {
            kind,
            count,
            fingerprint,
        } => {
            assert!(count >= 3, "count {count}");
            assert!(!fingerprint.is_empty());
            assert!(
                matches!(kind, LoopKind::Stable | LoopKind::StageCapped),
                "{kind:?}"
            );
        }
        other => panic!("expected boot_looping, got {other:?}"),
    }
}

#[test]
fn a_crash_loop_is_distinguished_from_never_booting() {
    let rig = Rig::new();
    let store = rig.ingest_text(
        "s12",
        None,
        &loop_corpus(
            5,
            "[    1.4] Kernel panic - not syncing: VFS: Unable to mount root fs\n\
             [    1.4] ---[ end Kernel panic - not syncing: VFS ]---\n",
        ),
    );
    match state_of(&store, CaptureState::Listening, 0) {
        ConsoleState::BootLooping { kind, .. } => {
            assert_eq!(kind, LoopKind::Crash, "it boots and *then* dies");
        }
        other => panic!("expected boot_looping, got {other:?}"),
    }
}

#[test]
fn a_watchdog_cycle_is_distinguished_from_a_crash() {
    let rig = Rig::new();
    let store = rig.ingest_text(
        "s13",
        None,
        &loop_corpus(
            5,
            "[   30.0] watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [swapper:1]\n",
        ),
    );
    match state_of(&store, CaptureState::Listening, 0) {
        ConsoleState::BootLooping { kind, .. } => assert_eq!(
            kind,
            LoopKind::Watchdog,
            "it hangs and gets shot, rather than crashing"
        ),
        other => panic!("expected boot_looping, got {other:?}"),
    }
}

#[test]
fn a_stage_capped_loop_never_reaches_userspace_and_emits_no_crash() {
    let rig = Rig::new();
    let mut text = String::new();
    for _ in 0..5 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        text.push_str("NOTICE:  BL31: v2.11(release):v2.11\n");
    }
    let store = rig.ingest_text("s14", None, &text);
    match state_of(&store, CaptureState::Listening, 0) {
        ConsoleState::BootLooping { kind, .. } => assert_eq!(kind, LoopKind::StageCapped),
        other => panic!("expected boot_looping, got {other:?}"),
    }
}

#[test]
fn diverging_fingerprints_without_progress_are_unstable_not_looping() {
    let rig = Rig::new();
    let mut text = String::new();
    // Each iteration differs, so no two fingerprints match.
    //
    // The difference must be in the WORDS, not in numbers: numeric literals are
    // masked before clustering (that is the point of the mask), so
    // "unique-line-1" and "unique-line-2" are the SAME template and would have
    // made this loop look stable rather than diverging.
    // Structurally different lines, not one line with a varying token: lines
    // that differ only in a value now MEET and generalize (that is the #32
    // fix), which would make this loop look stable instead of diverging. Real
    // divergence means genuinely different messages.
    const DIVERGENT: [&str; 6] = [
        "mmc0: error -110 whilst initialising SD card",
        "ubi0 warning: bad PEB detected during scan, moving on",
        "EXT4-fs (sda1): mounted filesystem without journal in ordered mode",
        "random: crng init done after waiting for entropy from the pool",
        "systemd-udevd timed out waiting for the device node to appear",
        "watchdog: BUG soft lockup CPU stuck for 22s in a kernel thread",
    ];
    for line in DIVERGENT {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        text.push_str("[    0.0] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n");
        text.push_str(&format!("[    1.0] {line}\n"));
    }
    let store = rig.ingest_text("s15", None, &text);
    // Judged AFTER the output stopped, which is when this verdict is meaningful.
    // `unstable` is a conclusion about a chain of finished epochs; a console that
    // produced a line moments ago is still talking, and round 4 measured the IQ10
    // being called `unstable` mid-boot while printing 6 KB in 8 s. Anchored to
    // the last line's own timestamp rather than a guessed constant, because the
    // ingest clock decides when these lines "happened".
    let last_at = store.recent_lines(1).unwrap()[0].ts_wall;
    // Past the HUNG THRESHOLD, which is where the epoch chain legitimately takes
    // over. "Recently enough to still count as talking" is the configured
    // `hung_after_s` (30 s here), not a constant: the IQ10 prints its GMU init
    // line every 15 s, and a two-second window called that board `unstable`
    // between messages.
    let s = state_of(&store, CaptureState::Listening, last_at + 35_000);
    assert!(
        matches!(
            s,
            ConsoleState::Unstable { .. }
                | ConsoleState::BootLooping {
                    kind: LoopKind::Flapping,
                    ..
                }
        ),
        "flaky must not be reported as a deterministic loop: {s:?}"
    );
}

// ------------------------------------------------------------- persistence ---

#[test]
fn the_state_is_rebuilt_from_the_store_not_from_process_memory() {
    let rig = Rig::new();
    let dev = rig.device("s16");
    let text = loop_corpus(5, "[    1.0] mmc0: card ready\n");
    {
        let _ = rig.ingest_text("s16", None, &text);
    }
    // A fresh handle — as a restarted minerd or a separate mcpd would have.
    let reopened = rig.store(&dev);
    let s = state_of(&reopened, CaptureState::Listening, 0);
    assert!(
        matches!(s, ConsoleState::BootLooping { .. }),
        "state survives a restart: {s:?}"
    );
}

#[test]
fn a_transaction_in_flight_outranks_every_other_signal() {
    let rig = Rig::new();
    let store = rig.ingest_text("s17", None, &corpus_text("linux/boot-oops.log"));
    let mut o = obs(CaptureState::Streaming, 0);
    o.active_txn = Some("txn-42".into());
    match derive(&store, &prompts(), &o).unwrap() {
        ConsoleState::InCommand { txn_id } => assert_eq!(txn_id, "txn-42"),
        other => panic!("expected in_command, got {other:?}"),
    }
}

/// EDL OUTRANKS THE TAIL.
///
/// Entering EDL re-enumerates the board's USB and takes the UART with it, so no
/// new bytes can arrive to contradict whatever was last on screen. The prompt
/// sitting in the store is a photograph of a console that no longer exists --
/// and classification kept reporting `at_prompt, commandable: true` from it, in
/// the response of the very `boot_mode(EDL)` call that had just made the console
/// disappear. An agent reading that types into nothing.
#[test]
fn a_board_in_edl_is_not_at_a_prompt_however_good_the_tail_looks() {
    let rig = Rig::new();
    let mut p = rig.pipeline("edl", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    // A perfectly good shell prompt, captured before the board went away.
    p.feed(b"[    1.0] Run /sbin/init as init process\n# ")
        .unwrap();
    p.finish().unwrap();
    let store = p.into_store();

    // Same store, same tail: only the capture attestation differs.
    let at_prompt = derive(&store, &prompts(), &obs(CaptureState::Listening, 5_000)).unwrap();
    assert!(
        matches!(at_prompt, ConsoleState::AtPrompt { .. }),
        "control: this tail IS a prompt while the console exists: {at_prompt:?}"
    );

    let in_edl = derive(&store, &prompts(), &obs(CaptureState::AwayInEdl, 5_000)).unwrap();
    assert!(
        matches!(in_edl, ConsoleState::AwayInEdl),
        "a board in EDL has no console to be at a prompt on: {in_edl:?}"
    );
    assert!(!in_edl.commandable(), "and nothing to type at: {in_edl:?}");
    assert!(
        in_edl
            .not_commandable_because()
            .is_some_and(|w| w.contains("EDL")),
        "the reason must name EDL, so the next action is obvious: {:?}",
        in_edl.not_commandable_because()
    );
}

/// ATTACHING TO A BOARD IS NOT THE BOARD REBOOTING.
///
/// Epochs open for several reasons and only some are the board restarting: a
/// `session` epoch is an agent attaching, and one that recorded zero bytes has
/// no behaviour to compare. Counting them made a false boot-loop generator --
/// measured on the Uno-Q, whose last epochs were three zero-byte `session`
/// attaches sharing one fingerprint (the hash of nothing happening), which is
/// exactly `loop_min`. A board sitting at its prompt answering commands was
/// reported as `boot_looping`.
#[test]
fn repeated_session_attaches_are_not_a_boot_loop() {
    let rig = Rig::new();
    let mut p = rig.pipeline("attaches", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    // Terminated, so nothing carries into the next epoch and each attach sees
    // exactly the same thing.
    p.feed(b"[    1.0] Run /sbin/init as init process\n")
        .unwrap();
    let mut p2 = p;
    // Three agent attaches. Each opens an epoch, and each sees the same thing --
    // the board sitting at its prompt -- so they share one fingerprint, exactly
    // as the Uno-Q's did. Three is `loop_min`.
    for _ in 0..3 {
        // Through the PIPELINE, which is what attributes bytes to an epoch --
        // opening one on the store behind its back left every byte in the first
        // epoch and the fixture proved nothing.
        p2.open_boot("session", None).unwrap();
        p2.feed(b"root@lab:~# \n").unwrap();
    }
    p2.finish().unwrap();
    let store = p2.into_store();

    // THE FIXTURE MUST ACTUALLY REPRODUCE THE SHAPE, or this gate proves nothing:
    // three `session` epochs that share one fingerprint. Asserted, because the
    // first version of this test passed with the fix reverted -- the epochs it
    // built carried no fingerprint at all, so the analysis bailed before the
    // filter could matter.
    let boots = store.list_boots(10).unwrap();
    let session_fps: Vec<String> = boots
        .iter()
        .filter(|b| b.opened_by == "session")
        .filter_map(|b| b.fingerprint.clone())
        .take(3)
        .collect();
    assert!(
        session_fps.len() >= 3 && session_fps.iter().all(|f| *f == session_fps[0]),
        "fixture premise not met: {} session epochs sharing a fingerprint, from {:?}",
        session_fps.len(),
        boots
            .iter()
            .map(|b| (b.opened_by.clone(), b.bytes, b.fingerprint.clone()))
            .collect::<Vec<_>>()
    );

    let state = state_of(&store, CaptureState::Listening, 5_000);
    assert!(
        !matches!(
            state,
            ConsoleState::BootLooping { .. } | ConsoleState::Unstable { .. }
        ),
        "three attaches are not three failed boots: {state:?}"
    );
}

/// ...and the real detector must survive it: a board that keeps rebooting into
/// the same failure is still a boot loop, which is the whole reason this
/// analysis exists.
#[test]
fn a_board_that_really_reboots_into_the_same_failure_is_still_a_loop() {
    let rig = Rig::new();
    let mut p = rig.pipeline("looper", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    // Four identical power-on attempts that each say something and never reach a
    // prompt: the genuine article.
    for _ in 0..4 {
        p.store_mut().open_boot("power", None, 1_000, None).unwrap();
        p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\nsynchronous external abort\n")
            .unwrap();
    }
    p.finish().unwrap();
    let store = p.into_store();

    let state = state_of(&store, CaptureState::Listening, 5_000);
    assert!(
        matches!(
            state,
            ConsoleState::BootLooping { .. } | ConsoleState::Unstable { .. }
        ),
        "a board rebooting into the same failure must still be caught: {state:?}"
    );
}

/// ORDINARY OUTPUT IS NOT AN UNRECOGNISED PROMPT.
///
/// Reported from the bench on boot 409: the Uno-Q reached its kernel, ran its
/// whole resident oracle, printed `CONSOLE`, and had not yet reached its shell.
/// conminer called `CONSOLE` an unknown prompt -- which invites an operator to
/// teach a pattern for a line that is not a prompt, and a taught `CONSOLE` would
/// then fire every later `follow {until: prompt}` on ordinary output.
///
/// Silence after a line that ENDED is silence. The board said something and
/// stopped; that is `hung`, and it is the honest answer.
#[test]
fn a_quiet_console_whose_last_output_just_ended_is_not_at_an_unknown_prompt() {
    let rig = Rig::new();
    let mut p = rig.pipeline("oracle", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    // Exactly the shape reported: the oracle finishes, and the last thing on the
    // wire is a COMPLETE line of output.
    p.feed(b"KTEST common.oracle PASS\nAPP admit\nCONSOLE\n")
        .unwrap();
    let store = p.into_store();

    let now = store.recent_lines(1).unwrap()[0].ts_wall + 60_000;
    let s = state_of(&store, CaptureState::Listening, now);
    assert!(
        !matches!(s, ConsoleState::AtUnknownPrompt { .. }),
        "`CONSOLE` is output the board finished printing, not a gate: {s:?}"
    );
    assert!(
        matches!(s, ConsoleState::Hung { .. }),
        "the board spoke and stopped, which is what hung means: {s:?}"
    );
}

/// ...and a console genuinely sitting on an unterminated line conminer does not
/// know is still reported, because that is the case worth teaching.
#[test]
fn a_console_resting_on_an_unrecognised_unterminated_line_is_still_flagged() {
    let rig = Rig::new();
    let mut p = rig.pipeline("gate", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[ 1.0] NOTICE: BL31 handoff\n").unwrap();
    let mut store = p.into_store();
    // The capture loop publishes the unterminated line it is holding; that
    // buffer is the only place a live prompt exists until enter is pressed.
    let seen_at = store.recent_lines(1).unwrap()[0].ts_wall;
    store.set_pending_tail("nucleus> ", seen_at).unwrap();

    let now = seen_at + 60_000;
    match state_of(&store, CaptureState::Listening, now) {
        ConsoleState::AtUnknownPrompt { observed_line } => {
            assert!(observed_line.contains("nucleus>"), "{observed_line}");
        }
        other => panic!("an unterminated unfamiliar line is exactly what to teach: {other:?}"),
    }
}

/// A BOARD CAN FINISH BOOTING BEFORE ITS OWN POWER EPOCH IS RECORDED.
///
/// The same discard that made `follow` blind made `console_state` say `hung`
/// and `boot_report` say `hung`, because all three read the prompt through
/// `pending_partial`. Measured on the Uno-Q, boot 483: the partial held
/// `sirocco> ` stamped 1786896518281 while the `power` epoch containing that
/// boot was stamped 1786896528401 -- 10.1 s LATER, because `power` spends up to
/// 8 s in its off phase before the epoch is recorded and this board boots in
/// well under a second. Judged by the clock alone the live prompt looked like
/// one from before the reboot.
#[test]
fn a_board_that_booted_before_its_power_epoch_was_recorded_is_at_a_prompt_not_hung() {
    use conminer_core::store::SessionSource;
    let rig = Rig::new();
    let mut p = rig.pipeline("late-power-epoch", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();

    // mcpd records the epoch with the time its hook finished, well after the
    // board had already booted and gone quiet.
    let late = rig.clock.now_wall_ms() + 10_120;
    let boot = p
        .store_mut()
        .open_boot("power", None, late, Some(sid))
        .unwrap();
    p.adopt_external_boot().unwrap();
    p.feed(b"APP admit\r\nCONSOLE\r\n# ").unwrap();
    p.tick().unwrap();
    p.tick().unwrap();
    let store = p.into_store();

    // NON-VACUITY: reproduce the measured ordering.
    let (_, seen_at) = store.pending_tail().unwrap().expect("a live partial");
    let row = store.boot(boot.id).unwrap();
    assert!(
        row.opened_at > seen_at && row.bytes > 0,
        "the epoch must be stamped after the partial AND contain the boot: {row:?}"
    );

    // Long past the hung threshold: silence is exactly what a board sitting at
    // a prompt looks like, which is why this must be decided by the prompt.
    let st = state_of(&store, CaptureState::Listening, row.opened_at + 200_000);
    assert!(
        matches!(st, ConsoleState::AtPrompt { .. }),
        "the board is sitting at its prompt, not hung: {st:?}"
    );
}

/// Report #21: a COLD BOOT that never reached its own prompt must not inherit
/// the previous boot's.
///
/// Measured on the Uno-Q: boot A reached `# ` (a commandable prompt that lives
/// only in the partial buffer, being unterminated). The board was power-cycled,
/// and the new boot printed its banner through `smp: bringup begin` and went
/// silent -- never reaching a prompt. `console_state` reported `at_prompt,
/// commandable` and `follow{prompt}` matched, both from boot A's stale `# `,
/// while `run_command` correctly got zero bytes.
///
/// The old staleness test discarded a partial only when a LATER actuation epoch
/// had EXACTLY zero bytes; a real off/on epoch emits a few noise bytes (a poke
/// echo, reset noise -- ~9 on the bench), so the stale prompt survived. The tell
/// is that a new boot began SPEAKING after the partial was seen.
#[test]
fn a_cold_boot_that_never_reached_a_prompt_does_not_inherit_the_previous_one() {
    use conminer_core::store::SessionSource;
    let rig = Rig::new();
    let mut p = rig.pipeline("cold-boot-stale", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();

    // The moment boot A's prompt was seen -- before the reboot.
    let seen_at = rig.clock.now_wall_ms();

    // The power-cycle: a COLD boot that begins speaking well after boot A's
    // prompt and stops before any prompt of its own. Its bytes are non-zero,
    // which is exactly what defeated the old bytes==0 test.
    rig.advance_ms(30_000);
    p.store_mut()
        .open_boot("power", None, rig.clock.now_wall_ms(), Some(sid))
        .unwrap();
    p.adopt_external_boot().unwrap();
    p.feed(b"[    0.10] cold boot\n[    0.55] smp: bringup begin requested=4\n")
        .unwrap();
    p.tick().unwrap();

    // The stale prompt from boot A is still sitting in the partial buffer, as it
    // was on hardware (the capture keeps the last non-empty partial). Placed
    // explicitly so the discard is what is under test, not framer timing.
    p.store_mut().set_pending_tail("# ", seen_at).unwrap();
    let store = p.into_store();

    // NON-VACUITY: the stale prompt really is present and really is a prompt.
    let (still, ts) = store
        .pending_tail()
        .unwrap()
        .expect("the stale partial must persist for this to be a real test");
    assert!(still.contains('#') && ts == seen_at, "{still:?} @ {ts}");

    // The cold boot's first line is stamped after `seen_at`, so the prompt is
    // from the previous boot and must be discarded.
    let st = state_of(&store, CaptureState::Listening, seen_at + 200_000);
    assert!(
        !matches!(
            st,
            ConsoleState::AtPrompt { .. } | ConsoleState::AtPromptWithTraffic { .. }
        ),
        "a cold boot that stopped before its own prompt must not inherit boot A's: {st:?}"
    );
}

/// Report #22: EDL/DevProg output that lands in a SESSION epoch must clear the
/// pre-EDL prompt.
///
/// Entering EDL re-enumerates the UART to a DevProg/Firehose console; ser2net
/// reconnects and its ~34 KB of output is captured in a `session` epoch (a
/// reconnect, not an actuation). The old staleness rule only invalidated a
/// partial on an ACTUATION epoch, so the pre-EDL `#` prompt survived into a
/// board that was being flashed -- follow(until prompt) matched it and
/// console_state read commandable, one keystroke from writing into Firehose.
#[test]
fn devprog_output_in_a_session_epoch_clears_the_pre_edl_prompt() {
    use conminer_core::store::SessionSource;
    let rig = Rig::new();
    let mut p = rig.pipeline("edl-devprog-stale", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();

    // The moment the pre-EDL prompt was seen.
    let seen_at = rig.clock.now_wall_ms();

    // The board enters EDL and re-enumerates; DevProg output is captured in a
    // SESSION epoch well after the prompt, and it is NOT a prompt.
    rig.advance_ms(30_000);
    p.store_mut()
        .open_boot("session", None, rig.clock.now_wall_ms(), Some(sid))
        .unwrap();
    p.adopt_external_boot().unwrap();
    p.feed(b"DevProg: DDR init\nUSB: ZLP received\nDevProg: flashing lun0\n")
        .unwrap();
    p.tick().unwrap();

    // The pre-EDL prompt is still in the partial buffer (the capture keeps the
    // last non-empty partial); placed explicitly so the discard is under test.
    p.store_mut().set_pending_tail("# ", seen_at).unwrap();
    let store = p.into_store();

    // NON-VACUITY: the stale prompt really is present.
    let (still, ts) = store
        .pending_tail()
        .unwrap()
        .expect("the stale partial must persist for this to be a real test");
    assert!(still.contains('#') && ts == seen_at, "{still:?} @ {ts}");

    // The session captured DevProg output after the prompt was seen, so the
    // prompt is stale -- even though no ACTUATION epoch was involved.
    let st = state_of(&store, CaptureState::Listening, seen_at + 200_000);
    assert!(
        !matches!(
            st,
            ConsoleState::AtPrompt { .. } | ConsoleState::AtPromptWithTraffic { .. }
        ),
        "DevProg output in a session epoch must clear the pre-EDL prompt (report #22): {st:?}"
    );
}
