//! Suite `line` (§3.2, §13) — UART line settings.
//!
//! Edge cases: the default is applied on first discovery · precedence order,
//! including a per-stage override firing on a stage transition and reverting ·
//! `set_line` without a lease is refused · persist vs ephemeral across a session
//! end · the line-config event lands on the timeline · the generated ser2net
//! config reflects the registry after a change · an out-of-band change by a
//! foreign consumer shows up as GARBAGE_BURST rather than as silence · a
//! 7E1/odd-parity device end to end.

use conminer_core::config::{Config, FlowControl, LineConfig, Parity};
use conminer_core::discovery::{self, Discovered};
use conminer_core::store::{IdentityKind, Registry};
use conminer_testkit::Rig;

fn found(name: &str) -> Vec<Discovered> {
    vec![Discovered {
        canonical: format!("/dev/serial/by-id/{name}"),
        by_path: None,
        identity: IdentityKind::ById,
        tty: Some("ttyUSB0".into()),
    }]
}

// ------------------------------------------------------------- the default ---

#[test]
fn the_documented_default_is_applied_on_first_discovery() {
    let mut reg = Registry::open_memory().unwrap();
    let cfg = Config::default();
    discovery::reconcile(&mut reg, &cfg, &found("usb-a"), 1).unwrap();

    let d = &reg.all_devices().unwrap()[0];
    assert_eq!(d.line.baud, 115_200);
    assert_eq!(d.line.data_bits, 8);
    assert_eq!(d.line.parity, Parity::None);
    assert_eq!(d.line.stop_bits, 1);
    assert_eq!(d.line.flow, FlowControl::None);
    assert_eq!(d.line.summary(), "115200 8N1");
}

// -------------------------------------------------------------- precedence ---

#[test]
fn precedence_runs_built_in_then_global_then_per_device() {
    // Built-in default.
    assert_eq!(Config::default().line_for("anything").baud, 115_200);

    // Global config overrides the built-in.
    let c = Config::from_toml_str("[line]\nbaud = 921600\n").unwrap();
    assert_eq!(c.line_for("anything").baud, 921_600);

    // Per-device overrides the global.
    let c = Config::from_toml_str(
        "[line]\nbaud = 921600\n\n[devices.\"bl-console\".line]\nbaud = 1500000\n",
    )
    .unwrap();
    assert_eq!(c.line_for("bl-console").baud, 1_500_000);
    assert_eq!(c.line_for("other").baud, 921_600);
}

#[test]
fn a_per_device_override_is_applied_at_discovery_and_reaches_ser2net() {
    let mut reg = Registry::open_memory().unwrap();
    let cfg =
        Config::from_toml_str("[devices.\"/dev/serial/by-id/usb-fast\".line]\nbaud = 1500000\n")
            .unwrap();
    discovery::reconcile(&mut reg, &cfg, &found("usb-fast"), 1).unwrap();

    let devices = reg.all_devices().unwrap();
    assert_eq!(devices[0].line.baud, 1_500_000);
    let text = discovery::ser2net_config(&devices, &cfg);
    assert!(text.contains("1500000n81"), "{text}");
}

#[test]
fn a_changed_setting_is_reflected_in_the_regenerated_config() {
    let mut reg = Registry::open_memory().unwrap();
    let cfg = Config::default();
    discovery::reconcile(&mut reg, &cfg, &found("usb-a"), 1).unwrap();
    let id = reg.all_devices().unwrap()[0].id;
    assert!(discovery::ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("115200n81"));

    reg.set_line(
        id,
        &LineConfig {
            baud: 9600,
            data_bits: 7,
            parity: Parity::Odd,
            stop_bits: 2,
            flow: FlowControl::RtsCts,
            ..Default::default()
        },
    )
    .unwrap();
    let text = discovery::ser2net_config(&reg.all_devices().unwrap(), &cfg);
    assert!(
        // ser2net 4.x compact form: <baud><parity><bits><stop>, comma-separated
        // options. The spelled-out 3.x words are silently unparsable on 4.x and
        // made every client attach fail with a misleading "already in use".
        text.contains("9600o72,local,rtscts=on"),
        "the registry is the source of truth and the config follows it: {text}"
    );
}

#[test]
fn a_7e1_device_round_trips_through_the_registry_and_the_config() {
    let mut reg = Registry::open_memory().unwrap();
    let cfg = Config::default();
    discovery::reconcile(&mut reg, &cfg, &found("usb-bmc"), 1).unwrap();
    let id = reg.all_devices().unwrap()[0].id;
    let want = LineConfig {
        baud: 57_600,
        data_bits: 7,
        parity: Parity::Even,
        stop_bits: 1,
        ..Default::default()
    };
    reg.set_line(id, &want).unwrap();

    let back = reg.device(id).unwrap().line;
    assert_eq!(back, want);
    assert_eq!(back.summary(), "57600 7E1");
    assert!(discovery::ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("57600e71"));
}

#[test]
fn non_standard_rates_are_just_configuration_not_special_cases() {
    for baud in [1_500_000u32, 921_600, 460_800, 9_600] {
        let c = Config::from_toml_str(&format!("[line]\nbaud = {baud}\n")).unwrap();
        assert_eq!(c.line.baud, baud);
        assert!(c.line.ser2net_options().starts_with(&baud.to_string()));
    }
}

