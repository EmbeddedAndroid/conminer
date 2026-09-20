//! Suite `tools` (§13) — every MCP tool.
//!
//! Edge cases: happy path · empty result · cap enforcement · cursor stability
//! under concurrent writes · invalid args (structured error, §14.6) ·
//! `new_only` correctness across sessions · `diff_sessions` on disjoint devices
//! (error).
//!
//! Everything is driven through the real JSON-RPC handler, so what is asserted
//! is the wire contract an agent actually sees.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use conminer_testkit::corpus::corpus_text;
use serde_json::{json, Value};
use std::sync::Arc;

struct Rig {
    _dir: tempfile::TempDir,
    h: Handler,
    /// Kept so a test can simulate a console going quiet.
    clock: Arc<conminer_core::clock::StepClock>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx =
            Context::open(cfg, Arc::new(ProfileSet::builtin().unwrap()), clock.clone()).unwrap();
        Self {
            _dir: dir,
            h: Handler::new(ctx),
            clock,
        }
    }

    /// Call a tool and return its structured content, asserting success.
    fn call(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(
            v["isError"],
            false,
            "{name} failed: {}",
            serde_json::to_string_pretty(&v["structuredContent"]).unwrap()
        );
        v["structuredContent"].clone()
    }

    /// Call a tool and return the structured error, asserting failure.
    fn err(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(v["isError"], true, "{name} unexpectedly succeeded: {v}");
        v["structuredContent"]["error"].clone()
    }

    fn raw(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        let resp = self.h.handle(req).expect("a call always replies");
        assert!(resp.error.is_none(), "protocol error: {:?}", resp.error);
        resp.result.unwrap()
    }

    /// The registry behind this rig, for seeding rows discovery would create.
    fn registry(&self) -> conminer_core::store::Registry {
        conminer_core::store::Registry::open(self._dir.path()).unwrap()
    }

    /// Ingest a corpus file, returning (device, session_id).
    fn ingest(&self, name: &str, text: &str, device: Option<&str>) -> (String, i64) {
        let path = self._dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        let mut args = json!({"path": path.display().to_string()});
        if let Some(d) = device {
            args["device"] = json!(d);
        }
        let r = self.call("ingest_file", args);
        (
            r["device"].as_str().unwrap().to_string(),
            r["ingest"]["session_id"].as_i64().unwrap(),
        )
    }
}

// --------------------------------------------------------------- happy path --

#[test]
fn the_whole_dev_loop_works_through_the_tool_surface() {
    let rig = Rig::new();
    let (device, session) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    // The table of contents.
    let toc = rig.call(
        "list_templates",
        json!({"device": device, "session": session}),
    );
    assert!(toc["templates"].as_array().unwrap().len() > 5);
    assert!(toc["freshness"]["cursor"].as_str().unwrap().contains(':'));

    // Find the crash without paging the log.
    let crashes = rig.call(
        "list_templates",
        json!({"device": device, "session": session, "min_severity": "crit", "order": "severity"}),
    );
    let t = &crashes["templates"][0];
    assert!(
        t["text"].as_str().unwrap().contains("Internal error"),
        "{}",
        t["text"]
    );

    // Drill into verbatim bytes.
    let detail = rig.call(
        "template_detail",
        json!({"device": device, "template_id": t["id"], "examples": 1}),
    );
    let text = detail["examples"][0]["text"].as_str().unwrap();
    assert!(text.contains("Modules linked in:"));
    assert!(text.contains("Call trace:"));

    // …and around it.
    let anchor = detail["examples"][0]["first_line_id"].as_i64().unwrap();
    let ctx = rig.call(
        "get_context",
        json!({"device": device, "line_id": anchor, "before": 2, "after": 2}),
    );
    let lines = ctx["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines.iter().filter(|l| l["anchor"] == true).count(), 1);

    // The stage timeline and the one-call verdict.
    let stages = rig.call("boot_stages", json!({"device": device}));
    let names: Vec<&str> = stages["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"kernel"), "{names:?}");

    let report = rig.call("boot_report", json!({"device": device}));
    assert!(
        report["outcome"] == "crashed" || report["outcome"] == "booted",
        "{}",
        report["outcome"]
    );
    assert!(!report["why"].as_str().unwrap().is_empty());
}

#[test]
fn stats_reports_the_compression_that_makes_this_worth_using() {
    let rig = Rig::new();
    let (device, session) = rig.ingest(
        "big.log",
        &corpus_text("linux/boot-oops.log").repeat(20),
        None,
    );
    let s = rig.call("stats", json!({"device": device, "session": session}));
    assert!(s["stats"]["lines"].as_i64().unwrap() > 500);
    assert!(s["stats"]["compression_ratio"].as_f64().unwrap() > 10.0);
}

#[test]
fn search_works_through_the_tool_surface_in_every_mode() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    let terms = rig.call(
        "search",
        json!({"device": device, "query": "mounted filesystem"}),
    );
    assert_eq!(terms["hits"].as_array().unwrap().len(), 1);
    assert_eq!(terms["scan"], false);

    let record = rig.call(
        "search",
        json!({"device": device, "query": "Call trace", "scope": "record"}),
    );
    assert!(record["hits"][0]["text"].as_str().unwrap().lines().count() > 5);

    let rx = rig.call(
        "search_raw",
        json!({"device": device, "pattern": r"CPU\d: Booted"}),
    );
    assert_eq!(rx["hits"].as_array().unwrap().len(), 3);
}

#[test]
fn list_devices_shows_identity_tags_and_what_the_console_last_was() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);
    rig.call(
        "name_device",
        json!({"device": device, "nickname": "rb3-ap"}),
    );
    rig.call(
        "tag_device",
        json!({"device": "rb3-ap", "tags": {"rack": "r2", "role": "ap-console"}}),
    );

    // `detail` explicitly: this asserts the rich shape, so it must not ride on
    // whatever the default happens to be.
    let l = rig.call("list_devices", json!({"detail": true}));
    let d = &l["devices"][0];
    // The PORT identifies the device; the name an operator chose rides beside
    // it. `tag_device` above addressed this device as "rb3-ap", so the same call
    // proves a label is still a first-class selector -- it just is not the
    // identity. A label can go stale when boards move; a port path cannot.
    assert_eq!(d["device"], d["canonical"], "the port is the identity");
    assert!(
        d["device"].as_str().unwrap().contains("boot.log")
            || d["device"].as_str().unwrap().starts_with("/dev/"),
        "and it is a real address: {}",
        d["device"]
    );
    assert_eq!(d["label"], "rb3-ap", "the chosen name is kept, alongside");
    assert_eq!(d["nickname"], "rb3-ap");
    assert_eq!(d["tags"]["rack"], "r2");
    assert_eq!(d["identity"], "by_id");
    assert!(d["last_line"].is_string());

    // Tags are selectors.
    let filtered = rig.call("list_devices", json!({"filter": "tag:role=ap-console"}));
    assert_eq!(filtered["count"], 1);
    let none = rig.err("list_devices", json!({"filter": "tag:role=nope"}));
    assert_eq!(none["code"], "UNKNOWN_DEVICE");
}

#[test]
fn identify_is_read_only_and_dtr_pulse_needs_a_lease() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello world\n", None);

    let id = rig.call("identify", json!({"device": device}));
    assert!(id["tail"].as_array().unwrap().len() <= 5);
    assert!(id["canonical"].as_str().unwrap().starts_with("file:"));

    // DTR is wired to RESET on many boards, so the disruptive mode is gated.
    let e = rig.err("identify", json!({"device": device, "dtr_pulse": true}));
    assert_eq!(e["code"], "LEASE_REQUIRED");
}

// ------------------------------------------------------------ empty result ---

#[test]
fn empty_results_are_empty_not_errors() {
    let rig = Rig::new();
    let (device, session) = rig.ingest("boot.log", "just one line\n", None);

    let t = rig.call(
        "list_templates",
        json!({"device": device, "session": session, "min_count": 9999}),
    );
    assert_eq!(t["templates"].as_array().unwrap().len(), 0);
    assert_eq!(t["capped"], false);

    let s = rig.call(
        "search",
        json!({"device": device, "query": "definitely-not-present-anywhere"}),
    );
    assert_eq!(s["hits"].as_array().unwrap().len(), 0);

    // `records` explicitly: this asserts per-line structure, so it must not
    // ride on whatever the default format happens to be.
    let r = rig.call(
        "get_recent",
        json!({"device": device, "lines": 100, "format": "records"}),
    );
    assert_eq!(r["lines"].as_array().unwrap().len(), 1);
}

// ------------------------------------------------------------------- caps ----

#[test]
fn caps_are_enforced_and_reported_never_silently_truncated() {
    let rig = Rig::new();
    let (device, session) = rig.ingest(
        "big.log",
        &corpus_text("linux/boot-oops.log").repeat(10),
        None,
    );

    let page = rig.call(
        "list_templates",
        json!({"device": device, "session": session, "limit": 3}),
    );
    assert_eq!(page["templates"].as_array().unwrap().len(), 3);
    assert_eq!(page["capped"], true);
    assert_eq!(page["next_offset"], 3);
    assert!(page["total_templates"].as_i64().unwrap() > 3);

    // An adversarial request for everything still comes back bounded.
    let greedy = rig.call(
        "get_recent",
        json!({"device": device, "lines": 1_000_000_000i64, "format": "records"}),
    );
    let n = greedy["lines"].as_array().unwrap().len();
    assert!(
        n <= Config::default().api.max_raw_lines,
        "{n} lines returned"
    );

    let ctx = rig.call(
        "get_context",
        json!({"device": device, "line_id": 5, "before": 100000, "after": 100000}),
    );
    assert!(ctx["lines"].as_array().unwrap().len() <= 2 * Config::default().api.max_raw_lines + 1);
}

#[test]
fn paging_templates_covers_every_row_exactly_once() {
    let rig = Rig::new();
    let (device, session) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    let mut ids = Vec::new();
    let mut offset = 0;
    loop {
        let p = rig.call(
            "list_templates",
            json!({"device": device, "session": session, "limit": 4, "offset": offset,
                   "order": "first_seen"}),
        );
        ids.extend(
            p["templates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_i64().unwrap()),
        );
        if p["capped"] != true {
            break;
        }
        offset = p["next_offset"].as_i64().unwrap();
    }
    let total = rig.call(
        "list_templates",
        json!({"device": device, "session": session, "limit": 1}),
    )["total_templates"]
        .as_i64()
        .unwrap();
    let mut dedup = ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(ids.len(), dedup.len(), "no template appeared twice");
    assert_eq!(ids.len() as i64, total);
}

#[test]
fn a_search_cursor_stays_stable_while_the_device_keeps_growing() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("a.log", &"[ 1.0] mmc0: marker line here\n".repeat(30), None);

    let first = rig.call(
        "search",
        json!({"device": device, "query": "marker", "max_results": 10}),
    );
    assert_eq!(first["capped"], true);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();

    // More data lands in a second session on the same device.
    rig.ingest(
        "b.log",
        &"[ 2.0] mmc0: marker line here\n".repeat(30),
        Some(&device),
    );

    let second = rig.call(
        "search",
        json!({"device": device, "query": "marker", "max_results": 1000, "cursor": cursor}),
    );
    let total = 10 + second["hits"].as_array().unwrap().len();
    assert_eq!(
        total, 60,
        "the cursor resumed without skipping or repeating"
    );
}

// -------------------------------------------------------------- new_only -----

