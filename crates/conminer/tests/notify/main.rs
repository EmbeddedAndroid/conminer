//! Suite `notify` (§13) — server-initiated notifications.
//!
//! Edge cases: novel template fires exactly once · stage transition ordering ·
//! subscriber disconnect/reconnect · notification storm during a boot loop
//! (coalescing).
//!
//! Driven through the real `Server`, including its HTTP surface, so what is
//! asserted is what a supervising agent would actually receive.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_mcp::{Context, Server};
use conminer_testkit::corpus::corpus_text;
use std::sync::Arc;

fn server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let ctx = Context::open(
        cfg,
        Arc::new(ProfileSet::builtin().unwrap()),
        Arc::new(conminer_core::clock::StepClock::default()),
    )
    .unwrap();
    (dir, Server::new(ctx))
}

#[test]
fn a_novel_template_fires_exactly_once_per_template() {
    let (_d, srv) = server();
    let ev = srv.events();
    let mut rx = ev.subscribe();

    // The same template seen a thousand times is one announcement.
    for i in 0..1000 {
        ev.novel_template("rb3-ap", 42, "Kernel panic - not syncing", i);
    }
    let n = rx.try_recv().expect("one event");
    assert_eq!(n.params["detail"]["event"], "novel_template");
    assert_eq!(n.params["detail"]["template_id"], 42);
    assert!(rx.try_recv().is_err(), "and only one");

    // A different novel template is still announced.
    ev.novel_template("rb3-ap", 43, "Unhandled fault", 1);
    assert_eq!(rx.try_recv().unwrap().params["detail"]["template_id"], 43);
}

