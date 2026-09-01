//! Suite `watches` — predicates that keep firing while nobody is listening.
//!
//! `follow` is a long poll: it answers "has it happened yet?" and returns on the
//! first hit. That is right for an agent sitting on the call and wrong for an
//! overnight soak, where the agent is gone when the interesting thing happens
//! and the firing is simply lost.
//!
//! A watch is evaluated against the *stored* stream, so the guarantee under test
//! is that being disconnected costs nothing: every firing is still there, with
//! the timestamp it actually had, in the order it actually happened.
//!
//! Edge cases: a firing that happens between polls is not lost · a poll is
//! consuming, so nothing is delivered twice · `peek` does not consume · `from:
//! start` replays history · an unknown watch name is a structured error · a
//! malformed predicate is rejected at creation, not at 3am · `any:[…]` returns a
//! timeline rather than per-predicate groups · deleting removes undelivered
//! hits.

use conminer_testkit::McpRig;
use serde_json::json;

fn boot_text(seq: u32) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n\
         [    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #{seq} SMP\n"
    )
}

/// Append more console output to an existing device, as if it kept running.
fn append(rig: &McpRig, device: &str, name: &str, text: &str) {
    let path = rig.dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    rig.call(
        "ingest_file",
        json!({"path": path.display().to_string(), "device": device}),
    );
}

// --------------------------------------------------------- the whole point ---

#[test]
fn a_firing_that_happens_while_nobody_is_polling_is_still_there_afterwards() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);

    rig.call(
        "create_watch",
        json!({"device": device, "name": "panics",
               "until": {"pattern": "Kernel panic"}}),
    );

    // Nobody is connected for this part.
    append(
        &rig,
        &device,
        "later.log",
        "[    9.100000] Kernel panic - not syncing: VFS: Unable to mount root fs\n",
    );

    let hits = rig.call("poll_watch", json!({"device": device, "name": "panics"}));
    assert_eq!(
        hits["returned"],
        1,
        "{}",
        serde_json::to_string_pretty(&hits).unwrap()
    );
    assert!(hits["hits"][0]["evidence"]["text"]
        .as_str()
        .unwrap()
        .contains("Kernel panic"));
    // The timestamp is the console's, not the poll's: the agent needs to know
    // when it happened, not when it asked.
    assert!(hits["hits"][0]["at"].as_i64().unwrap() > 0);
    assert!(hits["hits"][0]["stream_offset"].as_i64().unwrap() > 0);
}

#[test]
fn every_firing_is_kept_not_just_the_first() {
    // This is the difference from follow(): three resets while away are three
    // findings, not one.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "resets", "until": {"reset": true}}),
    );

    append(
        &rig,
        &device,
        "loop.log",
        &format!("{}{}{}", boot_text(2), boot_text(3), boot_text(4)),
    );

    let hits = rig.call("poll_watch", json!({"device": device, "name": "resets"}));
    assert!(
        hits["returned"].as_i64().unwrap() >= 3,
        "three resets happened: {}",
        serde_json::to_string_pretty(&hits).unwrap()
    );
}

#[test]
fn a_poll_is_consuming_so_nothing_is_delivered_twice() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "panics", "until": {"pattern": "Kernel panic"}}),
    );
    append(
        &rig,
        &device,
        "p.log",
        "[    9.1] Kernel panic - not syncing\n",
    );

    let first = rig.call("poll_watch", json!({"device": device, "name": "panics"}));
    assert_eq!(first["returned"], 1);
    assert_eq!(first["remaining"], 0);

    let second = rig.call("poll_watch", json!({"device": device, "name": "panics"}));
    assert_eq!(
        second["returned"], 0,
        "a delivered firing must not come back and be re-investigated"
    );
}

#[test]
fn peek_reads_without_consuming() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "panics", "until": {"pattern": "Kernel panic"}}),
    );
    append(
        &rig,
        &device,
        "p.log",
        "[    9.1] Kernel panic - not syncing\n",
    );

    let peeked = rig.call(
        "poll_watch",
        json!({"device": device, "name": "panics", "peek": true}),
    );
    assert_eq!(peeked["returned"], 1);
    let again = rig.call("poll_watch", json!({"device": device, "name": "panics"}));
    assert_eq!(again["returned"], 1, "peek left the hit in place");
}

#[test]
fn a_watch_created_from_start_replays_what_already_happened() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "history.log",
        &format!("{}[    9.1] Kernel panic - not syncing\n", boot_text(1)),
        None,
    );

    // The default is "from now", so a watch created after the fact sees nothing.
    rig.call(
        "create_watch",
        json!({"device": device, "name": "late", "until": {"pattern": "Kernel panic"}}),
    );
    assert_eq!(
        rig.call("poll_watch", json!({"device": device, "name": "late"}))["returned"],
        0
    );

    rig.call(
        "create_watch",
        json!({"device": device, "name": "replay", "until": {"pattern": "Kernel panic"},
               "from": "start"}),
    );
    let replayed = rig.call("poll_watch", json!({"device": device, "name": "replay"}));
    assert_eq!(
        replayed["returned"], 1,
        "the whole stored stream is fair game"
    );
}