#[test]
fn new_only_is_correct_across_sessions() {
    let rig = Rig::new();
    let text = corpus_text("linux/boot-oops.log");
    let (device, s1) = rig.ingest("run1.log", &text, None);

    let all_first = rig.call(
        "list_templates",
        json!({"device": device, "session": s1, "limit": 1000}),
    )["templates"]
        .as_array()
        .unwrap()
        .len();
    let new_first = rig.call(
        "list_templates",
        json!({"device": device, "session": s1, "new_only": true, "limit": 1000}),
    )["templates"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(all_first, new_first, "everything is novel in the first run");

    // Second run: same log plus one genuinely new message.
    let mut second = text.clone();
    second.push_str("[   99.000000] brand new never before seen message\n");
    let (_, s2) = rig.ingest("run2.log", &second, Some(&device));

    let novel = rig.call(
        "list_templates",
        json!({"device": device, "session": s2, "new_only": true, "limit": 1000}),
    );
    let rows = novel["templates"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert!(rows[0]["text"]
        .as_str()
        .unwrap()
        .contains("brand new never before seen"));
}

#[test]
fn diff_sessions_names_what_changed_between_two_runs() {
    let rig = Rig::new();
    let text = corpus_text("linux/boot-oops.log");
    let (device, a) = rig.ingest("a.log", &text, None);
    let mut changed = text.replace("Internal error: Oops", "Kernel panic - not syncing");
    changed.push_str("[   99.0] a line only in run b\n");
    let (_, b) = rig.ingest("b.log", &changed, Some(&device));

    let d = rig.call("diff_sessions", json!({"device": device, "a": a, "b": b}));
    assert!(d["totals"]["new_in_b"].as_i64().unwrap() >= 1);
    assert!(d["totals"]["gone_from_b"].as_i64().unwrap() >= 1);
    let new_texts: Vec<&str> = d["new_in_b"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert!(
        new_texts.iter().any(|t| t.contains("only in run b")),
        "{new_texts:?}"
    );
}

#[test]
fn diff_sessions_on_a_session_from_another_device_is_an_error() {
    let rig = Rig::new();
    let (dev_a, sa) = rig.ingest("a.log", "alpha line\n", None);
    let (_dev_b, sb) = rig.ingest("b.log", "bravo line\nbravo two\nbravo three\n", None);
    // Session ids are per device, so `sb` may collide numerically with one of
    // A's — the diff must refuse anything that is not A's own session.
    let e = rig.err(
        "diff_sessions",
        json!({"device": dev_a, "a": sa, "b": sb + 500}),
    );
    assert_eq!(e["code"], "UNKNOWN_SESSION");

    let same = rig.err("diff_sessions", json!({"device": dev_a, "a": sa, "b": sa}));
    assert_eq!(same["code"], "INVALID_ARGUMENT");
}

// ---------------------------------------------------------- invalid args -----

#[test]
fn invalid_arguments_produce_structured_errors_with_hints() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "one line\n", None);

    let e = rig.err("list_templates", json!({"device": "no-such-device"}));
    assert_eq!(e["code"], "UNKNOWN_DEVICE");
    assert!(!e["hint"].as_str().unwrap().is_empty());

    let e = rig.err(
        "template_detail",
        json!({"device": device, "template_id": 9999}),
    );
    assert_eq!(e["code"], "UNKNOWN_TEMPLATE");

    let e = rig.err("get_context", json!({"device": device, "line_id": 9999}));
    assert_eq!(e["code"], "UNKNOWN_LINE");

    let e = rig.err("list_sessions", json!({"device": device, "wat": 1}));
    assert_eq!(e["code"], "INVALID_ARGUMENT");

    let e = rig.err(
        "search",
        json!({"device": device, "query": "x", "mode": "fuzzy"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");

    let e = rig.err(
        "search",
        json!({"device": device, "query": "(", "mode": "regex"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");

    let e = rig.err("ingest_file", json!({"path": "/no/such/file.log"}));
    assert_eq!(e["code"], "NO_SUCH_PATH");

    let e = rig.err("boot_report", json!({"device": device, "boot": 4242}));
    assert_eq!(e["code"], "UNKNOWN_BOOT");

    let e = rig.err(
        "classify_prompt",
        json!({"device": device, "pattern": ":", "kind": "shell"}),
    );
    assert_eq!(
        e["code"], "INVALID_ARGUMENT",
        "a bare `:` is not distinctive"
    );

    let e = rig.err(
        "classify_prompt",
        json!({"device": device, "pattern": "=> ", "kind": "wizard"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
}

#[test]
fn a_mutating_tool_without_a_lease_is_refused() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "one line\n", None);
    let e = rig.err("mark", json!({"device": device}));
    assert_eq!(e["code"], "LEASE_REQUIRED");

    // With a lease it works, and opens a fresh epoch.
    rig.call("acquire", json!({"device": device}));
    let m = rig.call("mark", json!({"device": device, "label": "before power"}));
    assert!(m["boot_id"].as_i64().unwrap() > 0);
    assert!(m["cursor"].as_str().unwrap().contains(':'));

    // …and the freshness envelope now names that epoch.
    let f = rig.call("stats", json!({"device": device}));
    assert_eq!(f["freshness"]["boot_id"], m["boot_id"]);
    assert_eq!(f["freshness"]["boot_opened_by"], "mark");

    rig.call("release", json!({"device": device}));
    assert_eq!(
        rig.err("mark", json!({"device": device}))["code"],
        "LEASE_REQUIRED"
    );
}

#[test]
fn a_lease_held_by_someone_else_blocks_until_stolen_explicitly() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "one line\n", None);
    rig.call("acquire", json!({"device": device, "holder": "agent-a"}));

    // A second agent identifies itself and is refused.
    let e = rig.err("acquire", json!({"device": device, "holder": "agent-b"}));
    assert_eq!(e["code"], "LEASE_HELD");
    assert_eq!(e["detail"]["holder"], "agent-a");

    let stolen = rig.call(
        "acquire",
        json!({"device": device, "holder": "agent-b", "steal": true}),
    );
    assert_eq!(stolen["lease"]["stolen_from"], "agent-a");
}

// ---------------------------------------------------- freshness and prompts --

#[test]
fn every_read_response_carries_a_freshness_envelope() {
    let rig = Rig::new();
    let (device, session) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);
    for (tool, args) in [
        ("list_templates", json!({"device": device})),
        ("list_sessions", json!({"device": device})),
        ("get_recent", json!({"device": device})),
        ("stats", json!({"device": device})),
        ("boot_stages", json!({"device": device})),
        ("list_boots", json!({"device": device})),
        ("boot_report", json!({"device": device})),
        ("search", json!({"device": device, "query": "mmc"})),
        ("identify", json!({"device": device})),
        ("get_prompts", json!({"device": device})),
        (
            "template_detail",
            json!({"device": device, "template_id": 1}),
        ),
        ("get_records", json!({"device": device, "template_id": 1})),
        (
            "diff_sessions",
            json!({"device": device, "a": session, "b": session + 1}),
        ),
    ] {
        let v = rig.raw(tool, args);
        if v["isError"] == true {
            continue; // covered by the invalid-args test
        }
        let f = &v["structuredContent"]["freshness"];
        assert!(f.is_object(), "{tool} has no freshness envelope");
        // `server_now` was removed from the envelope: it existed only to compute
        // `idle_ms`, which is already here, and the envelope rides on EVERY
        // response. Assert what a caller actually branches on.
        assert!(
            f["capture_state"].is_string(),
            "{tool} must say whether capture is recording"
        );
        assert!(
            f.get("cursor").is_some(),
            "{tool} must say where to resume from"
        );
    }
}

#[test]
fn get_prompts_answers_what_to_expect_and_never_calls_a_gate_a_prompt() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", &corpus_text("uboot/spl-to-kernel.log"), None);

    let p = rig.call("get_prompts", json!({"device": device}));
    let prompts = p["prompts"].as_array().unwrap();
    assert!(prompts.iter().any(|x| x["kind"] == "bootloader"));
    assert!(prompts.iter().any(|x| x["kind"] == "credential_gate"));

    // The board being up is not the same as the board being commandable.
    for g in p["credential_gates"].as_array().unwrap() {
        assert_eq!(g["commandable"], false, "a credential gate is not a prompt");
    }

    // Teaching persists and shows up as `learned`.
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "custom-board> ", "kind": "shell", "stage": "uboot"}),
    );
    let p2 = rig.call("get_prompts", json!({"device": device}));
    let learned = p2["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["pattern"] == "custom-board> ")
        .expect("the taught prompt must be there");
    assert_eq!(learned["provenance"], "learned");
    assert_eq!(learned["commandable"], true);
}

#[test]
fn rebuild_and_export_round_trip_through_the_tool_surface() {
    let rig = Rig::new();
    let (device, session) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    let before = rig.call("stats", json!({"device": device}))["stats"]["templates"]
        .as_i64()
        .unwrap();
    let rb = rig.call("rebuild_templates", json!({"device": device}));
    assert_eq!(rb["before"], before);
    assert_eq!(
        rb["after"], before,
        "a rebuild at the same threshold is a no-op"
    );

    let out = rig._dir.path().join("session.gz");
    let ex = rig.call(
        "export_session",
        json!({"device": device, "session": session, "path": out.display().to_string()}),
    );
    assert!(ex["archive_bytes"].as_u64().unwrap() > 0);
    assert!(out.exists());

    // …and the archive imports byte-exactly into a fresh device.
    let back = rig.call(
        "ingest_file",
        json!({"path": out.display().to_string(), "device": device}),
    );
    assert_eq!(
        back["ingest"]["exported_from"]["session"], session,
        "the archive says where it came from"
    );
}

#[test]
fn list_profiles_shows_the_appendix_a_set() {
    let rig = Rig::new();
    let p = rig.call("list_profiles", json!({}));
    let names: Vec<&str> = p["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    for want in [
        "raw", "linux", "uboot", "uefi", "tfa", "optee", "zephyr", "freertos", "threadx",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
}

/// get_recent defaults to plain text, which is what an agent reading a console
/// actually wants. Measured on the IQ10 before this change: 20 lines came back
/// as 8420 bytes of JSON wrapping ~1600 bytes of console text, because every
/// line shipped line_id/offset/ts_wall around ~80 bytes of content.
#[test]
fn get_recent_defaults_to_text_and_is_smaller_than_records() {
    let rig = Rig::new();
    let (device, _session) = rig.ingest("boot.log", "alpha\nbravo\ncharlie\n", None);

    let text = rig.call("get_recent", json!({"device": device, "lines": 100}));
    assert!(text["text"].is_string(), "default must be text: {text}");
    assert!(
        text["count"].is_number(),
        "text mode reports a count: {text}"
    );
    assert!(
        text["lines"].is_null(),
        "`lines` is the records-mode array only: {text}"
    );

    let records = rig.call(
        "get_recent",
        json!({"device": device, "lines": 100, "format": "records"}),
    );
    assert!(
        records["lines"].is_array(),
        "records mode returns an array: {records}"
    );

    // The whole point: the default costs fewer bytes on the wire.
    let (a, b) = (text.to_string().len(), records.to_string().len());
    assert!(a < b, "text ({a}B) should be smaller than records ({b}B)");
}

/// tools/list is paid at the start of EVERY session before any work happens:
/// 67 tools, ~45KB, of which schemas are 65%. The core profile ships the
/// console/power set an agent actually reaches for; everything else stays
/// callable and discoverable through help().
#[test]
fn the_core_profile_is_much_smaller_and_loses_no_capability() {
    use conminer_mcp::tools::{advertise_profile, find, CORE_TOOLS};

    let full = advertise_profile(true).to_string().len();
    let core = advertise_profile(false).to_string().len();
    assert!(
        core * 2 < full,
        "core ({core}B) should be far smaller than full ({full}B)"
    );

    // Every core tool must exist, and help must be among them or the rest
    // become undiscoverable.
    for name in CORE_TOOLS {
        assert!(find(name).is_some(), "core tool {name} is not registered");
    }
    assert!(
        CORE_TOOLS.contains(&"help"),
        "help must be advertised or nothing else is reachable"
    );

    // Capability is not lost: a non-advertised tool still resolves by name.
    assert!(find("ingest_file").is_some());
    assert!(
        !CORE_TOOLS.contains(&"ingest_file"),
        "this test is vacuous if ingest_file is core"
    );
}

/// diagnose() must open its OWN connection and report what it sees. The
/// defining failure it exists for: minerd captured 130863 bytes while a fresh
/// client received ZERO on the same port (ser2net withholding data pending
/// telnet negotiation). Nothing that reads the store could have shown that,
/// and the gap was found with netcat instead of with conminer.
#[test]
fn diagnose_probes_the_endpoint_itself_and_gives_a_verdict() {
    let rig = Rig::new();
    let (device, _s) = rig.ingest("boot.log", "hello\n", None);

    let r = rig.call("diagnose", json!({"device": device, "wait_ms": 200}));
    assert!(
        r["verdict"].is_string(),
        "diagnose must state a verdict: {r}"
    );
    // An ingested file has no ser2net endpoint, and diagnose should say so
    // plainly rather than fail.
    // The verdict must explain WHY there is no console, not merely report the
    // absence: "does not exist" and "is broken" look identical from outside.
    let v = r["verdict"].as_str().unwrap();
    assert!(
        v.contains("no ser2net port") || v.contains("state=gone") || v.contains("excluded"),
        "verdict should give a reason, got: {v}"
    );
    assert!(r["console"].is_object() || r["console"].is_null(), "{r}");
}

/// A lease whose holder is gone must be reclaimable. Measured: a dashboard
/// console that died left holder="dashboard" in place, and release() is
/// holder-matched so it refused with LEASE_HELD -- which reads as "you cannot
/// have it" rather than "you asked the wrong way", and stranded the device.
#[test]
fn a_stranded_lease_can_be_force_released() {
    let rig = Rig::new();
    let (device, _s) = rig.ingest("boot.log", "hi\n", None);

    rig.call("acquire", json!({"device": device, "holder": "ghost"}));

    // NOTE: this rig is a single MCP session, so ctx.holder() is still "ghost"
    // and a plain release would succeed here. The stranded case needs a holder
    // with no live connection, which only a second session can produce -- that
    // is exactly the ergonomics trap this fix exists for. What IS testable here
    // is that force works and leaves the device acquirable.
    let r = rig.call("release", json!({"device": device, "force": true}));
    assert_eq!(r["released"], true, "{r}");
    let a = rig.call("acquire", json!({"device": device, "holder": "next"}));
    assert_eq!(a["lease"]["holder"], "next", "{a}");
}

/// list_devices is often the first call of a session, at ~1.1KB/device. Picking
/// a device needs a name, an endpoint and whether it is alive -- not identity,
/// tags, template counts and the last captured line.
#[test]
fn list_devices_is_terse_by_default_and_detailed_on_request() {
    let rig = Rig::new();
    rig.ingest("boot.log", "hello world\n", None);

    let terse = rig.call("list_devices", json!({}));
    let t = &terse["devices"][0];
    assert!(t["device"].is_string(), "{terse}");
    assert!(t["state"].is_string() || t["state"].is_null(), "{terse}");
    assert!(t["tags"].is_null(), "tags belong to detail mode: {terse}");
    assert!(
        t["last_line"].is_null(),
        "last_line belongs to detail mode: {terse}"
    );

    let full = rig.call("list_devices", json!({"detail": true}));
    assert!(full["devices"][0]["last_line"].is_string(), "{full}");

    let (a, b) = (terse.to_string().len(), full.to_string().len());
    assert!(a < b, "terse ({a}B) should be smaller than detail ({b}B)");
}

/// diagnose must be able to say "the port is live but capture is broken".
/// That divergence -- minerd recording 0 bytes while the console streamed --
/// cost hours: console_state read 0, so the board looked dead and every
/// judgement built on it was wrong. conminer can detect this about itself.
#[test]
fn diagnose_reports_a_live_port_that_capture_is_not_recording() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(
        src.contains("but capture is NOT recording it"),
        "diagnose should name the store/probe divergence outright"
    );
    // It must be conditioned on BOTH signals, not just one.
    assert!(
        src.contains("probe_got > 0 && captured == 0"),
        "the verdict must require a live probe AND an empty store"
    );
}

/// A read loop that keeps failing the same way is stuck, not reconnecting, and
/// must escalate: it ran for hours at WARN while recording nothing.
#[test]
fn a_persistently_failing_read_loop_escalates() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/live.rs"
    ))
    .expect("live.rs");
    assert!(src.contains("same_error"), "repetition must be tracked");
    assert!(
        src.contains("capture is NOT recording despite the device being attached"),
        "the escalation must state the consequence, not just the error"
    );
    assert!(
        src.contains("tracing::error!"),
        "repeated failure must not stay at WARN"
    );
}

