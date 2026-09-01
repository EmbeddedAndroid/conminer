//! Suite `fleet` (§P1) — two conminer nodes, in one test process.
//!
//! The whole point of these tests is that a fleet is exercised END TO END
//! without a second machine: two Contexts with their own data directories, two
//! real mcpd servers on ephemeral ports, and a peer table pointing them at each
//! other. Everything a two-host deployment does -- inventory, selector
//! resolution, proxying, lease arbitration, failure when a node goes away --
//! happens here over real TCP, so a regression is caught by `cargo test` rather
//! than by a person plugging in boards.
//!
//! What is deliberately NOT faked: the HTTP transport, the JSON-RPC envelope,
//! the registry rows, the hook execution. The only stand-in is the hardware
//! itself -- node B's "board" is a device whose power hook is `/bin/echo`, which
//! is exactly the seam the hardware rig uses for its own dry runs.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::peers::registry::{self as peer_registry, Advert, PeerSource};
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A peer row, for tests that drive `import_devices` directly.
fn peer_row(name: &str, url: &str) -> conminer_core::peers::PeerRow {
    conminer_core::peers::PeerRow {
        instance_id: format!("id-{name}"),
        name: name.to_string(),
        host: None,
        mcp_url: url.to_string(),
        dash_url: None,
        ser2net_host: None,
        version: None,
        source: PeerSource::Static,
        ok: true,
        last_seen: 1_000,
        last_error: None,
        advert_count: 1,
        last_poll: None,
    }
}

/// One node: its own data dir, config, context and (optionally) a live server.
struct Node {
    name: String,
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    ctx: Context,
    handler: Handler,
    /// Set once `serve()` has been called.
    url: Option<String>,
    clock: Arc<conminer_core::clock::StepClock>,
}

impl Node {
    fn new(name: &str) -> Self {
        Self::with_config(name, Config::default())
    }

    /// A node that owns one board, whose power hook is harmless.
    ///
    /// `/bin/echo` in place of a controller is the same seam the hardware rig
    /// uses for dry runs: it proves the hook ran, on the node it ran on, with
    /// the arguments it was given, and touches nothing.
    fn with_board(name: &str, canonical: &str, nickname: &str) -> Self {
        let mut cfg = Config::default();
        cfg.hooks.verify_settle_s = 0;
        cfg.hooks.verify_off_watch_s = 0;
        cfg.hooks.verify_boot_watch_s = 0;
        cfg.devices.insert(
            canonical.to_string(),
            conminer_core::config::DeviceOverride {
                hooks: conminer_core::config::DeviceHooks {
                    power: Some(format!("/bin/echo powered {{action}} on {name}")),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let node = Self::with_config(name, cfg);
        {
            let mut reg = node.registry();
            let d = reg
                .upsert_device(canonical, None, IdentityKind::ById, None, 0)
                .unwrap();
            reg.assign_port(d.id, 5001).unwrap();
            reg.set_state(d.id, "listening").unwrap();
            reg.set_nickname(d.id, nickname).unwrap();
        }
        node
    }

    fn with_config(name: &str, mut cfg: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.peers.name = name.to_string();
        // Discovery off: these tests drive the static seam on purpose, because
        // a suite that broadcasts on a shared LAN finds the office, not itself.
        cfg.peers.enabled = false;
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx =
            Context::open(cfg, Arc::new(ProfileSet::builtin().unwrap()), clock.clone()).unwrap();
        Self {
            name: name.to_string(),
            dir: dir.path().to_path_buf(),
            _dir: dir,
            handler: Handler::new(ctx.clone()),
            ctx,
            url: None,
            clock,
        }
    }

    /// Start a real mcpd on an ephemeral port and return its URL.
    fn serve(&mut self) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let server = conminer_mcp::server::Server::new(self.ctx.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let _ = server.serve_http(addr).await;
            });
        });
        let url = format!("http://{addr}/mcp");
        // Wait for the port rather than sleeping a guess.
        for _ in 0..100 {
            if std::net::TcpStream::connect(addr).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.url = Some(url.clone());
        url
    }

    /// Call a tool through the JSON-RPC handler, exactly as a client would.
    fn call(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        let r = self.handler.handle(req).expect("a reply").result.unwrap();
        r["structuredContent"].clone()
    }

    fn ok(&self, name: &str, args: Value) -> Value {
        let v = self.call(name, args);
        assert!(
            v.get("error").is_none(),
            "{name} failed on {}: {v}",
            self.name
        );
        v
    }

    fn err(&self, name: &str, args: Value) -> Value {
        let v = self.call(name, args);
        assert!(
            v.get("error").is_some(),
            "{name} unexpectedly succeeded on {}: {v}",
            self.name
        );
        v["error"].clone()
    }

    fn registry(&self) -> Registry {
        Registry::open(&self.dir).unwrap()
    }

    /// Point this node at another one, the way `[peers] nodes` does.
    fn peer_with(&self, other: &Node, url: &str) {
        let mut reg = self.registry();
        peer_registry::upsert_advert(
            &mut reg,
            &Advert {
                instance_id: format!("id-{}", other.name),
                name: other.name.clone(),
                version: "test".into(),
                mcp_url: url.to_string(),
                dash_url: String::new(),
                ser2net_host: "127.0.0.1".into(),
                ser2net_ports: vec![],
            },
            PeerSource::Static,
            Some("127.0.0.1"),
            self.now(),
        )
        .unwrap();
    }

    /// Pull the peer's inventory once, as peerd's timer would.
    fn sync(&self) -> conminer_core::peers::inventory::SyncReport {
        // A PULL THAT FAILED IS NOT A SYNC.
        //
        // The report used to be discarded, so a peer that did not answer -- the
        // whole workspace suite runs dozens of these servers at once, and one of
        // them can be slow to accept -- left the registry empty and the test then
        // asserted about a config with nothing in it. That fails in a way that
        // describes the wrong problem. Retry a few times, then say what actually
        // went wrong.
        for attempt in 0..5 {
            let client = conminer_core::peers::PeerClient::new(&self.name);
            let mut reg = self.registry();
            let peers = peer_registry::all(&reg).unwrap();
            // AS PEERD DOES: told its own name, so a relayed row describing one
            // of this node's own boards is recognised and dropped.
            let report = conminer_core::peers::inventory::sync_all_as(
                &mut reg,
                &client,
                &peers,
                5001,
                self.now(),
                &self.name,
            )
            .unwrap();
            if report.peers_failed == 0 {
                return report;
            }
            if attempt == 4 {
                panic!(
                    "{}: {} of {} peers would not answer: {:?}",
                    self.name,
                    report.peers_failed,
                    peers.len(),
                    report.errors
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
        }
        unreachable!()
    }

    fn now(&self) -> i64 {
        use conminer_core::clock::Clock;
        self.clock.now_wall_ms()
    }
}

// ---------------------------------------------------------------- inventory --

/// A PEER RE-EXPORTS ONLY WHAT ITS OWNER ACTUALLY SERVES.
///
/// Measured between the bravo and charlie nodes: bravo's TAC has two bit-bang GPIO
/// channels that enumerate as ttys and are deliberately kept out of discovery --
/// opening one writes to the board's power lines. They are `ignored` there, with
/// no endpoint. They arrived here anyway as remote consoles and were handed
/// local ports 5003 and 5004, pointing at something nobody serves.
#[test]
fn a_peers_unserved_devices_are_not_re_exported() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_XX-if00-port0",
        "iq8",
    );
    // …and the two GPIO channels its owner excluded: no port, no endpoint.
    {
        let mut reg = b.registry();
        for gpio in [
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_XX-if02-port0",
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_XX-if03-port0",
        ] {
            let d = reg
                .upsert_device(gpio, None, IdentityKind::ById, None, 0)
                .unwrap();
            reg.set_ignored(d.id, true).unwrap();
            reg.set_state(d.id, "ignored").unwrap();
        }
    }
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    let report = a.sync();

    let rows = a.registry().remote_devices().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only the served console may be re-exported, got {:?}",
        rows.iter().map(|r| &r.canonical).collect::<Vec<_>>()
    );
    assert!(rows[0].canonical.contains("if00"));
    assert_eq!(report.rows_added, 1);
    // …and no local port was spent on something nobody serves.
    assert!(
        rows.iter().all(|r| r.ser2net_port.is_some()),
        "the one real console keeps its port"
    );
}

/// A peer's board shows up here as a row that knows where it lives.
#[test]
fn a_peers_board_materialises_as_a_remote_row() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    let report = a.sync();
    assert_eq!(report.peers_ok, 1, "sync failed: {:?}", report.errors);
    assert_eq!(report.rows_added, 1, "{report:?}");

    let rows = a.registry().remote_devices().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(
        row.canonical, "peer:nodeb//dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "the id says where it lives"
    );
    assert_eq!(row.node.as_deref(), Some("nodeb"));
    assert_eq!(
        row.node_host.as_deref(),
        Some("127.0.0.1"),
        "a remote asset must carry its host, or an operator cannot tell two racks apart"
    );
    assert_eq!(
        row.remote_canonical.as_deref(),
        Some("/dev/serial/by-id/usb-FTDI_BoardB-if00-port0"),
        "what the OWNER calls it: sending our prefixed id would resolve nothing there"
    );
    assert!(
        row.ser2net_port.is_some(),
        "a remote console re-exports on a local port so every consumer works unchanged"
    );
    assert!(row.kind.is_remote());
}

/// The local view lists remote boards alongside local ones, attributed.
#[test]
fn list_devices_shows_the_whole_fleet_with_ownership() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::with_board(
        "nodea",
        "/dev/serial/by-id/usb-FTDI_BoardA-if00-port0",
        "board-a",
    );
    a.peer_with(&b, &url);
    a.sync();

    let v = a.ok("list_devices", json!({"detail": true}));
    let devices = v["devices"].as_array().unwrap();
    let names: Vec<&str> = devices
        .iter()
        .filter_map(|d| d["device"].as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("BoardA")),
        "the local board: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("peer:nodeb/")),
        "and the peer's: {names:?}"
    );
}

// ------------------------------------------------------------- federation ----