#[test]
fn an_any_predicate_comes_back_as_a_timeline() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "either",
               "until": {"any": [{"pattern": "Kernel panic"}, {"reset": true}]}}),
    );
    append(
        &rig,
        &device,
        "mixed.log",
        &format!("[    9.1] Kernel panic - not syncing\n{}", boot_text(2)),
    );

    let hits = rig.call("poll_watch", json!({"device": device, "name": "either"}));
    let offsets: Vec<i64> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["stream_offset"].as_i64().unwrap())
        .collect();
    assert!(offsets.len() >= 2, "both predicates fired: {hits}");
    let mut sorted = offsets.clone();
    sorted.sort();
    assert_eq!(
        offsets, sorted,
        "grouped by predicate the sequence is lost, and the sequence is the story"
    );
    // Each hit says which predicate it was, so waking up does not require
    // re-deriving why.
    assert!(hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| !h["matched"].as_str().unwrap_or("").is_empty()));
}

#[test]
fn a_quiet_predicate_fires_on_a_gap_in_the_stored_stream() {
    // Silence is an event, and because the timestamps are durable it is
    // recoverable after the fact rather than only observable live.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "settle", "until": {"quiet": 1},
               "from": "start"}),
    );
    let hits = rig.call("poll_watch", json!({"device": device, "name": "settle"}));
    assert!(
        hits["returned"].as_i64().unwrap() > 0,
        "the deterministic clock advances between lines, so gaps exist: {hits}"
    );
    assert!(hits["hits"][0]["evidence"]["idle_ms"].as_i64().is_some());
}

// ------------------------------------------------------------- management ----

#[test]
fn watches_are_listed_with_what_is_waiting() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "panics", "until": {"pattern": "Kernel panic"}}),
    );
    append(
        &rig,
        &device,
        "p.log",
        "[    9.1] Kernel panic - not syncing\n",
    );
    // Scanning happens on poll, so pending is zero until something looks.
    rig.call(
        "poll_watch",
        json!({"device": device, "name": "panics", "peek": true}),
    );

    let list = rig.call("list_watches", json!({"device": device}));
    assert_eq!(list["count"], 1);
    assert_eq!(list["watches"][0]["watch"]["name"], "panics");
    assert_eq!(list["watches"][0]["pending"], 1);
}

#[test]
fn deleting_a_watch_removes_its_undelivered_hits() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "panics", "until": {"pattern": "Kernel panic"}}),
    );
    append(
        &rig,
        &device,
        "p.log",
        "[    9.1] Kernel panic - not syncing\n",
    );
    rig.call(
        "poll_watch",
        json!({"device": device, "name": "panics", "peek": true}),
    );

    assert_eq!(
        rig.call("delete_watch", json!({"device": device, "name": "panics"}))["deleted"],
        true
    );
    assert_eq!(
        rig.call("list_watches", json!({"device": device}))["count"],
        0
    );
    let e = rig.err("poll_watch", json!({"device": device, "name": "panics"}));
    assert_eq!(e["code"], "UNKNOWN_WATCH");
}

#[test]
fn re_creating_a_name_replaces_the_watch() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    rig.call(
        "create_watch",
        json!({"device": device, "name": "w", "until": {"pattern": "one"}}),
    );
    rig.call(
        "create_watch",
        json!({"device": device, "name": "w", "until": {"pattern": "two"}}),
    );
    let list = rig.call("list_watches", json!({"device": device}));
    assert_eq!(list["count"], 1);
    assert_eq!(list["watches"][0]["watch"]["predicate"]["pattern"], "two");
}

#[test]
fn a_malformed_predicate_is_rejected_when_the_watch_is_created() {
    // Not when it is polled: a watch that silently never matches because its
    // regex was invalid is the worst possible failure mode for an overnight run.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);

    let bad_regex = rig.err(
        "create_watch",
        json!({"device": device, "name": "w", "until": {"pattern": "["}}),
    );
    assert_eq!(bad_regex["code"], "INVALID_ARGUMENT");

    let nonsense = rig.err(
        "create_watch",
        json!({"device": device, "name": "w", "until": {"whenever": true}}),
    );
    assert_eq!(nonsense["code"], "INVALID_ARGUMENT");
    assert_eq!(
        rig.call("list_watches", json!({"device": device}))["count"],
        0
    );
}

#[test]
fn polling_a_watch_that_was_never_created_is_a_structured_error() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("first.log", &boot_text(1), None);
    let e = rig.err("poll_watch", json!({"device": device, "name": "nope"}));
    assert_eq!(e["code"], "UNKNOWN_WATCH");
    assert!(e["hint"].as_str().unwrap().contains("list_watches"));
}
