//! A line mode (the MTP's USB_OFF/USB_ON) runs the controller hook on a running
//! board and opens no boot epoch; a real boot mode still opens one.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;

const AP: &str = "/dev/serial/by-id/usb-FTDI_ALPACA-LITE_MTP_DEBUG_BOARD_TESTMTP0-if00-port0";

fn call(h: &Handler, name: &str, args: Value) -> Value {
    let req: Request = serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": args}
    }))
    .unwrap();
    let r = h.handle(req).expect("a reply").result.unwrap();
    assert_eq!(r["isError"], false, "{name} failed: {r}");
    r["structuredContent"].clone()
}

fn boots(h: &Handler) -> usize {
    call(h, "list_boots", json!({"device": AP}))["boots"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

#[test]
fn a_line_mode_switches_without_opening_a_boot_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.paths.data_dir = dir.path().to_path_buf();
    c.hooks.power_timeout_s = 5;
    c.ser2net.connect_host = "127.0.0.1".into();
    let log = dir.path().join("hook.log");
    c.devices.insert(
        AP.to_string(),
        conminer_core::config::DeviceOverride {
            hooks: conminer_core::config::DeviceHooks {
                boot_mode: Some(format!("/bin/sh -c 'echo {{mode}} >> {}'", log.display())),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mtp = c
        .controller_for(AP)
        .expect("the built-in tac-mtp profile controls this console");
    assert_eq!(mtp.line_modes, vec!["USB_OFF", "USB_ON"]);
    let clock = Arc::new(conminer_core::clock::StepClock::default());
    let ctx = Context::open(c, Arc::new(ProfileSet::builtin().unwrap()), clock).unwrap();
    let h = Handler::new(ctx);
    {
        let mut reg = Registry::open(dir.path()).unwrap();
        let d = reg
            .upsert_device(AP, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_state(d.id, "listening").unwrap();
    }
    call(&h, "acquire", json!({"device": AP, "ttl_s": 600}));
    let before = boots(&h);

    // The dry run describes the same call: a line mode would open nothing,
    // and a boot mode still names the epochs it would open.
    for mode in ["USB_OFF", "USB_ON"] {
        let r = call(
            &h,
            "boot_mode",
            json!({"device": AP, "mode": mode, "dry_run": true}),
        );
        assert_eq!(r["line_mode"], true, "{r}");
        assert_eq!(r["would_open_epochs"], json!([]), "{r}");
    }
    let r = call(
        &h,
        "boot_mode",
        json!({"device": AP, "mode": "EDL", "dry_run": true}),
    );
    assert_eq!(r["line_mode"], false, "{r}");
    assert_eq!(
        r["would_open_epochs"].as_array().map(Vec::len),
        Some(1),
        "{r}"
    );
    assert!(!log.exists(), "a dry run must not run the hook");

    for mode in ["USB_OFF", "USB_ON"] {
        let r = call(&h, "boot_mode", json!({"device": AP, "mode": mode}));
        assert_eq!(r["line_mode"], true, "{r}");
        assert_eq!(r["boot"], false, "{r}");
    }
    assert_eq!(boots(&h), before, "a line mode must not open a boot epoch");
    let ran = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        ran.split_whitespace().collect::<Vec<_>>(),
        vec!["USB_OFF", "USB_ON"]
    );

    // A real boot mode still opens one.
    let r = call(&h, "boot_mode", json!({"device": AP, "mode": "EDL"}));
    assert!(r.get("line_mode").is_none(), "{r}");
    assert_eq!(boots(&h), before + 1);
}