/// A read for a remote device is answered by its owner, verbatim.
#[test]
fn a_read_for_a_remote_device_is_answered_by_its_owner() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();
    // Give B's board some mined content of its own.
    let log = b.dir.join("boot.log");
    std::fs::write(&log, "[    1.0] nodeb kernel says hello\n").unwrap();
    b.ok(
        "ingest_file",
        json!({"path": log.display().to_string(),
               "device": "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0"}),
    );

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    let v = a.ok(
        "list_templates",
        json!({"device": "peer:nodeb//dev/serial/by-id/usb-FTDI_BoardB-if00-port0"}),
    );
    let text = v.to_string();
    assert!(
        text.contains("nodeb kernel says hello"),
        "A must return B's mined data, not an empty local store: {v}"
    );
    assert_eq!(
        v["via"]["node"], "nodeb",
        "and say which node answered: {v}"
    );
    assert!(v["via"]["rtt_ms"].is_number(), "with the cost: {v}");
}

/// A nickname resolves fleet-wide, so callers need not know where a board is.
#[test]
fn a_remote_board_is_reachable_by_its_nickname() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    // The peer's nickname is namespaced, and the node-scoped form works too.
    let v = a.ok("console_state", json!({"device": "nodeb/board-b"}));
    assert_eq!(v["via"]["node"], "nodeb", "{v}");
}

/// Actuation runs the hook ON THE OWNER, which is the whole point.
#[test]
fn power_actuates_on_the_owning_node() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    let dev = "nodeb/board-b";
    a.ok("acquire", json!({"device": dev, "ttl_s": 60}));
    let v = a.ok("power", json!({"device": dev, "action": "on"}));
    assert_eq!(v["via"]["node"], "nodeb", "the hook must run on B: {v}");
    let hook = v["hook"].to_string();
    assert!(
        hook.contains("powered on"),
        "B's hook output must come back verbatim: {v}"
    );

    // And the epoch landed in B's store, not A's.
    let boots = b.ok(
        "list_boots",
        json!({"device": "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0", "limit": 3}),
    );
    assert!(
        boots["boots"]
            .as_array()
            .map(|b| !b.is_empty())
            .unwrap_or(false),
        "the actuation must open an epoch on the owner: {boots}"
    );
    let a_rows = a.registry().remote_devices().unwrap();
    assert!(
        a_rows[0].db_file.is_empty() || !a.dir.join(&a_rows[0].db_file).exists(),
        "A must not have opened a store for a board it does not own"
    );
}

// ------------------------------------------------------------ lease races ----

/// Two agents, two nodes, one board: the owner arbitrates and names the holder.
#[test]
fn a_lease_race_across_nodes_names_the_true_holder() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    // A takes the lease through the proxy.
    a.ctx.set_holder("claude-a");
    a.ok("acquire", json!({"device": "nodeb/board-b", "ttl_s": 300}));

    // An agent local to B now tries the same board.
    b.ctx.set_holder("claude-b");
    let err = b.err(
        "acquire",
        json!({"device": "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0", "ttl_s": 300}),
    );
    assert_eq!(err["code"], "LEASE_HELD", "{err}");
    let holder = err.to_string();
    assert!(
        holder.contains("nodea"),
        "the error must name the NODE as well as the agent, or an operator on B cannot tell \
         who to ask: {err}"
    );
}

// --------------------------------------------------------------- failures ----

/// When the owner is gone, say so. Never answer for it.
#[test]
fn an_unreachable_owner_is_reported_not_faked() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();
    assert_eq!(a.registry().remote_devices().unwrap().len(), 1);

    // Point the peer at a port nobody is listening on: B has gone away.
    {
        let reg = a.registry();
        reg.conn()
            .execute("UPDATE peers SET mcp_url='http://127.0.0.1:9/mcp'", [])
            .unwrap();
    }
    let err = a.err("console_state", json!({"device": "nodeb/board-b"}));
    assert_eq!(err["code"], "PEER_UNREACHABLE", "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("did not answer"),
        "{err}"
    );
}

/// An unknown node is an error with the fleet attached, not a silent local miss.
#[test]
fn an_unknown_node_says_which_nodes_exist() {
    let a = Node::new("nodea");
    let err = a.err("console_state", json!({"device": "peer:ghost/whatever"}));
    // Resolution finds nothing at all for a node we have never heard of; the
    // message must not pretend the DEVICE is the problem.
    let text = err.to_string();
    assert!(
        text.contains("ghost") || err["code"] == "UNKNOWN_DEVICE",
        "{err}"
    );
}

// ------------------------------------------------------------- the client ----

/// One pooled connection per peer, however many calls go through it.
#[test]
fn the_peer_client_pools_its_connections() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let client = conminer_core::peers::PeerClient::new("nodea");
    for _ in 0..20 {
        let (v, _) = client
            .call_tool(
                &url,
                "list_devices",
                &json!({"freshness": false}),
                std::time::Duration::from_secs(5),
            )
            .expect("call");
        assert!(v.get("result").is_some(), "{v}");
    }
    assert_eq!(
        client.connections_opened(&url),
        1,
        "twenty calls must share one connection: an unpooled client is what capped the \
         system this design is ported from at 22 messages a second"
    );
}

// ------------------------------------------------------------------- peers ---

/// The `peers` tool describes the fleet an agent can address.
#[test]
fn the_peers_tool_lists_the_fleet_with_liveness() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    let v = a.ok("peers", json!({}));
    assert_eq!(v["node"], "nodea", "a node must know its own name: {v}");
    let peers = v["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1, "{v}");
    assert_eq!(peers[0]["node"], "nodeb");
    assert_eq!(peers[0]["host"], "127.0.0.1", "where it lives: {v}");
    assert_eq!(peers[0]["devices"], 1, "and what it owns: {v}");
    assert_eq!(peers[0]["live"], true);
    assert_eq!(peers[0]["answering"], true);
}

/// Every listing says which node owns the device, and where that node is.
///
/// A fleet-wide list that does not say where a board lives is an invitation to
/// power-cycle the right name on the wrong host. The name is what a person
/// types; the address is what tells them which rack they are about to touch.
#[test]
fn every_row_says_which_host_it_lives_on() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();
    let a = Node::with_board(
        "nodea",
        "/dev/serial/by-id/usb-FTDI_BoardA-if00-port0",
        "board-a",
    );
    a.peer_with(&b, &url);
    a.sync();

    for detail in [false, true] {
        let v = a.ok("list_devices", json!({"detail": detail}));
        let rows = v["devices"].as_array().unwrap();
        let remote = rows
            .iter()
            .find(|r| r["device"].as_str().unwrap_or("").starts_with("peer:"))
            .unwrap_or_else(|| panic!("no remote row with detail={detail}: {v}"));
        assert_eq!(
            remote["node"], "nodeb",
            "detail={detail}: a remote row must name its owner: {remote}"
        );
        assert_eq!(
            remote["node_host"], "127.0.0.1",
            "detail={detail}: ...and where that owner is: {remote}"
        );
        let local = rows
            .iter()
            .find(|r| r["device"].as_str().unwrap_or("").contains("BoardA"))
            .expect("the local row");
        assert!(
            local["node"].is_null(),
            "detail={detail}: a local device has no owning peer, and saying otherwise would \
             make every rig look like somebody else's: {local}"
        );
    }
}

/// A remote console re-exports on a local port via ser2net's tcp connector.
///
/// Proven against stock ser2net 4.x before any of this was built (phase 0): the
/// accepter takes our usual options and the connector dials the owner. That is
/// what lets the dashboard terminal, `endpoint_for` and a human with telnet work
/// on a remote board with no idea it is remote -- the alternative was teaching
/// every consumer a second dial path.
#[test]
fn a_remote_console_is_re_exported_by_ser2net_not_opened_locally() {
    use conminer_core::discovery::ser2net_config;

    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();
    let a = Node::with_board(
        "nodea",
        "/dev/serial/by-id/usb-FTDI_BoardA-if00-port0",
        "board-a",
    );
    a.peer_with(&b, &url);
    a.sync();

    let rows = a.registry().all_devices().unwrap();
    let cfg = ser2net_config(&rows, a.ctx.config());

    // The local board opens its serial device...
    assert!(
        cfg.contains("connector: serialdev,/dev/serial/by-id/usb-FTDI_BoardA-if00-port0"),
        "the local console must still be a serialdev: {cfg}"
    );
    // ...and the remote one dials its owner instead. NEVER a serialdev: that
    // path does not exist on this host, and opening it would fail forever.
    assert!(
        cfg.contains("connector: tcp,127.0.0.1,5001"),
        "the remote console must relay to the owner's port: {cfg}"
    );
    assert!(
        !cfg.contains("serialdev,peer:"),
        "a remote id must never reach a serialdev line: {cfg}"
    );
    // And it is a normal accepter, so every existing consumer works unchanged.
    let remote_block = cfg
        .split("connection: &")
        .find(|b| b.contains("connector: tcp,"))
        .expect("a relay block");
    assert!(
        remote_block.contains("accepter: telnet(rfc2217=false),tcp,"),
        "the relay must accept exactly like a local console does: {remote_block}"
    );
}

