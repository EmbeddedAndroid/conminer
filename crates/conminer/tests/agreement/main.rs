//! Suite `agreement` (W2) — the tools must not contradict each other.
//!
//! WHY THIS EXISTS. Every defect reported against conminer in the round that
//! prompted this suite was two tools disagreeing about ONE store at ONE instant:
//!
//!   #4  `console_state` said `at_prompt, commandable` while `follow
//!       {until:{prompt:true}}` timed out and called the console hung.
//!   #1  `follow` returned `matched=prompt` while `boot_report` said the epoch
//!       "has not reached a terminal state".
//!   #7  one `diagnose` response carried `edl: true` AND `commandable: true`.
//!
//! Each tool had its own passing tests. Nothing asserted they AGREE, so every
//! disagreement was invisible in the lab and glaring on the bench -- an agent
//! asks two questions, gets two answers, and files a report. Three of those
//! defects also shared a cause the per-tool tests could not see: the same
//! question was implemented four times.
//!
//! So this suite fixes no bug. It states the invariants that make a bug of that
//! shape impossible to ship, and it is expected to FAIL until the readers are
//! collapsed (W1) -- its failures are the work order.
//!
//! THE FIXTURES ARE HARDWARE SHAPES, NOT INVENTED ONES. Every scenario below was
//! measured on the bench, because the previous round of fixtures encoded the
//! author's model instead of the board's behaviour and passed while the board
//! failed. Where a scenario has a timing relationship, it is asserted as a
//! PRECONDITION so the fixture cannot quietly drift into testing nothing.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::SessionSource;
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use conminer_testkit::Rig;
use serde_json::{json, Value};
use std::sync::Arc;

/// A console whose state we built ourselves, queried through the real tools.
///
/// The pipeline writes (that is the only way to reach states like "the prompt is
/// still in the partial buffer"), and the MCP surface reads, on the same data
/// dir -- so the answers are exactly what an agent would receive.
struct Bench {
    rig: Rig,
    h: Handler,
    device: String,
}

impl Bench {
    fn new(rig: Rig, device: &str) -> Self {
        let mut cfg = Config::default();
        cfg.paths.data_dir = rig.path().to_path_buf();
        let ctx = Context::open(
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            rig.clock.clone(),
        )
        .expect("context on the same data dir");
        Self {
            rig,
            h: Handler::new(ctx),
            device: device.to_string(),
        }
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        let resp = self.h.handle(req).expect("a call always replies");
        let v = resp.result.unwrap_or(Value::Null);
        v.get("structuredContent").cloned().unwrap_or(v)
    }

    fn teach_prompt(&self) {
        self.call(
            "classify_prompt",
            json!({"device": self.device, "pattern": "^# $", "kind": "shell"}),
        );
    }

    fn attest(&self, state: conminer_core::live::CaptureState) {
        let mut reg = self.rig.registry();
        let row = reg.resolve(&self.device).unwrap();
        conminer_core::live::publish_capture_state(&mut reg, row.id, state).unwrap();
    }