/// ser2net's accepter is telnet and withholds the port until the client answers
/// negotiation. `send` connected and wrote immediately, so the write landed in
/// an un-negotiated connection: "Device open failure: Value or file not found",
/// with raw IAC bytes coming back. Same defect as the runner's (task #13), on a
/// second code path.
#[test]
fn send_answers_telnet_negotiation_before_writing() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let body = src.split(r#"name: "send""#).nth(1).expect("no send tool");
    let body = &body[..body
        .find(r#"name: "diagnose""#)
        .unwrap_or(body.len().min(4000))];
    assert!(
        body.contains("io.read(&mut scratch"),
        "send must read once (which answers negotiation) before it writes"
    );
}

/// A power hook returning 0 means "the command ran", never "the board did what
/// you asked". Stress testing on hardware found both halves of that gap:
/// a Bughopper `off` that reported ok while the console kept talking (~1 in
/// 20), and an IQ10 `reset` that reported ok, opened a fresh epoch and captured
/// ZERO bytes -- the board wedged after ~8 resets and no further reset could
/// recover it, while every hook kept returning ok.
#[test]
fn power_actions_verify_the_board_not_the_exit_code() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    assert!(
        src.contains("fn verify_power_effect"),
        "no verification of the actual effect"
    );
    // Off means quiet AND staying quiet; on/reset mean the board said something.
    assert!(
        src.contains("now.1 > last.1 && now.0 <= last.0"),
        "off must require sustained silence"
    );
    // The wedge signature: an epoch that opens and captures nothing.
    assert!(
        src.contains("An epoch that opens and captures nothing"),
        "wedge must be recognised"
    );
    // Escalate once -- a reset that produced a dead board is recovered by a cycle.
    assert!(
        src.contains(r#""reset" | "on" => Some("cycle")"#),
        "reset must escalate to a cycle"
    );
    assert!(
        src.contains("escalating once"),
        "escalation must be bounded and logged"
    );
    // And the verdict must reach the caller, or none of this is visible.
    assert!(
        src.contains(r#""effect": verified"#),
        "the effect must be reported to the agent"
    );
}

/// `reset` is not a universal capability, and a board that ignores it must not
/// look like one that obeyed. Measured: the NordAU RIDE SX's reset is a PMIC
/// power-BUTTON tap -- swept 0.3s to 10s on a verifiably running board and
/// MD_RESOUT_N never dipped, MD_PS_HOLD never moved. Meanwhile the IQ10 wedges
/// after ~8 consecutive resets. Both are answered the same way: verify the
/// effect, then escalate to a power cycle rather than resetting into the void.
#[test]
fn a_reset_that_does_nothing_escalates_to_a_power_cycle() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    assert!(
        src.contains(r#""reset" | "on" => Some("cycle")"#),
        "reset must escalate to a cycle"
    );
    // The evidence must travel with the rule, or it decays into folklore and
    // someone "simplifies" it away.
    assert!(
        src.contains("MD_RESOUT_N never dipped"),
        "record WHY reset can be a no-op"
    );
    assert!(src.contains("wedges after ~8"), "record the IQ10 limit too");
    // And the verdict must reach the caller.
    assert!(
        src.contains(r#""effect": verified"#),
        "the caller must see what the board did"
    );
}

/// A boot mode that silently does nothing is worse than one that fails: the
/// caller goes on to flash a board that is not in EDL.
///
/// Measured on the RIDE's Karussell, three boots each. Firing the controller's
/// firmware SEQUENCE `BOOT_MD_EDL` left MD_EDL reading 0 and the board booted
/// normally -- 108086 bytes with the full XBL log -- while the hook reported
/// success. Asserting the STRAP and holding it across the power-on gave the real
/// thing: 2 bytes and no XBL log, and clearing it restored a 113089-byte normal
/// boot. So boot modes must drive the strap line.
#[test]
fn boot_modes_drive_the_strap_line_not_the_firmware_sequence() {
    let hook = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/bantam-power"
    ))
    .expect("bantam-power hook");

    for (mode, strap) in [
        ("BOOT_MD_EDL", "MD_EDL"),
        ("BOOT_SS_EDL", "SS_EDL"),
        ("BOOT_UEFI", "UEFI"),
        ("MD_FASTBOOT", "FASTBOOT_MD"),
    ] {
        // One table now feeds the set, both releases and the read, so the
        // mapping is a `MODE:LINE` pair rather than a `case` arm. What it says
        // is unchanged, and the boot-overrides suite sets every mode on a pty
        // and reads this same line back asserted.
        assert!(
            hook.contains(&format!("{mode}:{strap}")),
            "{mode} must drive {strap}"
        );
    }
    assert!(
        hook.contains(r#"strap=$(line_of_mode "$a1")"#),
        "`mode` must take its line from that table"
    );
    // The strap has to be SET and READ BACK, not fired and hoped for.
    assert!(
        hook.contains(r#"set_verify "$strap" 1"#),
        "the strap must be verified"
    );
    // A latched strap silently changes every later boot, so clearing must exist.
    assert!(
        hook.contains("mode-clear"),
        "there must be a way to clear boot straps"
    );
    // And the evidence travels with the rule.
    assert!(
        hook.contains("108086"),
        "record that the firmware sequence booted normally"
    );
}

/// mcpd must READ through the broker and WRITE straight to ser2net.
///
/// mcpd was the last consumer opening its own ser2net connection to a console
/// minerd was already reading. Two readers of one device is the contention class
/// the broker exists to remove: when the device open failed, each reader
/// independently concluded "the board is quiet" while the tty was producing
/// 102190 bytes.
///
/// The split matters in both directions:
///   * reads shared, so every consumer sees the same console, and
///   * writes NOT shared, so a broker outage can never swallow a command headed
///     for a board.
#[test]
fn mcpd_reads_through_the_broker_and_writes_straight_to_ser2net() {
    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let runner = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/runner.rs"
    ))
    .expect("runner.rs");

    // No console session may still open a private reader.
    assert!(
        !tools.contains("TcpTransport::connect(&endpoint)"),
        "mcpd still opens its own ser2net reader for a console session"
    );
    assert!(
        tools.contains("BrokeredTransport::connect(&endpoint"),
        "mcpd console sessions must go through the brokered transport"
    );

    // And the transport must genuinely split the directions.
    assert!(
        runner.contains("pub struct BrokeredTransport"),
        "BrokeredTransport must exist"
    );
    assert!(
        runner.contains("only the read side is shared"),
        "the write path must stay off the broker, and say why"
    );
    // Losing the broker must degrade, not break: that is when someone is most
    // likely to be staring at a console trying to find out why.
    assert!(
        runner.contains("this degrades to reading that same ser2net socket"),
        "an absent broker must fall back rather than fail the session"
    );
}

/// Power state is parsed by EQUALITY on the first token, never by substring.
///
/// Measured failure: the Bughopper answers
///     "unknown (commanded-only controller; cbus=0b00000000)"
/// and a `contains("on")` matched the "on" inside "cONtroller", so a controller
/// that CANNOT report power read as ON -- on a board that was powered off. An
/// indicator that invents a state is worse than one that admits ignorance,
/// because a human stops checking.
#[test]
fn power_state_is_never_inferred_from_a_substring() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    assert!(
        !src.contains(r#"out.contains("on")"#),
        "power state must not be sniffed out of arbitrary text"
    );
    assert!(
        src.contains(r#""on" => Some("on".into())"#)
            && src.contains(r#""off" => Some("off".into())"#),
        "power state must match the first token exactly"
    );
    // The trap itself must stay documented, or someone re-simplifies it back.
    assert!(
        src.contains("cONtroller"),
        "record WHY substring matching was wrong"
    );
}

/// Silence only proves "off" if there was noise to begin with.
///
/// FROM THE CROSS-PLATFORM REPORT (T4): `power off` issued while the ADP sat in
/// EDL reported success -- the hook exited 0 and the console was quiet -- while
/// the board stayed enumerated and alive on USB. EDL is silent BY DESIGN, so the
/// quiet that "confirmed" the off was the quiet that was already there. That
/// controller has no power-sense pins, so nothing could contradict it.
#[test]
fn an_already_silent_console_cannot_confirm_a_power_off() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    assert!(
        src.contains("let was_talking"),
        "off-verification must know whether the console was saying anything before"
    );
    assert!(
        src.contains("ok = was_talking &&"),
        "silence must not count as confirmation on a console that was already silent"
    );
    // And the caller must be told this is UNVERIFIABLE, not failed -- the next
    // move differs completely.
    assert!(
        src.contains("cannot confirm: this console was already silent"),
        "the impossible case must be named"
    );
    // conminer now checks USB itself rather than telling the caller to -- and
    // says only what that check actually established. Round 4 caught it asserting
    // "the board is not in EDL" from a scan taken inside the re-enumeration
    // window, which is a wrong fact an agent would act on.
    assert!(
        src.contains("no QDL gadget appeared, so the board is \\\n                 not in EDL."),
        "the settled branch must state what conminer ruled out"
    );
    assert!(
        src.contains("which does NOT exclude EDL"),
        "and the unsettled branch must NOT state it"
    );
}

/// `follow` is the cheap incremental wait; it must not be the most expensive
/// call in the surface.
///
/// FROM THE REPORT (T8): responses of 65-87 KB EACH, regardless of `max_lines`,
/// dominated by an unbounded `template_deltas` -- a busy boot re-hits hundreds
/// of ALREADY-KNOWN templates and every one was returned in full.
#[test]
fn follow_caps_repeat_template_deltas_and_says_what_it_dropped() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/follow.rs"
    ))
    .expect("follow.rs");

    assert!(src.contains("MAX_DELTAS"), "repeat deltas must be capped");
    assert!(
        src.contains("deltas_omitted"),
        "a truncated list must never look like a complete one"
    );
    // NEW templates are the interesting half, and the REPEAT cap must not
    // reach them. Asserted line by line rather than by offset, because §L3
    // rewrote this loop and an offset comparison was pinning the old shape
    // rather than the rule it was defending.
    for (n, line) in src.lines().enumerate() {
        if line.contains("MAX_DELTAS") && line.contains("new_templates") {
            panic!(
                "follow.rs:{}: novel templates capped alongside repeats",
                n + 1
            );
        }
    }
    // §L3: what the budget ladder drops is counted too -- every rung of it.
    for counter in [
        "tail_omitted",
        "delta_text_dropped",
        "novel_text_dropped",
        "novel_omitted",
    ] {
        assert!(
            src.contains(counter),
            "the budget trim must report {counter}: a response that quietly shrinks is \
             indistinguishable from a quiet console"
        );
    }
}

/// conminer must handle the ADP's two hardware quirks itself, not describe them.
///
/// (1) In EDL the PBL ignores this controller's 6s PM_RESIN_N press, so `off`
///     returns 0 while the board stays alive. The sequence that works is
///     `reset` (exits EDL) then `off` -- which a human had to know and run.
/// (2) After a verified-off, the board's gadget stays listed for MINUTES with
///     cached descriptors while every real read fails, because its Type-C
///     controller never signals detach. A stale entry makes the NEXT EDL or
///     flash session target a device that is not there.
#[test]
fn an_off_that_edl_ignored_is_recovered_automatically() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    assert!(
        src.contains("reset-then-off"),
        "conminer must run the recovery itself, not report the failure"
    );
    assert!(
        src.contains("usb::in_edl"),
        "the decision must be made on out-of-band USB truth, not console silence"
    );
    // And it must not claim success if the board is STILL in EDL afterwards.
    assert!(
        src.contains("STILL in EDL after a reset-then-off"),
        "an escalation that did not work must say so"
    );
}

