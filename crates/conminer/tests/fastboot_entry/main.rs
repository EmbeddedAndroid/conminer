//! `boot_mode FASTBOOT` on an MTP Alpaca-lite: the controller's volume-down
//! hold reaches the hook, and the entry is proven by a fastboot device with the
//! board's own serial, or refused with `FASTBOOT_NOT_ENTERED`. Driven by
//! `CONMINER_USB_FIXTURE`, which is process-wide, so the bus scenarios share
//! one sequential test.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

const AP: &str = "/dev/serial/by-id/usb-FTDI_ALPACA-LITE_MTP_DEBUG_BOARD_TESTMTP0-if00-port0";
const AP1: &str = "/dev/serial/by-id/usb-FTDI_ALPACA-LITE_MTP_DEBUG_BOARD_TESTMTP0-if01-port0";

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    h: Handler,
}

impl Rig {
    /// `hook`: replace the controller's boot-mode command with one that logs
    /// its arguments. `serial_file`: where the board's fastboot serial lives,
    /// configured on the console `on`.
    fn new(hook: bool, serial_file: Option<&str>) -> Self {
        Self::build(hook, serial_file, AP, None)
    }

    /// `hold_ms`: the tac-mtp profile's `fastboot_hold_ms`, as a node's
    /// conminer.toml would set it.
    fn with_hold(hook: bool, serial_file: Option<&str>, hold_ms: Option<u64>) -> Self {
        Self::build(hook, serial_file, AP, hold_ms)
    }

    fn build(hook: bool, serial_file: Option<&str>, on: &str, hold_ms: Option<u64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        if let Some(ms) = hold_ms {
            for p in c.controllers.iter_mut().filter(|p| p.name == "tac-mtp") {
                p.fastboot_hold_ms = ms;
            }
        }
        c.paths.data_dir = dir.path().to_path_buf();
        c.hooks.power_timeout_s = 5;
        c.hooks.fastboot_settle_s = 1;
        c.ser2net.connect_host = "127.0.0.1".into();
        let log = dir.path().join("hook.log");
        let mut over = conminer_core::config::DeviceOverride::default();
        if hook {
            over.hooks.boot_mode = Some(format!(
                "/bin/sh -c 'echo {{mode}} {{fastboot_hold_ms}} >> {}'",
                log.display()
            ));
        }
        c.devices.insert(AP.to_string(), over);
        if let Some(f) = serial_file {
            c.devices
                .entry(on.to_string())
                .or_default()
                .fastboot_serial_file = Some(dir.path().join(f));
        }
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx = Context::open(c, Arc::new(ProfileSet::builtin().unwrap()), clock).unwrap();
        let rig = Self {
            dir: dir.path().to_path_buf(),
            _dir: dir,
            h: Handler::new(ctx),
        };
        {
            let mut reg = Registry::open(&rig.dir).unwrap();
            for (i, console) in [AP, AP1].into_iter().enumerate() {
                let d = reg
                    .upsert_device(console, None, IdentityKind::ById, None, 0)
                    .unwrap();
                reg.set_state(d.id, "listening").unwrap();
                reg.assign_port(d.id, 5100 + i as u16 * 10).unwrap();
            }
        }
        rig.call("acquire", json!({"device": AP, "ttl_s": 600}));
        rig
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

    fn fastboot(&self) -> Value {
        self.call("boot_mode", json!({"device": AP, "mode": "FASTBOOT"}))
    }

    fn hook_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("hook.log")).unwrap_or_default()
    }
}

fn bus(dir: &Path, devices: Value) {
    let path = dir.join("usb-fixture.json");
    std::fs::write(&path, serde_json::to_string(&devices).unwrap()).unwrap();
    std::env::set_var("CONMINER_USB_FIXTURE", &path);
}

fn device(vid: u16, pid: u16, port: &str, serial: &str, fastboot: bool) -> Value {
    json!({"vendor_id": vid, "product_id": pid, "port_path": port, "serial": serial,
           "fastboot": fastboot, "live": true})
}

/// The shipped command line carries the controller's hold, and so does the
/// clear that `normal_boot` runs through the same template: an unrendered
/// placeholder there would reach the CLI as text and fail every normal boot.
#[test]
fn the_shipped_hook_carries_the_controller_hold() {
    let rig = Rig::new(false, None);
    let r = rig.call(
        "boot_mode",
        json!({"device": AP, "mode": "FASTBOOT", "dry_run": true}),
    );
    let argv: Vec<String> = serde_json::from_value(r["hook"]["command"].clone()).unwrap();
    let at = argv.iter().position(|a| a == "--fastboot-hold-ms").unwrap();
    assert_eq!(argv[at + 1], "10000", "{argv:?}");
    let r = rig.call("normal_boot", json!({"device": AP, "dry_run": true}));
    let clear = r["sequence"][0]["command"].to_string();
    assert!(clear.contains("clear"), "{clear}");
    assert!(!clear.contains('{'), "an unrendered placeholder: {clear}");

    // A node that tunes the hold gets its own value on the command line.
    let rig = Rig::with_hold(false, None, Some(14_000));
    let r = rig.call(
        "boot_mode",
        json!({"device": AP, "mode": "FASTBOOT", "dry_run": true}),
    );
    let argv: Vec<String> = serde_json::from_value(r["hook"]["command"].clone()).unwrap();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--fastboot-hold-ms", "14000"]),
        "{argv:?}"
    );
}

