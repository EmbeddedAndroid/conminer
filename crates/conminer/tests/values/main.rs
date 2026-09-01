//! Suite `values` — reading the numbers back out of a `<*>` slot.
//!
//! No-masking makes the wildcard the measurement, not a discarded detail. These
//! tests pin the two things that make the measurement trustworthy: the value is
//! the *raw* token, re-derived from the stored bytes rather than from a parse
//! cached at ingest time; and a slot that cannot be aligned is counted as
//! unaligned rather than guessed at.
//!
//! Edge cases: a template with no wildcards · a named slot that is not a
//! wildcard (error, with the valid set) · numeric summary only when every value
//! is numeric · hex and unit-suffixed values · a version string does not become
//! a number · samples are capped newest-last · scoping to one epoch.

use conminer_testkit::McpRig;
use serde_json::json;

/// Lines that differ only in one number, so exactly one slot is a wildcard.
fn timing_log(values: &[&str]) -> String {
    let mut s = String::new();
    for v in values {
        s.push_str(&format!("[    0.100000] boot took {v} ms\n"));
    }
    s
}

fn slot_of(rig: &McpRig, device: &str, needle: &str) -> i64 {
    let toc = rig.call(
        "list_templates",
        json!({"device": device, "limit": 200, "include_benign": true}),
    );
    toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["text"].as_str().unwrap_or("").contains(needle))
        .unwrap_or_else(|| panic!("no template contains {needle:?}"))["id"]
        .as_i64()
        .unwrap()
}

// -------------------------------------------------------------- the series ---

#[test]
fn a_numeric_slot_comes_back_as_a_series_with_min_max_and_drift() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("t.log", &timing_log(&["120", "460", "310"]), None);
    let id = slot_of(&rig, &device, "boot took");

    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    assert_eq!(v["unaligned"], 0, "every occurrence must align: {v}");
    let slots = v["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 1, "exactly one token varied");

    let s = &slots[0];
    assert_eq!(s["distinct"], 3);
    let n = &s["numeric"];
    assert_eq!(n["min"], 120.0);
    assert_eq!(n["max"], 460.0);
    // First and last in time order: the shape of a drift without the series.
    assert_eq!(n["first"], 120.0);
    assert_eq!(n["last"], 310.0);
    assert!((n["mean"].as_f64().unwrap() - 296.666_666).abs() < 0.01);

    // The literals either side identify *which* wildcard this is, so an agent
    // never has to count tokens to know what it is looking at.
    assert_eq!(s["before"], "took");
    assert_eq!(s["after"], "ms");
}

#[test]
fn the_values_are_the_raw_tokens_not_a_reformatted_copy() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("t.log", &timing_log(&["0120", "460"]), None);
    let id = slot_of(&rig, &device, "boot took");
    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    let values: Vec<&str> = v["slots"][0]["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["value"].as_str().unwrap())
        .collect();
    assert!(
        values.contains(&"0120"),
        "the leading zero is part of what the board printed: {values:?}"
    );
}

#[test]
fn hex_values_are_still_numbers() {
    // The leading timestamp is held constant on purpose: Drain's fixed-depth
    // tree branches on the *first* tokens, so two lines whose timestamps differ
    // never meet to be generalised. That is Drain working as designed, and it is
    // why a printk profile strips the timestamp before mining.
    let rig = McpRig::new();
    let text = "[    0.100000] probe failed at 0x88e1000\n\
                [    0.100000] probe failed at 0x88e4000\n";
    let (device, _) = rig.ingest("h.log", text, None);
    let id = slot_of(&rig, &device, "probe failed at");
    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    let n = &v["slots"][0]["numeric"];
    assert_eq!(n["min"], 143_527_936.0_f64, "0x88e1000");
    assert_eq!(n["max"], 143_540_224.0_f64, "0x88e4000");
}

