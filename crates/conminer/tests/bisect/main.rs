//! Suite `bisect` (§18.2) — searching an ordered list of builds.
//!
//! The algorithm itself is unit-tested in `conminer-core`; this suite pins the
//! behaviour an agent actually meets: that a verdict can be *derived* from an
//! epoch rather than asserted, that the search survives being put down and
//! picked up tomorrow, and that the two ways a bisect can lie (a skipped range,
//! an intermittent failure) are reported instead of resolved.

use conminer_testkit::McpRig;
use serde_json::json;

const BUILDS: [&str; 8] = ["v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8"];

fn boot(seq: u32, extra: &str) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         [    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #{seq} SMP\n\
         {extra}"
    )
}

fn start(rig: &McpRig, device: &str, predicate: serde_json::Value) -> serde_json::Value {
    let mut args = json!({"device": device, "name": "regress", "candidates": BUILDS});
    if !predicate.is_null() {
        args["predicate"] = predicate;
    }
    rig.call("bisect_start", args)
}

#[test]
fn the_ends_are_asked_for_first_then_the_range_is_halved() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    let s = start(&rig, &device, json!(null));
    assert_eq!(s["started"]["next"]["status"], "test");
    assert_eq!(s["started"]["next"]["index"], 0);

    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v1", "verdict": "good"}),
    );
    assert_eq!(r["next"]["index"], 7, "the other end is needed next");

    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v8", "verdict": "bad"}),
    );
    assert_eq!(r["next"]["index"], 3, "now it halves");
    assert!(r["next"]["max_further_tests"].as_i64().unwrap() <= 3);
}

#[test]
fn the_culprit_is_the_first_bad_build() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    start(&rig, &device, json!(null));
    for (c, v) in [("v1", "good"), ("v8", "bad"), ("v4", "good"), ("v6", "bad")] {
        rig.call(
            "bisect_report",
            json!({"device": device, "name": "regress", "candidate": c, "verdict": v}),
        );
    }
    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v5", "verdict": "bad"}),
    );
    assert_eq!(r["next"]["status"], "found");
    assert_eq!(r["next"]["candidate"], "v5");
    assert_eq!(r["next"]["last_good"], "v4");

    // And it is written down, so tomorrow's session does not re-derive it.
    let st = rig.call(
        "bisect_status",
        json!({"device": device, "name": "regress"}),
    );
    assert_eq!(st["status"]["bisect"]["state"], "done");
    assert_eq!(st["status"]["bisect"]["culprit"], "v5");
}

#[test]
fn a_verdict_can_be_derived_from_an_epoch_instead_of_asserted() {
    // This is the half that makes a bisect cheap: the agent flashes and boots,
    // hands over the epoch, and conminer decides using the same evidence it
    // would use for any other question.
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "bad.log",
        &boot(
            1,
            "[    9.0] Kernel panic - not syncing: VFS: Unable to mount root fs\n",
        ),
        None,
    );
    let toc = rig.call("list_templates", json!({"device": device, "limit": 200}));
    let panic_id = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["text"].as_str().unwrap().contains("Kernel panic"))
        .expect("the panic template")["id"]
        .as_i64()
        .unwrap();
    let boot_id = rig.call("list_boots", json!({"device": device, "limit": 5}))["boots"][0]["id"]
        .as_i64()
        .unwrap();

    start(&rig, &device, json!({"template_id": panic_id}));
    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v8", "boot": boot_id}),
    );
    assert_eq!(r["recorded"]["verdict"], "bad");
    assert!(
        r["recorded"]["classified_from_boot"]
            .as_str()
            .unwrap()
            .contains("fired"),
        "the classification states its evidence: {r}"
    );
}

#[test]
fn an_epoch_that_cannot_be_classified_is_skipped_not_guessed() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    let boot_id = rig.call("list_boots", json!({"device": device, "limit": 5}))["boots"][0]["id"]
        .as_i64()
        .unwrap();
    // A fingerprint predicate against an epoch with no fingerprint yet.
    start(&rig, &device, json!({"fingerprint": "deadbeef"}));
    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v4", "boot": boot_id}),
    );
    assert!(
        matches!(
            r["recorded"]["verdict"].as_str(),
            Some("skip") | Some("good")
        ),
        "an undecidable epoch must not be called bad: {r}"
    );
}

#[test]
fn a_range_of_only_skips_reports_inconclusive_rather_than_naming_a_build() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    start(&rig, &device, json!(null));
    for (c, v) in [
        ("v3", "good"),
        ("v6", "bad"),
        ("v4", "skip"),
        ("v5", "skip"),
    ] {
        rig.call(
            "bisect_report",
            json!({"device": device, "name": "regress", "candidate": c, "verdict": v}),
        );
    }
    let st = rig.call(
        "bisect_status",
        json!({"device": device, "name": "regress"}),
    );
    assert_eq!(st["status"]["next"]["status"], "inconclusive");
    assert_eq!(st["status"]["bisect"]["state"], "inconclusive");
    assert!(
        st["status"]["bisect"]["culprit"].is_null(),
        "no build is named"
    );
}

#[test]
fn a_non_monotonic_result_is_surfaced_because_it_changes_the_conclusion() {
    // good after bad means the failure is intermittent, and a halving that
    // ignored it would confidently pin an innocent build.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    start(&rig, &device, json!(null));
    rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v3", "verdict": "bad"}),
    );
    let r = rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v6", "verdict": "good"}),
    );
    let c = r["contradiction"]
        .as_str()
        .expect("a contradiction is reported");
    assert!(c.contains("not monotonic"), "{c}");
    assert!(c.contains("intermittent"), "{c}");
}

#[test]
fn a_bisect_survives_being_put_down_and_picked_up() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    start(&rig, &device, json!(null));
    rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v1", "verdict": "good"}),
    );
    assert_eq!(
        rig.call("list_bisects", json!({"device": device}))["count"],
        1
    );
    let st = rig.call(
        "bisect_status",
        json!({"device": device, "name": "regress"}),
    );
    assert_eq!(
        st["status"]["bisect"]["results"].as_array().unwrap().len(),
        1
    );
    assert_eq!(st["status"]["next"]["status"], "test");
}

#[test]
fn restarting_a_name_clears_the_previous_verdicts() {
    // Otherwise a re-run silently inherits yesterday's answers about a different
    // set of builds.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);
    start(&rig, &device, json!(null));
    rig.call(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v1", "verdict": "good"}),
    );
    start(&rig, &device, json!(null));
    let st = rig.call(
        "bisect_status",
        json!({"device": device, "name": "regress"}),
    );
    assert_eq!(
        st["status"]["bisect"]["results"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn candidates_and_names_are_validated() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("b.log", &boot(1, ""), None);

    let e = rig.err(
        "bisect_start",
        json!({"device": device, "name": "tiny", "candidates": ["only-one"]}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");

    start(&rig, &device, json!(null));
    let e = rig.err(
        "bisect_report",
        json!({"device": device, "name": "regress", "candidate": "v99", "verdict": "bad"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    assert!(
        e["detail"]["candidates"].is_array(),
        "the valid set is attached"
    );

    let e = rig.err("bisect_status", json!({"device": device, "name": "nope"}));
    assert_eq!(e["code"], "UNKNOWN_BISECT");
}