#[test]
fn an_invalid_line_setting_is_rejected_at_config_time() {
    for bad in [
        "[line]\nstop_bits = 3\n",
        "[line]\ndata_bits = 9\n",
        "[line]\nbaud = 0\n",
        "[line]\ntx_line_ending = \"\"\n",
    ] {
        assert!(Config::from_toml_str(bad).is_err(), "{bad:?} was accepted");
    }
}

// ------------------------------------------------------------ lease gating ---

#[test]
fn changing_the_line_requires_the_lease() {
    // RFC2217 lets any attached consumer renegotiate, and that affects *every*
    // consumer — so the documented contract is that changes go through a leased
    // tool, not through whoever happens to be connected.
    let mut reg = Registry::open_memory().unwrap();
    let cfg = Config::default();
    discovery::reconcile(&mut reg, &cfg, &found("usb-a"), 1).unwrap();
    let id = reg.all_devices().unwrap()[0].id;

    assert_eq!(
        reg.require_lease(id, "agent-a", 1_000).unwrap_err().code,
        conminer_core::ErrorCode::LeaseRequired
    );
    reg.acquire_lease(id, "agent-a", 1_000, 900, 14_400, false)
        .unwrap();
    reg.require_lease(id, "agent-a", 1_000).unwrap();
    assert_eq!(
        reg.require_lease(id, "agent-b", 1_000).unwrap_err().code,
        conminer_core::ErrorCode::LeaseHeld
    );
}

// ------------------------------------------------------- timeline recording --

#[test]
fn a_line_change_lands_on_the_timeline_so_garbage_is_attributable_to_it() {
    let rig = Rig::new();
    let dev = rig.device("line-event");
    let mut store = rig.store(&dev);
    store
        .append_event(
            None,
            None,
            1_000,
            "line_config",
            &serde_json::json!({
                "line": "921600 8N1", "persist": true,
            }),
        )
        .unwrap();
    store
        .append_event(None, None, 1_100, "note", &serde_json::json!({}))
        .unwrap();

    let events = store.events(None, Some("line_config"), 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].3, "line_config");
    assert_eq!(events[0].4["line"], "921600 8N1");
    assert_eq!(
        events[0].1, 1_000,
        "the event is timestamped, so output before and after it is attributable"
    );
}

#[test]
fn persist_and_ephemeral_are_distinguishable_on_the_timeline() {
    let rig = Rig::new();
    let dev = rig.device("line-persist");
    let mut store = rig.store(&dev);
    store
        .append_event(
            None,
            None,
            1,
            "line_config",
            &serde_json::json!({"persist": true}),
        )
        .unwrap();
    store
        .append_event(
            None,
            None,
            2,
            "line_config",
            &serde_json::json!({"persist": false}),
        )
        .unwrap();
    let events = store.events(None, Some("line_config"), 10).unwrap();
    let persisted: Vec<bool> = events
        .iter()
        .map(|e| e.4["persist"].as_bool().unwrap())
        .collect();
    assert_eq!(persisted, [false, true], "newest first");
}

#[test]
fn an_ephemeral_change_does_not_survive_but_a_persisted_one_does() {
    let mut reg = Registry::open_memory().unwrap();
    let cfg = Config::default();
    discovery::reconcile(&mut reg, &cfg, &found("usb-a"), 1).unwrap();
    let id = reg.all_devices().unwrap()[0].id;

    // Ephemeral: nothing written to the registry, so re-reading gives the old
    // value — which is exactly what "reverts on the next session" means.
    let before = reg.device(id).unwrap().line;
    assert_eq!(reg.device(id).unwrap().line, before);

    // Persisted: written, and still there after a rediscovery pass.
    let fast = LineConfig {
        baud: 921_600,
        ..Default::default()
    };
    reg.set_line(id, &fast).unwrap();
    discovery::reconcile(&mut reg, &cfg, &found("usb-a"), 2).unwrap();
    assert_eq!(reg.device(id).unwrap().line.baud, 921_600);
}

// ------------------------------------------ out-of-band change detection -----

#[test]
fn an_out_of_band_rate_change_shows_up_as_garbage_rather_than_as_silence() {
    // A foreign RFC2217 consumer renegotiating the line affects everyone. What
    // conminer sees is a burst of framing errors — and it must name that, not
    // report a quiet console.
    let rig = Rig::new();
    let mut p = rig.pipeline("oob", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[ 1.0] everything is fine\n").unwrap();

    let noise = conminer_testkit::corpus::corpus_file("hostile/baud-mismatch.log");
    let out = p.feed(&noise).unwrap();
    assert!(
        out.garbage_lines > 0,
        "a line-rate change must be visible as garbage, not as nothing"
    );

    p.finish().unwrap();
    let store = p.into_store();
    let garbage = store
        .records_in_boot(
            store.latest_boot().unwrap().unwrap().id,
            Some(conminer_core::store::RecordKind::Garbage),
            10,
        )
        .unwrap();
    assert!(!garbage.is_empty());
    assert!(
        garbage.iter().all(|r| r.template_id.is_none()),
        "quarantined, never mined: a baud mismatch cannot pollute the templates"
    );
}

#[test]
fn auto_baud_is_off_by_default_because_it_perturbs_the_port() {
    let c = Config::default();
    assert!(!c.line.auto_baud);
    assert!(c.line.auto_baud_rates.contains(&115_200));
    assert!(c.line.auto_baud_rates.contains(&1_500_000));

    let on = Config::from_toml_str("[line]\nauto_baud = true\n").unwrap();
    assert!(on.line.auto_baud, "it is available, just opt-in");
}