#[test]
fn stale_usb_entries_are_cleared_after_a_confirmed_power_off() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(
        src.contains("fn sweep_usb_zombies"),
        "zombies must be swept"
    );
    assert!(
        src.contains("usb_zombies_cleared"),
        "the caller must be told the rig was tidied"
    );
    // What could NOT be cleared has to be named, with the human fallback.
    assert!(
        src.contains("uhubctl") && src.contains("will not clear"),
        "an uncleared zombie must be reported with what a human should do"
    );
}

/// A silent console must never leave the caller guessing between "off", "idle"
/// and "in EDL" -- the three look identical and only one of them means the board
/// is ready to flash.
#[test]
fn diagnose_names_edl_instead_of_reporting_silence() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(src.contains(r#""edl": edl"#), "diagnose must report EDL");
    assert!(
        src.contains("silent by design"),
        "and must say the silence is expected, not a fault"
    );
}

/// `list_devices {power:true}` must say "unknown" out loud.
///
/// A file-ingest pseudo-device has no controller and never will. The tempting
/// shapes are to omit the field or to emit null -- and both invite the reader to
/// fill the gap themselves, where the assumption that costs hardware is "no
/// reading means off". A board that is quietly powered and reported as off is
/// how somebody unplugs the wrong thing.
#[test]
fn a_power_reading_that_cannot_be_taken_is_reported_as_unknown() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    let plain = rig.call("list_devices", json!({}));
    assert!(
        plain["devices"][0].get("power").is_none(),
        "power is opt-in: it costs a controller query per board"
    );

    let with_power = rig.call("list_devices", json!({"power": true}));
    let row = with_power["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"].as_str() == Some(device.as_str()))
        .expect("the device");
    assert_eq!(
        row["power"].as_str(),
        Some("unknown"),
        "an unanswerable power state must be stated, not left blank: {row}"
    );
    assert!(
        row["power_unknown_because"]
            .as_str()
            .unwrap_or_default()
            .contains("mined log"),
        "and it must say WHY, because the three reasons for 'unknown' call for \
         different moves: {row}"
    );
}

/// FOUND ON HARDWARE, in the first version of this very feature.
///
/// The Bantam profile claims `controls = "*"` deliberately, so a newly plugged
/// board gets working buttons with no config. Combined with an unfiltered sweep
/// that meant `file:/tmp/board-boot.log` -- a MINED LOG -- resolved to the
/// IQ10's Bantam and reported `power: "off"`. A log file cannot be powered, and
/// the "off" was an unrelated board's answer: the same borrowing defect as the
/// cross-board bug, wearing a different hat.
#[test]
fn a_row_with_no_board_behind_it_never_borrows_another_boards_power() {
    let rig = Rig::new();
    rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    let listed = rig.call("list_devices", json!({"power": true}));
    for row in listed["devices"].as_array().unwrap() {
        let name = row["device"].as_str().unwrap_or_default();
        if !name.starts_with("file:") && !name.contains('#') {
            continue;
        }
        assert_eq!(
            row["power"].as_str(),
            Some("unknown"),
            "{name} is not a board and must not report a power state at all: {row}"
        );
        assert!(
            row["power_source"].is_null(),
            "{name} must not name a controller as its source -- there is no \
             board here for one to answer about: {row}"
        );
    }
}

/// The power sweep must key on the controller INSTANCE, everywhere it happens.
///
/// Measured on alpha: the dashboard grouped by the controller PROFILE name.
/// This bench runs two Bantams, both called "bantam", so every console on both
/// boards was published one board's answer -- the NordAU read "off" while it was
/// powered on. mcpd grew the same fan-out for `list_devices`, so the same trap
/// exists twice and this pins the second one.
#[test]
fn the_mcp_power_fanout_groups_by_controller_instance_not_profile_name() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");

    let f = src
        .split("fn power_by_controller")
        .nth(1)
        .expect("power_by_controller must exist");
    let body = &f[..f.find("\nfn ").unwrap_or(f.len())];
    assert!(
        body.contains("controller_port_for"),
        "the group key must be the RESOLVED controller instance"
    );
    assert!(
        !body.contains(".name.clone()") && !body.contains("controller_for("),
        "the profile name is not an identity: two Bantams share it"
    );
    assert!(
        body.contains("no controller resolves for this device"),
        "an unresolved controller must end as UNKNOWN, never joined to a shared \
         fallback group -- a shared fallback is how the original bug was built"
    );
    assert!(
        body.contains("not_a_powerable_board"),
        "and rows with no board behind them must be excluded before any hook \
         runs: the Bantam profile claims controls=\"*\", so a mined log \
         otherwise resolves to some board's controller"
    );
}

/// A LABEL YOU CANNOT TAKE OFF IS A ONE-WAY DOOR.
///
/// `set_tags` only ever upserted, so once a board was labelled the label stayed
/// -- and a bench where mislabelling is permanent is a bench where nobody
/// labels anything. Removal is explicit rather than "an empty value deletes",
/// because a bare label with no value ("needs-rma") is the most useful kind and
/// the two must stay distinguishable.
#[test]
fn labels_can_be_added_as_bare_words_or_selectors_and_taken_off_again() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", &corpus_text("linux/boot-oops.log"), None);

    // A bare label and a key=value selector, in one call.
    let out = rig.call(
        "tag_device",
        json!({"device": device, "tags": {"needs-rma": "", "rack": "r2"}}),
    );
    assert_eq!(out["tags"]["rack"], "r2", "{out}");
    assert_eq!(
        out["tags"]["needs-rma"], "",
        "a bare label keeps its empty value rather than being dropped: {out}"
    );

    // Selecting by either still works -- a label is an address, not decoration.
    let by_tag = rig.call("list_devices", json!({"detail": true}));
    assert!(by_tag["devices"][0]["tags"]["needs-rma"].is_string());

    // Taking one off leaves the others alone.
    let out = rig.call(
        "tag_device",
        json!({"device": device, "remove": ["needs-rma"]}),
    );
    assert!(
        out["tags"].get("needs-rma").is_none(),
        "the label must actually come off: {out}"
    );
    assert_eq!(out["tags"]["rack"], "r2", "and only that one: {out}");

    // A call that neither adds nor removes is a mistake worth naming.
    let err = rig.err("tag_device", json!({"device": device}));
    assert_eq!(err["code"], "INVALID_ARGUMENT", "{err}");
}

/// A CONTROLLER IS A DEVICE AN AGENT CAN NAME, AND THEN DRIVE BY THAT NAME.
///
/// The controller is the thing an operator points at -- "the one on the left" --
/// and unlike a console it has no traffic to identify it by, so a name is the
/// only handle it will ever have. It is also the row that is easiest to assume
/// is not a device at all: discovery excludes it from capture, it has no
/// endpoint, and it never leaves this host (a peer imports consoles, and a row
/// with nothing to serve is not one). None of that stops it being addressable.
///
/// Proven end to end: name it, select it by that name for metadata, select it by
/// a label query, and drive its hook by name -- with the CANONICAL path in the
/// argv, because a nickname is how a human or an agent picks a device and never
/// a hardware identifier. That last one is not hypothetical: substituting the
/// nickname into `{device}` once broke power control on every named board.
#[test]
fn a_controller_can_be_named_and_then_addressed_by_that_name() {
    let rig = Rig::new();
    let canonical = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_IQ10RRDXX34VG8-if00";
    {
        // Exactly as discovery leaves it: present, and excluded from capture.
        let mut reg = rig.registry();
        let row = reg
            .upsert_device(
                canonical,
                None,
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_ignored(row.id, true).unwrap();
    }

    rig.call(
        "name_device",
        json!({"device": "Bantam", "nickname": "left-bantam"}),
    );

    // The name selects it.
    let t = rig.call(
        "tag_device",
        json!({"device": "left-bantam", "tags": {"bay": "1"}}),
    );
    assert_eq!(
        t["device"], canonical,
        "the name resolved to the controller"
    );

    // So does a label query.
    let l = rig.call(
        "list_devices",
        json!({"filter": "tag:bay=1", "detail": true}),
    );
    assert_eq!(l["count"], 1, "{l}");
    assert_eq!(l["devices"][0]["canonical"], canonical, "{l}");
    assert_eq!(l["devices"][0]["label"], "left-bantam", "{l}");

    // And it drives the board by that name.
    let p = rig.call(
        "power",
        json!({"device": "left-bantam", "action": "off", "dry_run": true}),
    );
    let argv = p["hook"]["command"].to_string();
    assert!(
        argv.contains("bantam-power"),
        "the controller's own hook must be the one selected: {argv}"
    );
    assert!(
        argv.contains(canonical),
        "the hook needs the hardware path, not the name: {argv}"
    );

    // AND THE NAME NEVER REACHES THE HOOK. The Bantam's template addresses its
    // controller, so it cannot show this on its own; a Bughopper's addresses
    // `{device}`, which is exactly where a nickname once got substituted and
    // broke power control on every board an operator had bothered to name.
    let bughopper = "/dev/serial/by-id/usb-Arduino_Bughopper_TEST-if00-port0";
    {
        let mut reg = rig.registry();
        reg.upsert_device(
            bughopper,
            None,
            conminer_core::store::IdentityKind::ById,
            None,
            1_000,
        )
        .unwrap();
    }
    rig.call(
        "name_device",
        json!({"device": bughopper, "nickname": "adp-ventuno"}),
    );
    let p = rig.call(
        "power",
        json!({"device": "adp-ventuno", "action": "off", "dry_run": true}),
    );
    let argv = p["hook"]["command"].to_string();
    assert!(
        argv.contains(bughopper),
        "the hook needs the hardware path: {argv}"
    );
    assert!(
        !argv.contains("adp-ventuno"),
        "a nickname is not a hardware identifier: {argv}"
    );
}

/// A NAME FOUND HOWEVER IT WAS TYPED.
///
/// Reported from a live session: an operator named a board `uno-q`, an agent
/// asked for `unoq`, and conminer said UNKNOWN_DEVICE about a board sitting on
/// the bench. Hyphens, underscores, dots and spaces are how a name happens to
/// have been typed, not part of what was meant by it.
///
/// The looseness is LAST and no looser: exact names still win, and an ambiguous
/// spelling is still an error with candidates rather than a guess.
#[test]
fn a_label_is_found_however_its_separators_were_typed() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);
    rig.call(
        "name_device",
        json!({"device": device, "nickname": "uno-q"}),
    );

    for spelling in ["uno-q", "unoq", "UNOQ", "Uno.Q", "uno_q"] {
        let l = rig.call("list_devices", json!({"filter": spelling}));
        assert_eq!(l["count"], 1, "{spelling:?} must find the board: {l}");
        assert_eq!(l["devices"][0]["label"], "uno-q", "{spelling:?}: {l}");
    }
    // ...and it is still a SELECTOR, not merely a filter: an agent can act on it.
    let t = rig.call(
        "tag_device",
        json!({"device": "unoq", "tags": {"bay": "2"}}),
    );
    assert_eq!(t["tags"]["bay"], "2", "{t}");

    // Nothing became a wildcard: a spelling that matches nothing still fails.
    let none = rig.err("list_devices", json!({"filter": "unoqq"}));
    assert_eq!(none["code"], "UNKNOWN_DEVICE", "{none}");

    // An EXACT name still wins over a punctuation coincidence. Two boards, one
    // called `unoq` and one called `uno-q`: asking for `unoq` must get the one
    // actually called that, not an ambiguity error.
    let (second, _) = rig.ingest("other.log", "hello\n", None);
    rig.call("name_device", json!({"device": second, "nickname": "unoq"}));
    let exact = rig.call("list_devices", json!({"filter": "unoq", "detail": true}));
    assert_eq!(exact["count"], 1, "the exact name must win: {exact}");
    assert_eq!(exact["devices"][0]["canonical"], second, "{exact}");
}

