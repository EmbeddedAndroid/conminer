//! Suite `verdicts` — persistent triage.
//!
//! The property under test is that an agent's judgement *survives the session*.
//! Everything else here follows from it: the table of contents gets quieter as a
//! device is understood, the regression gate stops needing its allowlist handed
//! to it on every call, and nothing is ever hidden without saying so.
//!
//! Edge cases: a verdict on an unknown template is an error, not a silent write ·
//! re-annotating replaces rather than duplicates · clearing restores visibility ·
//! `include_benign` overrides the default hide · an explicit `verdict` filter
//! implies you want to see them · hidden rows are counted in the response ·
//! `evaluate_policy` waives from the store and says how many it waived ·
//! verdicts survive reopening the database.

use conminer_testkit::corpus::corpus_text;
use conminer_testkit::McpRig;
use serde_json::json;

/// A device with a real table of contents, plus the id of its noisiest template.
fn device_with_toc(rig: &McpRig) -> (String, i64) {
    let (device, session) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);
    let toc = rig.call(
        "list_templates",
        json!({"device": device, "session": session, "order": "count"}),
    );
    let id = toc["templates"][0]["id"].as_i64().expect("a template");
    (device, id)
}

// ------------------------------------------------------------- the basics ----

#[test]
fn a_verdict_is_recorded_and_read_back_with_its_note() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);

    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "known_bad",
               "note": "the RPMh timeout, see the AMC-completion IRQ", "ticket": "LAB-42"}),
    );

    let list = rig.call("list_verdicts", json!({"device": device}));
    assert_eq!(list["count"], 1);
    let v = &list["verdicts"][0];
    assert_eq!(v["verdict"]["template_id"], id);
    assert_eq!(v["verdict"]["verdict"], "known_bad");
    assert_eq!(v["verdict"]["ticket"], "LAB-42");
    assert!(
        v["verdict"]["note"]
            .as_str()
            .unwrap()
            .contains("AMC-completion"),
        "the note is the whole point: it is what a later session reads"
    );
    assert!(
        !v["text"].as_str().unwrap().is_empty(),
        "a verdict list without the template text would need one call per row to use"
    );
}

#[test]
fn annotating_the_same_template_twice_replaces_rather_than_duplicates() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);

    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "investigating"}),
    );
    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign", "note": "it is noise"}),
    );

    let list = rig.call("list_verdicts", json!({"device": device}));
    assert_eq!(list["count"], 1, "a template has one standing verdict");
    assert_eq!(list["verdicts"][0]["verdict"]["verdict"], "benign");
}

#[test]
fn a_verdict_on_a_template_that_does_not_exist_is_a_structured_error() {
    let rig = McpRig::new();
    let (device, _) = device_with_toc(&rig);
    let e = rig.err(
        "annotate_template",
        json!({"device": device, "template_id": 999_999, "verdict": "benign"}),
    );
    assert_eq!(e["code"], "UNKNOWN_TEMPLATE");
}

#[test]
fn an_unknown_verdict_name_is_rejected_with_the_valid_set() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    let e = rig.err(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "probably_fine"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    assert!(e["message"].as_str().unwrap().contains("known_bad"));
}

// ------------------------------------------- what the table of contents shows -

#[test]
fn a_benign_verdict_removes_the_row_and_the_response_says_how_many() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    let before = rig.call("list_templates", json!({"device": device}));
    let n = before["templates"].as_array().unwrap().len();
    assert_eq!(before["hidden_by_verdict"], 0);

    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign"}),
    );

    let after = rig.call("list_templates", json!({"device": device}));
    assert_eq!(after["templates"].as_array().unwrap().len(), n - 1);
    assert!(after["templates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["id"] != id));
    // Silently shorter would be indistinguishable from a quiet console.
    assert_eq!(
        after["hidden_by_verdict"], 1,
        "hiding must be reported, never implicit"
    );
}

#[test]
fn include_benign_brings_the_row_back() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign"}),
    );

    let shown = rig.call(
        "list_templates",
        json!({"device": device, "include_benign": true}),
    );
    let row = shown["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .expect("the benign row is present when asked for");
    assert_eq!(row["verdict"], "benign");
    assert_eq!(shown["hidden_by_verdict"], 0);
}