#[test]
fn stage_transitions_arrive_in_boot_order() {
    let (_d, srv) = server();
    let ev = srv.events();
    let mut rx = ev.subscribe();

    for (i, stage) in ["bl1", "bl2", "bl31", "uboot", "kernel", "userspace"]
        .iter()
        .enumerate()
    {
        ev.stage_transition("rb3-ap", stage, false, Some(1), i as i64);
    }
    let got: Vec<String> = (0..6)
        .map(|_| {
            rx.try_recv().unwrap().params["detail"]["stage"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(got, ["bl1", "bl2", "bl31", "uboot", "kernel", "userspace"]);
}

#[test]
fn a_boot_loop_storm_coalesces_and_says_how_much_it_hid() {
    let (_d, srv) = server();
    let ev = srv.events();
    let mut rx = ev.subscribe();

    // 500 identical epochs, a minute apart in the log but milliseconds apart in
    // real time — exactly the shape that would otherwise drown the channel.
    ev.stage_transition("rb3-ap", "bl1", true, Some(0), 0);
    for i in 1..500 {
        ev.stage_transition("rb3-ap", "bl1", true, Some(i), i);
    }
    ev.stage_transition("rb3-ap", "bl1", true, Some(500), 10_000);

    let first = rx.try_recv().unwrap();
    assert!(first.params.get("coalesced").is_none());
    let second = rx.try_recv().unwrap();
    assert_eq!(second.params["coalesced"], 499);
    assert!(rx.try_recv().is_err(), "exactly two events for 500 resets");
}

#[test]
fn a_subscriber_that_disconnects_and_reconnects_resumes() {
    let (_d, srv) = server();
    let ev = srv.events();

    {
        let mut rx = ev.subscribe();
        ev.novel_template("dev", 1, "first", 0);
        assert!(rx.try_recv().is_ok());
    } // subscriber goes away mid-boot

    assert_eq!(ev.subscribers(), 0);
    ev.novel_template("dev", 2, "missed while away", 1);

    let mut rx = ev.subscribe();
    ev.novel_template("dev", 3, "after reconnect", 2);
    let n = rx.try_recv().unwrap();
    assert_eq!(
        n.params["detail"]["template_id"], 3,
        "a reconnected subscriber resumes from now, and the store still holds \
         everything it missed"
    );
}

#[test]
fn several_subscribers_all_receive_the_same_events() {
    let (_d, srv) = server();
    let ev = srv.events();
    let mut a = ev.subscribe();
    let mut b = ev.subscribe();
    assert_eq!(ev.subscribers(), 2);

    ev.novel_template("dev", 9, "shared", 0);
    assert_eq!(a.try_recv().unwrap().params["detail"]["template_id"], 9);
    assert_eq!(b.try_recv().unwrap().params["detail"]["template_id"], 9);
}

#[test]
fn console_state_transitions_are_published_once_each() {
    let (_d, srv) = server();
    let ev = srv.events();
    let mut rx = ev.subscribe();

    for (i, state) in ["booting", "at_prompt", "hung"].iter().enumerate() {
        assert!(ev.console_state("dev", state, serde_json::json!({}), i as i64));
    }
    let got: Vec<String> = (0..3)
        .map(|_| {
            rx.try_recv().unwrap().params["detail"]["state"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(got, ["booting", "at_prompt", "hung"]);
}

#[test]
fn notifications_are_addressed_to_a_resource_the_client_can_subscribe_to() {
    let (_d, srv) = server();

    // The device appears as a subscribable resource…
    let path = srv.handler().context().data_dir().join("seed.log");
    std::fs::write(&path, corpus_text("linux/boot-oops.log")).unwrap();
    let reply = srv
        .dispatch_raw(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "ingest_file", "arguments": {"path": path.display().to_string()}}
            })
            .to_string(),
        )
        .unwrap();
    assert!(!reply.contains("\"isError\":true"), "{reply}");

    let listed: serde_json::Value = serde_json::from_str(
        &srv.dispatch_raw(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#)
            .unwrap(),
    )
    .unwrap();
    let resources = listed["result"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1);
    let uri = resources[0]["uri"].as_str().unwrap();
    assert!(uri.starts_with("conminer://device/"), "{uri}");

    // …and events for it carry that exact uri.
    let mut rx = srv.events().subscribe();
    let device = uri.trim_start_matches("conminer://device/");
    srv.events().novel_template(device, 1, "x", 0);
    assert_eq!(rx.try_recv().unwrap().params["uri"], uri);
}

#[test]
fn the_server_advertises_resource_subscription_so_agents_know_to_listen() {
    let (_d, srv) = server();
    let v: serde_json::Value = serde_json::from_str(
        &srv.dispatch_raw(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["result"]["capabilities"]["resources"]["subscribe"], true);
}

/// THE INSTRUCTIONS ARE A CONTRACT WITH EVERY CLIENT, and nothing tested them.
///
/// They are prepended to the context of every agent that connects, which makes
/// them the only place a rule reaches an agent BEFORE it does the damage. Two
/// rules earn their space there, and both were learned the hard way on this
/// bench:
///
/// * Never open the console yourself. A raw telnet or nc takes no lease, is
///   paced by nobody, and its bytes belong to no epoch -- and a moment of tty
///   contention leaves ser2net serving "Device open failure" to everyone until
///   it is restarted.
/// * Report defects instead of working around them. A silent workaround costs
///   every later agent the same hour; a report with expected-versus-observed
///   becomes a regression test.
///
/// Asserted on the SERVED payload rather than on handler.rs, because what
/// matters is what a client is actually told.
#[test]
fn the_served_instructions_forbid_raw_console_access_and_point_at_reporting() {
    let (_d, srv) = server();
    let v: serde_json::Value = serde_json::from_str(
        &srv.dispatch_raw(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap(),
    )
    .unwrap();
    let text = v["result"]["instructions"]
        .as_str()
        .expect("the server must ship instructions")
        .to_string();

    for forbidden in ["telnet", "nc", "socat", "/dev/tty"] {
        assert!(
            text.contains(forbidden),
            "the instructions must name {forbidden:?} as something never to use directly"
        );
    }
    assert!(
        text.contains("ONLY WAY TO TOUCH A CONSOLE"),
        "and say plainly that this MCP is the only route: {text}"
    );

    for tool in ["report_issue", "list_reports", "confirm_report"] {
        assert!(
            text.contains(tool),
            "an agent cannot report what it does not know exists: {tool} missing"
        );
    }
    assert!(
        text.contains("EXPECTED") && text.contains("OBSERVED"),
        "and must ask for the pair that turns a report into a test"
    );

    // Every tool the instructions name must actually exist, or they teach a
    // call that fails.
    let tools: serde_json::Value = serde_json::from_str(
        &srv.dispatch_raw(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
            .unwrap(),
    )
    .unwrap();
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    for tool in [
        "report_issue",
        "list_reports",
        "confirm_report",
        "resolve_report",
    ] {
        assert!(
            names.iter().any(|n| n == tool),
            "instructions name {tool}, but the server does not serve it"
        );
    }
}