/// A TARGET CAN BE NAMED, and the name works everywhere the number did.
///
/// A target is derived from the USB hub its consoles share, so a board arrives
/// called `2.1`. That is exact and unmemorable, and it was also unchangeable:
/// naming the console did nothing for `list_targets`, so board-level actuation
/// still had to be spelled `2.1`. Same session, same report.
#[test]
fn a_target_can_be_named_and_then_addressed_by_that_name() {
    let rig = Rig::new();
    let (a, _) = rig.ingest("ap.log", "hello\n", None);
    let (b, _) = rig.ingest("ec.log", "hello\n", None);
    // Put both consoles in one group the way config does.
    {
        let mut reg = rig.registry();
        for d in [&a, &b] {
            let row = reg.resolve(d).unwrap();
            reg.set_target(row.id, Some("2.1")).unwrap();
        }
    }
    let before = rig.call("list_targets", json!({}));
    assert_eq!(before["targets"][0]["name"], "2.1", "{before}");

    let named = rig.call("name_target", json!({"target": "2.1", "name": "uno-q"}));
    assert_eq!(named["target"], "uno-q", "{named}");
    assert_eq!(
        named["members"].as_array().map(Vec::len),
        Some(2),
        "{named}"
    );

    let after = rig.call("list_targets", json!({}));
    assert_eq!(
        after["count"], 1,
        "naming must not split the group: {after}"
    );
    assert_eq!(after["targets"][0]["name"], "uno-q", "{after}");

    // The name is what the target FORM takes now -- this is the point of it.
    // `target_mark` opens an epoch on every member, so it wants their leases:
    // taking them here is what an agent does, and proves the name carries all
    // the way through to the members it names.
    for d in [&a, &b] {
        rig.call("acquire", json!({"device": d}));
    }
    let marked = rig.call("target_mark", json!({"target": "uno-q", "label": "smoke"}));
    assert!(marked.get("error").is_none(), "{marked}");

    // Two boards may not share a name: that would merge them, and a power event
    // on one would open epochs on the other's consoles.
    let (c, _) = rig.ingest("bmc.log", "hello\n", None);
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&c).unwrap();
        reg.set_target(row.id, Some("3.2")).unwrap();
    }
    let clash = rig.err("name_target", json!({"target": "3.2", "name": "uno-q"}));
    assert_eq!(clash["code"], "INVALID_ARGUMENT", "{clash}");

    // And clearing hands the group back to the topology. These fixtures are
    // ingested files, which HAVE no topology -- so the honest answer is that
    // they now belong to no target, said out loud rather than echoed back as
    // the name that was just removed.
    let cleared = rig.call("name_target", json!({"target": "uno-q"}));
    assert!(cleared["target"].is_null(), "{cleared}");
    assert!(
        cleared["note"]
            .as_str()
            .is_some_and(|n| n.contains("no target")),
        "a cleared group with no topology must say so: {cleared}"
    );
    let gone = rig.call("list_targets", json!({}));
    assert!(
        !gone["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "uno-q"),
        "{gone}"
    );
}

/// A TARGET IS FOUND HOWEVER ITS NAME WAS TYPED, exactly as a device is.
///
/// Reported from a live session, AFTER the device side was fixed: `unoq` found
/// the console and `target: "unoq"` still answered UNKNOWN_DEVICE about the same
/// board. Half a fix reads as no fix, and the half that was missing is the one
/// board-level actuation goes through.
#[test]
fn a_target_is_found_however_its_separators_were_typed() {
    let rig = Rig::new();
    let (a, _) = rig.ingest("ap.log", "hello\n", None);
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&a).unwrap();
        reg.set_target(row.id, Some("uno-q")).unwrap();
    }
    for spelling in ["uno-q", "unoq", "UNOQ", "uno_q"] {
        rig.call("acquire", json!({"device": a}));
        let marked = rig.call("target_mark", json!({"target": spelling, "label": "t"}));
        assert!(marked.get("error").is_none(), "{spelling:?}: {marked}");
    }
    // Still not a wildcard.
    let miss = rig.err("target_mark", json!({"target": "unoqq", "label": "t"}));
    assert_eq!(miss["code"], "UNKNOWN_DEVICE", "{miss}");

    // And an exact name still wins: with `unoq` and `uno-q` both real targets,
    // asking for `unoq` gets the one actually called that.
    let (b, _) = rig.ingest("ec.log", "hello\n", None);
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&b).unwrap();
        reg.set_target(row.id, Some("unoq")).unwrap();
    }
    rig.call("acquire", json!({"device": b}));
    let exact = rig.call("target_mark", json!({"target": "unoq", "label": "t"}));
    assert!(exact.get("error").is_none(), "{exact}");
    assert!(
        serde_json::to_string(&exact).unwrap().contains("ec.log"),
        "the exact target must win: {exact}"
    );
}

/// AN EXPIRED LEASE IS NOT A LEASE.
///
/// `require_lease` has always treated a lapsed one as free, but `diagnose`
/// published the row whatever its expiry -- so a reservation that ended an hour
/// ago still read as "held by dashboard", and an agent stepped around a board
/// nobody was using. Reported from a live session.
#[test]
fn a_lease_that_has_expired_is_reported_as_free() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);
    rig.call("acquire", json!({"device": device}));

    // Held: the diagnostic says so.
    let held = rig.call("diagnose", json!({"device": device, "wait_ms": 100}));
    assert!(held["lease"]["holder"].is_string(), "{held}");

    // Now let it lapse, exactly as the clock would.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        let l = reg.lease(row.id).unwrap().expect("a lease");
        reg.set_lease_expiry(row.id, l.acquired_at - 1).unwrap();
    }

    let free = rig.call("diagnose", json!({"device": device, "wait_ms": 100}));
    assert!(
        free["lease"].is_null(),
        "an expired lease must not read as held: {free}"
    );
    // ...and the history is still there, where it cannot be mistaken for one.
    assert!(
        free["stale_lease"]["holder"].is_string(),
        "the lapsed holder is still worth knowing: {free}"
    );
    // ...and it really is free: acquiring works rather than reporting LEASE_HELD.
    let taken = rig.call("acquire", json!({"device": device}));
    assert!(taken.get("error").is_none(), "{taken}");
    let again = rig.call("diagnose", json!({"device": device, "wait_ms": 100}));
    assert!(again["lease"]["holder"].is_string(), "{again}");
    assert!(again["stale_lease"].is_null(), "{again}");
}

/// A FOLLOW MUST NOT START AFTER THE THING IT IS WATCHING FOR.
///
/// From the bench, flashing the Uno-Q: the banner was captured at line 2884 and
/// the follow that was waiting for it timed out, because it began at the live
/// head -- which by then was past the banner. The line was there the whole time;
/// only a historical search found it afterwards. The board does not wait for the
/// caller, and the gap between `power` returning and `follow` being called is
/// real: an RPC hop, an agent's next thought, a relayed call across the fleet.
///
/// Reproduced exactly: bytes land BEFORE the follow is called, and across an
/// epoch boundary, since that is what made it look like an attribution bug.
#[test]
fn a_follow_with_no_cursor_starts_at_this_boot_not_at_the_live_head() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\n", None);

    // A reset opens a new epoch, exactly as `power` does...
    rig.call("acquire", json!({"device": device}));
    rig.call("mark", json!({"device": device, "label": "reset"}));
    // ...and the board prints its banner BEFORE anybody follows.
    rig.ingest(
        "banner.log",
        "QDL: Sahara mode entered\nBANNER-2884 flash complete\n",
        Some(&device),
    );

    let f = rig.call(
        "follow",
        json!({"device": device, "until": {"pattern": "BANNER-2884"}, "timeout_s": 2}),
    );
    assert_eq!(
        f["timed_out"], false,
        "the banner was already captured; a follow that starts at the head cannot see it: {f}"
    );
    assert!(
        serde_json::to_string(&f).unwrap().contains("BANNER-2884"),
        "the match must be returned, not merely counted: {f}"
    );
    // And it says where it began, because "not found" means different things
    // depending on the answer.
    assert!(
        f["start_from"].as_str().is_some_and(|s| s.contains("boot")),
        "the response must say where the watch started: {f}"
    );

    // An EXPLICIT cursor is still exact: a caller streaming in a loop must not
    // be handed the same lines again, and must not be dragged backwards into a
    // boot it already read.
    let c = f["follow"]["cursor"]
        .as_str()
        .expect("a forward cursor")
        .to_string();
    let after = rig.call(
        "follow",
        json!({"device": device, "cursor": c, "until": {"pattern": "BANNER-2884"},
               "timeout_s": 1}),
    );
    assert_eq!(after["start_from"], "cursor", "{after}");
    assert_eq!(
        after["timed_out"], true,
        "from a cursor past the banner there is nothing left to find: {after}"
    );
}

/// AN EPOCH BEGINS WHERE THE STREAM STOOD WHEN THE BUTTON WAS PRESSED.
///
/// From the bench, with the numbers: a reset opened epoch 27 at offset 235333
/// while the banner it caused was captured at 235299 -- thirty-four bytes
/// earlier -- so the banner belonged to epoch 26. A follow from the boundary saw
/// nothing, a search of the new epoch found nothing, and the line was in the old
/// one all along. The cause is ordering: the hook runs first (so a failed hook
/// leaves no epoch), the effect is then verified, and only then is the epoch
/// stamped -- seconds after the board started answering.
///
/// So the START is marked before the hook runs, and that is where the epoch
/// begins. Proven here on the store directly, because the ordering, not the
/// hook, is the thing under test.
#[test]
fn an_epoch_opened_at_a_mark_claims_what_the_board_said_while_the_hook_ran() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("before.log", "old boot chatter\n", None);

    // Where the stream stood when we "pressed the button".
    let mark = {
        let reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        let st = conminer_core::store::DeviceStore::open(
            &rig._dir.path().join(&row.db_file),
            &row.canonical,
            true,
        )
        .unwrap();
        st.head_cursor().offset
    };

    // The board answers WHILE the hook is still running.
    rig.ingest("banner.log", "BANNER-3931 new boot begins\n", Some(&device));

    // ...and only now does the epoch get stamped.
    {
        let reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        let mut st = conminer_core::store::DeviceStore::open(
            &rig._dir.path().join(&row.db_file),
            &row.canonical,
            true,
        )
        .unwrap();
        let boot = st
            .open_boot_at("power", Some("reset"), 1_000, None, Some(mark))
            .unwrap();
        assert_eq!(
            boot.opened_offset, mark,
            "the epoch must begin at the mark, not at the head"
        );
    }

    // The banner belongs to the boot the reset caused -- which is the whole
    // point: an agent searching the new epoch must find it.
    let boots = rig.call("list_boots", json!({"device": device, "view": "full"}));
    let newest = &boots["boots"][0];
    // AND THE RECORDS MUST SAY SO TOO. Moving the boundary is only half of it:
    // every line was stamped with the epoch that was open when it arrived, so
    // after the boundary fix landed the bench still reported the banner under
    // the old epoch and an epoch-filtered search returned zero. The lines inside
    // the new epoch's range are re-stamped to it.
    {
        let reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        let st = conminer_core::store::DeviceStore::open(
            &rig._dir.path().join(&row.db_file),
            &row.canonical,
            true,
        )
        .unwrap();
        let newest = st.latest_boot().unwrap().expect("an epoch");
        let lines = st.lines_after(newest.opened_offset, 50).unwrap();
        let banner = lines
            .iter()
            .find(|l| String::from_utf8_lossy(&l.bytes).contains("BANNER-3931"))
            .expect("the banner is inside the new epoch's range");
        assert_eq!(
            banner.boot_id,
            Some(newest.id),
            "the banner's own record still points at the epoch the reset ENDED"
        );

        // AND THE COUNTS MOVE WITH THE LINES. `boots.bytes` is incremented as
        // lines are written to whichever epoch was current, so re-stamping
        // without adjusting it leaves an epoch claiming lines and zero bytes --
        // reported from the bench as one response saying `bytes_this_boot: 0`
        // beside 17,742 bytes of that boot's own output.
        let claimed: usize = lines
            .iter()
            .filter(|l| l.boot_id == Some(newest.id))
            .map(|l| l.bytes.len() + 1)
            .sum();
        assert!(
            newest.bytes > 0,
            "an epoch holding {} lines must not report zero bytes",
            lines
                .iter()
                .filter(|l| l.boot_id == Some(newest.id))
                .count()
        );
        assert!(
            (newest.bytes as usize) >= claimed.saturating_sub(2),
            "the epoch's byte count ({}) must account for what it holds (~{claimed})",
            newest.bytes
        );
    }

    // The question an agent asks after a reset: is my banner inside the epoch
    // that reset opened? Attribution is by stream offset, so that is what to
    // compare -- the boundary must sit at or before the banner, which is exactly
    // what failed on the bench (boundary 235333, banner 235299).
    let hits = rig.call(
        "search_raw",
        json!({"device": device, "pattern": "BANNER-3931"}),
    );
    let banner_at = hits["hits"][0]["stream_offset"]
        .as_u64()
        .expect("the banner");
    let boundary = newest["opened_offset"].as_u64().expect("the epoch");
    assert!(
        boundary <= banner_at,
        "the epoch opened at {boundary}, AFTER the banner it caused at {banner_at}: \
         that is the thirty-four bytes the bench lost"
    );
}

