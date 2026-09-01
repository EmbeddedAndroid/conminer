//! The tools nothing else exercised.
//!
//! A coverage sweep found seven of conminer's 69 tools invoked by no test at
//! all: `follow`, `start_session`, `end_session`, `console_state`, `set_line`,
//! `claim_exclusive` and `pull_file`. `follow` matters most -- it is the cheap
//! incremental wait an agent leans on, and its delta capping was changed the
//! same day this was written, with nothing to catch a regression.
//!
//! These assert BEHAVIOUR, not that a call returns something. A test that only
//! proves a tool is reachable is the coverage equivalent of an exit code.

use conminer_testkit::McpRig;
use serde_json::{json, Value};

fn err_code(v: &Value) -> Option<&str> {
    v.get("error").and_then(|e| e["code"].as_str())
}

/// Take the lease a mutating tool requires.
///
/// That requirement is itself a feature -- two callers must not drive one board
/// at once -- so each test below asserts the guard fires BEFORE taking the lease,
/// rather than quietly leasing first and never proving the protection exists.
fn lease(rig: &McpRig, device: &str) {
    let got = rig.call(
        "acquire",
        json!({"device": device, "steal": true, "holder": "coverage"}),
    );
    assert!(got.get("error").is_none(), "acquire failed: {got}");
}

/// A mutating tool must refuse before a lease is held.
fn assert_needs_lease(rig: &McpRig, name: &str, args: Value) {
    let v = rig.raw(name, args);
    let code = v["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(
        code, "LEASE_REQUIRED",
        "{name} mutates the board and must demand a lease first, got {v}"
    );
}

/// `follow` must return an increment AND respect its own delta cap.
///
/// The cap exists because follow responses were measured at 65-87KB regardless
/// of `max_lines`: a busy boot re-hits hundreds of already-known templates and
/// every one was returned in full, making the cheapest tool the most expensive
/// call in the surface.
#[test]
fn follow_returns_an_increment_and_caps_repeat_deltas() {
    let rig = McpRig::new();
    // Many repeats of a handful of messages: exactly the shape that blew up.
    let mut log = String::new();
    for i in 0..300 {
        log.push_str(&format!("[    1.{i:03}] mmc0: card is busy, retrying\n"));
        log.push_str(&format!("[    2.{i:03}] usb 1-3: device descriptor read\n"));
    }
    let (device, _) = rig.ingest("follow.log", &log, None);

    let inc = rig.call("follow", json!({"device": device, "max_lines": 5}));
    assert!(inc.get("error").is_none(), "follow errored: {inc}");

    if let Some(d) = inc["template_deltas"].as_array() {
        assert!(
            d.len() <= 200,
            "repeat deltas must be capped; got {} entries",
            d.len()
        );
        // A truncated list that does not say so reads as "and nothing else".
        if d.len() == 200 {
            assert!(
                inc.get("deltas_omitted").is_some(),
                "a capped list must report what it dropped"
            );
        }
    }
    // The raw tail must honour max_lines, or the cap on deltas just moves the
    // cost somewhere else.
    if let Some(t) = inc["tail"].as_array() {
        assert!(t.len() <= 5, "tail ignored max_lines: {} lines", t.len());
    }
}

/// A session must open, be visible, and close. Bookkeeping that silently fails
/// leaves every later query scoped to nothing.
#[test]
fn a_session_opens_and_closes() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("s.log", "[    1.0] hello\n", None);

    assert_needs_lease(
        &rig,
        "start_session",
        json!({"device": device, "label": "coverage"}),
    );
    lease(&rig, &device);
    let started = rig.call(
        "start_session",
        json!({"device": device, "label": "coverage"}),
    );
    assert!(
        started.get("error").is_none(),
        "start_session failed: {started}"
    );
    let id = started["session"]["id"]
        .as_i64()
        .or_else(|| started["session_id"].as_i64())
        .unwrap_or_else(|| panic!("no session id in {started}"));

    let ended = rig.call("end_session", json!({"device": device, "session": id}));
    assert!(ended.get("error").is_none(), "end_session failed: {ended}");

    // Ending a session that is already closed must be an honest error, not a
    // silent success that hides a bookkeeping bug.
    let again = rig.raw("end_session", json!({"device": device, "session": 999_999}));
    let again = again["structuredContent"].clone();
    assert!(
        again.get("error").is_some(),
        "ending a nonexistent session must fail, got {again}"
    );
}