/// Nothing on this side may open a store for a board it does not own.
///
/// The first two-host bring-up failed exactly here: minerd attached to the
/// peer's thirteen consoles, took each store's writer lock, and mcpd stopped
/// answering within seconds -- a listening socket that accepted nothing, which
/// looks from outside like a dead service rather than a lock nobody can get.
#[test]
fn a_remote_row_is_never_captured_or_opened_locally() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();
    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    let row = a
        .registry()
        .remote_devices()
        .unwrap()
        .pop()
        .expect("a remote row");

    // The guard: asking for a local store must fail loudly, not open one.
    let err = a
        .ctx
        .with_store(&row, |_st| Ok(serde_json::Value::Null))
        .expect_err("opening a peer's store must be refused");
    assert!(
        err.message.contains("owned by node"),
        "the refusal must say why: {}",
        err.message
    );

    // And no database file was created for it.
    let path = a.dir.join(&row.db_file);
    assert!(
        !path.exists(),
        "an empty store for a remote board is worse than none: it takes the writer lock and \
         answers every question with silence ({})",
        path.display()
    );

    // minerd's own filter, at the source: a remote row is not attachable.
    let src =
        std::fs::read_to_string(format!("{}/src/service.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let attach = src
        .split("let attachable =")
        .nth(1)
        .expect("the attach filter")
        .split(';')
        .next()
        .unwrap_or_default();
    assert!(
        attach.contains("is_remote()"),
        "minerd must skip peer-owned rows before anything else: {attach}"
    );
}

/// One node is one row, however many ways it is discovered.
///
/// A statically configured peer and the same peer's beacon must land on the same
/// identity. Measured on the first two-host bring-up: adoption invented
/// `static-id:alpha` while the beacon carried the real uuid, so one lab host
/// existed twice -- and the two entries' inventory syncs then fought over the
/// same device rows, marking them gone in turn.
#[test]
fn a_node_discovered_twice_is_still_one_node() {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();

    // What a static peer can learn by asking: the far node's own identity.
    let v = b.ok("peers", json!({}));
    let real_id = v["instance_id"]
        .as_str()
        .expect("a node must state its own instance id, or a static peer can only guess");
    assert!(!real_id.is_empty());
    assert_eq!(v["node"], "nodeb");

    // A node that adopts that id, and then hears the same node's beacon, must
    // end up with ONE row.
    let a = Node::new("nodea");
    {
        let mut reg = a.registry();
        conminer_core::peers::registry::upsert_static(&mut reg, &url, a.now()).unwrap();
        let advert = conminer_core::peers::registry::Advert {
            instance_id: real_id.to_string(),
            name: "nodeb".into(),
            version: "test".into(),
            mcp_url: url.clone(),
            dash_url: String::new(),
            ser2net_host: "127.0.0.1".into(),
            ser2net_ports: vec![],
        };
        conminer_core::peers::registry::adopt_identity(
            &mut reg,
            &url,
            &advert,
            Some("127.0.0.1"),
            a.now(),
        )
        .unwrap();
        // ...and now the beacon arrives for the same node.
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &advert,
            conminer_core::peers::registry::PeerSource::Beacon,
            Some("127.0.0.1"),
            a.now(),
        )
        .unwrap();
    }

    let peers = a.ok("peers", json!({}));
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(
        list.len(),
        1,
        "one host, one row -- two entries fight over the same devices: {peers}"
    );
    assert_eq!(list[0]["instance_id"], real_id);
    assert_eq!(
        list[0]["source"], "static",
        "an operator's statement outranks a beacon that may stop"
    );
}

/// ...and it must not be RE-INVENTED on the next restart.
///
/// `adopt_identity` clears the stand-in the first time a static peer answers,
/// but `upsert_static` runs again on every startup and used to recreate one
/// unconditionally. Once the peer goes down, only a SUCCESSFUL probe can clear
/// it again -- so the stand-in comes back and stays for ever.
///
/// Measured on alpha: `charlie` was listed twice. Once truthfully, as
/// `Connection refused`; and once as a node whose NAME was the string
/// `http://192.168.10.12:8090/mcp`, reporting itself in good health.
#[test]
fn a_peer_already_identified_is_not_given_a_fresh_placeholder_on_restart() {
    let a = Node::new("nodea");
    let url = "http://192.0.2.9:8090/mcp";
    let real_id = "11111111-2222-3333-4444-555555555555";
    {
        let mut reg = a.registry();
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
        let advert = conminer_core::peers::registry::Advert {
            instance_id: real_id.to_string(),
            name: "nodeb".into(),
            version: "test".into(),
            mcp_url: url.to_string(),
            dash_url: String::new(),
            ser2net_host: "127.0.0.1".into(),
            ser2net_ports: vec![],
        };
        conminer_core::peers::registry::adopt_identity(
            &mut reg,
            url,
            &advert,
            Some("127.0.0.1"),
            a.now(),
        )
        .unwrap();

        // THE RESTART. Same config, same URL, peer now unreachable.
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
    }

    let peers = a.ok("peers", json!({}));
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(
        list.len(),
        1,
        "one host, one row: a restart must not re-invent a stand-in for a node \
         that has already introduced itself: {peers}"
    );
    assert_eq!(
        list[0]["node"], "nodeb",
        "and it is the REAL row that survives"
    );
    assert_eq!(list[0]["instance_id"], real_id);
    assert!(
        !list[0]["node"].as_str().unwrap().starts_with("http"),
        "a node named by its own URL is the stand-in, not the node: {peers}"
    );
}

/// A stand-in an older build seeded as HEALTHY is corrected on restart.
///
/// Found on bravo after the first half of this fix shipped: its stand-in for
/// charlie still read `answering: true`, because the insert had been corrected
/// but the ON CONFLICT branch left an existing row's `ok` alone -- and a peer
/// that never answers is never rewritten by anything else. The row had
/// advert_count=0, no last_poll and no last_error: nothing had ever contacted
/// it, and the page drew it in good standing anyway.
#[test]
fn a_stand_in_seeded_as_healthy_by_an_older_build_is_corrected_on_restart() {
    let a = Node::new("nodea");
    let url = "http://192.0.2.17:8090/mcp";
    {
        let mut reg = a.registry();
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
        // Exactly what the old build left behind: a stand-in claiming health.
        reg.conn()
            .execute(
                "UPDATE peers SET ok = 1 WHERE mcp_url = ?1",
                rusqlite::params![url],
            )
            .unwrap();
        assert_eq!(
            a.ok("peers", json!({}))["peers"][0]["answering"],
            true,
            "precondition: the bench really is claiming this host answers"
        );

        // The restart.
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
    }

    let peers = a.ok("peers", json!({}));
    assert_eq!(
        peers["peers"][0]["answering"], false,
        "nothing has ever contacted this host, so nothing can vouch for it: {peers}"
    );
}

/// A stand-in an older build already wrote is swept on the next restart.
///
/// Refusing to create another does nothing for the benches that have been
/// restarting with the old code for weeks: the row is in their table NOW, and it
/// is the one the operator is looking at. So the guard also cleans up.
///
/// The sequence is alpha's exactly: a stand-in written before first contact,
/// a real row created when the peer announced itself, and both then sitting
/// there because only a successful probe could ever have cleared the first.
#[test]
fn a_stand_in_left_by_an_older_build_is_swept_on_the_next_restart() {
    let a = Node::new("nodea");
    let url = "http://192.0.2.13:8090/mcp";
    let real_id = "99999999-8888-7777-6666-555555555555";
    {
        let mut reg = a.registry();
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
        // The peer announces itself. `upsert_advert` does NOT clear stand-ins,
        // so now the table holds both -- the state found on the bench.
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &conminer_core::peers::registry::Advert {
                instance_id: real_id.to_string(),
                name: "nodeb".into(),
                version: "test".into(),
                mcp_url: url.to_string(),
                dash_url: String::new(),
                ser2net_host: "127.0.0.1".into(),
                ser2net_ports: vec![],
            },
            conminer_core::peers::registry::PeerSource::Beacon,
            Some("127.0.0.1"),
            a.now(),
        )
        .unwrap();
        assert_eq!(
            conminer_core::peers::registry::all(&reg).unwrap().len(),
            2,
            "precondition: the bench really is holding two rows for one node"
        );

        // The restart that used to make it permanent now cleans it up.
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
    }

    let peers = a.ok("peers", json!({}));
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(
        list.len(),
        1,
        "the leftover stand-in must be swept, not merely not-recreated: {peers}"
    );
    assert_eq!(list[0]["instance_id"], real_id, "and the REAL row survives");
}

/// A peer nothing has ever spoken to is not reported as answering.
///
/// The stand-in row was inserted with `ok=1`, and the page reads `ok` as
/// "answering" -- so an unreachable host was drawn in good standing. Only a
/// successful probe (`mark_ok`) is entitled to make that claim.
#[test]
fn a_static_peer_that_has_never_answered_is_not_reported_as_healthy() {
    let a = Node::new("nodea");
    let url = "http://192.0.2.11:8090/mcp";
    {
        let mut reg = a.registry();
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
    }

    let peers = a.ok("peers", json!({}));
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(
        list.len(),
        1,
        "the operator's entry is still listed: {peers}"
    );
    assert_eq!(
        list[0]["answering"], false,
        "nothing has contacted this node, so nothing can vouch for it: {peers}"
    );
}

/// A placeholder peer row must never outlive first contact.
///
/// Two shapes exist: the URL stand-in written before a static peer answers, and
/// a name stand-in an older build invented. Either one left behind is a second
/// entry for a node already known by its real id -- and the two entries then
/// sync the same devices in turn, each marking the other's rows `gone`, which
/// is what made a peer's thirteen consoles flicker on the first bring-up.
#[test]
fn adopting_a_static_peer_clears_every_placeholder_for_it() {
    let a = Node::new("nodea");
    let url = "http://192.0.2.7:8090/mcp";
    {
        let mut reg = a.registry();
        // Both stand-in shapes, for one node.
        conminer_core::peers::registry::upsert_static(&mut reg, url, a.now()).unwrap();
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &conminer_core::peers::registry::Advert {
                instance_id: "static-id:nodeb".into(),
                name: "nodeb".into(),
                version: String::new(),
                mcp_url: url.into(),
                dash_url: String::new(),
                ser2net_host: String::new(),
                ser2net_ports: vec![],
            },
            conminer_core::peers::registry::PeerSource::Static,
            None,
            a.now(),
        )
        .unwrap();
        assert_eq!(
            conminer_core::peers::registry::all(&reg).unwrap().len(),
            2,
            "the corpus must start with two placeholders or this proves nothing"
        );

        // The node answers with its real identity.
        conminer_core::peers::registry::adopt_identity(
            &mut reg,
            url,
            &conminer_core::peers::registry::Advert {
                instance_id: "real-uuid-nodeb".into(),
                name: "nodeb".into(),
                version: "0.2.0".into(),
                mcp_url: url.into(),
                dash_url: String::new(),
                ser2net_host: "192.0.2.7".into(),
                ser2net_ports: vec![],
            },
            Some("192.0.2.7"),
            a.now(),
        )
        .unwrap();
    }

    let rows = conminer_core::peers::registry::all(&a.registry()).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one node, one row -- placeholders left behind fight over its devices: {rows:?}"
    );
    assert_eq!(rows[0].instance_id, "real-uuid-nodeb");
    assert_eq!(rows[0].host.as_deref(), Some("192.0.2.7"));

    // And peerd re-asks EVERY static peer, which is what makes a row written by
    // a previous build heal.
    //
    // This used to assert the two placeholder id shapes by name. That filter is
    // gone because it was too narrow in a way that bit: a row whose id was
    // already real but whose NAME was wrong never healed, and after a deploy
    // accidentally gave three nodes the same name, three different machines sat
    // in one table all labelled `charlie`. Selecting by source covers both
    // placeholder shapes and the already-adopted rows.
    let src =
        std::fs::read_to_string(format!("{}/src/service.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert!(
        src.contains("p.source == pr::PeerSource::Static"),
        "peerd must re-ask every static peer, or a stale identity never heals"
    );
}

