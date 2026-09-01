//! Suite `integration` (§15) — the round-2 gap items that turn the miner into
//! lab infrastructure rather than a diagnostic toy.
//!
//! Covers: build-identity correlation and `diff_builds` (§15.4) · pstore
//! ingestion (§15.6) · LAVA-native ingestion (§15.7) · multi-console targets
//! (§15.8) · auto-baud recovery (§15.9) · file transfer (§15.10) · CI gating
//! policy (§15.11) · backpressure (§14.3) · symbolization (§15.5).

use conminer_core::store::SessionSource;
use conminer_core::{codec, symbolize, target, transfer};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use conminer_testkit::Rig;
use serde_json::{json, Value};
use std::sync::Arc;

struct Mcp {
    dir: tempfile::TempDir,
    h: Handler,
}

impl Mcp {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = conminer_core::config::Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let ctx = Context::open(
            cfg,
            Arc::new(conminer_core::framer::ProfileSet::builtin().unwrap()),
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        Self {
            dir,
            h: Handler::new(ctx),
        }
    }

    fn raw(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        self.h.handle(req).unwrap().result.unwrap()
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(v["isError"], false, "{name}: {}", v["structuredContent"]);
        v["structuredContent"].clone()
    }

    fn err(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(v["isError"], true, "{name} unexpectedly succeeded");
        v["structuredContent"]["error"].clone()
    }

    fn write(&self, name: &str, body: &[u8]) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.display().to_string()
    }
}

// ------------------------------------------------- §15.7 LAVA ingestion ------

const LAVA_JOB: &str = r#"- {"dt": "2026-01-04T12:00:00", "lvl": "info", "msg": "start: 2 uboot-action"}
- {"dt": "2026-01-04T12:00:01", "lvl": "target", "msg": "U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)"}
- {"dt": "2026-01-04T12:00:02", "lvl": "target", "msg": "Starting kernel ..."}
- {"dt": "2026-01-04T12:00:03", "lvl": "target", "msg": "[    0.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP"}
- {"dt": "2026-01-04T12:00:04", "lvl": "target", "msg": "[    1.470000] Kernel panic - not syncing: VFS: Unable to mount root fs"}
- {"dt": "2026-01-04T12:00:05", "lvl": "debug", "msg": "Waiting for prompt"}
"#;