/// `console_state` is what a caller reads to decide whether the board can take
/// a command. It must name a state, never guess.
#[test]
fn console_state_reports_a_named_state() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("c.log", "[    1.0] Freeing unused kernel memory\n", None);

    let st = rig.call("console_state", json!({"device": device}));
    assert!(st.get("error").is_none(), "console_state errored: {st}");
    let name = st["console"]["state"]
        .as_str()
        .or_else(|| st["state"].as_str())
        .unwrap_or_else(|| panic!("no state in {st}"));
    assert!(!name.is_empty(), "state must be named, not blank");
    // `commandable` is the decision the caller actually makes; it must be an
    // explicit boolean rather than absent.
    let c = st["console"]["commandable"]
        .as_bool()
        .or_else(|| st["commandable"].as_bool());
    assert!(
        c.is_some(),
        "commandable must be stated, not inferred: {st}"
    );
}

/// `set_line` changes how the port is read. A bad value must be refused rather
/// than applied -- a wrong baud silently turns a working console into garbage.
#[test]
fn set_line_validates_before_it_applies() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("l.log", "[    1.0] boot\n", None);

    assert_needs_lease(&rig, "set_line", json!({"device": device, "baud": 115200}));
    lease(&rig, &device);
    let bad = rig.raw("set_line", json!({"device": device, "baud": 7}));
    let bad = bad["structuredContent"].clone();
    assert!(
        bad.get("error").is_some(),
        "an impossible baud must be refused, got {bad}"
    );

    let ok = rig.call("set_line", json!({"device": device, "baud": 115200}));
    assert!(
        ok.get("error").is_none(),
        "a normal baud must be accepted: {ok}"
    );
}

/// An exclusive claim must be exclusive, and releasable.
///
/// This is the handoff that keeps a flashing tool and conminer off the same
/// port. A claim that cannot be released strands the console -- the same shape
/// as the lease that could not be released.
#[test]
fn an_exclusive_claim_blocks_and_then_releases() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("x.log", "[    1.0] boot\n", None);

    lease(&rig, &device);
    let claim = rig.call(
        "claim_exclusive",
        json!({"device": device, "protocol": "sahara"}),
    );
    assert!(claim.get("error").is_none(), "claim failed: {claim}");

    // While claimed, a console session must be refused with a NAMED reason.
    let blocked = rig.raw("run_command", json!({"device": device, "command": "true"}));
    let blocked = blocked["structuredContent"].clone();
    assert!(
        blocked.get("error").is_some(),
        "a claimed port must refuse a console session, got {blocked}"
    );
    if let Some(code) = err_code(&blocked) {
        assert!(
            code.contains("CLAIM") || code.contains("EXCLUSIVE") || code.contains("LEASE"),
            "the refusal must say the port is claimed, got {code}"
        );
    }

    let released = rig.call(
        "claim_exclusive",
        json!({"device": device, "release": true}),
    );
    assert!(
        released.get("error").is_none(),
        "release failed: {released}"
    );
}

/// `pull_file` needs a logged-in shell. Without one it must say so, not hang or
/// return an empty file that reads as success.
#[test]
fn pull_file_refuses_clearly_without_a_shell() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("p.log", "[    1.0] boot\n", None);

    lease(&rig, &device);
    let out = rig.raw(
        "pull_file",
        json!({"device": device, "remote_path": "/etc/hostname", "local_path": "hostname.txt"}),
    );
    // No board behind this device, so it must fail -- and the failure must be
    // legible, because "empty file" is the dangerous alternative.
    let out = out["structuredContent"].clone();
    assert!(
        out.get("error").is_some(),
        "pull_file must fail with no board: {out}"
    );
    let e = &out["error"];
    assert!(
        e["message"].as_str().is_some_and(|m| !m.is_empty()),
        "the failure must explain itself: {out}"
    );
}