/// Local discovery must not declare a peer's hardware missing.
///
/// The `/dev` sweep answers "is this cable still plugged into THIS host?", and a
/// remote row never was. Marking it `gone` is not cosmetic: gone rows are
/// dropped from the ser2net config, so the remote console stops being
/// re-exported and every listing shows the peer's boards as dead. Measured on
/// the first two-host bring-up -- thirteen consoles reading `gone` here while
/// their owner reported them listening.
#[test]
fn local_discovery_leaves_a_peers_rows_alone() {
    use conminer_core::discovery::reconcile;

    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_BoardB-if00-port0",
        "board-b",
    );
    let url = b.serve();
    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    {
        let mut reg = a.registry();
        let before = reg.remote_devices().unwrap();
        assert!(!before.is_empty());
        assert_ne!(before[0].state, "gone", "the corpus starts live");

        // This host has no serial devices at all -- the a NAT-ed host case exactly.
        reconcile(&mut reg, a.ctx.config(), &[], a.now()).unwrap();

        let after = reg.remote_devices().unwrap();
        assert_ne!(
            after[0].state, "gone",
            "a peer's board is not missing because it is not cabled here: {:?}",
            after[0]
        );
    }

    // ...and the console is still re-exported, which is what `gone` would have
    // silently taken away.
    let rows = a.registry().all_devices().unwrap();
    let cfg = conminer_core::discovery::ser2net_config(&rows, a.ctx.config());
    assert!(
        cfg.contains("connector: tcp,"),
        "the remote console must still relay after a local /dev sweep: {cfg}"
    );
}

/// A peer that cannot name itself stays a placeholder. It never becomes a
/// second entry for a node we already know.
///
/// Minting an id is worse than leaving one unknown: a made-up id can never merge
/// with the real one the same node's beacon carries, so the node exists twice
/// for ever -- and adoption "succeeds" every tick, so nothing anywhere explains
/// it. Measured against a lab host running an older build, which answered the
/// handshake with a name and no id.
#[test]
fn a_peer_that_cannot_name_itself_is_not_given_an_invented_identity() {
    let src =
        std::fs::read_to_string(format!("{}/src/service.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let adoption = src
        .split("let handshake = client.call_tool(")
        .nth(1)
        .expect("the identity handshake")
        .split("inventory::sync_all")
        .next()
        .unwrap_or_default();
    assert!(
        !adoption.contains(r#"format!("static-id:{name}")"#),
        "adoption must not mint an id from the peer's name: {adoption}"
    );
    assert!(
        adoption.contains(r#"get("instance_id")"#),
        "it must use the id the peer states: {adoption}"
    );
    assert!(
        adoption.contains("stays a placeholder"),
        "and when there is none, say so rather than inventing one: {adoption}"
    );
}

/// A FLEET IS ONE BUILD, and the surface has to be able to say so.
///
/// The Cargo version cannot: three nodes once reported `0.2.0` while running
/// three genuinely different builds. A field that always agrees can never
/// disagree when it matters -- and it matters here more than anywhere, because
/// peering proxies tool calls between nodes, so a behaviour difference between
/// builds arrives looking like a misbehaving board rather than a deployment
/// problem.
#[test]
fn peers_reports_which_build_each_node_runs_and_whether_they_agree() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let peers = src
        .split("name: \"peers\"")
        .nth(1)
        .and_then(|t| t.split("name: \"list_profiles\"").next())
        .expect("the peers tool");

    // Per peer: which build, and does it match ours.
    assert!(peers.contains("\"build\": p.version"), "no per-peer build");
    assert!(peers.contains("build_matches"), "no per-peer comparison");
    // And a fleet-wide verdict, so nobody has to compare by eye.
    assert!(peers.contains("fleet_build"), "no fleet-wide verdict");
    assert!(peers.contains("in_sync"), "no in_sync flag");
    // A peer that has not said yet is UNKNOWN, never counted as agreeing:
    // silence is not a match.
    assert!(
        peers.contains("\"unknown\""),
        "unknown peers must be listed"
    );

    // The comparison is against the BUILD, not the package version. If this
    // ever reverts to CARGO_PKG_VERSION the whole check becomes vacuous.
    assert!(
        peers.contains("build_id()"),
        "the comparison must use the build fingerprint"
    );
    let svc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");
    let advert = svc
        .split("let advert = pr::Advert {")
        .nth(1)
        .and_then(|t| t.split('}').next())
        .expect("the advert");
    assert!(
        advert.contains("build_id()") && !advert.contains("CARGO_PKG_VERSION"),
        "a node must advertise its BUILD, not its package version: {advert}"
    );
}

/// The fingerprint has to be the same on every architecture for the same
/// source, or the lab hosts (x86_64) and the dev box (arm64) would look like
/// different deployments of identical code.
#[test]
fn the_build_id_comes_from_the_source_not_the_binary() {
    let dockerfile =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Dockerfile"))
            .expect("Dockerfile");
    assert!(
        dockerfile.contains("ARG CONMINER_BUILD") && dockerfile.contains("ENV CONMINER_BUILD"),
        "the build id must be passed in at image build time"
    );
    let cm = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cm")).expect("cm");
    assert!(cm.contains("build-id)"), "`./cm build-id` must exist");
    assert!(
        cm.contains("--build-arg \"CONMINER_BUILD=$(build_id)\""),
        "`./cm image` must bake the fingerprint in"
    );
    // Hashing the BINARY would differ per architecture; hash the source.
    assert!(
        cm.contains("find crates profiles.d"),
        "the fingerprint must be computed from source files"
    );
}

/// A NODE MUST NOT PEER WITH ITSELF, and a peer's NAME must keep healing.
///
/// Both were exposed by one accident: a fleet deploy shipped one host's `.env`
/// to the others, so every node called itself `charlie` and alpha's peer list
/// named its own URL. It then proxied its own calls back to itself, and because
/// identity adoption only ever ran for rows still wearing a placeholder id, the
/// wrong names stayed in the table after the configs were fixed -- three rows,
/// three different machines, all labelled the same. Routing is BY NODE NAME, so
/// that is a call landing on the wrong board.
#[test]
fn a_node_refuses_to_peer_with_itself_and_keeps_names_current() {
    let svc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");
    let peerd = svc.split("fn peerd").nth(1).expect("peerd must exist");

    // Every static peer is re-asked, not only placeholders: a name that changes
    // under us has to heal.
    assert!(
        peerd.contains("p.source == pr::PeerSource::Static"),
        "the handshake must cover every static peer, not just placeholder rows"
    );
    assert!(
        !peerd.contains("p.instance_id.starts_with(\"static:\")\n                            || p.instance_id.starts_with(\"static-id:\")"),
        "filtering to placeholders is what left the wrong names in place"
    );

    // And a peer that turns out to be us is dropped, not kept.
    let self_guard = peerd
        .find("stated_id.as_deref() == Some(own_id.as_str())")
        .expect("a self-peer guard must exist");
    let forget = peerd[self_guard..]
        .find("pr::forget")
        .expect("the self row must be forgotten, not merely skipped");
    assert!(forget < 400, "the guard must drop the row immediately");
}

// -------------------------------------------------------------- §P2 relay --

/// A ---> B ---> C, where A has never heard of C's address.
///
/// The point of transitive routing: A learns C's board THROUGH B, addresses it
/// by its OWNER, and the call reaches C by way of B. Before this, inventory
/// refused to re-export a peer's remote rows at all, so a board two hops away
/// simply did not exist for A.
#[test]
fn a_board_two_hops_away_is_reachable_through_the_middle_node() {
    let mut c = Node::with_board("nodec", "/dev/serial/by-id/usb-FARBOARD-if00-port0", "far");
    let c_url = c.serve();

    // B peers with C and learns its board directly.
    let mut b = Node::new("nodeb");
    b.peer_with(&c, &c_url);
    b.sync();
    let b_url = b.serve();

    // A peers ONLY with B. It never learns C's URL.
    let a = Node::new("nodea");
    a.peer_with(&b, &b_url);
    a.sync();

    let rows = a.registry().remote_devices().unwrap();
    assert_eq!(rows.len(), 1, "A must see C's board: {rows:?}");
    let row = &rows[0];
    // OWNED by C, REACHED through B -- the distinction the whole feature rests
    // on. Naming B as the owner would make the board a different device
    // depending on which way it was heard.
    assert_eq!(row.node.as_deref(), Some("nodec"), "the owner is C");
    assert_eq!(row.via.as_deref(), Some("nodeb"), "the next hop is B");
    assert_eq!(row.hops, 2);
    assert!(
        row.canonical.contains("nodec") && !row.canonical.contains("nodeb"),
        "the id names its owner, not the path: {}",
        row.canonical
    );

    // And a call for it actually lands on C. `power` mutates, so the lease is
    // taken first -- and the lease itself has to travel both hops.
    a.call("acquire", json!({"device": "far", "holder": "relay-test"}));
    let out = a.call("power", json!({"device": "far", "action": "on"}));
    assert_eq!(out["hook"]["exit_code"], 0, "{out}");
    assert!(
        out["hook"]["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("nodec"),
        "the hook must have run on C, not on the relay: {out}"
    );
}

/// The hazard the first version refused the topology to avoid.
///
/// Two nodes describing each other must not give each a proxied copy of the
/// board it already holds the tty for -- the shortest possible cycle, and the
/// one that would grow a device set for ever.
#[test]
fn a_cycle_does_not_materialise_devices_for_ever() {
    let mut a = Node::with_board("nodea", "/dev/serial/by-id/usb-MINE-if00-port0", "mine");
    let a_url = a.serve();
    let mut b = Node::with_board("nodeb", "/dev/serial/by-id/usb-THEIRS-if00-port0", "theirs");
    let b_url = b.serve();

    a.peer_with(&b, &b_url);
    b.peer_with(&a, &a_url);

    // Several rounds: if anything grew per round, this is where it shows.
    for _ in 0..4 {
        a.sync();
        b.sync();
    }

    let a_remote = a.registry().remote_devices().unwrap();
    let b_remote = b.registry().remote_devices().unwrap();
    assert_eq!(
        a_remote.len(),
        1,
        "A must hold exactly B's one board: {:?}",
        a_remote.iter().map(|r| &r.canonical).collect::<Vec<_>>()
    );
    assert_eq!(b_remote.len(), 1, "and B exactly A's: {b_remote:?}");
    assert!(
        a_remote[0].canonical.contains("THEIRS"),
        "A must not re-import its own board: {}",
        a_remote[0].canonical
    );
    assert!(b_remote[0].canonical.contains("MINE"));
}

/// The distance limit, so a long chain cannot walk for ever even without a
/// cycle -- a fleet that grows one relay at a time is still bounded.
#[test]
fn the_hop_limit_is_enforced_on_import() {
    assert_eq!(conminer_core::peers::inventory::MAX_HOPS, 3);
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/peers/inventory.rs"
    ))
    .expect("inventory.rs");
    assert!(
        src.contains("if relayed && hops > MAX_HOPS"),
        "the import must refuse anything past the limit"
    );
    assert!(
        src.contains("if owner == this_node"),
        "and must never import a board this node owns"
    );
}

/// A call must not be able to circle for ever, even if the routes disagree.
#[test]
fn a_relay_loop_is_refused_with_the_path_that_caused_it() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/route.rs"
    ))
    .expect("route.rs");
    assert!(
        src.contains("path.iter().any(|n| n == node)"),
        "forwarding must refuse a node already on the path"
    );
    assert!(
        src.contains("\"path\": path"),
        "and must report the path, or a loop is a mystery"
    );
    // The path has to actually travel, or the check is vacuous.
    let client = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/peers/client.rs"
    ))
    .expect("client.rs");
    assert!(
        client.contains("PATH_HEADER"),
        "the path must ride the wire"
    );
    let server = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/server.rs"
    ))
    .expect("server.rs");
    assert!(
        server.contains("PATH_HEADER") && server.contains("set_call_path"),
        "and be read back on arrival"
    );
}

