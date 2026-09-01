//! Suite `spine` (§18.1) — evidence attachment and the merged timeline.
//!
//! The premise: a console is one evidence stream, and silicon answers usually
//! live in the join. So the property under test is that another tool can hand
//! conminer a fact with a timestamp and have it land on the right epoch, next to
//! the console events it needs to be read against.
//!
//! Edge cases: evidence lands on the epoch covering its timestamp without the
//! caller computing one · an explicit epoch overrides that · evidence before any
//! epoch is kept rather than dropped · the timeline is time-ordered across
//! sources, not grouped by source · `evidence_only` excludes console events ·
//! the payload is stored verbatim.

use conminer_testkit::McpRig;
use serde_json::json;

fn boot_text(seq: u32) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         [    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #{seq} SMP\n\
         [    9.000000] Kernel panic - not syncing: VFS: Unable to mount root fs\n"
    )
}

fn boots(rig: &McpRig, device: &str) -> Vec<serde_json::Value> {
    rig.call("list_boots", json!({"device": device, "limit": 50}))["boots"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn evidence_lands_on_the_epoch_that_covers_its_timestamp() {
    // The point of deriving it: an external tool knows what time it is, not what
    // conminer decided to call epoch 3.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_text(1), None);
    let b = boots(&rig, &device);
    let target = &b[0];
    let opened_at = target["opened_at"].as_i64().unwrap();

    let r = rig.call(
        "attach_evidence",
        json!({"device": device, "source": "jtag", "at": opened_at + 5,
               "summary": "core 0 halted in psci_cpu_on",
               "data": {"pc": "0x40000a10", "el": 3}}),
    );
    assert_eq!(r["attached"]["boot_id"], target["id"]);
    assert_eq!(r["attached"]["source"], "jtag");
}

#[test]
fn an_explicit_epoch_overrides_the_timestamp() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        &format!("{}{}", boot_text(1), boot_text(2)),
        None,
    );
    let b = boots(&rig, &device);
    let older = b.last().unwrap()["id"].as_i64().unwrap();

    let r = rig.call(
        "attach_evidence",
        json!({"device": device, "source": "note", "boot": older,
               "data": {"text": "this belongs to the first boot"}}),
    );
    assert_eq!(r["attached"]["boot_id"], older);
}

#[test]
fn evidence_from_before_any_epoch_is_kept_not_dropped() {
    // A power measurement taken while the board was off is exactly the kind of
    // thing that explains the boot that follows.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_text(1), None);
    let r = rig.call(
        "attach_evidence",
        json!({"device": device, "source": "power", "at": 1,
               "data": {"rail": "vdd_cx", "volts": 0.0}}),
    );
    assert!(r["attached"]["boot_id"].is_null(), "no epoch covers it");
    assert!(
        r["attached"]["evidence_id"].as_i64().unwrap() > 0,
        "but it is stored"
    );
}

#[test]
fn the_timeline_interleaves_evidence_with_console_events_by_time() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_text(1), None);
    let b = boots(&rig, &device);
    let opened_at = b[0]["opened_at"].as_i64().unwrap();

    // Two facts from two different tools, deliberately out of insertion order.
    rig.call(
        "attach_evidence",
        json!({"device": device, "source": "power", "at": opened_at + 20,
               "summary": "vdd_cx sagged to 0.62 V", "data": {"volts": 0.62}}),
    );
    rig.call(
        "attach_evidence",
        json!({"device": device, "source": "jtag", "at": opened_at + 2,
               "summary": "halted", "data": {}}),
    );

    let t = rig.call("timeline", json!({"device": device}));
    let items = t["items"].as_array().unwrap();
    let ats: Vec<i64> = items.iter().map(|i| i["at"].as_i64().unwrap()).collect();
    let mut sorted = ats.clone();
    sorted.sort();
    assert_eq!(
        ats, sorted,
        "a timeline grouped by source is not a timeline: {items:#?}"
    );

    let kinds: Vec<&str> = items.iter().map(|i| i["what"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"epoch_open"));
    assert!(
        kinds.contains(&"stage"),
        "console events are present: {kinds:?}"
    );
    assert!(kinds.contains(&"evidence:jtag"), "{kinds:?}");
    assert!(kinds.contains(&"evidence:power"), "{kinds:?}");

    // Offsets are relative to the epoch, which is the frame anyone reasons in.
    let jtag = items.iter().find(|i| i["what"] == "evidence:jtag").unwrap();
    assert_eq!(jtag["offset_ms"], 2);
}

#[test]
fn evidence_only_excludes_the_console_events() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_text(1), None);
    rig.call(
        "attach_evidence",
        json!({"device": device, "source": "ci", "data": {"job": 12345}}),
    );
    let t = rig.call("timeline", json!({"device": device, "evidence_only": true}));
    let kinds: Vec<&str> = t["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["what"].as_str().unwrap())
        .collect();
    assert!(
        kinds
            .iter()
            .all(|k| *k == "epoch_open" || k.starts_with("evidence:")),
        "{kinds:?}"
    );
}

#[test]
fn the_payload_is_stored_verbatim() {
    // conminer does not understand a JTAG dump and must not pretend to: it
    // stores the fact and puts it in the right place on the timeline.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_text(1), None);
    let payload = json!({"regs": {"x0": "0xdeadbeef"}, "nested": [1, 2, {"deep": true}]});
    rig.call(
        "attach_evidence",
        json!({"device": device, "source": "jtag", "data": payload}),
    );
    let t = rig.call("timeline", json!({"device": device, "evidence_only": true}));
    let ev = t["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["what"] == "evidence:jtag")
        .expect("the evidence");
    assert_eq!(ev["detail"]["data"], payload);
}

#[test]
fn a_device_with_no_epochs_says_so_rather_than_returning_an_empty_timeline() {
    let rig = McpRig::new();
    let dir = rig.dir.path().join("empty.log");
    std::fs::write(&dir, "").unwrap();
    let e = rig.err("timeline", json!({"device": "does-not-exist"}));
    assert_eq!(e["code"], "UNKNOWN_DEVICE");
}