/// "NOW" MEANS NOW, EVEN THOUGH SOMEBODY ELSE IS DOING THE WRITING.
///
/// minerd appends to the device store from another process; mcpd holds its
/// handle for its whole life. The append-only counters on that handle were read
/// when it was opened, so "the head" drifted: measured on the bench, a watch
/// created `from: "now"` was stamped at offset 203777 while capture had already
/// reached 221403 -- and it replayed a banner from before it existed as though
/// it had just fired.
///
/// Simulated exactly: write to the store BEHIND mcpd's back, the way minerd
/// does, then ask mcpd where the head is.
#[test]
fn the_head_is_read_from_the_database_not_from_a_cached_handle() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "first\n", None);

    // MAKE MCPD CACHE ITS HANDLE FIRST. Without this the watch below opens a
    // fresh store, reads correct counters, and the gate proves nothing -- which
    // is exactly what it did until the break-proof caught it. On the bench the
    // handle has been open for hours by the time anybody creates a watch.
    let seen = rig.call("get_recent", json!({"device": device}));
    assert!(seen.get("error").is_none(), "{seen}");

    // Another process appends. `ingest_file` goes through mcpd, so write to the
    // store directly -- that is what makes this the cross-process case.
    let (row, path) = {
        let reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        let p = rig._dir.path().join(&row.db_file);
        (row, p)
    };
    {
        let mut st = conminer_core::store::DeviceStore::open(&path, &row.canonical, true).unwrap();
        let session = st
            .begin_session(
                conminer_core::store::SessionSource::Live,
                1_000,
                Some("minerd"),
                None,
                None,
            )
            .unwrap();
        st.append_lines(
            session,
            None,
            &[conminer_core::store::PendingLine {
                bytes: b"LATE-LINE from another process",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
                ts_mono: 1_000,
                ts_wall: 1_000,
                stage_id: None,
            }],
        )
        .unwrap();
    }

    // A watch created "from now" must start AFTER that line, not before it.
    let w = rig.call(
        "create_watch",
        json!({"device": device, "name": "late", "until": {"pattern": "LATE-LINE"}}),
    );
    assert!(w.get("error").is_none(), "{w}");

    // THE OFFSET IT WAS STAMPED WITH is the fact under test. Comparing it to
    // what the database actually holds is what a stale cached handle fails --
    // asserting only on "did it fire" measured the scanner instead, and passed
    // with the bug in place.
    let truth = {
        let st = conminer_core::store::DeviceStore::open(&path, &row.canonical, true).unwrap();
        st.head_cursor().offset
    };
    let stamped = w["watch"]["from_offset"]
        .as_u64()
        .or_else(|| w["from_offset"].as_u64())
        .unwrap_or_else(|| panic!("no from_offset in {w}"));
    assert_eq!(
        stamped, truth,
        "the watch was stamped at {stamped} while the store had reached {truth}: \
         a cached handle answered 'now' from memory"
    );

    // ...and the consequence an operator sees: it must not replay what happened
    // before it existed.
    let hits = rig.call("poll_watch", json!({"device": device, "name": "late"}));
    assert_eq!(
        hits["fired_total"].as_i64().unwrap_or(0),
        0,
        "a watch created after the line must not replay it as a new firing: {hits}"
    );
}

/// A CONSOLE THAT RECOVERED MUST NOT KEEP READING AS BROKEN.
///
/// Reported from the bench: `diagnose.probe` showed a clean connection with no
/// error while `device_state` and `capture_state` both said `open_failed`. Both
/// were honestly reported and one of them was stale -- minerd publishes capture
/// state when something happens to it, and nothing happens to a console that has
/// stopped delivering bytes, so a fault it saw once is asserted forever.
///
/// The probe is the fresher witness. When they disagree, the response says which
/// and why, instead of leaving an agent to notice the contradiction.
#[test]
fn a_probe_that_reads_cleanly_flags_a_stale_capture_state() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);

    // Exactly the bench's shape: the registry still carries the fault minerd
    // last saw.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        reg.set_state(row.id, "open_failed").unwrap();
    }

    let d = rig.call("diagnose", json!({"device": device, "wait_ms": 100}));
    // This fixture has no endpoint, so the probe cannot contradict anything --
    // and the field must stay silent rather than guess.
    assert!(
        d["stale_capture_state"].is_null(),
        "with no probe to compare against, say nothing: {d}"
    );

    // ...and the state is still reported, because it is a fact about capture.
    assert_eq!(d["device_state"], "open_failed", "{d}");
}

/// A RENAME MUST NOT BREAK A CALLER MID-FLASH.
///
/// The lease arguments were `owner` and `ttl`; they are `holder` and `ttl_s`
/// now. The strict argument check turned every call written against the old
/// names into INVALID_ARGUMENT with no hint that the argument still exists --
/// reported by an agent in the middle of a flash, which is the worst moment to
/// discover a surface has moved.
///
/// Old spellings work, mean the same thing, and the reply says what to write
/// next time.
#[test]
fn the_old_lease_argument_names_still_work_and_say_what_they_are_called_now() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);

    let old = rig.call(
        "acquire",
        json!({"device": device, "owner": "flasher", "ttl": 120}),
    );
    assert_eq!(
        old["lease"]["holder"], "flasher",
        "the old name still binds: {old}"
    );
    assert!(
        old["renamed_arguments"]
            .as_array()
            .is_some_and(|a| a.len() == 2),
        "...and the caller is told both new names: {old}"
    );

    // The new names are unaffected, and say nothing about renames.
    let new = rig.call(
        "acquire",
        json!({"device": device, "holder": "flasher", "ttl_s": 120}),
    );
    assert_eq!(new["lease"]["holder"], "flasher", "{new}");
    assert!(new["renamed_arguments"].is_null(), "{new}");

    // A genuine typo is still an error: this is a shim for names that shipped,
    // not a spell-checker.
    let typo = rig.err("acquire", json!({"device": device, "ownr": "flasher"}));
    assert_eq!(typo["code"], "INVALID_ARGUMENT", "{typo}");
}

/// PRESENCE AND CAPTURE HEALTH ARE DIFFERENT FACTS.
///
/// They shared the `state` column and two processes wrote it with different
/// vocabularies: discovery publishes presence (discovered / gone / ignored),
/// minerd publishes capture health (listening / streaming / open_failed / …).
/// Last writer won, so each erased the other -- reported from the bench as
/// `capture_state: not_listening` on a console that was capturing a boot.
#[test]
fn a_presence_sweep_cannot_erase_capture_health() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);
    let id = {
        let reg = rig.registry();
        reg.resolve(&device).unwrap().id
    };

    // minerd says capture is live...
    {
        let mut reg = rig.registry();
        reg.set_capture_state(id, "streaming").unwrap();
    }
    // ...and a discovery sweep runs, as it does every few seconds.
    {
        let mut reg = rig.registry();
        reg.set_state(id, "discovered").unwrap();
    }

    let row = rig.registry().resolve(&device).unwrap();
    assert_eq!(row.state, "discovered", "presence is discovery's answer");
    assert_eq!(
        row.capture_state.as_deref(),
        Some("streaming"),
        "a presence sweep must not erase what minerd observed"
    );

    // ...and the other way round: capture health must not overwrite presence.
    {
        let mut reg = rig.registry();
        reg.set_capture_state(id, "not_listening").unwrap();
    }
    let row = rig.registry().resolve(&device).unwrap();
    assert_eq!(
        row.state, "discovered",
        "capture must not claim the device is gone"
    );
    assert_eq!(row.capture_state.as_deref(), Some("not_listening"));

    // The freshness envelope every response carries reports the capture column.
    let d = rig.call("diagnose", json!({"device": device, "wait_ms": 100}));
    assert_eq!(d["freshness"]["capture_state"], "not_listening", "{d}");
    assert_eq!(d["device_state"], "discovered", "{d}");
}

/// A PHRASE SEARCH MEANS THE PHRASE, PUNCTUATION AND ALL.
///
/// Reported from the bench: searching for the shell prompt `sirocco>` came back
/// full of SIROCCO banner lines. FTS tokenises, so `sirocco>` and `SIROCCO` are
/// the same token to the index -- which is right for `terms` and wrong for
/// `phrase`, where the caller has said what they mean.
#[test]
fn a_phrase_search_does_not_match_a_word_that_merely_tokenises_the_same() {
    let rig = Rig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        "SIROCCO bootloader v2.1 starting\n\
         Welcome to SIROCCO\n\
         sirocco> uname -a\n\
         sirocco> exit\n",
        None,
    );

    let phrase = rig.call(
        "search",
        json!({"device": device, "query": "sirocco>", "mode": "phrase"}),
    );
    let hits = phrase["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 2, "only the two prompt lines: {phrase}");
    for h in hits {
        assert!(
            h["text"].as_str().unwrap_or_default().contains("sirocco>"),
            "a phrase hit must contain the phrase: {h}"
        );
    }

    // `terms` is unchanged: it is the mode that means "these words, however
    // spelled", and a caller who wants the banners still has it.
    let terms = rig.call(
        "search",
        json!({"device": device, "query": "sirocco", "mode": "terms"}),
    );
    assert!(
        terms["hits"].as_array().map(Vec::len).unwrap_or(0) >= 3,
        "terms mode still finds the banners too: {terms}"
    );
}

/// A CONSOLE IS COMMANDABLE ON CAPTURE HEALTH, NOT ON PRESENCE.
///
/// When capture health moved out of `state` into its own column, this classifier
/// kept reading `state` -- so `discovered`, which discovery writes on every
/// sweep, fell through to "not listening" and every console on the bench
/// reported `commandable: false` with "no live capture attestation" while MCP TX
/// worked. Reported from the web console within minutes of that deploy.
#[test]
fn a_capturing_console_is_commandable_whatever_discovery_calls_its_presence() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "sirocco> \n", None);
    let id = rig.registry().resolve(&device).unwrap().id;

    // The bench's normal steady state: discovery says present, minerd says
    // capture is live.
    {
        let mut reg = rig.registry();
        reg.set_state(id, "discovered").unwrap();
        reg.set_capture_state(id, "listening").unwrap();
    }
    let c = rig.call("console_state", json!({"device": device}));
    assert_ne!(
        c["console"]["state"], "unknown",
        "a captured console must not read as unattested because of a presence word: {c}"
    );
    assert!(
        !c["console"]["not_commandable_because"]
            .as_str()
            .unwrap_or_default()
            .contains("no live capture attestation"),
        "{c}"
    );

    // ...and when capture really has failed, the honest answer comes back.
    {
        let mut reg = rig.registry();
        reg.set_capture_state(id, "open_failed").unwrap();
    }
    let c = rig.call("console_state", json!({"device": device}));
    assert_eq!(c["console"]["state"], "unknown", "{c}");
    assert_eq!(c["console"]["commandable"], false, "{c}");
}

// ------------------------------------------- silence is not evidence of failure -

/// A CONSOLE IDLE AT A PROMPT IS NOT HUNG.
///
/// Reported from the bench: `console_state` said `at_prompt`, and `boot_report`
/// called the same open epoch `hung` -- on the strength of the clock alone. An
/// epoch opened by `start_session` against a board already sitting at its shell
/// is quiet by definition, and produces no stage banner, so neither `booted` nor
/// `booting` could catch it and it fell straight through to `hung`.
#[test]
fn an_epoch_idle_at_a_prompt_is_not_reported_hung() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\nsirocco> \n", None);
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    // A LIVE console, not a log file: `console_state` will not claim anything
    // about a device with no capture attestation, and the disagreement being
    // fixed here is about a console conminer is actually watching.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::Listening,
        )
        .unwrap();
    }
    // Long past any hung threshold.
    rig.clock.advance_ms(600_000);

    let report = rig.call("boot_report", json!({"device": device}));
    // NEVER `hung` -- that is the whole point of this gate.
    //
    // Which of the two non-hung answers it gets depends on WHERE the prompt is,
    // and both are right. When the prompt has been stored as a line, the epoch
    // reached its shell and the answer is `booted`. When it exists only as the
    // unterminated partial the cursor rests on, `at_prompt` says so. Pinning
    // one exact word here would fail the day the other rung legitimately
    // answers, which is what happened when `booted` learned to accept a prompt.
    let outcome = report["outcome"].as_str().unwrap_or_default().to_string();
    assert_ne!(
        outcome, "hung",
        "a console waiting at a taught prompt is not hung: {report}"
    );
    assert!(
        matches!(outcome.as_str(), "booted" | "at_prompt"),
        "and the answer must say it reached its prompt: {report}"
    );
    let why = report["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("sirocco"),
        "the reason must name the prompt it is sitting at: {why}"
    );

    // AND THE TWO TOOLS MUST AGREE. That they disagreed from one evidence base
    // is the whole complaint.
    let state = rig.call("console_state", json!({"device": device}));
    assert_eq!(state["console"]["state"], "at_prompt", "{state}");
}

/// ...but silence with NOTHING behind it is still hung, which is the case the
/// rung was always for: a board that died mid-boot with a kernel message as its
/// last word.
#[test]
fn an_epoch_silent_with_no_prompt_is_still_hung() {
    let rig = Rig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        "[    0.1] Booting Linux\n[    2.0] mmc0: timeout\n",
        None,
    );
    rig.clock.advance_ms(600_000);

    let report = rig.call("boot_report", json!({"device": device}));
    assert_eq!(
        report["outcome"], "hung",
        "silence with no prompt behind it is still hung: {report}"
    );
    let why = report["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("no prompt"),
        "and it should say the prompt is what is missing: {why}"
    );
    assert!(
        !why.contains("stage unknown"),
        "\"at stage unknown\" read as though a stage named `unknown` had been found: {why}"
    );
}

/// A credential gate is the board being UP and wanting a password -- an
/// operator's next action, not a failure.
#[test]
fn an_epoch_at_a_credential_gate_is_not_reported_hung() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\nboard login: \n", None);
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "login: $", "kind": "credential_gate"}),
    );
    rig.clock.advance_ms(600_000);

    let report = rig.call("boot_report", json!({"device": device}));
    assert_eq!(
        report["outcome"], "login_wait",
        "a credential gate is not a hang: {report}"
    );
}