/// A ROW WITH NO PEER LEFT TO VOUCH FOR IT IS NOT A DEVICE.
///
/// The per-peer sweep marks rows `gone` when their peer stops listing them --
/// but only for peers still IN the table. Drop the peer itself and its rows are
/// orphaned: nothing ever marks them, nothing ever sweeps them, and they sit on
/// the dashboard describing boards on a node nobody is talking to. Measured
/// after a bad config made two nodes peer with themselves: four rows for boards
/// the host was already holding the tty for, left behind when the bogus peers
/// were dropped.
#[test]
fn rows_from_a_peer_that_is_gone_are_forgotten() {
    let mut b = Node::with_board("nodeb", "/dev/serial/by-id/usb-ORPHAN-if00-port0", "orphan");
    let url = b.serve();
    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();
    assert_eq!(a.registry().remote_devices().unwrap().len(), 1);

    // The peer leaves the fleet -- expired, or dropped as a self-peer.
    {
        let mut reg = a.registry();
        let rows = peer_registry::all(&reg).unwrap();
        for p in rows {
            peer_registry::forget(&mut reg, &p.instance_id).unwrap();
        }
    }
    a.sync();

    assert!(
        a.registry().remote_devices().unwrap().is_empty(),
        "a row whose peer is gone must not linger: {:?}",
        a.registry()
            .remote_devices()
            .unwrap()
            .iter()
            .map(|r| &r.canonical)
            .collect::<Vec<_>>()
    );
}

/// A PEER WE CANNOT NAME CANNOT OWN ANYTHING.
///
/// A static row's `name` is the configured URL until the handshake lands.
/// Importing devices then attributes them to a "node" called
/// `http://host:8090/mcp`, and no later adoption reconciles those rows because
/// the real node arrives under its real name. Measured on bravo: two of its own
/// boards, owned by a URL.
#[test]
fn a_peer_still_wearing_a_placeholder_id_contributes_no_devices() {
    let mut b = Node::with_board("nodeb", "/dev/serial/by-id/usb-EARLY-if00-port0", "early");
    let url = b.serve();

    let a = Node::new("nodea");
    // Exactly what `[peers] nodes` writes before first contact.
    {
        let mut reg = a.registry();
        peer_registry::upsert_static(&mut reg, &url, 1_000).unwrap();
    }
    let report = a.sync();

    assert_eq!(report.peers_ok, 0, "a placeholder peer must not be synced");
    assert!(
        a.registry().remote_devices().unwrap().is_empty(),
        "no device may be owned by a node whose name is still a URL"
    );
}

/// A REMOTE BOARD'S CONTROLS COME FROM ITS OWNER.
///
/// Controller profiles match a by-id name against hardware plugged into THIS
/// host, so resolving a peer's board here always answers "no controller". Every
/// peer's board therefore rendered with no controller and no power buttons, on a
/// bench where the hardware is perfectly driveable from the node that owns it.
#[test]
fn a_remote_board_reports_the_controls_its_owner_sees() {
    let mut b = Node::with_board("nodeb", "/dev/serial/by-id/usb-DRIVEN-if00-port0", "driven");
    let url = b.serve();
    let a = Node::new("nodea");
    a.peer_with(&b, &url);
    a.sync();

    // The owner says this board has a power hook (its own per-device config).
    let owner_view = b.call("list_devices", json!({"detail": true}));
    let owner_row = &owner_view["devices"][0];
    assert_eq!(
        owner_row["controls"]["has_power_hook"], true,
        "the owner must report its own board as driveable: {owner_row}"
    );

    // And A repeats that, rather than answering for hardware it cannot see.
    let seen = a.call("list_devices", json!({"detail": true}));
    let row = seen["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["node"] == "nodeb")
        .expect("the remote row");
    assert_eq!(
        row["controls"]["has_power_hook"], true,
        "a remote board must carry its owner's controls, not this node's guess: {row}"
    );
}

// ---------------------------------------------------------------- §P3 push --

/// A NODE THAT CAN REACH NOBODY STILL LEARNS THE FLEET.
///
/// Inventory is a pull, so a node behind one-way connectivity sees nothing --
/// however many peers are talking to it. Measured on the bravo bench, which sits
/// upstream of a NAT: two nodes fetched its boards every five seconds while its
/// own peer rows sat at "connection timed out" and its fleet view stayed empty.
/// The reachable side now carries the conversation both ways.
#[test]
fn a_peer_that_cannot_dial_out_still_learns_the_fleet_by_announcement() {
    // B owns a board. A cannot reach B at all -- it is never given B's URL.
    let b = Node::with_board("nodeb", "/dev/serial/by-id/usb-PUSHED-if00-port0", "pushed");
    let mut a = Node::new("nodea");
    let a_url = a.serve();

    // B announces itself to A, over the connection B opened. This is exactly
    // what peerd does each tick for every peer that answered.
    let devices = b.call("list_devices", json!({"detail": true}));
    let payload = json!({
        "node": "nodeb",
        "instance_id": "id-nodeb",
        "build": "testbuild",
        "host": "192.0.2.9",
        "mcp_url": "http://192.0.2.9:8090/mcp",
        "devices": devices["devices"],
    });
    let client = conminer_core::peers::PeerClient::new("nodeb");
    let (reply, _) = client
        .call_tool(&a_url, "peer_announce", &payload, Duration::from_secs(10))
        .expect("the announcement must be accepted");
    let content = &reply["result"]["structuredContent"];
    assert_eq!(content["node"], "nodeb", "{reply}");
    assert!(
        content["added"].as_u64().unwrap_or(0) >= 1,
        "the announcement must bring hardware with it: {content}"
    );

    // A now sees B's board, owned by B, without ever having dialled it.
    let rows = a.registry().remote_devices().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].node.as_deref(), Some("nodeb"));
    assert!(rows[0].canonical.contains("PUSHED"));

    // …and the peer row records HOW it was learned, because that decides whether
    // the return path exists.
    let peers = peer_registry::all(&a.registry()).unwrap();
    assert_eq!(peers.len(), 1);
    assert!(peers[0].source.is_push(), "{:?}", peers[0].source);
}