    /// The three answers, taken as close together as a caller could take them.
    fn answers(&self) -> Answers {
        let cs = self.call("console_state", json!({"device": self.device}));
        let fo = self.call(
            "follow",
            json!({"device": self.device, "until": {"prompt": true},
                   "timeout_s": 1, "max_lines": 3}),
        );
        let br = self.call("boot_report", json!({"device": self.device}));
        Answers {
            console: cs["console"]["state"].as_str().unwrap_or("?").to_string(),
            commandable: cs["console"]["commandable"] == Value::Bool(true),
            why_not: cs["console"]["not_commandable_because"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            idle_ms: cs["freshness"]["idle_ms"].as_i64().unwrap_or(0),
            capture: cs["freshness"]["capture_state"]
                .as_str()
                .unwrap_or("?")
                .to_string(),
            follow_fired: fo["follow"]["matched"].as_str() == Some("prompt"),
            outcome: br["outcome"].as_str().unwrap_or("?").to_string(),
        }
    }
}

#[derive(Debug)]
struct Answers {
    console: String,
    commandable: bool,
    why_not: String,
    idle_ms: i64,
    capture: String,
    follow_fired: bool,
    outcome: String,
}

/// The invariants. Each returns the complaint when it is violated.
///
/// Stated as "these two answers cannot both be true", never as "this tool must
/// say X" -- the point is consistency, and pinning one tool's exact wording here
/// would just move the duplication into the test.
fn violations(a: &Answers) -> Vec<String> {
    let mut out = Vec::new();

    // R1. If the console is commandable, the predicate for "is it at a prompt"
    // must fire. These are the same question asked twice (#4).
    if a.commandable && !a.follow_fired {
        out.push(format!(
            "console says commandable ({}), but follow{{prompt}} did not fire",
            a.console
        ));
    }

    // R2. And the converse: a fired prompt predicate cannot coexist with a
    // console verdict that denies there is a prompt.
    if a.follow_fired && matches!(a.console.as_str(), "hung" | "no_signal" | "streaming") {
        out.push(format!(
            "follow{{prompt}} fired, but console says {}",
            a.console
        ));
    }

    // R3. A board at a prompt has, by definition, got somewhere. `boot_report`
    // calling that epoch unfinished is the third reader disagreeing (#1).
    if a.follow_fired && matches!(a.outcome.as_str(), "in_progress" | "hung") {
        out.push(format!(
            "follow{{prompt}} fired, but boot_report says {}",
            a.outcome
        ));
    }

    // R4. EDL removes the UART. Nothing may offer it (#7).
    if a.capture == "away_in_edl" && (a.commandable || a.follow_fired) {
        out.push(format!(
            "capture is away_in_edl, but commandable={} follow_fired={}",
            a.commandable, a.follow_fired
        ));
    }

    // R5. NO RESPONSE MAY ASSERT ACTIVITY ITS OWN FRESHNESS CONTRADICTS.
    // Measured on bravo: state=streaming with "the board is still producing
    // output" while idle_ms in the same response read 14,567 ms.
    const TALKING_MS: i64 = 2_000;
    if a.idle_ms > TALKING_MS && a.why_not.contains("still producing output") {
        out.push(format!(
            "console claims it is still producing output, but idle_ms={}",
            a.idle_ms
        ));
    }

    out
}

fn check(name: &str, a: Answers) -> Vec<String> {
    let v = violations(&a);
    if !v.is_empty() {
        eprintln!("--- {name}: {a:?}");
        for x in &v {
            eprintln!("      DISAGREEMENT: {x}");
        }
    }
    v.into_iter().map(|x| format!("{name}: {x}")).collect()
}

// ------------------------------------------------------------- the shapes ---

/// A board that reached its prompt and TERMINATED the line. The easy case, and
/// the only one the old fixtures could reach, because `feed`+`finish` flushed
/// the partial into a stored line.
fn terminated_prompt() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "term";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(b"APP admit\nCONSOLE\n# \n").unwrap();
        p.tick().unwrap();
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::Listening);
    (b, "terminated prompt, power epoch")
}

/// The resting state of a real board: the prompt has NO newline, so it lives in
/// the capture loop's partial buffer and is not a stored line at all.
fn unterminated_prompt() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "unterm";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(b"APP admit\nCONSOLE\n# ").unwrap();
        p.tick().unwrap();
        p.tick().unwrap();
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::Listening);
    (b, "unterminated prompt (the resting state of a real board)")
}

/// #4's measured cause: the board booted and went quiet BEFORE its own `power`
/// epoch was recorded, because the hook spends seconds in its off phase while
/// this board boots in under one. Uno-Q boot 483: partial seen 1786896518281,
/// epoch stamped 1786896528401 -- 10.1 s later.
fn epoch_stamped_after_its_own_output() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "late-epoch";
    {
        let mut p = rig.pipeline(dev, None);
        let sid = p
            .begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        let late = rig.clock.now_wall_ms() + 10_120;
        p.store_mut()
            .open_boot("power", None, late, Some(sid))
            .unwrap();
        p.adopt_external_boot().unwrap();
        p.feed(b"APP admit\nCONSOLE\n# ").unwrap();
        p.tick().unwrap();
        p.tick().unwrap();
        // PRECONDITION: the fixture must really reproduce the inverted order.
        let (_, seen_at) = p.store().pending_tail().unwrap().expect("a live partial");
        let row = p.store().latest_boot().unwrap().unwrap();
        assert!(
            row.opened_at > seen_at,
            "fixture must stamp the epoch AFTER the partial ({} vs {seen_at})",
            row.opened_at
        );
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::Listening);
    (b, "epoch stamped after its own output (Uno-Q boot 483)")
}

/// #4's other half: `session` epochs open on every capture reconnect -- one per
/// deploy -- without the board restarting. Uno-Q: epoch 473 was the power-on
/// that reached the prompt, 474-477 were empty session markers on top.
fn session_epochs_stacked_on_the_boot() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "stacked";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(b"APP admit\nCONSOLE\n# \n").unwrap();
        for _ in 0..4 {
            p.open_boot("session", None).unwrap();
        }
        p.tick().unwrap();
        // PRECONDITION: the newest epoch must be an empty session marker.
        let newest = p.store().latest_boot().unwrap().unwrap();
        assert_eq!(newest.opened_by, "session");
        assert_eq!(newest.bytes, 0);
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::Listening);
    (
        b,
        "empty session epochs stacked on the boot (Uno-Q 473 + 474-477)",
    )
}

/// A board that said something and then went quiet WITHOUT reaching a prompt --
/// the ordinary state of a board that is boot looping, which is the common case
/// during firmware development.
fn quiet_without_a_prompt() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "quiet";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(b"APP admit\nMCU UART APP READY probe retained-baud expected=115200\n")
            .unwrap();
        p.tick().unwrap();
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::Listening);
    // Long past TALKING_MS, well short of the hung threshold: the window where
    // "still producing output" was measured to be false.
    b.rig.advance_ms(15_000);
    (b, "quiet 15s without a prompt (a boot-looping board)")
}