#[test]
fn asking_for_benign_templates_by_name_does_not_then_hide_them() {
    // The default hide exists to reduce noise, not to make a filter useless.
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign"}),
    );

    let only = rig.call(
        "list_templates",
        json!({"device": device, "verdict": ["benign"]}),
    );
    assert_eq!(only["templates"].as_array().unwrap().len(), 1);
    assert_eq!(only["templates"][0]["id"], id);
}

#[test]
fn clearing_a_verdict_restores_the_row() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign"}),
    );
    let cleared = rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": null}),
    );
    assert_eq!(cleared["cleared"], true);

    let after = rig.call("list_templates", json!({"device": device}));
    assert_eq!(after["hidden_by_verdict"], 0);
    assert!(after["templates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == id));
    assert_eq!(
        rig.call("list_verdicts", json!({"device": device}))["count"],
        0
    );
}

#[test]
fn a_non_benign_verdict_is_carried_on_the_row_without_hiding_it() {
    let rig = McpRig::new();
    let (device, id) = device_with_toc(&rig);
    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "known_bad",
               "note": "known: the UFS -110"}),
    );

    let toc = rig.call("list_templates", json!({"device": device}));
    let row = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .expect("known_bad stays visible");
    assert_eq!(row["verdict"], "known_bad");
    assert_eq!(row["note"], "known: the UFS -110");
    assert_eq!(toc["hidden_by_verdict"], 0);
}

// -------------------------------------------------------- the gate uses them -

#[test]
fn evaluate_policy_waives_from_the_store_instead_of_from_its_arguments() {
    let rig = McpRig::new();
    let (device, session) = rig.ingest("oops.log", &corpus_text("linux/boot-oops.log"), None);

    let failing = rig.call(
        "evaluate_policy",
        json!({"device": device, "session": session, "fail_at_or_above": "err"}),
    );
    assert_eq!(
        failing["verdict"], "fail",
        "the corpus contains a real oops"
    );
    let offenders: Vec<i64> = failing["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["template_id"].as_i64().unwrap())
        .collect();
    assert!(!offenders.is_empty());

    for id in &offenders {
        rig.call(
            "annotate_template",
            json!({"device": device, "template_id": id, "verdict": "benign",
                   "note": "expected on this bring-up board"}),
        );
    }

    let passing = rig.call(
        "evaluate_policy",
        json!({"device": device, "session": session, "fail_at_or_above": "err"}),
    );
    assert_eq!(
        passing["verdict"], "pass",
        "the allowlist now lives in the store, not in the agent's context"
    );
    assert_eq!(passing["waived_by_stored_verdict"], offenders.len());

    // And the escape hatch still works: a CI job that wants the unfiltered
    // truth can ask for it.
    let strict = rig.call(
        "evaluate_policy",
        json!({"device": device, "session": session, "fail_at_or_above": "err",
               "use_verdicts": false}),
    );
    assert_eq!(strict["verdict"], "fail");
    assert_eq!(strict["waived_by_stored_verdict"], 0);
}

// ------------------------------------------------------------- persistence ---

#[test]
fn verdicts_survive_the_process_that_wrote_them() {
    // The entire point is cross-session memory, so the durable path is asserted
    // rather than assumed: write, drop every handle, reopen the file.
    use conminer_core::store::{DeviceStore, TemplateQuery, Verdict};
    let rig = conminer_testkit::Rig::new();
    let store = rig.ingest_text("usb-x", None, &corpus_text("linux/boot-oops.log"));
    // `TemplateQuery::default()` has limit 0: the store never guesses how much
    // a caller wants.
    let template_id = store
        .list_templates(&TemplateQuery {
            limit: 1,
            ..Default::default()
        })
        .unwrap()[0]
        .id;
    drop(store);

    let dev = rig.device("usb-x");
    let path = rig.store_path(&dev);
    {
        let mut st = DeviceStore::open(&path, &dev.canonical, false).unwrap();
        st.set_verdict(
            template_id,
            Some(Verdict::Benign),
            Some("noise"),
            None,
            Some("agent-a"),
            2_000,
        )
        .unwrap();
    }

    let reopened = DeviceStore::open(&path, &dev.canonical, false).unwrap();
    let v = reopened
        .verdict(template_id)
        .unwrap()
        .expect("the verdict outlived the handle that wrote it");
    assert_eq!(v.verdict, Verdict::Benign);
    assert_eq!(v.note.as_deref(), Some("noise"));
    assert_eq!(v.author.as_deref(), Some("agent-a"));
}