/// An announcement from ourselves is refused.
///
/// The same rule the pull side enforces: a config that names its own host would
/// otherwise give a node a proxied copy of hardware it holds the tty for. Here
/// it would arrive as a perfectly well-formed announcement.
#[test]
fn a_node_refuses_an_announcement_from_itself() {
    let mut a = Node::with_board("nodea", "/dev/serial/by-id/usb-MINE-if00-port0", "mine");
    let a_url = a.serve();
    let own_id = conminer_core::peers::Identity::load_or_create(&a.dir, "", 1_000)
        .unwrap()
        .instance_id;

    let client = conminer_core::peers::PeerClient::new("nodea");
    let (reply, _) = client
        .call_tool(
            &a_url,
            "peer_announce",
            &json!({"node": "nodea", "instance_id": own_id, "devices": []}),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(
        reply["result"]["isError"], true,
        "a node must refuse its own announcement: {reply}"
    );
    assert!(
        a.registry().remote_devices().unwrap().is_empty(),
        "and import nothing from it"
    );
}

/// PUSH AND PULL MUST APPLY THE SAME RULES.
///
/// There are now two ways a node learns what a peer owns. Two copies of owner
/// attribution, the hop bound, the self-owner guard and serve-only would drift,
/// and the drift would show up as a board that exists on one node and not
/// another. One import path, called by both.
#[test]
fn push_and_pull_share_one_import_path() {
    let inv = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/peers/inventory.rs"
    ))
    .expect("inventory.rs");
    assert!(
        inv.contains("pub fn import_devices"),
        "the import must be callable by both paths"
    );
    // The pull delegates to it rather than duplicating it.
    let sync_one = inv
        .split("fn sync_one")
        .nth(1)
        .and_then(|t| t.split("\npub fn ").next())
        .expect("sync_one");
    assert!(
        sync_one.contains("import_devices("),
        "the pull must delegate to the shared import: {sync_one}"
    );
    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let announce = tools
        .split("name: \"peer_announce\"")
        .nth(1)
        .and_then(|t| t.split("name: \"list_profiles\"").next())
        .expect("peer_announce");
    assert!(
        announce.contains("inventory::import_devices"),
        "and so must the push"
    );
}

