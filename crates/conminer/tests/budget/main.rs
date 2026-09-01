//! Suite `budget` — what a table of contents costs to read.
//!
//! The whole argument for this tool is that reading the deduplicated templates
//! is cheaper than reading the log. That is a claim about *bytes*, and it was
//! very nearly false: measured on a 109-template device the full template row
//! was ~510 bytes against ~55 bytes for the average console line it stood for,
//! so a 10x dedup win was handed straight back as per-row metadata. `tokens`
//! alone was 18% of the response and is a second copy of `text`.
//!
//! So the compact projection is not a convenience, it is the feature working.
//! These tests are the ratchet that keeps it working: a field added carelessly
//! to the default view will fail them.
//!
//! The rule they encode is "cheaper, not lossier": everything compact drops must
//! still be reachable, and anything hidden must be counted.

use conminer_testkit::McpRig;
use serde_json::{json, Value};

/// A device whose table of contents is worth measuring.
fn busy_device(rig: &McpRig) -> String {
    let mut text = String::new();
    for boot in 0..12 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        text.push_str("U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n");
        text.push_str("[    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n");
        // GENUINELY DISTINCT messages, not one message with an index.
        //
        // This used to be `subsystem{i}: probe deferred`, which relied on the
        // miner fragmenting one message into twelve templates. That fragmenting
        // is the bug #32 fixed: lines differing only in a value now meet and
        // disagree, yielding a single row. The property under test here is that
        // TRIAGE removes whole rows, so the fixture needs twelve rows that are
        // legitimately different things to triage.
        // Different SHAPES, not just different words: same-length lines sharing
        // most of their tokens legitimately merge now, so each of these differs
        // in structure as well as vocabulary.
        const SUBSYSTEMS: [&str; 12] = [
            "i2c: probe deferred",
            "spi bus: probe deferred, retrying",
            "usb host controller: probe deferred, waiting on phy",
            "mmc slot ready but regulator absent: probe deferred",
            "gpu: probe deferred pending smmu attach",
            "venus firmware not yet loaded, probe deferred here",
            "camss clocks unavailable: probe deferred",
            "display panel link down; probe deferred until retrain completes",
            "audio codec absent: probe deferred",
            "modem rproc not booted, probe deferred for now",
            "wifi pcie link training incomplete: probe deferred",
            "crypto rng seed unavailable so probe deferred at this point",
        ];
        for sub in SUBSYSTEMS {
            text.push_str(&format!("[    4.000000] {sub} ({boot})\n"));
        }
        text.push_str("[    9.000000] Kernel panic - not syncing: VFS: Unable to mount root fs\n");
    }
    rig.ingest("busy.log", &text, None).0
}

fn bytes(v: &Value) -> usize {
    serde_json::to_string(v).unwrap().len()
}

// ------------------------------------------------------------- the ratchet ---

#[test]
fn the_compact_table_of_contents_is_much_cheaper_than_the_full_one() {
    let rig = McpRig::new();
    let device = busy_device(&rig);

    let compact = rig.call("list_templates", json!({"device": device, "limit": 200}));
    let full = rig.call(
        "list_templates",
        json!({"device": device, "limit": 200, "view": "full"}),
    );
    assert_eq!(compact["view"], "compact", "compact is the default");
    assert_eq!(
        compact["returned"], full["returned"],
        "the same rows, projected differently — this is not a smaller page"
    );

    let (c, f) = (bytes(&compact), bytes(&full));
    assert!(
        c * 2 < f,
        "the compact view must be at least 2x cheaper or it is not worth having: \
         compact={c} full={f}"
    );
}

#[test]
fn the_table_of_contents_is_cheaper_than_the_log_it_replaces() {
    // The claim the whole tool rests on, asserted rather than assumed.
    let rig = McpRig::new();
    let mut text = String::new();
    for boot in 0..25 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        for i in 0..20 {
            text.push_str(&format!(
                "[    4.000000] subsystem{i}: probe deferred, retrying ({boot})\n"
            ));
        }
    }
    let (device, _) = rig.ingest("big.log", &text, None);

    let toc = bytes(&rig.call("list_templates", json!({"device": device, "limit": 1000})));
    assert!(
        toc < text.len(),
        "reading the table of contents must beat reading the log: toc={toc} log={}",
        text.len()
    );
}

