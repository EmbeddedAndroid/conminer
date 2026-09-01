//! Suite `baselines` — "new versus the last boot that worked", and `diff_boots`.
//!
//! `new_only` is scoped to a session, which stops being the question the moment
//! a device has history: what an agent actually asks is whether *this* boot
//! differs from a known-good one. A baseline is that reference point, and it is
//! durable, so the answer survives the agent restarting.
//!
//! `diff_boots` covers the half a fingerprint deliberately ignores. Two boots
//! that hit the same stages in the same order hash the same whether handoff took
//! 40 ms or 4 s, so "it still boots but it got slower" is invisible to every
//! other tool here.
//!
//! Edge cases: no baseline set (structured error, not an empty list) · a named
//! baseline beside the default · re-blessing moves it · clearing · diffing an
//! epoch against itself is rejected · a stage present in only one epoch is
//! marked rather than silently dropped · the largest regression sorts first.

use conminer_testkit::McpRig;
use serde_json::json;

/// One boot: bootloader banner, kernel handoff at `handoff_ms`, then `extra`.
fn boot(seq: u32, handoff_ms: u32, extra: &str) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         [    {handoff_ms}.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #{seq} SMP\n\
         {extra}"
    )
}

fn boots_of(rig: &McpRig, device: &str) -> Vec<i64> {
    let b = rig.call("list_boots", json!({"device": device, "limit": 50}));
    b["boots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_i64().unwrap())
        .collect()
}

// -------------------------------------------------------------- baselines ----

#[test]
fn a_baseline_is_blessed_and_listed_with_what_it_contained() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("good.log", &boot(1, 3, "[    4.0] mmc0: ready\n"), None);

    let set = rig.call("set_baseline", json!({"device": device}));
    assert_eq!(set["baseline"]["name"], "default");
    assert!(set["baseline"]["templates"].as_i64().unwrap() > 0);

    let list = rig.call("list_baselines", json!({"device": device}));
    assert_eq!(list["count"], 1);
    assert_eq!(list["baselines"][0]["baseline"]["name"], "default");
}

#[test]
fn vs_baseline_shows_only_what_the_good_boot_never_printed() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("good.log", &boot(1, 3, "[    4.0] mmc0: ready\n"), None);
    rig.call("set_baseline", json!({"device": device}));

    // A second boot that adds a line the good boot never had.
    let path = rig.dir.path().join("bad.log");
    std::fs::write(
        &path,
        boot(
            2,
            3,
            "[    4.0] mmc0: ready\n[    5.0] ufshcd: link startup failed -110\n",
        ),
    )
    .unwrap();
    rig.call(
        "ingest_file",
        json!({"path": path.display().to_string(), "device": device}),
    );

    let novel = rig.call(
        "list_templates",
        json!({"device": device, "vs_baseline": true, "limit": 50}),
    );
    let texts: Vec<&str> = novel["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("link startup failed")),
        "the new failure must be in the diff: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("mmc0: ready")),
        "a line the good boot also printed is not news: {texts:?}"
    );
    assert_eq!(novel["vs_baseline"]["name"], "default");
}

#[test]
fn asking_for_a_baseline_that_was_never_set_is_a_structured_error() {
    // Not an empty list: "nothing is new" and "there is nothing to compare
    // against" are different answers and must not look alike.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("a.log", &boot(1, 3, ""), None);
    let e = rig.err(
        "list_templates",
        json!({"device": device, "vs_baseline": true}),
    );
    assert_eq!(e["code"], "UNKNOWN_BASELINE");
}

#[test]
fn baselines_can_be_named_so_one_can_exist_per_image() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("a.log", &boot(1, 3, "[    4.0] mmc0: ready\n"), None);
    rig.call("set_baseline", json!({"device": device}));
    rig.call(
        "set_baseline",
        json!({"device": device, "name": "v2.11", "note": "last known good BL31"}),
    );

    let list = rig.call("list_baselines", json!({"device": device}));
    assert_eq!(list["count"], 2);
    let named = rig.call(
        "list_templates",
        json!({"device": device, "vs_baseline": "v2.11"}),
    );
    assert_eq!(named["vs_baseline"]["name"], "v2.11");
    assert_eq!(named["vs_baseline"]["note"], "last known good BL31");
}

#[test]
fn re_blessing_moves_the_baseline_and_clearing_removes_it() {
    let rig = McpRig::new();
    let text = format!(
        "{}{}",
        boot(1, 3, "[    4.0] mmc0: ready\n"),
        boot(2, 3, "[    4.0] mmc0: ready\n")
    );
    let (device, _) = rig.ingest("a.log", &text, None);
    let ids = boots_of(&rig, &device);
    assert!(ids.len() >= 2, "two banners make two epochs");

    rig.call("set_baseline", json!({"device": device, "boot": ids[1]}));
    let first = rig.call("list_baselines", json!({"device": device}));
    assert_eq!(first["baselines"][0]["baseline"]["boot_id"], ids[1]);

    rig.call("set_baseline", json!({"device": device, "boot": ids[0]}));
    let moved = rig.call("list_baselines", json!({"device": device}));
    assert_eq!(moved["count"], 1, "re-blessing replaces, never accumulates");
    assert_eq!(moved["baselines"][0]["baseline"]["boot_id"], ids[0]);

    let cleared = rig.call("set_baseline", json!({"device": device, "clear": true}));
    assert_eq!(cleared["cleared"], true);
    assert_eq!(
        rig.call("list_baselines", json!({"device": device}))["count"],
        0
    );
}