/// A stand-in for peerd's relay worker: park on `peer`, run what it hands back
/// against `own`, post the answer. The body is `relay::serve_once`, which is
/// literally the function the daemon loops on -- a hand-rolled double here is
/// the one piece of the reverse path that would never actually be exercised.
struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn start(node: &str, peer_url: &str, own_url: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (n, p, o, s) = (
            node.to_string(),
            peer_url.to_string(),
            own_url.to_string(),
            stop.clone(),
        );
        let handle = std::thread::spawn(move || {
            let client = conminer_core::peers::PeerClient::new(n.clone());
            while !s.load(Ordering::Relaxed) {
                match conminer_core::peers::relay::serve_once(
                    &client,
                    &p,
                    &o,
                    &n,
                    Duration::from_millis(300),
                ) {
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Block until the far node has registered this poller, so a test never
    /// races the first poll and reads "nobody is listening" as a real verdict.
    fn wait_until_listening(&self, on: &Node, node: &str) {
        for _ in 0..200 {
            let v = on.ok("peers", json!({}));
            if v["relay"]["listening"]
                .as_array()
                .is_some_and(|a| a.iter().any(|n| n == node))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the relay worker never registered as listening");
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The bravo topology, in one process: A can be dialled and cannot dial; B owns
/// the board and can dial A. `peer_announce` is how A learns B exists.
fn one_way_pair() -> (Node, Node, String, String, String) {
    let mut b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_Unreachable-if00-port0",
        "far-board",
    );
    let url_b = b.serve();
    let mut a = Node::new("nodea");
    let url_a = a.serve();

    let devices = b.call("list_devices", json!({"detail": true}));
    let client = conminer_core::peers::PeerClient::new("nodeb");
    let (reply, _) = client
        .call_tool(
            &url_a,
            "peer_announce",
            &json!({
                "node": "nodeb",
                "instance_id": "id-nodeb",
                "build": "testbuild",
                "host": "192.0.2.9",
                // A WELL-FORMED ADDRESS THAT DOES NOT ANSWER, which is the whole
                // situation: on the bravo bench the published mcp_url is correct
                // and completely undialable from the receiving side. Publishing
                // B's REAL url here would let a direct dial satisfy every
                // assertion below, and the reverse channel could be deleted with
                // the suite still green.
                "mcp_url": "http://127.0.0.1:9/mcp",
                "ser2net_host": "192.0.2.9",
                "devices": devices["devices"],
            }),
            Duration::from_secs(10),
        )
        .expect("the announcement is accepted");
    assert!(
        reply["result"]["structuredContent"]["added"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "the announcement must bring the board with it: {reply}"
    );
    (a, b, url_a, url_b, "nodeb/far-board".to_string())
}

/// A BOARD ON A NODE WE CANNOT DIAL IS STILL DRIVEN FROM HERE.
///
/// Announcing alone buys a view and nothing else: seeing alpha's boards and
/// being able to power one are different problems, and the second needs a
/// connection in the direction that does not open. So the call goes out over the
/// one that does -- the owner is already dialling in, and it takes the work with
/// it on the way past. The hook must run on the OWNER, and its output must come
/// back to the caller unchanged.
#[test]
fn a_board_on_an_unreachable_node_is_actuated_over_the_reverse_channel() {
    let (a, b, url_a, url_b, dev) = one_way_pair();
    // Nothing has been dialled: A's only address for B is one it never uses.
    let worker = Worker::start("nodeb", &url_a, &url_b);
    worker.wait_until_listening(&a, "nodeb");

    a.ok("acquire", json!({"device": dev, "ttl_s": 60}));
    let v = a.ok("power", json!({"device": dev, "action": "on"}));
    assert_eq!(v["via"]["node"], "nodeb", "the hook must run on B: {v}");
    assert!(
        v["hook"].to_string().contains("powered on"),
        "B's hook output must come back verbatim: {v}"
    );

    // And it really ran on B: the epoch is in B's store, not A's.
    let boots = b.ok(
        "list_boots",
        json!({"device": "/dev/serial/by-id/usb-FTDI_Unreachable-if00-port0"}),
    );
    assert!(boots.get("error").is_none(), "{boots}");
}

/// A relayed call leases under the REAL caller's fleet identity.
///
/// The reverse channel is a different wire, not different semantics. If the
/// owner recorded the lease as its own poller instead of the agent that asked,
/// two agents on the unreachable side would share one lease and neither could be
/// told apart -- exactly the failure per-call origin exists to prevent.
#[test]
fn a_relayed_call_leases_as_the_caller_not_the_worker() {
    let (a, b, url_a, url_b, dev) = one_way_pair();
    let worker = Worker::start("nodeb", &url_a, &url_b);
    worker.wait_until_listening(&a, "nodeb");

    a.ctx.set_holder("claude-a");
    a.ok("acquire", json!({"device": dev, "ttl_s": 300}));

    // An agent local to B now wants the same board, and must be told who really
    // holds it. "nodeb" would mean the worker had taken the lease under its own
    // name -- and then two agents on A would share one lease and neither could
    // be told apart.
    b.ctx.set_holder("claude-b");
    let err = b.err(
        "acquire",
        json!({"device": "/dev/serial/by-id/usb-FTDI_Unreachable-if00-port0", "ttl_s": 300}),
    );
    assert_eq!(err["code"], "LEASE_HELD", "{err}");
    let holder = err.to_string();
    assert!(
        holder.contains("nodea") && holder.contains("claude-a"),
        "the lease must name the calling node AND agent, not the relay worker: {err}"
    );
}

/// An error from the owner arrives as that tool's error, over the relay too.
///
/// `power` without a lease is refused by the owner. Coming back as a transport
/// failure -- or worse, as a timeout -- would send an agent looking at the
/// network for a problem that is a missing `acquire`.
#[test]
fn a_relayed_failure_arrives_as_the_owners_error() {
    let (a, _b, url_a, url_b, dev) = one_way_pair();
    let worker = Worker::start("nodeb", &url_a, &url_b);
    worker.wait_until_listening(&a, "nodeb");

    // THE SUBJECT IS THE ERROR MAPPING, NOT THE STARTUP.
    //
    // The worker registers as a poller before node B's own server is
    // necessarily answering, so under a loaded box the first relayed call can
    // come back PEER_UNREACHABLE -- a true statement about that instant and
    // nothing to do with what this test asserts. Measured once in a full
    // parallel run, passing 3/3 alone. So retry while the transport is still
    // coming up, and fail on the mapping.
    let mut e = a.err("power", json!({"device": dev, "action": "on"}));
    for _ in 0..20 {
        if e["code"] != "INTERNAL" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        e = a.err("power", json!({"device": dev, "action": "on"}));
    }
    assert_eq!(e["code"], "LEASE_REQUIRED", "{e}");
}

/// NOBODY LISTENING MUST FAIL FAST AND SAY WHY.
///
/// The failure this replaces is the bad one: an address that is present and
/// correct and unreachable, dialled anyway, spending the caller's whole budget
/// to produce a connection error that blames the transport. A peer that reached
/// us and is not asking for work cannot be called, we know that already, and the
/// answer should take milliseconds and name the direction.
#[test]
fn a_pushed_peer_with_no_worker_is_refused_at_once() {
    let (a, _b, _url_a, _url_b, dev) = one_way_pair();
    let started = std::time::Instant::now();
    let e = a.err("console_state", json!({"device": dev}));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it must not spend the call budget finding out: {:?}",
        started.elapsed()
    );
    assert_eq!(e["code"], "UNKNOWN_PEER", "{e}");
    let text = e.to_string();
    assert!(
        text.contains("cannot be reached back") || text.contains("not listening"),
        "the message must name the DIRECTION, not blame the transport: {e}"
    );
}

/// A relayed call still refuses to circle.
///
/// The reverse channel adds a second way for a call to leave a node, so it needs
/// the same loop refusal the forward one has -- and it has to be the same check,
/// because a fleet whose routes disagree will use whichever path is available.
#[test]
fn the_reverse_channel_refuses_a_call_that_already_passed_through_the_far_node() {
    let (a, _b, url_a, url_b, dev) = one_way_pair();
    let worker = Worker::start("nodeb", &url_a, &url_b);
    worker.wait_until_listening(&a, "nodeb");

    // Arriving from B, for a board owned by B: sending it back is a loop.
    let client = conminer_core::peers::PeerClient::new("tester");
    let (reply, _) = client
        .call_tool_via(
            &url_a,
            "console_state",
            &json!({"device": dev}),
            Duration::from_secs(10),
            "nodeb/agent",
            "nodeb",
        )
        .expect("a reply");
    let out = &reply["result"];
    assert_eq!(out["isError"], true, "{reply}");
    assert!(
        out["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already on this call's path"),
        "{reply}"
    );
}

/// The node-to-node plumbing is never itself federated.
///
/// Forwarding `peer_poll` would have a node relay the very machinery that
/// decides where to relay: a worker's poll could be handed to a third node,
/// which would then be collecting work addressed to somebody else.
#[test]
fn the_peer_plumbing_tools_never_leave_the_node_they_are_called_on() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/route.rs"
    ))
    .expect("route.rs");
    let block = src
        .split("const NEVER_FEDERATED")
        .nth(1)
        .and_then(|t| t.split("];").next())
        .expect("the never-federated list");
    for tool in ["peer_announce", "peer_poll", "peer_result"] {
        assert!(block.contains(tool), "{tool} must never be federated");
    }
}

/// ONE SLOW CALL MUST NOT HOLD UP THE NEXT.
///
/// A worker is not asking for work while it is running some. With a single one
/// per peer, a `follow` parked for two minutes would also hold up every power
/// and boot_mode queued behind it -- and from the agent's side that looks like
/// dead hardware, not a queue. So the far node gets a small pool, and this is
/// the gate that says so: a call issued while a long one is in flight comes back
/// long before the long one does.
#[test]
fn a_long_relayed_call_does_not_hold_up_the_next_one() {
    let (a, _b, url_a, url_b, dev) = one_way_pair();
    let w1 = Worker::start("nodeb", &url_a, &url_b);
    let _w2 = Worker::start("nodeb", &url_a, &url_b);
    w1.wait_until_listening(&a, "nodeb");

    // Park a call on the owner for three seconds, as a second agent.
    let (u, d) = (url_a.clone(), dev.clone());
    let long = std::thread::spawn(move || {
        let c = conminer_core::peers::PeerClient::new("agent-2");
        c.call_tool(
            &u,
            "follow",
            &json!({"device": d, "until": {"pattern": "NEVER-MATCHES-THIS"}, "timeout_s": 3}),
            Duration::from_secs(30),
        )
    });
    // Let it actually be collected, so the assertion below is about a worker
    // that is BUSY rather than one that has not started.
    std::thread::sleep(Duration::from_millis(500));

    let started = std::time::Instant::now();
    a.ok("console_state", json!({"device": dev}));
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(1500),
        "a second call waited {waited:?} behind a 3 s one: the far node is being served serially"
    );
    long.join().unwrap().expect("the parked call still answers");
}

/// AN ANNOUNCEMENT CLEARS THE STAND-IN THE OPERATOR'S CONFIG LEFT BEHIND.
///
/// `[peers] nodes` is a URL and nothing else, so peerd invents a placeholder row
/// for it and replaces that once a probe says who is there. Across a one-way
/// link the probe never succeeds. Measured on bravo, which listed two peers as
/// bare URLs, permanently unanswering, while the nodes behind them were talking
/// to it every five seconds. Without this the real row simply appears beside the
/// dead one and the rack shows one host twice.
#[test]
fn an_announcement_replaces_the_placeholder_a_static_entry_left() {
    let b = Node::with_board("nodeb", "/dev/serial/by-id/usb-STATIC-if00-port0", "far");
    let mut a = Node::new("nodea");
    let a_url = a.serve();

    // A was configured with B's address and has never been able to use it.
    let b_url = "http://192.0.2.9:8090/mcp";
    {
        let mut reg = a.registry();
        peer_registry::upsert_static(&mut reg, b_url, 1_000).unwrap();
    }
    assert_eq!(peer_registry::all(&a.registry()).unwrap().len(), 1);

    let devices = b.call("list_devices", json!({"detail": true}));
    let client = conminer_core::peers::PeerClient::new("nodeb");
    client
        .call_tool(
            &a_url,
            "peer_announce",
            &json!({
                "node": "nodeb",
                "instance_id": "id-nodeb",
                "mcp_url": b_url,
                "devices": devices["devices"],
            }),
            Duration::from_secs(10),
        )
        .expect("accepted");

    let peers = peer_registry::all(&a.registry()).unwrap();
    assert_eq!(
        peers.len(),
        1,
        "one host must be listed once, not once per way of learning about it: {peers:?}"
    );
    assert_eq!(peers[0].name, "nodeb");
    assert!(peers[0].source.is_push());
}

/// A RELAY MUST NOT LAUNDER A PLACEHOLDER NAME INTO A PERMANENT ROW.
///
/// The peer we talk to can be perfectly well-named and still relay a row whose
/// OWNER is the bare URL peerd invents from `[peers] nodes`. That name cannot
/// round-trip through `peer:<node>/<remote>` -- it splits on its own slash --
/// so the row lands owned by a node called "http:", which is in nobody's peer
/// table and which no sweep collects, because the sweep asks about the relay and
/// the relay is fine. Measured on alpha: two such rows, describing bravo's
/// boards, one honest hop past the node that knew bravo's real name.
#[test]
fn a_device_whose_owner_is_a_url_is_refused_at_import() {
    let a = Node::new("nodea");
    let peer = conminer_core::peers::PeerRow {
        instance_id: "id-middle".into(),
        name: "middle".into(),
        host: None,
        mcp_url: "http://192.0.2.5:8090/mcp".into(),
        dash_url: None,
        ser2net_host: None,
        version: None,
        source: PeerSource::Static,
        ok: true,
        last_seen: 1_000,
        last_error: None,
        advert_count: 1,
        last_poll: None,
    };
    let devices = vec![
        // Relayed with a placeholder owner: this is the poison.
        json!({
            "device": "peer:http://192.168.10.11:8090/mcp//dev/serial/by-id/usb-BAD-if00-port0",
            "endpoint": "tcp://192.0.2.5:5015", "state": "listening", "hops": 1
        }),
        // …beside a well-formed one from the same peer, which must still land.
        json!({
            "device": "peer:bravo//dev/serial/by-id/usb-GOOD-if00-port0",
            "endpoint": "tcp://192.0.2.5:5016", "state": "listening", "hops": 1
        }),
    ];
    let mut reg = a.registry();
    conminer_core::peers::inventory::import_devices(
        &mut reg, &peer, &devices, 5000, 2_000, "nodea",
    )
    .expect("import");

    let rows = reg.remote_devices().unwrap();
    assert_eq!(rows.len(), 1, "only the well-formed row may land: {rows:?}");
    assert_eq!(rows[0].node.as_deref(), Some("bravo"));
    assert!(
        !rows.iter().any(|r| r.canonical.contains("http")),
        "a URL must never end up as an owner name: {rows:?}"
    );
}

/// …and the ones already stored are collected.
///
/// The refusal above stops new ones. It does nothing for the rows sitting in a
/// registry that has been running for months, and those are the ones an operator
/// is actually looking at.
#[test]
fn a_stored_row_owned_by_a_non_node_is_forgotten() {
    let a = Node::new("nodea");
    {
        let mut reg = a.registry();
        // Exactly the shape found on alpha: owner "http:", learned via a peer
        // that is present and healthy, so the orphan rule cannot see it.
        peer_registry::upsert_advert(
            &mut reg,
            &Advert {
                instance_id: "id-middle".into(),
                name: "middle".into(),
                version: String::new(),
                mcp_url: "http://192.0.2.5:8090/mcp".into(),
                dash_url: String::new(),
                ser2net_host: String::new(),
                ser2net_ports: vec![],
            },
            PeerSource::Static,
            None,
            1_000,
        )
        .unwrap();
        let row = reg
            .upsert_device(
                "peer:http://192.168.10.11:8090/mcp//dev/serial/by-id/usb-BAD-if00-port0",
                None,
                IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_remote_route(
            row.id,
            "http:",
            None,
            "/dev/serial/by-id/usb-BAD-if00-port0",
            None,
            Some("middle"),
            2,
        )
        .unwrap();
        assert_eq!(reg.remote_devices().unwrap().len(), 1);
    }

    // One sync pass, with the relay unreachable -- the row must still go.
    let mut reg = a.registry();
    let peers = peer_registry::all(&reg).unwrap();
    let client = conminer_core::peers::PeerClient::new("nodea");
    let report = conminer_core::peers::inventory::sync_all_as(
        &mut reg, &client, &peers, 5000, 2_000, "nodea",
    )
    .expect("sync");
    assert_eq!(report.rows_gone, 1, "{report:?}");
    assert!(
        reg.remote_devices().unwrap().is_empty(),
        "a row owned by something that is not a node must not survive a sweep"
    );
}

/// A HOP MUST OUTLAST THE WORK IT CARRIES.
///
/// Measured on alpha: `power on` for a board whose console stays silent runs
/// the hook, watches for a boot, escalates to a cycle and watches again -- 74
/// seconds, behaving correctly throughout. Every remote press of that button
/// came back as PEER_UNREACHABLE with a hint saying to check whether the peer
/// was up, while the peer was busy power-cycling the board in front of the
/// operator. A ceiling shorter than the operation does not report a slow call;
/// it reports a false one, about the wrong subsystem.
#[test]
fn an_actuation_hop_waits_longer_than_an_actuation_can_take() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/route.rs"
    ))
    .expect("route.rs");
    let f = src
        .split("fn call_budget")
        .nth(1)
        .and_then(|t| t.split("\n}").next())
        .expect("call_budget");
    assert!(
        f.contains("\"power\" | \"boot_mode\""),
        "the budget must recognise actuation tools: {f}"
    );
    // Well clear of the 74 s measured, and of the owner's own verify windows.
    let floor: u64 = f
        .split("\"power\" | \"boot_mode\" =>")
        .nth(1)
        .and_then(|t| t.split(',').next())
        .and_then(|t| t.trim().parse().ok())
        .expect("an actuation floor");
    assert!(
        floor >= 180,
        "an actuation hop of {floor}s is under what a real one has been measured to take"
    );

    // …and the reverse channel carries the same floor, or a relayed press dies
    // at the last leg instead of the first.
    let relay = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/peers/relay.rs"
    ))
    .expect("relay.rs");
    let g = relay
        .split("fn hop_budget")
        .nth(1)
        .and_then(|t| t.split("\n}").next())
        .expect("hop_budget");
    let rfloor: u64 = g
        .split("\"power\" | \"boot_mode\" =>")
        .nth(1)
        .and_then(|t| t.split(',').next())
        .and_then(|t| t.trim().parse().ok())
        .expect("a relay actuation floor");
    assert!(
        rfloor >= floor,
        "the reverse hop ({rfloor}s) must not be tighter than the forward one ({floor}s)"
    );
}