#[test]
fn a_slot_of_non_numeric_tokens_reports_no_statistics() {
    // A mode name has no mean. The honest answer is the distinct values.
    let rig = McpRig::new();
    let text = "[    0.100000] mmc0: switching to mode HS200\n\
                [    0.100000] mmc0: switching to mode HS400\n";
    let (device, _) = rig.ingest("m.log", text, None);
    let id = slot_of(&rig, &device, "switching to mode");
    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    let slot = &v["slots"][0];
    assert!(
        slot["numeric"].is_null(),
        "a mode name is not a quantity: {slot}"
    );
    assert_eq!(slot["distinct"], 2);
    assert_eq!(slot["top"].as_array().unwrap().len(), 2);
    assert_eq!(slot["before"], "mode");
}

// ------------------------------------------------------------- the edges -----

#[test]
fn a_template_with_no_wildcards_says_so_instead_of_returning_nothing() {
    let rig = McpRig::new();
    // A line with NO numeric literal at all. `timing_log(["120","120","120"])`
    // used to serve here because three identical numbers produced no wildcard --
    // but numeric literals are now masked before clustering, so any digit
    // becomes a slot regardless of whether it varies. The property under test is
    // "a template with no slots explains itself", so the fixture has to be
    // genuinely slot-free.
    let text = "boot finished cleanly\nboot finished cleanly\nboot finished cleanly\n";
    let (device, _) = rig.ingest("c.log", text, None);
    let id = slot_of(&rig, &device, "boot finished");
    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    assert_eq!(v["slots"].as_array().unwrap().len(), 0);
    assert!(v["why"].as_str().unwrap().contains("byte-identical"));
}

#[test]
fn naming_a_token_that_is_not_a_wildcard_lists_the_ones_that_are() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("t.log", &timing_log(&["120", "460"]), None);
    let id = slot_of(&rig, &device, "boot took");
    let e = rig.err(
        "template_values",
        json!({"device": device, "template_id": id, "slot": 0}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    assert!(
        e["hint"].as_str().unwrap().contains("wildcard slots are"),
        "the error has to say which slots exist, or the agent has to guess: {e}"
    );
}

#[test]
fn an_unknown_template_is_a_structured_error() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("t.log", &timing_log(&["1", "2"]), None);
    let e = rig.err(
        "template_values",
        json!({"device": device, "template_id": 987_654}),
    );
    assert_eq!(e["code"], "UNKNOWN_TEMPLATE");
}

#[test]
fn samples_are_capped_but_the_statistics_still_cover_everything() {
    let rig = McpRig::new();
    let values: Vec<String> = (1..=50).map(|i| (i * 10).to_string()).collect();
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let (device, _) = rig.ingest("many.log", &timing_log(&refs), None);
    let id = slot_of(&rig, &device, "boot took");

    let v = rig.call(
        "template_values",
        json!({"device": device, "template_id": id, "samples": 5}),
    );
    let s = &v["slots"][0];
    assert_eq!(s["samples"].as_array().unwrap().len(), 5);
    // The cap trims what is *shown*, never what is measured — the same rule the
    // follow tail obeys.
    assert_eq!(s["distinct"], 50);
    assert_eq!(s["numeric"]["min"], 10.0);
    assert_eq!(s["numeric"]["max"], 500.0);
    assert_eq!(
        s["samples"][4]["value"], "500",
        "the newest sample is last: a trend is read from where it ended up"
    );
}

#[test]
fn scoping_to_one_epoch_measures_only_that_epoch() {
    let rig = McpRig::new();
    // Two boots, each with its own timing, separated by a bootloader banner so
    // the framer opens a second epoch.
    let text = format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n{}NOTICE:  BL1: v2.11(release):v2.11\n{}",
        timing_log(&["100"]),
        timing_log(&["900"])
    );
    let (device, _) = rig.ingest("two.log", &text, None);
    let id = slot_of(&rig, &device, "boot took");

    let boots = rig.call("list_boots", json!({"device": device, "limit": 10}));
    let latest = boots["boots"][0]["id"].as_i64().unwrap();

    let all = rig.call(
        "template_values",
        json!({"device": device, "template_id": id}),
    );
    assert_eq!(all["slots"][0]["distinct"], 2);

    let scoped = rig.call(
        "template_values",
        json!({"device": device, "template_id": id, "boot": latest}),
    );
    assert_eq!(scoped["slots"][0]["distinct"], 1);
    assert_eq!(scoped["slots"][0]["numeric"]["min"], 900.0);
}