#[test]
fn boot_report_summarises_against_the_baseline_without_enumerating() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("good.log", &boot(1, 3, "[    4.0] mmc0: ready\n"), None);
    let ids = boots_of(&rig, &device);
    rig.call("set_baseline", json!({"device": device, "boot": ids[0]}));

    let path = rig.dir.path().join("bad.log");
    std::fs::write(
        &path,
        boot(2, 9, "[   10.0] ufshcd: link startup failed -110\n"),
    )
    .unwrap();
    rig.call(
        "ingest_file",
        json!({"path": path.display().to_string(), "device": device}),
    );

    let report = rig.call("boot_report", json!({"device": device}));
    let vs = &report["vs_baseline"];
    assert_eq!(vs["baseline"], "default");
    assert!(
        vs["new_vs_baseline"].as_i64().unwrap() > 0,
        "the bad boot printed something the good one never did: {vs}"
    );
    // A summary, deliberately: the enumeration costs tokens and is one call away
    // once the summary says it is worth having.
    assert!(vs["detail"].as_str().unwrap().contains("diff_boots"));
}

// ------------------------------------------------------------- diff_boots ----

#[test]
fn diff_boots_reports_stage_timing_that_the_fingerprint_cannot() {
    let rig = McpRig::new();
    // Two boots with the same stages in the same order — identical fingerprints
    // — but the second reaches the kernel six seconds later.
    let text = format!("{}{}", boot(1, 3, ""), boot(2, 9, ""));
    let (device, _) = rig.ingest("slow.log", &text, None);
    let ids = boots_of(&rig, &device);
    assert!(ids.len() >= 2);

    let d = rig.call(
        "diff_boots",
        json!({"device": device, "a": ids[1], "b": ids[0]}),
    );
    let kernel = d["stage_deltas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == "kernel")
        .expect("the kernel stage is in both epochs");
    assert!(
        kernel["delta_ms"].as_i64().unwrap() != 0,
        "the whole point of this tool is that a timing change is visible: {kernel}"
    );
    assert!(kernel["reached_at_ms_a"].as_i64().is_some());
    assert!(kernel["reached_at_ms_b"].as_i64().is_some());
}

#[test]
fn the_largest_regression_sorts_first() {
    let rig = McpRig::new();
    let text = format!("{}{}", boot(1, 3, ""), boot(2, 9, ""));
    let (device, _) = rig.ingest("slow.log", &text, None);
    let ids = boots_of(&rig, &device);
    let d = rig.call(
        "diff_boots",
        json!({"device": device, "a": ids[1], "b": ids[0]}),
    );
    // Comparable stages lead, in descending delta; anything without a delta
    // must come after them rather than sorting as if it were a huge negative.
    let raw: Vec<Option<i64>> = d["stage_deltas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["delta_ms"].as_i64())
        .collect();
    let with_delta: Vec<i64> = raw.iter().flatten().copied().collect();
    let mut sorted = with_delta.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        with_delta, sorted,
        "what got slowest should be the first line read, not something to scan for"
    );
    let first_none = raw.iter().position(Option::is_none).unwrap_or(raw.len());
    assert!(
        raw[first_none..].iter().all(Option::is_none),
        "stages with no delta must all sort after the ones that have one: {raw:?}"
    );
}

#[test]
fn a_stage_present_in_only_one_epoch_is_marked_not_dropped() {
    let rig = McpRig::new();
    // The second boot never reaches the kernel: it dies in U-Boot.
    let text = format!(
        "{}NOTICE:  BL1: v2.11(release):v2.11\nU-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         => \n",
        boot(1, 3, "")
    );
    let (device, _) = rig.ingest("died.log", &text, None);
    let ids = boots_of(&rig, &device);

    let d = rig.call(
        "diff_boots",
        json!({"device": device, "a": ids[1], "b": ids[0]}),
    );
    let kernel = d["stage_deltas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == "kernel")
        .expect("a stage missing from B still has to appear");
    assert_eq!(
        kernel["only_in"], "a",
        "'it never got there' is the finding, and dropping the row would hide it"
    );
    assert!(kernel["delta_ms"].is_null());
}

#[test]
fn diffing_an_epoch_against_itself_is_rejected() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("a.log", &boot(1, 3, ""), None);
    let ids = boots_of(&rig, &device);
    let e = rig.err(
        "diff_boots",
        json!({"device": device, "a": ids[0], "b": ids[0]}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
}

#[test]
fn diff_boots_finds_the_template_that_only_the_bad_boot_printed() {
    let rig = McpRig::new();
    let text = format!(
        "{}{}",
        boot(1, 3, "[    4.0] mmc0: ready\n"),
        boot(
            2,
            3,
            "[    4.0] mmc0: ready\n[    5.0] ufshcd: link startup failed -110\n"
        )
    );
    let (device, _) = rig.ingest("both.log", &text, None);
    let ids = boots_of(&rig, &device);

    let d = rig.call(
        "diff_boots",
        json!({"device": device, "a": ids[1], "b": ids[0]}),
    );
    assert!(
        d["new_in_b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["text"].as_str().unwrap().contains("link startup failed")),
        "{}",
        serde_json::to_string_pretty(&d["new_in_b"]).unwrap()
    );
}