#[test]
fn compact_rows_carry_everything_triage_branches_on() {
    let rig = McpRig::new();
    let device = busy_device(&rig);
    let toc = rig.call(
        "list_templates",
        json!({"device": device, "limit": 200, "order": "severity"}),
    );
    let row = &toc["templates"][0];

    // Cheaper must not mean unusable: these are the fields an agent decides
    // with, and dropping any of them would force a detail call per row, which
    // costs far more than it saves.
    for field in ["id", "text", "count", "severity"] {
        assert!(
            !row[field].is_null(),
            "compact row is missing {field}: {row}"
        );
    }
    // And these are the ones that were pure weight.
    for field in ["tokens", "first_seen_ts", "first_seen_session", "head_only"] {
        assert!(
            row[field].is_null(),
            "{field} does not belong in the default view: {row}"
        );
    }
}

#[test]
fn nothing_compact_drops_is_unreachable() {
    let rig = McpRig::new();
    let device = busy_device(&rig);
    let compact = rig.call("list_templates", json!({"device": device, "limit": 5}));
    let id = compact["templates"][0]["id"].as_i64().unwrap();

    let full_row = &rig.call(
        "list_templates",
        json!({"device": device, "limit": 200, "view": "full"}),
    )["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .cloned()
        .expect("the same template in the full view");
    assert!(!full_row["tokens"].is_null());

    // …and the drill-down carries it too, so an agent that never asks for the
    // full view loses nothing.
    let detail = rig.call(
        "template_detail",
        json!({"device": device, "template_id": id}),
    );
    assert!(!detail["template"]["tokens"].is_null());
    assert_eq!(detail["template"]["id"], id);
}

#[test]
fn the_boot_list_is_compact_by_default_too() {
    // A looping board is exactly where the most epochs get read, so this is the
    // list that most needs to be cheap.
    let rig = McpRig::new();
    let device = busy_device(&rig);
    let compact = rig.call("list_boots", json!({"device": device, "limit": 50}));
    let full = rig.call(
        "list_boots",
        json!({"device": device, "limit": 50, "view": "full"}),
    );
    assert!(bytes(&compact) < bytes(&full));

    let row = &compact["boots"][0];
    // Enough to answer "is this the same boot again?", which is the question.
    for field in ["id", "seq", "fingerprint"] {
        assert!(
            !row[field].is_null(),
            "compact boot row needs {field}: {row}"
        );
    }
    assert!(row["opened_offset"].is_null());
}

// ------------------------------------------------ cheaper is not lossier -----

#[test]
fn triage_compounds_with_the_projection_and_is_still_counted() {
    // Annotating removes whole rows, which is the largest saving available and
    // the one that grows as a device becomes understood. It must still be
    // impossible to confuse a well-triaged console with a quiet one.
    let rig = McpRig::new();
    let device = busy_device(&rig);
    let before = rig.call("list_templates", json!({"device": device, "limit": 200}));
    let noisy: Vec<i64> = before["templates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["text"].as_str().unwrap_or("").contains("probe deferred"))
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert!(noisy.len() > 5, "the corpus has plenty of noise to triage");

    for id in &noisy {
        rig.call(
            "annotate_template",
            json!({"device": device, "template_id": id, "verdict": "benign",
                   "note": "known deferred-probe churn on this board"}),
        );
    }

    let after = rig.call("list_templates", json!({"device": device, "limit": 200}));
    assert!(
        bytes(&after) < bytes(&before),
        "triage has to pay off in bytes or it is only bookkeeping"
    );
    assert_eq!(
        after["hidden_by_verdict"],
        noisy.len(),
        "the response must say exactly how much it is not showing"
    );
    // The count of what matched is still honest about the whole device.
    assert!(after["total_templates"].as_i64().unwrap() >= noisy.len() as i64);
}

#[test]
fn a_capped_response_says_so_and_offers_the_next_page() {
    let rig = McpRig::new();
    let device = busy_device(&rig);
    let page = rig.call("list_templates", json!({"device": device, "limit": 3}));
    assert_eq!(page["templates"].as_array().unwrap().len(), 3);
    assert_eq!(page["capped"], true);
    assert_eq!(page["next_offset"], 3);
    assert!(
        page["matching"].as_i64().unwrap() > 3,
        "an agent must be able to tell a page from the whole answer"
    );
}