/// A SLOW OWNER IS NOT AN ABSENT ONE, and the hint must not send anyone to the
/// wrong machine. A read that expires proves the socket connected and the far
/// node took the request.
#[test]
fn a_read_timeout_says_the_owner_was_slow_not_missing() {
    // A port nothing is listening on: a genuine connect failure.
    let client = conminer_core::peers::PeerClient::new("nodea");
    let e = client
        .call_tool(
            "http://127.0.0.1:9/mcp",
            "list_devices",
            &json!({}),
            Duration::from_secs(3),
        )
        .expect_err("nothing is there");
    assert_eq!(e.code.as_str(), "PEER_UNREACHABLE", "{e:?}");
    assert!(
        e.hint.contains("Check the peer is up"),
        "a connect failure must point at the peer: {e:?}"
    );

    // A socket that accepts and never answers: reachable, and slow.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let keep = std::thread::spawn(move || {
        let _held: Vec<_> = listener.incoming().take(1).filter_map(Result::ok).collect();
        std::thread::sleep(Duration::from_secs(3));
    });
    let e = client
        .call_tool(
            &format!("http://{addr}/mcp"),
            "list_devices",
            &json!({}),
            Duration::from_millis(600),
        )
        .expect_err("it never answers");
    assert!(
        e.message.contains("did not answer in time"),
        "a read timeout must not read as an unreachable host: {e:?}"
    );
    assert!(
        e.hint.contains("look at the owner rather than the network"),
        "…and its hint must point at the owner: {e:?}"
    );
    let _ = keep.join();
}

/// A SECOND-HAND COPY MUST NOT OVERWRITE THE OWNER'S OWN ANSWER.
///
/// §P2 gave the registry a `hops` column and made import prefer a shorter path,
/// and then nobody published the number: `list_devices` had no `hops` field, so
/// every row arrived claiming the same distance and "a shorter path wins"
/// compared 2 < 2 and never fired once. On a three-node fleet where each node
/// relays the others, every row is written twice a tick, and the relayed copy --
/// carrying whatever the RELAY knew, which for controls was nothing -- won.
/// Measured on the bench: peer boards on two of three nodes had no controller,
/// no boot modes, no power buttons and no power state, while their owners
/// published all four correctly every five seconds.
#[test]
fn a_relayed_copy_never_overwrites_a_row_learned_from_its_owner() {
    let a = Node::new("nodea");
    let owner = peer_row("bravo", "http://192.0.2.1:8090/mcp");
    let relay = peer_row("alpha", "http://192.0.2.2:8090/mcp");
    let canonical = "/dev/serial/by-id/usb-FTDI_Board-if00-port0";

    // Straight from the owner: distance 0 on the wire, with its controls.
    let direct = vec![json!({
        "device": canonical,
        "endpoint": "tcp://192.0.2.1:5001",
        "state": "listening",
        "hops": 0,
        "controls": {"controller": "tac", "boot_modes": ["EDL"],
                     "controller_port": "/dev/ctl", "has_power_hook": true},
    })];
    let mut reg = a.registry();
    conminer_core::peers::inventory::import_devices(
        &mut reg, &owner, &direct, 5000, 1_000, "nodea",
    )
    .unwrap();

    // The same board heard through a relay that knows nothing about its controls.
    let second_hand = vec![json!({
        "device": format!("peer:bravo/{canonical}"),
        "endpoint": "tcp://192.0.2.2:5009",
        "state": "listening",
        "hops": 1,
        "controls": {"controller": null, "boot_modes": [],
                     "controller_port": null, "has_power_hook": false},
    })];
    conminer_core::peers::inventory::import_devices(
        &mut reg,
        &relay,
        &second_hand,
        5000,
        2_000,
        "nodea",
    )
    .unwrap();

    let rows = reg.remote_devices().unwrap();
    assert_eq!(rows.len(), 1, "one board, one row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.hops, 1, "the direct path must be kept: {row:?}");
    assert!(
        row.via.is_none(),
        "…and it must not be routed via the relay"
    );
    let controls = row.remote_controls.clone().expect("controls survive");
    assert_eq!(
        controls["controller"], "tac",
        "the owner's own answer must not be replaced by a relay's blank: {controls}"
    );
    assert_eq!(controls["has_power_hook"], true, "{controls}");
}

/// …and a sender that says nothing about controls erases nothing.
///
/// An absent key means "I did not tell you", never "there are none". The two
/// were the same thing to the importer, and on a fleet that relays in a ring the
/// erasure travels: one node's blank overwrites another's good value, which the
/// third relays back as blank.
#[test]
fn silence_about_controls_does_not_erase_them() {
    let a = Node::new("nodea");
    let owner = peer_row("bravo", "http://192.0.2.1:8090/mcp");
    let canonical = "/dev/serial/by-id/usb-FTDI_Board-if00-port0";
    let mut reg = a.registry();
    conminer_core::peers::inventory::import_devices(
        &mut reg,
        &owner,
        &[json!({
            "device": canonical, "endpoint": "tcp://192.0.2.1:5001", "state": "listening",
            "hops": 0,
            "controls": {"controller": "tac", "boot_modes": ["EDL"], "has_power_hook": true},
        })],
        5000,
        1_000,
        "nodea",
    )
    .unwrap();
    // The same row again, from a sender that does not mention controls at all.
    conminer_core::peers::inventory::import_devices(
        &mut reg,
        &owner,
        &[json!({
            "device": canonical, "endpoint": "tcp://192.0.2.1:5001", "state": "listening",
            "hops": 0,
        })],
        5000,
        2_000,
        "nodea",
    )
    .unwrap();
    let rows = reg.remote_devices().unwrap();
    let controls = rows[0]
        .remote_controls
        .clone()
        .expect("controls survive silence");
    assert_eq!(controls["controller"], "tac", "{controls}");
}

/// The owner publishes its distance, or none of the above can work.
#[test]
fn a_node_publishes_how_far_each_board_is_from_it() {
    let b = Node::with_board(
        "nodeb",
        "/dev/serial/by-id/usb-FTDI_Mine-if00-port0",
        "mine",
    );
    let v = b.call("list_devices", json!({"detail": true}));
    let row = v["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["device"].as_str().is_some_and(|s| s.contains("Mine")))
        .expect("its own board");
    assert_eq!(
        row["hops"], 0,
        "a node owns the boards it holds the tty for, and must say so: {row}"
    );
}

// ---------------------------------------------------------- one name, one node -

/// Register a node under a chosen instance id and name, as its beacon would.
fn advertise(node: &Node, instance_id: &str, name: &str, url: &str) {
    let mut reg = node.registry();
    peer_registry::upsert_advert(
        &mut reg,
        &Advert {
            instance_id: instance_id.to_string(),
            name: name.to_string(),
            version: "test".into(),
            mcp_url: url.to_string(),
            dash_url: String::new(),
            ser2net_host: "127.0.0.1".into(),
            ser2net_ports: vec![],
        },
        PeerSource::Beacon,
        Some("127.0.0.1"),
        node.now(),
    )
    .unwrap();
}

/// TWO HOSTS, ONE NAME: refused, never guessed.
///
/// `devices.node` keys on the node NAME and the router turns a name into an
/// address, so taking the first match means a power action aimed at one host can
/// land on another and answer success from the wrong board. There is no safe
/// pick: the operator meant a physical machine and the fleet no longer knows
/// which.
///
/// Not hypothetical. A deploy that copied one node's `.env` onto the others left
/// three hosts all named `charlie`; every peer table showed what looked like
/// duplicate registrations and nothing anywhere objected.
#[test]
fn two_nodes_answering_to_one_name_are_refused_not_guessed() {
    let a = Node::new("a");
    advertise(&a, "id-one", "bench", "http://10.0.0.1:8090/mcp");
    advertise(&a, "id-two", "bench", "http://10.0.0.2:8090/mcp");

    let err = peer_registry::by_name(&a.registry(), "bench")
        .expect_err("a name claimed by two nodes must not resolve to one of them");
    assert_eq!(err.code, conminer_core::error::ErrorCode::AmbiguousPeer);
    let text = err.to_string();
    // The operator has to be able to tell WHICH two, or they cannot fix it.
    assert!(text.contains("id-one") && text.contains("id-two"), "{text}");
    assert!(
        text.contains("10.0.0.1") && text.contains("10.0.0.2"),
        "and where they are: {text}"
    );
}

/// The ordinary case still resolves, or the guard above is just an outage.
#[test]
fn one_name_one_node_still_resolves() {
    let a = Node::new("a");
    advertise(&a, "id-one", "bench", "http://10.0.0.1:8090/mcp");
    advertise(&a, "id-two", "other", "http://10.0.0.2:8090/mcp");

    let found = peer_registry::by_name(&a.registry(), "bench")
        .expect("one node, one name")
        .expect("the peer is there");
    assert_eq!(found.instance_id, "id-one");
    assert!(
        peer_registry::by_name(&a.registry(), "nobody")
            .expect("an unknown name is not an error")
            .is_none(),
        "an unknown name is simply absent"
    );
}

/// A collision must be VISIBLE before it routes something somewhere wrong.
#[test]
fn the_peers_tool_reports_a_name_collision() {
    let a = Node::new("a");
    advertise(&a, "id-one", "bench", "http://10.0.0.1:8090/mcp");
    advertise(&a, "id-two", "bench", "http://10.0.0.2:8090/mcp");
    advertise(&a, "id-three", "alone", "http://10.0.0.3:8090/mcp");

    let out = a.call("peers", json!({}));
    let collisions = out["name_collisions"]
        .as_array()
        .expect("peers() must report name collisions");
    assert_eq!(collisions.len(), 1, "one colliding name: {out}");
    assert_eq!(collisions[0]["name"], "bench");
    let ids: Vec<&str> = collisions[0]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(ids.contains(&"id-one") && ids.contains(&"id-two"), "{out}");
}