/// #7: the board is in EDL, so its UART re-enumerated away -- but the prompt it
/// was sitting at is still in the buffer.
fn in_edl_with_a_stale_prompt() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "edl";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(b"APP admit\nCONSOLE\n# ").unwrap();
        p.tick().unwrap();
        p.tick().unwrap();
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::AwayInEdl);
    (b, "in EDL with the old prompt still in the buffer")
}

/// Report #22, the shape that reopened it: the board reached a prompt, then
/// entered a flash/recovery mode whose progress serial (DevProg/Firehose) streams
/// real output in a SESSION epoch -- so `capture_state` is a live `away_in_edl`
/// while bytes keep arriving, not the quiet EDL of the case above. Every reader
/// must still refuse the pre-flash prompt: `follow` matched it and a set_image
/// freshness view read commandable while diagnose said away_in_edl. On hardware
/// the capture layer now sets `away_in_edl` from the recovery gadget on the
/// board's ports (fix #1); here it is attested directly, and the invariant is
/// that no reader offers a commandable console once it is set.
fn flashing_with_devprog_output_after_a_prompt() -> (Bench, &'static str) {
    let rig = Rig::new();
    let dev = "flash";
    {
        let mut p = rig.pipeline(dev, None);
        p.begin_session(SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();
        p.feed(
            b"APP admit
CONSOLE
# ",
        )
        .unwrap();
        p.tick().unwrap();
        // The UART re-enumerated to the flasher's progress serial: real output,
        // in a SESSION epoch, well after the prompt -- and not a prompt itself.
        rig.advance_ms(30_000);
        p.open_boot("session", None).unwrap();
        p.feed(
            b"DevProg: DDR init
USB: ZLP received
DevProg: flashing lun0
",
        )
        .unwrap();
        p.tick().unwrap();
    }
    let b = Bench::new(rig, dev);
    b.teach_prompt();
    b.attest(conminer_core::live::CaptureState::AwayInEdl);
    (
        b,
        "flashing: DevProg output in a session epoch after a prompt (report #22)",
    )
}

// ------------------------------------------------------------ the invariant --

/// THE ONE TEST. Every shape, every invariant, one verdict.
///
/// Reported as a single list rather than one test per shape on purpose: the
/// value is the FULL set of disagreements, because that set is the work order.
/// The gap that let report #23 through: this suite asserted the READERS agree on
/// `commandable`, but never that a TRANSMITTER refuses to push bytes into a board
/// whose console is authoritatively `away_in_edl`. `run_command` probed the UART
/// with a newline and returned NO_PROMPT while the board was being flashed. The
/// contract is not just "no reader claims commandable" -- it is "no tool that
/// transmits may transmit into a non-commandable recovery console".
#[test]
fn a_transmit_tool_refuses_when_the_board_is_in_recovery_mode() {
    fn refuses(b: &Bench, name: &str) {
        b.call("acquire", json!({"device": b.device, "ttl_s": 60}));
        // run_command must refuse BEFORE probing the UART, with a recovery-mode
        // error -- never a NO_PROMPT after a one-second newline probe.
        let rc = b.call(
            "run_command",
            json!({"device": b.device, "command": "version", "timeout_s": 3}),
        );
        assert_eq!(
            rc["error"]["code"], "AWAY_IN_EDL",
            "{name}: run_command must refuse in recovery mode, not probe the UART: {rc}"
        );
        // send must not transmit either: refused for recovery, or disabled by
        // config -- both mean zero bytes reach a board being flashed.
        let sd = b.call("send", json!({"device": b.device, "data": "\n"}));
        assert!(
            matches!(
                sd["error"]["code"].as_str(),
                Some("AWAY_IN_EDL") | Some("SEND_DISABLED")
            ),
            "{name}: send must not push bytes into a board in recovery mode: {sd}"
        );
    }
    let (b, n) = in_edl_with_a_stale_prompt();
    refuses(&b, n);
    let (b, n) = flashing_with_devprog_output_after_a_prompt();
    refuses(&b, n);
}

#[test]
fn no_two_tools_may_disagree_about_one_console() {
    let mut all: Vec<String> = Vec::new();

    let (b, n) = terminated_prompt();
    all.extend(check(n, b.answers()));
    let (b, n) = unterminated_prompt();
    all.extend(check(n, b.answers()));
    let (b, n) = epoch_stamped_after_its_own_output();
    all.extend(check(n, b.answers()));
    let (b, n) = session_epochs_stacked_on_the_boot();
    all.extend(check(n, b.answers()));
    let (b, n) = quiet_without_a_prompt();
    all.extend(check(n, b.answers()));
    let (b, n) = in_edl_with_a_stale_prompt();
    all.extend(check(n, b.answers()));
    let (b, n) = flashing_with_devprog_output_after_a_prompt();
    all.extend(check(n, b.answers()));

    assert!(
        all.is_empty(),
        "the tools disagree about {} case(s):\n  {}",
        all.len(),
        all.join("\n  ")
    );
}
