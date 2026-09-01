//! Suite `decode` (§18.3) — the silicon decoder ring, through the tool surface.
//!
//! The tables themselves are unit-tested in `conminer-core`. What this suite
//! pins is the contract an agent depends on: that decoding is a derived
//! annotation which never touches the raw line, that the memory map comes from
//! the *device* (two boards on one host have different maps), and that an
//! ambiguous value comes back with every reading rather than one confident
//! guess.

use conminer_core::config::{Config, DeviceOverride, MemoryRegion};
use conminer_testkit::McpRig;
use serde_json::json;

/// A rig whose config carries a memory map, keyed by a nickname the device is
/// then given. The map is per-device by design, and a device's config key is its
/// display name, so a nickname is how the two are tied together.
fn rig_with_map() -> (McpRig, String) {
    let mut cfg = Config::default();
    let dev = DeviceOverride {
        memory_map: vec![
            MemoryRegion {
                name: "usb_dp_combo_phy".into(),
                base: 0x088e_1000,
                size: 0x1000,
                note: Some("DP0 combo PHY".into()),
            },
            MemoryRegion {
                name: "mdss_dp0".into(),
                base: 0x0af5_4000,
                size: 0x1000,
                note: None,
            },
        ],
        ..Default::default()
    };
    cfg.devices.insert("board-a".to_string(), dev);

    let rig = McpRig::with_config(cfg);
    let (device, _) = rig.ingest(
        "boot.log",
        "[    1.000000] ufshcd: link startup failed -110\n",
        None,
    );
    rig.call(
        "name_device",
        json!({"device": device, "nickname": "board-a"}),
    );
    (rig, "board-a".to_string())
}

#[test]
fn an_errno_is_named_without_needing_a_device() {
    // ETIMEDOUT means the same thing on every board, and refusing to say so
    // without board context would be unhelpful.
    let rig = McpRig::new();
    let d = rig.call("decode", json!({"value": "-110"}));
    let readings = d["decoded"][0]["readings"].as_array().unwrap();
    assert!(readings
        .iter()
        .any(|r| r["meaning"].as_str().unwrap().contains("ETIMEDOUT")));
}

#[test]
fn the_gic_off_by_thirty_two_is_stated_in_both_directions() {
    // The trap this exists for: a device tree writes `GIC_SPI 436`, every
    // register dump says INTID 468, and the 32 between them costs an afternoon.
    let rig = McpRig::new();
    let d = rig.call("decode", json!({"text": "hwirq 468"}));
    let gic = d["decoded"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["readings"].as_array().unwrap())
        .find(|r| r["kind"] == "gic")
        .expect("a GIC reading");
    assert!(
        gic["meaning"].as_str().unwrap().contains("SPI 436"),
        "{gic}"
    );
    assert_eq!(gic["detail"]["dt_number"], 436);
}

#[test]
fn an_esr_is_decoded_to_its_class_and_fault_status() {
    let rig = McpRig::new();
    let d = rig.call(
        "decode",
        json!({"text": "Unhandled fault: esr 0x96000021 far 0x0"}),
    );
    let esr = d["decoded"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["readings"].as_array().unwrap())
        .find(|r| r["kind"] == "esr")
        .expect("an ESR reading");
    let m = esr["meaning"].as_str().unwrap();
    assert!(m.contains("data abort"), "{m}");
    assert!(m.contains("alignment"), "{m}");
}

#[test]
fn an_ambiguous_value_returns_every_reading_with_its_assumption() {
    // 61 is a valid INTID and also ENODATA. Silently picking one is how an agent
    // ends up chasing the wrong interrupt for an hour.
    let rig = McpRig::new();
    let d = rig.call("decode", json!({"value": "61"}));
    let readings = d["decoded"][0]["readings"].as_array().unwrap();
    let kinds: Vec<&str> = readings
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"gic"), "{kinds:?}");
    assert!(kinds.contains(&"errno"), "{kinds:?}");
    assert!(
        readings.iter().all(|r| r["assuming"].is_string()),
        "every ambiguous reading states what it assumes: {readings:#?}"
    );
}

#[test]
fn a_value_with_no_known_meaning_decodes_to_nothing() {
    let rig = McpRig::new();
    let d = rig.call("decode", json!({"text": "the quick brown fox"}));
    assert_eq!(
        d["decoded"].as_array().unwrap().len(),
        0,
        "nothing is invented"
    );
}

#[test]
fn an_address_resolves_against_this_devices_memory_map() {
    let (rig, device) = rig_with_map();
    let d = rig.call("decode", json!({"device": device, "value": "0x88e1004"}));
    let addr = d["decoded"][0]["readings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "address")
        .expect("an address reading");
    assert_eq!(addr["meaning"], "usb_dp_combo_phy + 0x4");
    assert_eq!(addr["detail"]["note"], "DP0 combo PHY");
    assert_eq!(d["regions_known"], 2);
}

#[test]
fn an_address_outside_every_known_region_is_not_invented() {
    // A device with no map of its own now inherits the RIG map, so decoding
    // works out of the box (S6: it used to report regions_known: 0 forever,
    // because per-device config was never written). What must still hold is the
    // original property: an address that matches nothing resolves to NOTHING,
    // never to a nearby region or to some other board's map, because a confident
    // wrong answer is worse than silence.
    let rig = McpRig::new();
    let probe = "0xdead0000"; // deliberately outside every known block
    let (device, _) = rig.ingest("b.log", &format!("[    1.0] probe at {probe}\n"), None);
    let d = rig.call("decode", json!({"device": device, "value": probe}));
    let has_address = d["decoded"][0]["readings"]
        .as_array()
        .map(|rs| rs.iter().any(|r| r["kind"] == "address"))
        .unwrap_or(false);
    assert!(!has_address);
}

#[test]
fn a_stored_line_can_be_decoded_by_its_anchor() {
    // The realistic path: search finds a line, decode explains it, and the raw
    // line is untouched throughout.
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "b.log",
        "[    1.000000] ufshcd: link startup failed -110\n",
        None,
    );
    let hits = rig.call(
        "search",
        json!({"device": device, "query": "link startup failed", "mode": "terms"}),
    );
    let line_id = hits["hits"][0]["line_id"].as_i64().expect("an anchor");

    let d = rig.call("decode", json!({"device": device, "line_id": line_id}));
    assert!(
        d["input"].as_str().unwrap().contains("-110"),
        "the raw line comes back verbatim: {}",
        d["input"]
    );
    assert!(d["decoded"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["readings"].as_array().unwrap())
        .any(|r| r["meaning"].as_str().unwrap().contains("ETIMEDOUT")));

    // And decoding changed nothing: the stored line is still what arrived.
    let ctx = rig.call("get_context", json!({"device": device, "line_id": line_id}));
    assert!(serde_json::to_string(&ctx).unwrap().contains("-110"));
}

#[test]
fn calling_it_with_nothing_to_decode_is_a_structured_error() {
    let rig = McpRig::new();
    let e = rig.err("decode", json!({}));
    assert_eq!(e["code"], "INVALID_ARGUMENT");
}