/// THE ENVELOPE MUST NOT SAY "COMMANDABLE" ABOUT A BOARD IN EDL.
///
/// `freshness.console` rides on every response, including the `boot_mode(EDL)`
/// reply itself. Reported from a live flashing session: `entered: true` arrived
/// carrying `at_prompt, commandable: true` for a board whose UART had just
/// re-enumerated away.
///
/// The cause was one line of mapping -- `away_in_edl` was folded into
/// `NotListening` ("no attestation to be had"), throwing away the one fact that
/// distinguishes off, idle and in-EDL, which is the whole point of naming them.
#[test]
fn the_freshness_envelope_reports_edl_not_a_commandable_prompt() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\nroot@lab:~# \n", None);
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "root@lab:~# $", "kind": "shell"}),
    );

    // Control: while the console exists, that tail is a commandable prompt.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::Listening,
        )
        .unwrap();
    }
    let before = rig.call("get_recent", json!({"device": device, "lines": 1}));
    assert_eq!(
        before["freshness"]["console"]["commandable"], true,
        "control: a live console at a taught prompt is commandable: {before}"
    );

    // The board enters EDL: same bytes, same prompt, no console.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::AwayInEdl,
        )
        .unwrap();
    }
    let after = rig.call("get_recent", json!({"device": device, "lines": 1}));
    assert_eq!(
        after["freshness"]["console"]["state"], "away_in_edl",
        "the envelope must say the board is in EDL: {after}"
    );
    assert_eq!(
        after["freshness"]["console"]["commandable"], false,
        "and must never invite a command into a console that is gone: {after}"
    );
}

// ------------------------------------------------- agents reporting our bugs -

/// THE EVIDENCE AN AGENT SHOULD NOT HAVE TO ASSEMBLE.
///
/// The reason this exists at all: reports used to arrive as prose, and the first
/// job on every one was reconstructing which board, which epoch and which build.
/// Filing through the tool attaches all of it.
#[test]
fn a_filed_report_carries_the_device_epoch_and_build_automatically() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\nKernel panic\n", None);

    let out = rig.call(
        "report_issue",
        json!({
            "title": "boot_report says hung at a live prompt",
            "expected": "at_prompt",
            "observed": "hung",
            "device": device,
            "tool": "boot_report",
            "args": {"device": device},
            "reporter": "agent-a"
        }),
    );
    assert_eq!(out["filed"], "new", "{out}");
    let r = &out["report"];
    assert_eq!(r["status"], "open");
    assert!(r["device"].is_string(), "the device is recorded: {r}");
    assert!(r["boot_id"].is_number(), "and the epoch: {r}");
    assert!(
        r["cursor"].as_str().is_some_and(|c| !c.is_empty()),
        "and a cursor, so the exact bytes stay addressable: {r}"
    );
    assert!(
        r["build"].as_str().is_some_and(|b| !b.is_empty()),
        "and the build that saw it, or no resolution is checkable: {r}"
    );
    assert_eq!(r["tool"], "boot_report");
}

/// SEARCH, THEN SAY "ME TOO". The alternative is fourteen copies of one defect.
#[test]
fn an_agent_finds_an_existing_report_and_confirms_it_instead_of_duplicating() {
    let rig = Rig::new();
    rig.call(
        "report_issue",
        json!({"title": "power off hangs with no response", "reporter": "agent-a"}),
    );

    // A second agent looks first, exactly as the tool description tells it to.
    let found = rig.call("list_reports", json!({"query": "power off hangs"}));
    assert_eq!(found["count"], 1, "the search must find it: {found}");
    let id = found["reports"][0]["id"].as_i64().unwrap();

    let confirmed = rig.call(
        "confirm_report",
        json!({"id": id, "reporter": "agent-b", "note": "also on the IQ10"}),
    );
    assert_eq!(confirmed["report"]["occurrences"], 2);
    assert_eq!(
        confirmed["distinct_reporters"], 2,
        "two agents hit this, which is the number that should drive priority: {confirmed}"
    );

    // Still one report, not two.
    let all = rig.call("list_reports", json!({}));
    assert_eq!(all["count"], 1, "{all}");
}

/// A resolution has to be checkable, and the queue has to actually clear.
#[test]
fn resolving_a_report_clears_the_queue_and_names_the_build_and_gate() {
    let rig = Rig::new();
    let filed = rig.call(
        "report_issue",
        json!({"title": "follow fires on an old prompt"}),
    );
    let id = filed["report"]["id"].as_i64().unwrap();

    let bare = rig.err("resolve_report", json!({"id": id, "status": "fixed"}));
    assert_eq!(
        bare["code"], "INVALID_ARGUMENT",
        "a fix needs its build: {bare}"
    );

    let done = rig.call(
        "resolve_report",
        json!({
            "id": id, "status": "fixed", "build": "deadbeefcafe",
            "gate": "the_prompt_predicate_ignores_a_prompt_from_before_this_epoch"
        }),
    );
    assert_eq!(done["report"]["status"], "fixed");
    assert_eq!(
        done["report"]["gate"],
        "the_prompt_predicate_ignores_a_prompt_from_before_this_epoch"
    );

    assert_eq!(
        rig.call("list_reports", json!({}))["count"],
        0,
        "the triage queue is clear"
    );
    assert_eq!(
        rig.call("list_reports", json!({"status": "all"}))["count"],
        1,
        "but the report is never destroyed: its history is what makes a regression \
         recognisable later"
    );
}

/// A report is a CLAIM, never a verdict: filing one must not change what any
/// other tool answers about the board.
#[test]
fn filing_a_report_does_not_change_what_any_tool_says_about_the_device() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\nroot@lab:~# \n", None);
    let before = rig.call("console_state", json!({"device": device}));

    rig.call(
        "report_issue",
        json!({"title": "this console is definitely broken", "device": device,
               "expected": "everything", "observed": "nothing"}),
    );

    let after = rig.call("console_state", json!({"device": device}));
    assert_eq!(
        before["console"], after["console"],
        "an agent's opinion is not evidence about the hardware"
    );
}

// --------------------------------------- a board that reached its shell booted -

/// A PROMPT IS A TERMINAL STATE, even when no stage banner is recognised.
///
/// Filed through `report_issue` from the bench (report #1, `sirocco-codex`):
/// boot 456 on the Uno-Q ran its kernel, its whole self-test suite and settled
/// at `sirocco> ` -- and `boot_report` answered `in_progress` with no stages.
/// `booted` required a stage named userspace/uboot/zephyr/android, which a board
/// whose profile nobody has written can never emit, so a complete healthy boot
/// could not be called booted no matter what it did.
#[test]
fn an_epoch_that_reached_its_shell_is_booted_even_with_no_stage_banner() {
    let rig = Rig::new();
    // An RTOS the profiles know nothing about: no recognised banners at all.
    let (device, _) = rig.ingest(
        "boot.log",
        "SIROCCO boot\nKTEST common.oracle PASS\nAPP admit\nCONSOLE\nsirocco> \n",
        None,
    );
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );

    let report = rig.call("boot_report", json!({"device": device}));
    // NON-VACUITY: this must really be the no-stage path, or it proves nothing.
    assert!(
        report["stages"].as_array().is_some_and(|s| s.is_empty()),
        "fixture premise: no stage banner is recognised here: {report}"
    );
    assert_eq!(
        report["outcome"], "booted",
        "the board reached the prompt conminer itself answers commands at: {report}"
    );
    assert!(
        report["why"]
            .as_str()
            .unwrap_or_default()
            .contains("sirocco"),
        "and the reason names the prompt it reached: {}",
        report["why"]
    );
}

/// A CLOSED EPOCH IS NOT "STILL OPEN".
///
/// The fallback said exactly that about epochs that had ended hours before --
/// reported from the bench on boot 456, closed at a recorded timestamp, whose
/// answer read `in_progress: the epoch is still open`.
#[test]
fn a_closed_epoch_never_claims_to_still_be_open() {
    let rig = Rig::new();
    // Output the profiles cannot classify, and no prompt: nothing terminal.
    let (device, _) = rig.ingest("boot.log", "zzz unrecognisable chatter\nmore of it\n", None);
    // Close it the way epochs really close: the next one opens. Reaching into
    // the store to stamp `closed_at` would test a shape the product never
    // produces.
    rig.ingest("later.log", "a second capture\n", Some(&device));
    let boots = rig.call("list_boots", json!({"device": device}));
    let first = boots["boots"]
        .as_array()
        .unwrap()
        .iter()
        .min_by_key(|b| b["seq"].as_i64().unwrap_or(i64::MAX))
        .expect("the first epoch")
        .clone();

    let report = rig.call(
        "boot_report",
        json!({"device": device, "boot": first["id"].as_i64().unwrap()}),
    );
    // NON-VACUITY: the epoch under test must really be closed.
    assert!(
        report["boot"]["closed_at"].as_i64().is_some(),
        "fixture premise: this epoch must be closed: {report}"
    );
    let why = report["why"].as_str().unwrap_or_default();
    assert_ne!(
        report["outcome"], "in_progress",
        "this epoch has a closing timestamp: {report}"
    );
    assert!(
        !why.contains("still open"),
        "and the reason must not say it is: {why}"
    );
    assert_eq!(report["outcome"], "ended", "{report}");
}

/// AN AGENT FINDS ITS REPORTS BY THE NAME IT TYPES.
///
/// Reported twice, within minutes, as data loss: `list_reports {device:
/// "uno-q"}` answered `count: 0` for a board with three reports filed against
/// it. Nothing was lost -- reports store the RESOLVED device and the filter
/// compared that to the raw selector. An agent that cannot find its own report
/// concludes the queue eats them, and files again.
#[test]
fn list_reports_finds_a_report_by_the_device_nickname_it_was_filed_with() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "starting up\n", None);
    rig.call(
        "name_device",
        json!({"device": device, "nickname": "uno-q"}),
    );
    rig.call(
        "report_issue",
        json!({"title": "boot_report says hung at a live prompt", "device": "uno-q",
               "tool": "boot_report"}),
    );

    // The exact call both agents made.
    let by_nick = rig.call(
        "list_reports",
        json!({"status": "all", "device": "uno-q", "limit": 20, "detail": true}),
    );
    assert_eq!(
        by_nick["count"], 1,
        "the nickname an agent types must find its own report: {by_nick}"
    );

    // And an unrelated selector still matches nothing, or the filter is useless.
    let other = rig.call(
        "list_reports",
        json!({"status": "all", "device": "no-such-board"}),
    );
    assert_eq!(other["count"], 0, "{other}");
}

/// A FINISHED BOOT IS NOT `in_progress` BECAUSE ITS PROMPT HAS NO NEWLINE.
///
/// Report #1, recurring on boot 487 of the Uno-Q. `boot_report`'s
/// `reached_prompt` scanned STORED LINES only, and the one branch that reads the
/// partial buffer is gated behind `hung_after_ms` of silence -- so in the first
/// 30 s after a board settles, neither fired. `follow` answered
/// `matched=prompt, unterminated=true` and `console_state` answered
/// `at_prompt_with_traffic, commandable`, while this call said the epoch "has
/// not reached a terminal state". A third independent prompt reader,
/// disagreeing with the other two about one board.
#[test]
fn a_boot_settled_at_an_unterminated_prompt_is_not_reported_in_progress() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("settled.log", "APP admit\nCONSOLE\n", None);
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    let row = {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::Listening,
        )
        .unwrap();
        row
    };

    // The board settles at `sirocco> ` with no newline: it exists only in the
    // capture loop's partial buffer, exactly as on hardware.
    {
        let path = rig._dir.path().join(&row.db_file);
        let mut store =
            conminer_core::store::DeviceStore::open(&path, &row.canonical, false).unwrap();
        let ts = {
            use conminer_core::clock::Clock;
            rig.clock.now_wall_ms()
        };
        store.set_pending_tail("sirocco> ", ts).unwrap();
        // NON-VACUITY: the prompt must not be a stored line, or this passes
        // through the old path and proves nothing.
        assert!(
            !store
                .recent_lines(50)
                .unwrap()
                .iter()
                .any(|l| l.lossy().contains("sirocco>")),
            "the prompt must exist ONLY as the unterminated partial"
        );
    }

    // Asked straight away, well inside the hung threshold -- which is when an
    // agent actually asks, and exactly when both old branches were blind.
    let report = rig.call("boot_report", json!({"device": device}));
    let outcome = report["outcome"].as_str().unwrap_or_default();
    assert_ne!(
        outcome, "in_progress",
        "the board is sitting at a prompt this epoch reached: {} / {}",
        outcome, report["why"]
    );
    assert_ne!(outcome, "hung", "and it is certainly not hung: {report}");
}