#[test]
fn fastboot_entry_is_proven_by_the_boards_own_fastboot_device() {
    // No serial configured: the mode runs, and says it could not check.
    let rig = Rig::new(true, None);
    bus(&rig.dir, json!([]));
    let r = rig.fastboot();
    assert_eq!(r["fastboot"]["checked"], false, "{r}");
    assert!(r["fastboot"].get("found").is_none(), "{r}");
    assert_eq!(rig.hook_log().trim(), "FASTBOOT 10000");

    // A configured file that is missing is said to be unreadable, not "absent".
    let rig = Rig::new(true, Some("fastboot-serial"));
    bus(&rig.dir, json!([]));
    let r = rig.fastboot();
    assert_eq!(r["fastboot"]["checked"], false, "{r}");
    assert!(
        r["fastboot"]["why"]
            .as_str()
            .unwrap()
            .contains("cannot read"),
        "{r}"
    );

    // The board's fastboot device is on the bus: found, with where it is.
    std::fs::write(rig.dir.join("fastboot-serial"), "0123456789\n").unwrap();
    bus(
        &rig.dir,
        json!([device(0x18d1, 0x4ee0, "1-3.4", "0123456789", true)]),
    );
    let r = rig.fastboot();
    assert_eq!(r["fastboot"]["found"], true, "{r}");
    assert_eq!(r["fastboot"]["usb_id"], "18d1:4ee0");
    assert_eq!(r["fastboot"]["port"], "1-3.4");
    assert!(r["boot_id"].is_i64(), "{r}");

    // Another board in fastboot, and this board's serial on a gadget that is
    // not fastboot (its running OS): neither is this board in fastboot.
    bus(
        &rig.dir,
        json!([
            device(0x18d1, 0x4ee0, "1-3.6", "fedcba98", true),
            device(0x05c6, 0x901d, "1-3.4", "0123456789", false),
        ]),
    );
    let e = rig.err("boot_mode", json!({"device": AP, "mode": "FASTBOOT"}));
    assert_eq!(e["code"], "FASTBOOT_NOT_ENTERED", "{e}");
    assert!(e["message"].as_str().unwrap().contains("0123456789"), "{e}");
    let d = &e["detail"];
    assert!(d["boot_id"].is_i64(), "the boot it did start is named: {e}");
    assert_eq!(d["fastboot"]["found"], false);
    assert!(d["fastboot"]["waited_ms"].as_u64().unwrap() >= 1000, "{e}");
    assert_eq!(
        d["fastboot"]["seen"],
        json!([{"serial": "fedcba98", "usb_id": "18d1:4ee0", "port": "1-3.6", "answers": true}])
    );
    assert!(!e["hint"].as_str().unwrap_or_default().is_empty(), "{e}");

    // This board's fastboot gadget still listed but no longer answering (a
    // ghost from an earlier entry): not proof.
    let mut ghost = device(0x18d1, 0x4ee0, "1-3.4", "0123456789", true);
    ghost["live"] = json!(false);
    bus(&rig.dir, json!([ghost]));
    let e = rig.err("boot_mode", json!({"device": AP, "mode": "FASTBOOT"}));
    assert_eq!(e["code"], "FASTBOOT_NOT_ENTERED", "{e}");
    assert_eq!(e["detail"]["fastboot"]["seen"][0]["answers"], false, "{e}");

    // The check is FASTBOOT's alone: EDL does not wait for a fastboot device.
    let r = rig.call("boot_mode", json!({"device": AP, "mode": "EDL"}));
    assert!(r.get("fastboot").is_none(), "{r}");

    // The serial is the board's: configured on its other console, it still
    // applies to a call that names this one.
    let rig = Rig::build(true, Some("fastboot-serial"), AP1, None);
    std::fs::write(rig.dir.join("fastboot-serial"), "0123456789\n").unwrap();
    bus(
        &rig.dir,
        json!([device(0x18d1, 0x4ee0, "1-3.4", "0123456789", true)]),
    );
    let r = rig.fastboot();
    assert_eq!(r["fastboot"]["found"], true, "{r}");

    std::env::remove_var("CONMINER_USB_FIXTURE");
}

/// A hold the hook timeout cannot cover is refused before any TAC is opened.
#[test]
fn tac_power_refuses_a_hold_past_its_limit() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_conminer"))
        .args([
            "tac-power",
            "mode",
            "FASTBOOT",
            "--fastboot-hold-ms",
            "50001",
        ])
        .args(["--device", AP, "--dry-run"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("50000 ms limit"), "{err}");
}
