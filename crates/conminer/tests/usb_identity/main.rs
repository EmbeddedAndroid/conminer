//! A board's USB devices are learned by identity after it is powered on, and
//! its ports are resolved from that identity on every call, so a re-plugged
//! cable follows the hardware. Driven by `CONMINER_USB_FIXTURE`, which is
//! process-wide, so this binary holds one sequential test.

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

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    h: Handler,
}

impl Rig {
    fn new() -> Self {
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
            AP.to_string(),
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
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let got = self.tags(AP).get(key).cloned().unwrap_or_default();
            if got == want || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn scope_ports(&self) -> Value {
        self.call("diagnose", json!({"device": AP}))["usb_zombies_scope"]["ports"].clone()
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