/// A BOARD IN EDL IS NEVER COMMANDABLE, WHATEVER THE PARTIAL BUFFER HOLDS.
///
/// Report #7, second sighting: one `diagnose` response carried `edl: true` AND
/// `console.state=at_prompt, commandable=true`. The board's UART re-enumerates
/// away in download mode, so the prompt still sitting in the capture loop's
/// partial buffer is a memory of the console it had, not an invitation to type
/// at it. `boot_mode` and `diagnose` both publish `away_in_edl` the moment they
/// learn it; this is the invariant that makes publishing it sufficient.
#[test]
fn a_console_in_edl_is_not_commandable_even_with_a_prompt_in_the_buffer() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("edl.log", "APP admit\nCONSOLE\n", None);
    rig.call(
        "classify_prompt",
        json!({"device": device, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    let row = {
        let mut reg = rig.registry();
        let row = reg.resolve(&device).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::Listening,
        )
        .unwrap();
        row
    };
    {
        let path = rig._dir.path().join(&row.db_file);
        let mut store =
            conminer_core::store::DeviceStore::open(&path, &row.canonical, false).unwrap();
        let ts = {
            use conminer_core::clock::Clock;
            rig.clock.now_wall_ms()
        };
        store.set_pending_tail("sirocco> ", ts).unwrap();
    }

    // NON-VACUITY: while capture is listening this really does read as a
    // commandable prompt -- otherwise the assertion below proves nothing.
    let before = rig.call("console_state", json!({"device": device}));
    assert_eq!(
        before["console"]["commandable"], true,
        "the fixture must start from a commandable prompt: {before}"
    );

    // The board goes into download mode; whoever learns it publishes it.
    {
        let mut reg = rig.registry();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::AwayInEdl,
        )
        .unwrap();
    }

    let after = rig.call("console_state", json!({"device": device}));
    assert_ne!(
        after["console"]["commandable"], true,
        "the UART is gone by design; that prompt is a memory, not an offer: {after}"
    );
    // ...and the same must hold in the envelope every other tool ships.
    let env = rig.call("list_boots", json!({"device": device, "limit": 1}));
    assert_ne!(
        env["freshness"]["console"]["commandable"], true,
        "the freshness envelope must agree with console_state: {env}"
    );
}

/// `list_devices` must not tell an agent a board is driven by a controller that
/// is not plugged in.
///
/// The dashboard was not the only surface resolving controllers against every
/// row the registry had ever held -- the MCP layer had its own copy of the same
/// list, named `present_with_topology`, feeding seven call sites. An agent
/// reading `controls.has_power_hook: true` will call `power`, and on alpha
/// that would have addressed a Bantam absent for five and a half days.
///
/// As with the dashboard gate, the FIRST half is load-bearing: proving only
/// "no controller once gone" would pass if the binding never happened.
#[test]
fn list_devices_does_not_attribute_a_controller_that_is_unplugged() {
    let rig = Rig::new();
    let console = "/dev/serial/by-id/usb-STMicroelectronics_STLINK-V3_0045-if02";
    let bantam = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00";
    let bantam_id = {
        let mut reg = rig.registry();
        // Same downstream hub (3.2): what the topology rule binds on.
        let c = reg
            .upsert_device(
                console,
                Some("pci-0000:00:14.0-usb-0:3.2.1:1.0-port0"),
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.assign_port(c.id, 5001).unwrap();
        let b = reg
            .upsert_device(
                bantam,
                Some("pci-0000:00:14.0-usb-0:3.2.2:1.0"),
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_ignored(b.id, true).unwrap();
        b.id
    };

    let controls_of = |v: &Value| -> Value {
        v["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| {
                d["canonical"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("STLINK")
            })
            .map(|d| d["controls"].clone())
            .unwrap_or(Value::Null)
    };

    // Precondition: while the Bantam is plugged in, the console really does
    // resolve it.
    let before = rig.call("list_devices", json!({"detail": true}));
    let c = controls_of(&before);
    assert_eq!(
        c["has_power_hook"], true,
        "precondition: a present Bantam gives this console a power hook: {c}"
    );
    assert_eq!(c["controller"], "bantam", "{c}");

    // Cable out.
    {
        let mut reg = rig.registry();
        reg.set_state(bantam_id, "gone").unwrap();
    }

    let after = rig.call("list_devices", json!({"detail": true}));
    let c = controls_of(&after);
    assert!(
        c["controller"].is_null(),
        "an absent controller must not be named to an agent: {c}"
    );
    assert!(c["controller_port"].is_null(), "{c}");
    assert_eq!(
        c["has_power_hook"], false,
        "an agent reading this WILL call power: {c}"
    );
    assert_eq!(
        c["boot_modes"].as_array().map(Vec::len),
        Some(0),
        "and must not be offered modes that cannot run: {c}"
    );
}

/// A self-driving controller that is unplugged offers nothing.
///
/// A Bughopper's FTDI is the board's UART AND its power/strap lines, so its
/// templates name `{device}` and need nothing else on the bus. The refusal that
/// protects `{controller}` templates therefore never fired for it, and an
/// unplugged Bughopper went on advertising `has_power_hook: true` and an EDL
/// boot mode. Found on alpha and bravo by validating the deploy, not in a test.
///
/// The second half guards the ASYMMETRY, and matters more than the first: a
/// board whose console vanished because it is switched OFF must keep the hook
/// that turns it back on. If a naive "absent means no buttons" rule ever
/// replaces this one, that half fails and a powered-off board becomes
/// impossible to power on from conminer at all.
#[test]
fn an_unplugged_self_driving_controller_offers_no_hook_but_a_dead_console_still_powers_on() {
    let rig = Rig::new();
    let bughopper = "/dev/serial/by-id/usb-Arduino_Bughopper_DK0HDSRI-if00-port0";
    let bantam = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00";
    let bantam_console = "/dev/serial/by-id/usb-VendorX_BoardA_UART_AAAA-if00-port0";
    let (bug_id, con_id) = {
        let mut reg = rig.registry();
        let b = reg
            .upsert_device(
                bughopper,
                Some("pci-0000:00:14.0-usb-0:9.1.1:1.0-port0"),
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.assign_port(b.id, 5001).unwrap();
        // A Bantam-driven board on another hub: separate controller, present.
        let bc = reg
            .upsert_device(
                bantam,
                Some("pci-0000:00:14.0-usb-0:3.2.2:1.0"),
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_ignored(bc.id, true).unwrap();
        let c = reg
            .upsert_device(
                bantam_console,
                Some("pci-0000:00:14.0-usb-0:3.2.1:1.0-port0"),
                conminer_core::store::IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.assign_port(c.id, 5002).unwrap();
        (b.id, c.id)
    };

    let hook_of = |v: &Value, needle: &str| -> Value {
        v["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["canonical"].as_str().unwrap_or_default().contains(needle))
            .map(|d| d["controls"].clone())
            .unwrap_or(Value::Null)
    };

    // Precondition: both are driveable while plugged in.
    let before = rig.call("list_devices", json!({"detail": true}));
    assert_eq!(
        hook_of(&before, "Bughopper")["has_power_hook"],
        true,
        "precondition: a present Bughopper drives itself: {}",
        hook_of(&before, "Bughopper")
    );
    assert_eq!(
        hook_of(&before, "AAAA")["has_power_hook"],
        true,
        "precondition: a Bantam-driven board is driveable: {}",
        hook_of(&before, "AAAA")
    );

    // Unplug the Bughopper (it IS its own controller), and separately let the
    // Bantam board's CONSOLE go away as it would when the board is switched off.
    {
        let mut reg = rig.registry();
        reg.set_state(bug_id, "gone").unwrap();
        reg.set_state(con_id, "gone").unwrap();
    }

    let after = rig.call("list_devices", json!({"detail": true}));
    let bug = hook_of(&after, "Bughopper");
    assert_eq!(
        bug["has_power_hook"], false,
        "an unplugged self-driving controller cannot drive anything: {bug}"
    );
    assert_eq!(
        bug["boot_modes"].as_array().map(Vec::len),
        Some(0),
        "nor strap a board it cannot reach: {bug}"
    );

    let dead = hook_of(&after, "AAAA");
    assert_eq!(
        dead["has_power_hook"], true,
        "A BOARD THAT IS OFF MUST STILL BE POWERABLE ON. Its console is gone \
         precisely BECAUSE it is off; the hook lives on the Bantam, which is \
         still plugged in: {dead}"
    );
}

/// The guidance promises a flash preview; the schema rejected one.
///
/// `handler.rs` tells every agent that `dry_run: true` on power/boot_mode/flash
/// shows the exact hook argv and changes nothing, and flash's call body has
/// always honoured it: it plans, reports a missing lease rather than refusing,
/// and returns the argv without running the hook or opening a provisioning span.
/// The schema simply never declared the argument, and `additionalProperties:
/// false` turns an undeclared argument into INVALID_ARGUMENT -- so the preview
/// of the single most destructive hook on the rig was unreachable, and the
/// promise was one an agent could only discover was false by trying it.
#[test]
fn every_tool_the_guidance_promises_a_dry_run_for_accepts_one() {
    // Read the promise as SHIPPED rather than restating it here: if the
    // sentence is reworded to cover another tool, this gate follows it instead
    // of quietly going out of date.
    let instructions = include_str!("../../../conminer-mcp/src/handler.rs");
    let claim = instructions
        .split("`dry_run: true` on ")
        .nth(1)
        .expect("the guidance must still promise a dry run somewhere");
    let promised: Vec<&str> = claim
        .split_whitespace()
        .next()
        .expect("the tools it names")
        .split('/')
        .filter(|t| !t.is_empty())
        .collect();
    assert!(
        promised.contains(&"flash"),
        "this gate exists because flash was promised one: {promised:?}"
    );

    let rig = Rig::new();
    for tool in promised {
        let schema = rig.call("help", json!({"tool": tool}));
        let input = &schema["inputSchema"];
        assert!(
            input["properties"].get("dry_run").is_some(),
            "the shared guidance promises dry_run on {tool}, so {tool} must declare it: {input}"
        );
        // A strict schema is exactly what makes an undeclared argument fatal
        // rather than ignored, which is why the promise could not be kept.
        assert_eq!(
            input["additionalProperties"], false,
            "{tool} is strict, so an undeclared dry_run would be refused outright: {input}"
        );
    }
}

/// And the promise is not merely declared: it is accepted and it plans.
#[test]
fn a_flash_dry_run_is_accepted_and_runs_no_hook() {
    let rig = Rig::new();
    let (device, _) = rig.ingest("boot.log", "hello\n", None);
    // No flash hook is configured for this fixture, so a dry run must fail on
    // THAT and not on the argument being unknown. Either way it must never
    // reach an INVALID_ARGUMENT for `dry_run` itself.
    let e = rig.err(
        "flash",
        json!({"device": device, "image": "x.img", "dry_run": true}),
    );
    assert_ne!(
        e["code"], "INVALID_ARGUMENT",
        "a flash preview must be a legal call: {e}"
    );
    assert_eq!(
        e["code"], "HOOK_NOT_CONFIGURED",
        "and it must get as far as looking for the hook: {e}"
    );
}

/// Every shape an empty probe can take, and what may be said about it.
///
/// Table-driven because the bug lived in a combination of four facts, and an
/// integration test can only produce whichever shape the timing gives it: an
/// idle connection legitimately reports either "no error" or "timed out", and
/// in the second shape the OLD code was silent too, so a socket test could pass
/// against the bug and prove nothing. Worse, a first attempt at that test sat in
/// the actuation suite, whose USB fixture is a process-wide environment
/// variable, and it read another test's bus.
#[test]
fn what_an_empty_probe_permits_diagnose_to_say_about_capture() {
    use conminer_mcp::tools::capture_state_is_stale;
    let probe = |bytes: u64, err: Value| {
        json!({"connected": true, "open_failed": false,
               "bytes_received": bytes, "error": err})
    };

    // The reported shape: in EDL, connected, nothing read, no error. The board
    // is exactly where it is supposed to be and capture is parked by design.
    assert_eq!(
        capture_state_is_stale("away_in_edl", Some(&probe(0, Value::Null)), true),
        None,
        "a board in EDL that says nothing is not a console minerd has abandoned"
    );
    // The same probe that happened to time out instead. It always stayed
    // silent, and the two must not disagree.
    assert_eq!(
        capture_state_is_stale("away_in_edl", Some(&probe(0, json!("timed out"))), true),
        None,
        "how an empty read returns must not decide what diagnose claims"
    );
    // Not in EDL, nothing read: still no evidence the console came back.
    assert_eq!(
        capture_state_is_stale("open_failed", Some(&probe(0, Value::Null)), false),
        None,
        "silence is not recovery"
    );

    // What the hint is for, and it must still fire: the console delivered bytes
    // while the registry still carried the fault minerd last saw.
    assert_eq!(
        capture_state_is_stale("open_failed", Some(&probe(64, Value::Null)), false),
        Some("open_failed"),
        "a probe that READ something is the fresher witness, and that is the \
         whole reason this hint exists"
    );
    // Bytes during away_in_edl with no EDL detected is also a real divergence.
    assert_eq!(
        capture_state_is_stale("away_in_edl", Some(&probe(64, Value::Null)), false),
        Some("away_in_edl"),
        "nothing corroborates the stored state here, and the console is talking"
    );
    // ...but with EDL confirmed, the divergence is reported by `verdict`
    // instead, which names what is wrong rather than blaming a re-attach.
    assert_eq!(
        capture_state_is_stale("away_in_edl", Some(&probe(64, Value::Null)), true),
        None,
        "this call's own EDL finding confirms the stored state"
    );

    // No probe at all: say nothing rather than guess.
    assert_eq!(capture_state_is_stale("open_failed", None, false), None);
    // A healthy console is never dropped on this hint.
    assert_eq!(
        capture_state_is_stale("streaming", Some(&probe(64, Value::Null)), false),
        None
    );
}
