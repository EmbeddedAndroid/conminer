//! Actuation contract: one workflow per board at a time, and the EDL-driven
//! paths of `power` -- exercised against a bus described by
//! `CONMINER_USB_FIXTURE` instead of a board wedged in download mode.
//!
//! ONE TEST, SEQUENTIAL SCENARIOS, ON PURPOSE. The fixture is an environment
//! variable, so it is process-wide; two scenarios in parallel threads would each
//! see the other's bus. Every scenario here writes its own bus first.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;

const AP: &str = "/dev/serial/by-id/usb-Fixture_Board-if00-port0";
const CTL: &str = "/dev/serial/by-id/usb-Fixture_Ctl-if00";
const BOARD_PORT: &str = "2-2.4";

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    h: Handler,
}

impl Rig {
    fn with_config(mut cfg: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx = Context::open(cfg, Arc::new(ProfileSet::builtin().unwrap()), clock).unwrap();
        Self {
            dir: dir.path().to_path_buf(),
            _dir: dir,
            h: Handler::new(ctx),
        }
    }

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

    fn err(&self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args);
        assert_eq!(r["isError"], true, "{name} unexpectedly succeeded: {r}");
        r["structuredContent"]["error"].clone()
    }

    fn board(&self, target: &str) {
        let mut reg = Registry::open(&self.dir).unwrap();
        let d = reg
            .upsert_device(AP, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_target(d.id, Some(target)).unwrap();
        // A live capture attestation, so console_state can vouch for a prompt.
        reg.set_state(d.id, "listening").unwrap();
        let ctl = reg
            .upsert_device(CTL, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_target(ctl.id, Some(target)).unwrap();
        reg.set_ignored(ctl.id, true).unwrap();
        self.call("acquire", json!({"device": AP, "ttl_s": 600}));
        // The console was TALKING a moment ago, so its silence after the press
        // is evidence and the verifier goes on to ask USB.
        let path = self.dir.join("talk.log");
        std::fs::write(&path, "APP admit\nCONSOLE\nsirocco> \n").unwrap();
        self.call(
            "ingest_file",
            json!({"path": path.display().to_string(), "device": AP}),
        );
    }
}

/// A board with a harmless, instant hook, its own USB port declared, and every
/// hardware wait zeroed -- except the escalation's own floors, which are the
/// thing under test.
fn cfg() -> Config {
    let mut c = Config::default();
    c.hooks.power_timeout_s = 5;
    c.hooks.verify_settle_s = 0;
    c.hooks.verify_off_watch_s = 0;
    c.hooks.verify_boot_watch_s = 0;
    c.hooks.edl_settle_s = 0;
    c.ser2net.connect_host = "127.0.0.1".into();
    c.devices.insert(
        AP.to_string(),
        conminer_core::config::DeviceOverride {
            usb_ports: vec![BOARD_PORT.into()],
            hooks: conminer_core::config::DeviceHooks {
                power: Some("/bin/true {action} {device}".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    c
}

/// Write the bus and point the scanner at it. Returns the file so it lives as
/// long as the scenario.
fn bus(dir: &std::path::Path, devices: Value) -> std::path::PathBuf {
    let path = dir.join("usb-fixture.json");
    std::fs::write(&path, serde_json::to_string(&devices).unwrap()).unwrap();
    std::env::set_var("CONMINER_USB_FIXTURE", &path);
    path
}

fn qdl_gadget_on(port: &str) -> Value {
    json!({"vendor_id": 0x05c6, "product_id": 0x9008, "bus": 2, "address": 3,
           "port_path": port, "live": true})
}

#[test]
fn the_edl_paths_of_power_off() {
    // ---- Scenario 1: report #12 / #14. The board is alive in EDL on ITS OWN
    // port. `off` must escalate -- and answer NOW, not after the escalation.
    let rig = Rig::with_config(cfg());
    rig.board("fixture");
    let _bus = bus(&rig.dir, json!([qdl_gadget_on(BOARD_PORT)]));

    let t0 = std::time::Instant::now();
    let r = rig.call("power", json!({"target": "fixture", "action": "off"}));
    let answered_after = t0.elapsed();
    assert_eq!(
        r["effect"]["escalation"]["state"], "running",
        "an off that finds its board in EDL must say the escalation is RUNNING: {r}"
    );
    // Report #18: a RUNNING escalation is not a FAILED off. `verified` must be
    // null (pending), never false -- an agent that reads false concludes the
    // off failed, sees the escalation's own reset-boot, and files an overlap
    // that never happened.
    assert!(
        r["effect"]["verified"].is_null(),
        "a running escalation must report verified: null, not a terminal verdict: {r}"
    );
    assert_eq!(r["effect"]["pending"], true, "{r}");
    assert!(
        r["effect"]["escalation"]["note"]
            .as_str()
            .unwrap_or_default()
            .contains("part of the off"),
        "and it must warn that the board will boot once more as part of the off: {r}"
    );
    assert_eq!(r["effect"]["escalation"]["poll"], "actuation_status");
    assert!(
        r["boot_id"].is_i64() || r["boot_id"].is_u64(),
        "the epoch is open before the caller is answered: {r}"
    );
    assert!(
        answered_after < std::time::Duration::from_secs(20),
        "the caller must not sit through the reset-then-off (33 s of floors): took {answered_after:?}"
    );

    // Meanwhile the board is CLAIMED, by name and phase...
    let e = rig.err("power", json!({"target": "fixture", "action": "on"}));
    assert_eq!(e["code"], "ACTUATION_IN_FLIGHT", "{e}");
    assert!(
        e["detail"]["phase"]
            .as_str()
            .unwrap_or_default()
            .starts_with("escalation"),
        "the refusal names the phase so the caller can budget: {e}"
    );
    // ...and actuation_status says so, without a lease and without cost.
    let st = rig.call("actuation_status", json!({"target": "fixture"}));
    assert_eq!(st["board_free"], false, "{st}");
    assert_eq!(st["in_flight"]["action"], "off", "{st}");

    // The escalation runs to its end on its own: reset, 25 s, off, 8 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let done = loop {
        let st = rig.call("actuation_status", json!({"device": AP}));
        if st["board_free"] == true {
            break st;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "escalation never finished: {st}"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    let last = &done["last"];
    assert_eq!(last["action"], "off", "{done}");
    assert_eq!(last["effect"]["escalation"]["state"], "done", "{done}");
    assert_eq!(
        last["effect"]["escalation"]["kind"], "reset-then-off",
        "{done}"
    );
    assert!(
        last["effect"]["escalation"]["ms"]["total"]
            .as_u64()
            .unwrap_or(0)
            >= 33_000,
        "the floors were honoured: {done}"
    );
    // The fixture still shows the gadget, so the honest verdict is "still in EDL".
    assert_eq!(last["effect"]["verified"], false, "{done}");
    // Report #18, the durable check straight from the store: across the whole
    // escalation the ONLY power events are the off's own two (initial answer +
    // escalation completion). If the guard ever let an on interleave, a third
    // event with action=on would be here. This is the record an mcpd restart
    // cannot lose, and it is what proved the guard held on bravo.
    {
        let dev = Registry::open(&rig.dir)
            .unwrap()
            .device_by_canonical(AP)
            .unwrap()
            .unwrap();
        let path = rig.dir.join(&dev.db_file);
        let st = conminer_core::store::DeviceStore::open(&path, AP, true).unwrap();
        let evs = st.events(None, Some("power"), 100).unwrap();
        let ons = evs
            .iter()
            .filter(|(_, _, _, _, data)| data["action"] == "on")
            .count();
        assert_eq!(
            ons, 0,
            "no on may be accepted while the off escalates: {evs:?}"
        );
        let offs = evs
            .iter()
            .filter(|(_, _, _, _, data)| data["action"] == "off")
            .count();
        assert_eq!(
            offs, 2,
            "exactly the off's own two events (initial + done): {evs:?}"
        );
    }
    // And the board is free again for the next actuation.
    rig.call("power", json!({"target": "fixture", "action": "on"}));

    // ---- Scenario 2: the latent gap. Some OTHER board on the bench is in EDL.
    // This board's `off` must not take that as its own and reset-then-off a
    // board that simply powered down.
    let rig = Rig::with_config(cfg());
    rig.board("fixture");
    let _bus = bus(&rig.dir, json!([qdl_gadget_on("9-9.1")]));
    let r = rig.call("power", json!({"target": "fixture", "action": "off"}));
    // The generic "press once more" retry may run (`escalated_to: "off"`); the
    // EDL reset-then-off, which would REBOOT a board that had powered down,
    // must not.
    assert!(
        r["effect"]["escalation"].is_null() && r["effect"]["escalated_to"] != "reset-then-off",
        "a bench-mate's EDL is not this board's: {r}"
    );
    let st = rig.call("actuation_status", json!({"device": AP}));
    assert_eq!(st["board_free"], true, "{st}");

    // ---- Scenario 3: report #15. The board booted to `sirocco> ` mid-workflow
    // (the escalation's reset took it out of EDL), then the escalation powered
    // it off and that off VERIFIED. console_state must not keep asserting the
    // pre-off prompt as what the console is doing now: the epoch is not
    // "quiet" (a full boot log is in it), the Bughopper has no sense line, and
    // three run_commands got zero bytes while console_state said commandable.
    let rig = Rig::with_config(cfg());
    rig.board("fixture");
    rig.call(
        "classify_prompt",
        json!({"device": AP, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    let before = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        before["console"]["commandable"], true,
        "precondition: the console is at its prompt: {before}"
    );
    let bus_file = bus(&rig.dir, json!([qdl_gadget_on(BOARD_PORT)]));
    let r = rig.call("power", json!({"target": "fixture", "action": "off"}));
    assert_eq!(r["effect"]["escalation"]["state"], "running", "{r}");
    // The reset takes the board out of EDL: the gadget leaves the bus before
    // the escalation's final check, so the off verifies.
    std::fs::write(&bus_file, "[]").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let done = loop {
        let st = rig.call("actuation_status", json!({"device": AP}));
        if st["board_free"] == true {
            break st;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "escalation never finished: {st}"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert_eq!(done["last"]["effect"]["verified"], true, "{done}");
    let after = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        after["console"]["commandable"], false,
        "a verified off must retire the pre-off prompt claim: {after}"
    );
    assert_eq!(after["console"]["state"], "no_signal", "{after}");
    assert!(
        after["console"]["last_known"]["decayed_because"]
            .as_str()
            .unwrap_or_default()
            .contains("power off was verified"),
        "and say why: {after}"
    );

    // ---- Scenario 4: report #7 (reopened). `diagnose` must not ship
    // `edl: true` beside a commandable console in the SAME response. The probe
    // finds the board in EDL (its own port has a QDL gadget) while a recognised
    // `sirocco> ` still sits in the buffer; diagnose used to compute its console
    // block BEFORE publishing the EDL discovery, so the block read at_prompt.
    let rig = Rig::with_config(cfg());
    rig.board("fixture");
    rig.call(
        "classify_prompt",
        json!({"device": AP, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    // Precondition: with NO gadget on the bus, the console really is commandable
    // -- otherwise this gate would pass on a board that was never at a prompt.
    let _clean = bus(&rig.dir, json!([]));
    let healthy = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        healthy["console"]["commandable"], true,
        "precondition: the console is at a commandable prompt when not in EDL: {healthy}"
    );
    // Now the board is in EDL on its own port.
    let _bus = bus(&rig.dir, json!([qdl_gadget_on(BOARD_PORT)]));
    let diag = rig.call("diagnose", json!({"device": AP}));
    assert_eq!(
        diag["edl"], true,
        "the probe must find the board in EDL: {diag}"
    );
    assert_ne!(
        diag["console"]["commandable"], true,
        "a diagnose that reports edl:true must NOT also report a commandable console: {diag}"
    );
    assert_ne!(
        diag["console"]["state"], "at_prompt",
        "the EDL board has no live prompt, whatever the buffer holds: {diag}"
    );

    // ---- Scenario 5: report #19 (reopened). A silent probe while the console
    // sits at a commandable prompt must NOT read as "maybe powered off". This
    // goes through the REAL diagnose path -- the reopen proved the earlier unit
    // test was vacuous, because diagnose passes the DEVICE-row state to
    // console_verdict, not the console state, so the idle-at-a-prompt branch
    // never matched in production even though the isolated test passed.
    //
    // A listener that accepts and sends nothing is an idle shell: the probe
    // connects and reads zero bytes.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            std::mem::forget(sock);
        }
    });
    let rig = Rig::with_config(cfg());
    {
        let mut reg = Registry::open(&rig.dir).unwrap();
        let d = reg
            .upsert_device(AP, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_state(d.id, "listening").unwrap();
        // Point the probe at the silent listener.
        assert_eq!(reg.assign_port(d.id, port).unwrap(), port);
    }
    rig.call("acquire", json!({"device": AP, "ttl_s": 600}));
    let ppath = rig.dir.join("prompt.log");
    std::fs::write(
        &ppath,
        "APP admit
CONSOLE
sirocco> 
",
    )
    .unwrap();
    rig.call(
        "ingest_file",
        json!({"path": ppath.display().to_string(), "device": AP}),
    );
    rig.call(
        "classify_prompt",
        json!({"device": AP, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    // Empty bus: NOT in EDL, and no power sense on this controller -> power
    // unknown, which is exactly when the old ladder fell through to maybe-off.
    let _clean = bus(&rig.dir, json!([]));
    let cs = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        cs["console"]["commandable"], true,
        "precondition: the console is at a commandable prompt: {cs}"
    );
    let diag = rig.call("diagnose", json!({"device": AP, "wait_ms": 300}));
    assert_eq!(diag["edl"], false, "not in EDL: {diag}");
    assert_eq!(
        diag["probe"]["bytes_received"], 0,
        "the idle shell sent nothing, which is the whole point: {diag}"
    );
    let verdict = diag["verdict"].as_str().unwrap_or("");
    assert!(
        !verdict.contains("powered off") && !verdict.contains("may be"),
        "a silent probe at a commandable prompt must not read as maybe-off: {verdict:?} ({diag})"
    );
    assert!(
        verdict.contains("idle") && verdict.contains("commandable prompt"),
        "it must name the idle-at-a-prompt reading: {verdict:?}"
    );

    std::env::remove_var("CONMINER_USB_FIXTURE");
}
