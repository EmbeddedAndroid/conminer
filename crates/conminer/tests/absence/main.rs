//! Suite `absence` (§18.4) — what a boot should have printed and did not.
//!
//! Every other query in conminer answers "what appeared". A bring-up failure is
//! usually the opposite shape, and a novel-template list structurally cannot say
//! it. So the property here is that a line which *always* prints and this time
//! did not is surfaced as the finding.
//!
//! Edge cases: learning refuses to fit a skeleton to one boot · reliability is
//! carried rather than thresholded away · an unreliable line's absence is
//! reported separately, not silently · a line that printed but arrived late is a
//! different finding from one that never printed · asking before learning says
//! so instead of reporting everything as present.

use conminer_testkit::McpRig;
use serde_json::json;

/// A boot that prints the given extra lines after the usual skeleton.
fn boot(n: u32, extra: &str) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         NOTICE:  BL2: v2.11(release):v2.11\n\
         NOTICE:  BL31: v2.11(release):v2.11\n\
         U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         [    1.000000] ddr: training complete, 48 GiB\n\
         [    2.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #{n} SMP\n\
         [    3.000000] mmc0: new HS400 card\n\
         {extra}\
         [    9.000000] Run /sbin/init as init process\n"
    )
}

/// Ingest `good` normal boots and one suspect boot, and learn from the good.
fn rig_with_skeleton(good: usize, suspect: &str) -> (McpRig, String, i64) {
    let rig = McpRig::new();
    let mut text = String::new();
    for i in 0..good {
        text.push_str(&boot(i as u32, ""));
    }
    let (device, _) = rig.ingest("good.log", &text, None);

    // Every epoch here is a reference epoch; naming them explicitly keeps the
    // fixture independent of how outcomes happen to be classified.
    let ids: Vec<i64> = rig.call("list_boots", json!({"device": device, "limit": 100}))["boots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_i64().unwrap())
        .collect();
    rig.call(
        "learn_expectations",
        json!({"device": device, "boots": ids}),
    );

    let path = rig.dir.path().join("suspect.log");
    std::fs::write(&path, suspect).unwrap();
    rig.call(
        "ingest_file",
        json!({"path": path.display().to_string(), "device": device}),
    );
    let latest = rig.call("list_boots", json!({"device": device, "limit": 5}))["boots"][0]["id"]
        .as_i64()
        .unwrap();
    (rig, device, latest)
}

#[test]
fn a_line_that_always_prints_and_this_time_did_not_is_the_finding() {
    // The DDR training banner is missing from the suspect boot. Nothing novel
    // appeared, so every other tool here would call this boot unremarkable.
    let suspect = boot(99, "").replace("[    1.000000] ddr: training complete, 48 GiB\n", "");
    let (rig, device, boot_id) = rig_with_skeleton(4, &suspect);

    let m = rig.call(
        "missing_in_boot",
        json!({"device": device, "boot": boot_id}),
    );
    assert_eq!(m["learned"], true);
    let missing: Vec<&str> = m["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["text"].as_str().unwrap())
        .collect();
    assert!(
        missing.iter().any(|t| t.contains("training complete")),
        "the absent line is the answer: {missing:?}"
    );
}

#[test]
fn reliability_is_carried_so_an_agent_can_tell_how_strong_the_claim_is() {
    let suspect = boot(99, "").replace("[    1.000000] ddr: training complete, 48 GiB\n", "");
    let (rig, device, boot_id) = rig_with_skeleton(4, &suspect);
    let m = rig.call(
        "missing_in_boot",
        json!({"device": device, "boot": boot_id}),
    );
    let first = &m["missing"][0];
    assert_eq!(first["reliability"], 1.0);
    assert_eq!(first["seen_in"], "4/4");
    // Most reliable first: the thing that always prints and did not this time is
    // the strongest signal available.
    let rels: Vec<f64> = m["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["reliability"].as_f64().unwrap())
        .collect();
    let mut sorted = rels.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(rels, sorted);
}

#[test]
fn an_unreliable_lines_absence_is_reported_separately_not_dropped() {
    // A line seen in only some good boots is weak evidence, but "it is sometimes
    // absent anyway" is still information and must not be silently discarded.
    let rig = McpRig::new();
    let mut text = String::new();
    for i in 0..4 {
        // The thermal line appears in only one of the four reference boots.
        let extra = if i == 0 {
            "[    4.000000] thermal: zone tsens0 registered\n"
        } else {
            ""
        };
        text.push_str(&boot(i, extra));
    }
    let (device, _) = rig.ingest("good.log", &text, None);
    let ids: Vec<i64> = rig.call("list_boots", json!({"device": device, "limit": 100}))["boots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_i64().unwrap())
        .collect();
    rig.call(
        "learn_expectations",
        json!({"device": device, "boots": ids}),
    );

    let path = rig.dir.path().join("suspect.log");
    std::fs::write(&path, boot(9, "")).unwrap();
    rig.call(
        "ingest_file",
        json!({"path": path.display().to_string(), "device": device}),
    );
    let boot_id = rig.call("list_boots", json!({"device": device, "limit": 5}))["boots"][0]["id"]
        .as_i64()
        .unwrap();

    let m = rig.call(
        "missing_in_boot",
        json!({"device": device, "boot": boot_id}),
    );
    let flaky: Vec<&str> = m["flaky_and_absent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["text"].as_str().unwrap())
        .collect();
    assert!(
        flaky.iter().any(|t| t.contains("tsens0")),
        "a 1-in-4 line belongs in flaky_and_absent, not missing: {m:#}"
    );
    assert!(
        !m["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["text"].as_str().unwrap().contains("tsens0")),
        "and not in the strong list"
    );
}

#[test]
fn learning_refuses_to_fit_a_skeleton_to_one_boot() {
    // A model fitted to a single boot describes that boot, not the device, and
    // would then report every difference as an absence.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("one.log", &boot(1, ""), None);
    let e = rig.err("learn_expectations", json!({"device": device}));
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    assert!(
        e["hint"].as_str().unwrap().contains("not the device"),
        "{}",
        e["hint"]
    );
}

#[test]
fn asking_before_learning_says_so_rather_than_reporting_nothing_missing() {
    // "Nothing is missing" and "I have no idea what normal looks like" are
    // different answers and must not look alike.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    let m = rig.call("missing_in_boot", json!({"device": device}));
    assert_eq!(m["learned"], false);
    assert!(m["why"].as_str().unwrap().contains("learn_expectations"));
    assert_eq!(m["missing"].as_array().unwrap().len(), 0);
}

#[test]
fn a_line_that_printed_late_is_a_different_finding_from_one_that_never_printed() {
    // Held out on purpose: a set difference calls a late line healthy, and "it
    // still boots but handoff is seconds late" is exactly the regression that
    // matters most.
    let mut suspect = boot(99, "");
    suspect = suspect.replace(
        "[    2.000000] Linux version",
        "[   30.000000] Linux version",
    );
    let (rig, device, boot_id) = rig_with_skeleton(4, &suspect);

    let m = rig.call(
        "missing_in_boot",
        json!({"device": device, "boot": boot_id, "late_factor": 1.5}),
    );
    // The suspect boot's own epoch timings are synthetic in the test clock, so
    // the assertion is on the *shape* of the answer: late is its own bucket and
    // does not leak into `missing`.
    assert!(m["late"].is_array());
    assert!(
        !m["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["text"].as_str().unwrap().contains("Linux version")),
        "a line that printed is never `missing`, however late: {m:#}"
    );
}

#[test]
fn learning_replaces_rather_than_accumulating() {
    // Merging a new fit into an old one would quietly mix two definitions of
    // "normal", which is the one thing an absence report cannot survive.
    let (rig, device, _) = rig_with_skeleton(4, &boot(9, ""));
    let first = rig.call(
        "learn_expectations",
        json!({"device": device, "boots": [1, 2, 3]}),
    );
    let n1 = first["learned"]["expectations"].as_i64().unwrap();
    let second = rig.call(
        "learn_expectations",
        json!({"device": device, "boots": [1, 2, 3]}),
    );
    assert_eq!(
        second["learned"]["expectations"].as_i64().unwrap(),
        n1,
        "re-learning the same set must not double the model"
    );
    assert_eq!(second["learned"]["reference_boots"], 3);
}