#[test]
fn a_lava_job_log_is_a_first_class_input_with_no_preprocessing() {
    let m = Mcp::new();
    let path = m.write("job.yaml", LAVA_JOB.as_bytes());
    let r = m.call("ingest_file", json!({"path": path}));
    assert_eq!(r["ingest"]["wrapper"], "lava");

    let device = r["device"].as_str().unwrap().to_string();
    // The dispatcher's framing is gone and the console text was mined normally.
    let t = m.call(
        "list_templates",
        json!({"device": device, "order": "severity", "limit": 5}),
    );
    let texts: Vec<&str> = t["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["text"].as_str().unwrap())
        .collect();
    assert!(
        texts.iter().any(|x| x.contains("Kernel panic")),
        "{texts:?}"
    );
    // …and the stage machine saw the boot chain inside the job log.
    let stages = m.call("boot_stages", json!({"device": device}));
    let names: Vec<&str> = stages["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"kernel"), "{names:?}");
}

#[test]
fn an_ordinary_capture_is_not_mistaken_for_a_container() {
    assert_eq!(
        codec::detect(b"[    0.000000] Linux version 6.12.9\n"),
        codec::Wrapper::None
    );
}

// -------------------------------------------------- §15.6 pstore ingestion ---

#[test]
fn a_pstore_dump_is_mined_as_the_crash_the_console_missed() {
    let m = Mcp::new();
    // Seed a device with a live session, as if the board had rebooted.
    let seed = m.write("boot.log", b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n");
    let device = m.call("ingest_file", json!({"path": seed}))["device"]
        .as_str()
        .unwrap()
        .to_string();

    let dump = m.write(
        "dmesg-ramoops-0",
        b"Panic#1 Part1\n\
          <4>[  123.456] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n\
          <4>[  123.456] Modules linked in: qcom_q6v5_pas\n\
          <4>[  123.456] Call trace:\n\
          <4>[  123.456]  really_probe+0xc8/0x3a0\n",
    );
    let r = m.call("ingest_pstore", json!({"device": device, "path": dump}));
    assert_eq!(r["source"], "pstore");
    assert!(r["ingest"]["crash_records"].as_i64().unwrap() >= 1);

    // Tagged so it is distinguishable from live capture.
    let sessions = m.call("list_sessions", json!({"device": device}));
    assert_eq!(sessions["sessions"][0]["source"], "pstore");
    assert!(sessions["sessions"][0]["label"]
        .as_str()
        .unwrap()
        .contains("pstore"));
}

// -------------------------------------- §15.4 build identity and diff_builds -

#[test]
fn diff_builds_aggregates_across_every_epoch_of_each_build() {
    let m = Mcp::new();
    let a = m.write(
        "a.log",
        b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n[ 1.0] mmc0: card ready\n",
    );
    let device = m.call("ingest_file", json!({"path": a}))["device"]
        .as_str()
        .unwrap()
        .to_string();
    m.call("acquire", json!({"device": device}));
    m.call(
        "set_image",
        json!({"device": device, "name": "v1", "git_sha": "aaaa111"}),
    );

    let b = m.write(
        "b.log",
        b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n[ 1.0] mmc0: card ready\n\
          [ 2.0] regression: new failure mode appeared\n",
    );
    m.call("ingest_file", json!({"path": b, "device": device}));
    m.call("mark", json!({"device": device, "label": "v2"}));
    m.call(
        "set_image",
        json!({"device": device, "name": "v2", "git_sha": "bbbb222"}),
    );

    let d = m.call(
        "diff_builds",
        json!({"device": device, "a": "v1", "b": "v2"}),
    );
    assert_eq!(d["a"]["build"], "v1");
    assert_eq!(d["b"]["build"], "v2");
    assert!(d["a"]["epochs"].as_i64().unwrap() >= 1);
    assert!(d["b"]["epochs"].as_i64().unwrap() >= 1);

    // A build nobody bound is a structured error, not an empty diff.
    let e = m.err(
        "diff_builds",
        json!({"device": device, "a": "v1", "b": "v99"}),
    );
    assert_eq!(e["code"], "UNKNOWN_BOOT");
    assert!(e["hint"].as_str().unwrap().contains("set_image"));
}

// ---------------------------------------------------- §15.11 gating policy ---

#[test]
fn evaluate_policy_is_a_machine_verdict_with_its_evidence() {
    let m = Mcp::new();
    let path = m.write(
        "run.log",
        b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n\
          [ 1.0] mmc0: card ready\n\
          [ 1.4] Kernel panic - not syncing: VFS: Unable to mount root fs\n\
          [ 1.4] ---[ end Kernel panic - not syncing: VFS ]---\n",
    );
    let r = m.call("ingest_file", json!({"path": path}));
    let device = r["device"].as_str().unwrap().to_string();
    let session = r["ingest"]["session_id"].as_i64().unwrap();

    let fail = m.call(
        "evaluate_policy",
        json!({"device": device, "session": session}),
    );
    assert_eq!(fail["verdict"], "fail");
    assert!(fail["summary"].as_str().unwrap().starts_with("FAIL"));
    let violations = fail["violations"].as_array().unwrap();
    assert!(!violations.is_empty());
    let offending = violations[0]["template_id"].as_i64().unwrap();

    // An allowlisted known-flaky template waives it.
    let pass = m.call(
        "evaluate_policy",
        json!({
            "device": device, "session": session,
            "allow_templates": violations.iter()
                .map(|v| v["template_id"].as_i64().unwrap()).collect::<Vec<_>>(),
        }),
    );
    assert_eq!(pass["verdict"], "pass");
    assert!(pass["waived"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["template_id"] == offending));

    // Raising the threshold above the finding also passes.
    let lenient = m.call(
        "evaluate_policy",
        json!({"device": device, "session": session, "fail_at_or_above": "emerg"}),
    );
    let strict = m.call(
        "evaluate_policy",
        json!({"device": device, "session": session, "fail_at_or_above": "warn"}),
    );
    assert!(
        strict["violations"].as_array().unwrap().len()
            >= lenient["violations"].as_array().unwrap().len(),
        "a stricter threshold cannot find less"
    );
}

#[test]
fn fail_only_on_novel_crash_lets_a_known_bad_board_through() {
    let m = Mcp::new();
    let text = b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n\
                 [ 1.4] Kernel panic - not syncing: VFS: Unable to mount root fs\n";
    let first = m.write("first.log", text);
    let r = m.call("ingest_file", json!({"path": first}));
    let device = r["device"].as_str().unwrap().to_string();

    // Second run of the same failure: nothing is novel any more.
    let second = m.write("second.log", text);
    let r2 = m.call("ingest_file", json!({"path": second, "device": device}));
    let s2 = r2["ingest"]["session_id"].as_i64().unwrap();

    let novel_only = m.call(
        "evaluate_policy",
        json!({"device": device, "session": s2, "novel_only": true}),
    );
    assert_eq!(
        novel_only["verdict"], "pass",
        "a known-bad board must not fail every run under novel-only mode"
    );
    let all = m.call("evaluate_policy", json!({"device": device, "session": s2}));
    assert_eq!(
        all["verdict"], "fail",
        "…but the default mode still reports it"
    );
}

// ------------------------------------------------- §15.8 multi-console targets

#[test]
fn a_target_groups_consoles_and_interleaves_them_by_host_time() {
    let m = Mcp::new();
    let ap = m.write("ap.log", b"[ 9999.0] AP: Kernel panic - not syncing: x\n");
    let ec = m.write("ec.log", b"[    1.0] EC: Watchdog! Reset cause: AP hang\n");
    let ap_dev = m.call("ingest_file", json!({"path": ap}))["device"]
        .as_str()
        .unwrap()
        .to_string();
    let ec_dev = m.call("ingest_file", json!({"path": ec}))["device"]
        .as_str()
        .unwrap()
        .to_string();

    {
        let mut reg = m.h.context().registry();
        for name in [&ap_dev, &ec_dev] {
            let d = reg.resolve(name).unwrap();
            reg.set_target(d.id, Some("rb3")).unwrap();
        }
    }

    let targets = m.call("list_targets", json!({}));
    assert_eq!(targets["count"], 1);
    assert_eq!(
        targets["targets"][0]["members"].as_array().unwrap().len(),
        2
    );

    // A moment in host time, with both consoles merged around it.
    let ctx = m.call(
        "target_context",
        json!({"target": "rb3", "around_ts": 1_577_836_800_000i64, "window_ms": 600_000}),
    );
    let devices: Vec<&str> = ctx["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["device"].as_str().unwrap())
        .collect();
    assert!(devices.contains(&ap_dev.as_str()), "{devices:?}");
    assert!(devices.contains(&ec_dev.as_str()), "{devices:?}");
    let ts: Vec<i64> = ctx["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["ts_wall"].as_i64().unwrap())
        .collect();
    assert!(
        ts.windows(2).all(|w| w[0] <= w[1]),
        "merged in host-time order"
    );

    // A power event opens an epoch on every member.
    for name in [&ap_dev, &ec_dev] {
        m.call("acquire", json!({"device": name}));
    }
    let marked = m.call("target_mark", json!({"target": "rb3", "label": "cycle"}));
    assert_eq!(marked["opened"].as_array().unwrap().len(), 2);
}

#[test]
fn an_unknown_target_is_a_structured_error() {
    let m = Mcp::new();
    let e = m.err("target_context", json!({"target": "nope", "around_ts": 0}));
    assert_eq!(e["code"], "UNKNOWN_DEVICE");
}

// ------------------------------------------------------- §15.10 file transfer

#[test]
fn a_push_is_chunked_and_verified_by_the_targets_own_checksum() {
    let data = b"the quick brown fox jumps over the lazy dog".repeat(40);
    let plan = transfer::plan_push(&data, "/tmp/blob.bin", 1 << 20).unwrap();
    assert!(plan.chunks > 1, "chunked so the tty buffer cannot overrun");
    assert_eq!(plan.bytes, data.len() as u64);

    // Only the target's agreement completes it.
    transfer::verify_push(&plan, &format!("{}  /tmp/blob.bin", plan.sha256)).unwrap();
    assert!(transfer::verify_push(&plan, "0000 /tmp/blob.bin").is_err());
}

#[test]
fn a_pull_refuses_to_return_bytes_that_do_not_match() {
    let data = b"payload";
    let sha = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    };
    let b64 = transfer::base64_encode(data);
    assert_eq!(
        transfer::finish_pull(&format!("{sha}  f"), &b64).unwrap(),
        data
    );
    assert!(transfer::finish_pull("deadbeef f", &b64).is_err());
}

#[test]
fn a_strategy_needing_the_whole_port_is_refused_with_a_working_alternative() {
    let m = Mcp::new();
    let seed = m.write("seed.log", b"x\n");
    let device = m.call("ingest_file", json!({"path": seed}))["device"]
        .as_str()
        .unwrap()
        .to_string();
    m.call("acquire", json!({"device": device}));
    let local = m.write("payload.bin", b"hello");

    let e = m.err(
        "push_file",
        json!({
            "device": device, "local_path": local,
            "remote_path": "/tmp/x", "strategy": "zmodem"
        }),
    );
    assert_eq!(e["code"], "UNSUPPORTED");
    assert!(e["hint"].as_str().unwrap().contains("base64"));
}

// --------------------------------------------------------- §15.9 auto-baud ---

#[test]
fn auto_baud_picks_the_rate_whose_output_reads_as_console_text() {
    let probes = vec![
        transfer::BaudProbe {
            baud: 9600,
            printable_ratio: transfer::printable_ratio(&[0x9au8; 512]),
            sample: String::new(),
        },
        transfer::BaudProbe {
            baud: 115_200,
            printable_ratio: transfer::printable_ratio(
                b"U-Boot 2026.01 (Jan 04 2026)\r\nDRAM:  8 GiB\r\n",
            ),
            sample: "U-Boot".into(),
        },
    ];
    assert_eq!(transfer::best_rate(&probes, 0.7).unwrap().baud, 115_200);
    assert!(
        transfer::best_rate(&probes[..1], 0.7).is_none(),
        "nothing readable means no answer, not a guess"
    );
}

#[test]
fn auto_baud_stays_opt_in_because_it_perturbs_a_shared_port() {
    assert!(!conminer_core::config::Config::default().line.auto_baud);
}

// ----------------------------------------------------- §14.3 backpressure ----

#[test]
fn a_firehose_degrades_mining_but_never_capture_and_counts_what_it_skipped() {
    let rig = Rig::new();
    let mut p = rig.pipeline("firehose", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.set_sample_threshold(100);

    // One block far larger than any healthy console produces at once.
    let block: String = (0..5_000)
        .map(|i| format!("[ 1.0] trace event {i}\n"))
        .collect();
    let out = p.feed(block.as_bytes()).unwrap();

    assert_eq!(out.lines, 5_000, "every line was captured");
    assert!(p.is_degraded(), "and the degradation is visible");
    assert!(
        p.sampled_out() > 4_000,
        "mining was sampled: {} skipped",
        p.sampled_out()
    );

    p.finish().unwrap();
    let store = p.into_store();
    assert_eq!(
        store.line_count().unwrap(),
        5_000,
        "raw capture never stops — loss would be a counter, and it is zero"
    );
    assert!(store.template_count().unwrap() > 0, "sampled, not disabled");
}

#[test]
fn an_ordinary_console_is_never_degraded() {
    let rig = Rig::new();
    let mut p = rig.pipeline("normal", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    for i in 0..200 {
        p.feed(format!("[ 1.0] line {i}\n").as_bytes()).unwrap();
    }
    assert!(!p.is_degraded());
    assert_eq!(p.sampled_out(), 0);
}

// ------------------------------------------------------ §15.5 symbolization --

#[test]
fn symbolization_annotates_a_crash_without_touching_its_raw_bytes() {
    let m = Mcp::new();
    let log = m.write(
        "crash.log",
        b"[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n\
          [ 1.0] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n\
          [ 1.0] Modules linked in: foo\n\
          [ 1.0] Call trace:\n\
          [ 1.0] [<ffffffff81003040>] really_probe+0x40/0x3a0\n\
          [ 1.0] ---[ end trace 0000000000000000 ]---\n",
    );
    let r = m.call("ingest_file", json!({"path": log}));
    let device = r["device"].as_str().unwrap().to_string();

    let crash = m.call(
        "list_templates",
        json!({"device": device, "min_severity": "crit", "order": "severity"}),
    );
    let tid = crash["templates"][0]["id"].as_i64().unwrap();
    let recs = m.call(
        "get_records",
        json!({"device": device, "template_id": tid, "n": 1}),
    );
    let record_id = recs["records"][0]["record_id"].as_i64().unwrap();
    let before = recs["records"][0]["text"].as_str().unwrap().to_string();

    let symbols = m.write(
        "System.map",
        b"ffffffff81000000 T _stext\nffffffff81003000 T really_probe\n",
    );
    let sym = m.call(
        "symbolize",
        json!({"device": device, "record_id": record_id, "symbols": symbols}),
    );
    assert_eq!(
        sym["best_effort"], true,
        "resolution is explicitly best-effort"
    );
    let named: Vec<&str> = sym["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["symbol"].as_str())
        .collect();
    assert!(named.contains(&"really_probe"), "{:?}", sym["frames"]);

    // The record itself is unchanged: annotation is a derived view.
    let after = m.call(
        "get_records",
        json!({"device": device, "template_id": tid, "n": 1}),
    );
    assert_eq!(after["records"][0]["text"].as_str().unwrap(), before);
}

#[test]
fn a_bogus_symbol_file_is_refused_rather_than_producing_wrong_names() {
    assert!(symbolize::SymbolTable::parse("not a symbol table").is_err());
}

// --------------------------------------------- the whole tool surface holds --

#[test]
fn every_advertised_tool_still_has_a_usable_schema() {
    let m = Mcp::new();
    let req: Request =
        serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let tools = m.h.handle(req).unwrap().result.unwrap()["tools"]
        .as_array()
        .unwrap()
        .clone();
    // The advertised surface is the CORE profile by default (api.full_toolset),
    // so this is deliberately small -- tools/list is paid at the start of every
    // session and schemas were 65% of 45KB. What must hold is that the core set
    // is genuinely usable on its own and that `help` is in it, or nothing else
    // is reachable.
    assert!(
        tools.len() >= 12,
        "the core profile should still be a usable console surface, got {}",
        tools.len()
    );
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"help"),
        "help must be advertised or the rest is undiscoverable"
    );
    for must in ["list_devices", "run_command", "power", "diagnose"] {
        assert!(
            names.contains(&must),
            "core profile is missing {must}: {names:?}"
        );
    }

    // And EVERY tool -- advertised or not -- must have a usable schema, since
    // any of them can be called by name.
    let full = conminer_mcp::tools::advertise_profile(true)["tools"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        full.len() >= 40,
        "full profile shrank unexpectedly: {}",
        full.len()
    );
    for t in &full {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        assert_eq!(
            t["inputSchema"]["additionalProperties"], false,
            "{}",
            t["name"]
        );
    }
    for t in &tools {
        assert!(t["name"].is_string());
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        assert_eq!(
            t["inputSchema"]["additionalProperties"], false,
            "{}",
            t["name"]
        );
        assert!(
            t["description"].as_str().unwrap().len() > 40,
            "{} has no usable description",
            t["name"]
        );
    }
}

#[test]
fn the_target_module_is_reachable_without_the_tool_layer() {
    // The library is importable on its own (§2): uart-mcp, or anyone, can vendor
    // the miner without taking the MCP server with it.
    let rig = Rig::new();
    let reg = rig.registry();
    assert!(target::list(&reg).unwrap().is_empty());
}
