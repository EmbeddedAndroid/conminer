//! A board's USB devices are learned by identity after it is powered on, and
//! its ports are resolved from that identity on every call, so a re-plugged
//! cable follows the hardware. Driven by `CONMINER_USB_FIXTURE`, which is
//! process-wide, so every test here holds [`BUS`] for its whole run.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AP: &str = "/dev/serial/by-id/usb-Fixture_Board-if00-port0";
const OTHER: &str = "/dev/serial/by-id/usb-Fixture_Other-if00-port0";

static BUS: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    h: Handler,
}

impl Rig {
    fn new() -> Self {
        Self::with(AP)
    }

    fn with(board: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.paths.data_dir = dir.path().to_path_buf();
        c.hooks.power_timeout_s = 5;
        c.hooks.verify_settle_s = 0;
        c.hooks.verify_off_watch_s = 0;
        c.hooks.verify_boot_watch_s = 0;
        c.hooks.edl_settle_s = 0;
        c.ser2net.connect_host = "127.0.0.1".into();
        c.devices.insert(
            board.to_string(),
            conminer_core::config::DeviceOverride {
                hooks: conminer_core::config::DeviceHooks {
                    power: Some("/bin/true {action} {device}".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx = Context::open(c, Arc::new(ProfileSet::builtin().unwrap()), clock).unwrap();
        Self {
            dir: dir.path().to_path_buf(),
            _dir: dir,
            h: Handler::new(ctx),
        }
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        let r = self.h.handle(req).expect("a reply").result.unwrap();
        assert_eq!(r["isError"], false, "{name} failed: {r}");
        r["structuredContent"].clone()
    }

    fn tags(&self, dev: &str) -> std::collections::BTreeMap<String, String> {
        Registry::open(&self.dir)
            .unwrap()
            .all_devices()
            .unwrap()
            .into_iter()
            .find(|d| d.canonical == dev)
            .map(|d| d.tags)
            .unwrap_or_default()
    }

    fn wait_tag(&self, key: &str, want: &str) -> String {
        self.wait_tag_on(AP, key, want)
    }

    fn wait_tag_on(&self, dev: &str, key: &str, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let got = self.tags(dev).get(key).cloned().unwrap_or_default();
            if got == want || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn scope_ports(&self) -> Value {
        self.scope_of(AP)
    }

    fn scope_of(&self, dev: &str) -> Value {
        self.call("diagnose", json!({"device": dev}))["usb_zombies_scope"]["ports"].clone()
    }
}

fn bus(dir: &std::path::Path, devices: Value) {
    let path = dir.join("usb-fixture.json");
    std::fs::write(&path, serde_json::to_string(&devices).unwrap()).unwrap();
    std::env::set_var("CONMINER_USB_FIXTURE", &path);
}

fn gadget(port: &str) -> Value {
    json!({"vendor_id": 0x05c6, "product_id": 0x901d, "bus": 1, "address": 9,
           "port_path": port, "serial": "c0ffee01", "product": "Test-MTP", "live": true})
}

#[test]
fn a_boards_usb_ports_are_learned_and_follow_a_replug() {
    let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new();
    {
        let mut reg = Registry::open(&rig.dir).unwrap();
        let d = reg
            .upsert_device(AP, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_target(d.id, Some("mtp")).unwrap();
        reg.set_state(d.id, "listening").unwrap();
        // Another board already owns a device that will show up during our
        // window: it must not be attributed to us.
        let o = reg
            .upsert_device(OTHER, None, IdentityKind::ById, None, 0)
            .unwrap();
        let mut t = std::collections::BTreeMap::new();
        t.insert("usb_identity".to_string(), "18d1:4ee7:theirs".to_string());
        reg.set_tags(o.id, &t).unwrap();
    }
    rig.call("acquire", json!({"device": AP, "ttl_s": 600}));
    let talk = rig.dir.join("talk.log");
    std::fs::write(&talk, "Linux version 7.3.0\nlogin: \n").unwrap();
    rig.call(
        "ingest_file",
        json!({"path": talk.display().to_string(), "device": AP}),
    );

    // Nothing learned yet, nothing configured: no attribution.
    bus(&rig.dir, json!([]));
    assert_eq!(rig.scope_ports(), json!([]));

    let r = rig.call("power", json!({"device": AP, "action": "on"}));
    assert_eq!(r["effect"]["verified"], true, "{r}");

    // The board's gadget enumerates after the press, beside a hub, a device
    // with no identity strings, and another board's device.
    bus(
        &rig.dir,
        json!([
            gadget("1-3.2"),
            {"vendor_id": 0x2109, "product_id": 0x2811, "port_path": "1-4", "class": 9,
             "product": "USB2.0 Hub", "live": true},
            {"vendor_id": 0x1234, "product_id": 0x0001, "port_path": "1-3.4", "live": true},
            {"vendor_id": 0x18d1, "product_id": 0x4ee7, "port_path": "1-3.3",
             "serial": "theirs", "live": true},
        ]),
    );
    assert_eq!(
        rig.wait_tag("usb_identity", "05c6:901d:c0ffee01"),
        "05c6:901d:c0ffee01"
    );
    assert_eq!(rig.wait_tag("usb_ports", "1-3.2"), "1-3.2");
    assert_eq!(rig.scope_ports(), json!(["1-3.2"]));

    // Re-plugged on another hub: the identity resolves to the new port.
    bus(&rig.dir, json!([gadget("2-1.4")]));
    assert_eq!(rig.scope_ports(), json!(["2-1.4"]));

    // Board off the bus: the last-seen port stands in.
    bus(&rig.dir, json!([]));
    assert_eq!(rig.scope_ports(), json!(["1-3.2"]));

    std::env::remove_var("CONMINER_USB_FIXTURE");
}

const BOARD_B: &str = "/dev/serial/by-id/usb-FTDI_Board_B_UART_UARTB001-if00-port0";
const A_UART: &str = "/dev/serial/by-id/usb-FTDI_Board_A_UART_UARTA001-if00-port0";
const A_SPI: &str = "/dev/serial/by-id/usb-FTDI_Board_A_SPI_SPIA0001-if00-port0";
const A_CTL: &str = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_CTLA0001-if00";
const C_DEBUG: &str = "/dev/serial/by-id/usb-FTDI_Board_C_DEBUG_DBGC0001-if00-port0";

fn bp(chain: &str) -> String {
    format!("pci-0000:00:14.0-usb-0:{chain}:1.0-port0")
}

/// Two boards behind one hub (the actuated board at 2.2.1, a second board's
/// UART, SPI and Bantam at 2.2.2-2.2.4) plus a third board's consoles, gone.
fn shared_hub_bench(rig: &Rig) {
    let mut reg = Registry::open(&rig.dir).unwrap();
    let mut put = |canonical: &str, chain: &str, state: &str| {
        let d = reg
            .upsert_device(canonical, Some(&bp(chain)), IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_state(d.id, state).unwrap();
        d.id
    };
    put(BOARD_B, "2.2.1", "listening");
    put(A_UART, "2.2.2", "gone");
    put(A_SPI, "2.2.3", "gone");
    let b = put(A_CTL, "2.2.4", "gone");
    put(C_DEBUG, "2.3.1", "gone");
    reg.set_ignored(b, true).unwrap();
}

fn harness_a() -> Vec<Value> {
    vec![
        json!({"vendor_id": 0x0403, "product_id": 0x6011, "port_path": "1-2.2.2",
               "serial": "UARTA001", "product": "Board A UART", "live": true}),
        json!({"vendor_id": 0x0403, "product_id": 0x6010, "port_path": "1-2.2.3",
               "serial": "SPIA0001", "product": "Board A SPI", "live": true}),
        json!({"vendor_id": 0x04d8, "product_id": 0x000a, "port_path": "1-2.2.4",
               "serial": "CTLA0001", "product": "Bantam", "live": true}),
    ]
}

fn edl(port: &str) -> Value {
    json!({"vendor_id": 0x05c6, "product_id": 0x9008, "port_path": port,
           "product": "QUSB_BULK_CID:0001_SN:0C0FFEE1", "live": true})
}

/// Another board's harness enumerating inside this board's learning window
/// must not be learned as this board's gadgets: a device that carries a
/// console or controller this node knows is a debug adapter, never a gadget.
#[test]
fn a_harness_plugged_in_during_the_window_is_not_learned_as_a_gadget() {
    let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::with(BOARD_B);
    shared_hub_bench(&rig);
    rig.call("acquire", json!({"device": BOARD_B, "ttl_s": 600}));
    let talk = rig.dir.join("talk.log");
    std::fs::write(&talk, "Linux version 7.3.0\nlogin: \n").unwrap();
    rig.call(
        "ingest_file",
        json!({"path": talk.display().to_string(), "device": BOARD_B}),
    );
    bus(&rig.dir, json!([]));
    let r = rig.call("power", json!({"device": BOARD_B, "action": "on"}));
    assert_eq!(r["effect"]["verified"], true, "{r}");

    // The other board's harness and this board's EDL gadget arrive together.
    let mut now = harness_a();
    now.push(edl("1-3.1"));
    bus(&rig.dir, Value::Array(now));
    let want = "05c6:9008:QUSB_BULK_CID:0001_SN:0C0FFEE1";
    assert_eq!(rig.wait_tag_on(BOARD_B, "usb_identity", want), want);
    assert_eq!(rig.wait_tag_on(BOARD_B, "usb_ports", "1-3.1"), "1-3.1");
    // Give the learner more ticks: nothing else may join later.
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        rig.tags(BOARD_B).get("usb_identity").map(String::as_str),
        Some(want)
    );
    assert_eq!(rig.scope_of(BOARD_B), json!(["1-3.1"]));

    std::env::remove_var("CONMINER_USB_FIXTURE");
}

/// Tags written before the rule existed: resolution must not hand another
/// board's hub to this board as its EDL scope, through identity or through the
/// last-seen port fallback, and discovery heals the stored tags.
#[test]
fn polluted_tags_never_scope_another_boards_hub_and_discovery_heals_them() {
    let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::with(BOARD_B);
    shared_hub_bench(&rig);
    let board_b_id = {
        let mut reg = Registry::open(&rig.dir).unwrap();
        for d in reg.all_devices().unwrap() {
            if d.canonical.contains("Board_A") || d.canonical.contains("CTLA0001") {
                let s = if d.ignored { "ignored" } else { "discovered" };
                reg.set_state(d.id, s).unwrap();
            }
        }
        let d = reg.device_by_canonical(BOARD_B).unwrap().unwrap();
        let mut t = std::collections::BTreeMap::new();
        t.insert(
            "usb_identity".to_string(),
            "18d1:d00d:fb000001,04d8:000a:CTLA0001,\
             0403:6011:UARTA001,0403:6010:SPIA0001"
                .to_string(),
        );
        t.insert(
            "usb_ports".to_string(),
            "1-2.2.2,1-2.2.3,1-2.2.4".to_string(),
        );
        reg.set_tags(d.id, &t).unwrap();
        d.id
    };

    // Only the other board's harness on the bus: by identity it would resolve
    // to 1-2.2.x, and with nothing resolved the stale tag would stand in.
    bus(&rig.dir, Value::Array(harness_a()));
    assert_eq!(rig.scope_of(BOARD_B), json!([]));
    // This board's own fastboot gadget still resolves.
    let mut now = harness_a();
    now.push(
        json!({"vendor_id": 0x18d1, "product_id": 0xd00d, "port_path": "1-3.1",
                    "serial": "fb000001", "live": true}),
    );
    bus(&rig.dir, Value::Array(now));
    assert_eq!(rig.scope_of(BOARD_B), json!(["1-3.1"]));

    let mut reg = Registry::open(&rig.dir).unwrap();
    let healed = conminer_core::attribution::heal(&mut reg).unwrap();
    assert_eq!(healed, vec![BOARD_B.to_string()]);
    let tags = reg.device(board_b_id).unwrap().tags;
    assert_eq!(
        tags.get("usb_identity").map(String::as_str),
        Some("18d1:d00d:fb000001")
    );
    assert_eq!(tags.get("usb_ports"), None, "{tags:?}");
    // Idempotent: a second pass changes nothing.
    assert!(conminer_core::attribution::heal(&mut reg)
        .unwrap()
        .is_empty());

    std::env::remove_var("CONMINER_USB_FIXTURE");
}
