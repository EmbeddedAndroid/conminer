//! Long-running services (§3): discoveryd, ser2net supervision, minerd, mcpd.
//!
//! A service that cannot do its job refuses *loudly*. A compose stack reporting
//! itself healthy while capturing nothing is exactly the staleness trap §8.4
//! exists to close.

use crate::app::App;
use anyhow::{Context as _, Result};
use conminer_core::discovery;
use conminer_core::live::Capture;
use conminer_core::store::Registry;
use conminer_mcp::{Context, Server};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// §P3. How many relayed calls one peer may have in flight here at once.
///
/// A worker is not polling while it runs a call, so this is the concurrency the
/// far node gets. Four covers a bench: a parked `follow` plus actuation plus
/// headroom. Higher costs a thread each and buys nothing on hardware that can
/// only be power-cycled one board at a time.
const RELAY_WORKERS_PER_PEER: usize = 4;

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

/// Cancelled on SIGTERM/SIGINT so compose can stop a container cleanly.
fn shutdown_channel() -> (
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
) {
    tokio::sync::watch::channel(false)
}

async fn wait_for_signal(tx: tokio::sync::watch::Sender<bool>) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    tracing::info!("shutting down");
    let _ = tx.send(true);
}

// ---------------------------------------------------------------- discoveryd -

/// Watch for serial devices, keep the registry true, regenerate `ser2net.yaml`.
///
/// Discovery polls `/dev/serial/by-id` rather than subscribing to udev netlink.
/// That is the §11 fallback promoted to the primary path on purpose: netlink
/// availability inside a container varies by host distro, and a discovery
/// mechanism that works everywhere at 1 Hz is worth more than one that is
/// faster on some hosts and silently dead on others.
/// §P1. peerd: find the other nodes, and keep the fleet's inventory fresh.
///
/// Its own process for one reason that matters: multicast and broadcast do not
/// cross a docker bridge network, so this service runs with host networking
/// while everything else stays on the bridge. Putting the beacon inside minerd
/// or mcpd would mean giving THEM the host's network, which is a much larger
/// blast radius for a postcard-sized advert.
///
/// Two loops, deliberately independent:
///   * the beacon: shout every `beacon_interval_s`, listen continuously
///   * inventory: every `inventory_interval_s`, ask each live peer what it owns
///
/// If discovery fails entirely (no multicast on this network, a NAT-ed host
/// behind NAT), the static `[peers] nodes` list still works: it is a statement
/// by the operator, and it never expires./// Should a running capture be torn down for a device that is no longer
/// attachable?
///
/// A POWER CYCLE IS NOT A REMOVAL. Every power action takes the console's USB
/// away for a few seconds and the device goes `gone`; stopping capture there
/// meant it could only return once discoveryd re-detected the device, measured
/// at 9.4s on the bench, and the board printed its whole bootloader into that
/// hole. The capture task already knows how to sit out an absent console, so a
/// brief absence keeps it. Anything that is not a transient absence -- remote,
/// ignored, no port -- stops at once, as before.
fn should_stop_capture(
    is_remote: bool,
    ignored: bool,
    has_port: bool,
    state: &str,
    absent_for: Duration,
    grace: Duration,
) -> bool {
    let transient = !is_remote && !ignored && has_port && state == "gone";
    !(transient && absent_for < grace)
}

pub fn peerd(config: &Option<PathBuf>, data: &Option<PathBuf>) -> Result<()> {
    use conminer_core::peers::{beacon::Beacon, inventory, registry as pr, Identity, PeerClient};

    let app = App::load(config.as_deref(), data.as_deref())?;
    let cfg = app.config.clone();
    let now = app.clock.now_wall_ms();
    let identity = Identity::load_or_create(&app.data_dir, &cfg.peers.name, now)?;
    let advertise = advertise_host(&cfg);
    let advert = pr::Advert {
        instance_id: identity.instance_id.clone(),
        name: identity.name.clone(),
        // The BUILD, not the package version. `0.2.0` was reported by three
        // nodes running three different builds; what a peer needs to know is
        // whether it is talking to the same code it is.
        version: conminer_core::build_id().to_string(),
        mcp_url: format!("http://{advertise}:{}/mcp", cfg.mcpd.port),
        dash_url: format!(
            "http://{advertise}:{}",
            cfg.dashboard.bind.rsplit(':').next().unwrap_or("8080")
        ),
        ser2net_host: advertise.clone(),
        ser2net_ports: vec![],
    };

    tracing::info!(
        node = %identity.name,
        instance = %identity.instance_id,
        advertise = %advertise,
        udp_port = cfg.peers.udp_port,
        statics = cfg.peers.nodes.len(),
        "peerd starting"
    );
    if advertise.starts_with("127.") || advertise == "localhost" {
        // §M2's lesson, one layer out: an address that means "me" to every
        // listener is an address no peer can use.
        tracing::warn!(
            advertise = %advertise,
            "advertising a loopback address: no other node can reach this one. Set \
             [peers] advertise_host to this host's LAN address."
        );
    }

    // Static peers are recorded before anything else: they are how a fleet works
    // where broadcast cannot reach, and they must be usable the moment peerd
    // starts rather than after a discovery round that may never happen.
    {
        let mut reg = Registry::open(&app.data_dir)?;
        for url in &cfg.peers.nodes {
            pr::upsert_static(&mut reg, url, now)?;
        }
    }

    let beacon = if cfg.peers.enabled {
        match Beacon::bind(
            std::net::Ipv4Addr::UNSPECIFIED,
            cfg.peers.udp_port,
            &identity.instance_id,
        ) {
            Ok(b) => Some(b),
            Err(e) => {
                // A bound port is not a reason to give up the whole service: the
                // static list and inventory sync still work without a beacon.
                tracing::warn!(error = %e.message, "peer beacon disabled");
                None
            }
        }
    } else {
        None
    };

    let client = PeerClient::new(&identity.name);
    let rt = runtime()?;
    let (tx, mut rx) = shutdown_channel();
    rt.block_on(async move {
        tokio::spawn(wait_for_signal(tx));
        let mut last_beacon =
            std::time::Instant::now() - Duration::from_secs(cfg.peers.beacon_interval_s.max(1));
        let mut last_inventory = last_beacon;
        // §P3. Which peers already have a reverse-call worker parked on them.
        // The flag is how a worker that gave up tells this loop to start a fresh
        // one, without this loop having to reach into the thread.
        let mut relay_workers: HashMap<String, Arc<AtomicBool>> = HashMap::new();
        loop {
            if *rx.borrow_and_update() {
                break;
            }
            let now = app.clock.now_wall_ms();

            if let Some(b) = &beacon {
                if last_beacon.elapsed() >= Duration::from_secs(cfg.peers.beacon_interval_s.max(1))
                {
                    if let Err(e) = b.announce(&advert) {
                        tracing::debug!(error = %e.message, "beacon announce failed");
                    }
                    last_beacon = std::time::Instant::now();
                }
                // Drain whatever has arrived. The socket has a short read
                // timeout, so this returns promptly on a quiet network.
                while let Some((heard, from)) = b.recv() {
                    let mut reg = Registry::open(&app.data_dir)?;
                    // WHERE THE PEER SAYS IT IS, not where the packet came from.
                    //
                    // A broadcast's source address is whatever the local network
                    // stack chose, and on a bridged/NAT'd host (a NAT-ed host, for one) it
                    // arrives as loopback -- which is an address that means "me"
                    // to whoever reads it. The advert carries the address the
                    // peer wants to be reached on; the packet source is only a
                    // fallback for an advert that failed to state one.
                    let (advertised, _, _) =
                        conminer_core::peers::client::split_url(&heard.mcp_url);
                    let host = if advertised.is_empty() || advertised.starts_with("127.") {
                        from.ip().to_string()
                    } else {
                        advertised
                    };
                    pr::upsert_advert(&mut reg, &heard, pr::PeerSource::Beacon, Some(&host), now)?;
                }
            }

            if last_inventory.elapsed()
                >= Duration::from_secs(cfg.peers.inventory_interval_s.max(1))
            {
                last_inventory = std::time::Instant::now();
                let mut reg = Registry::open(&app.data_dir)?;
                let peers = pr::live(&reg, now, cfg.peers.ttl_s)?;
                let own_id = identity.instance_id.clone();
                // A static peer answers with its real identity the first time it
                // is reached, which replaces the placeholder row.
                for p in peers
                    .iter()
                    // BOTH placeholder shapes mean "identity unknown": the URL
                    // stand-in written before first contact, and the name
                    // stand-in an older build invented when it could not ask.
                    // A row left in either shape is a second entry for a node
                    // already known by its real id, and two entries sync the
                    // same devices in turn -- each marking the other's gone.
                    // EVERY static peer, every tick -- not only the ones still
                    // wearing a placeholder id.
                    //
                    // A peer's name is soft state like everything else in this
                    // table: it can change under us, and when it does, routing
                    // by node name sends calls to the wrong machine. Measured
                    // across all three nodes after a deploy accidentally shipped
                    // one host's config to the others: every node called itself
                    // `charlie`, and because adoption only ever ran for
                    // placeholders, the wrong names stayed in the table after
                    // the configs were corrected -- one row per node, all three
                    // labelled the same, with nothing to make them heal.
                    .filter(|p| p.source == pr::PeerSource::Static)
                {
                    // TEN SECONDS, NOT THREE. Measured against the lab host:
                    // a busy mcpd answers `diagnose` in 4.5 s, and the identity
                    // handshake was quietly timing out at 3 s every tick -- so
                    // the placeholder row never healed and the node kept showing
                    // up twice, once under the stand-in id and once under the
                    // real one from its beacon. A handshake that runs once per
                    // peer can afford to wait; the inventory call after it is
                    // the one that must stay snappy.
                    let handshake = client.call_tool(
                        &p.mcp_url,
                        "peers",
                        &serde_json::json!({}),
                        Duration::from_secs(10),
                    );
                    if let Err(e) = &handshake {
                        // A handshake that fails silently is why a placeholder
                        // row can sit in the table for ever, and why one node
                        // shows up twice with no explanation anywhere.
                        tracing::warn!(
                            peer = %p.mcp_url,
                            error = %e.message,
                            "identity handshake failed: this peer stays a placeholder"
                        );
                    }
                    if let Ok((reply, _)) = handshake {
                        let content = reply
                            .get("result")
                            .and_then(|r| r.get("structuredContent"))
                            .cloned()
                            .unwrap_or_default();
                        // The peer's REAL id, asked for rather than invented.
                        //
                        // NEVER MINT ONE. A made-up id can never merge with the
                        // real id the same node's beacon carries, so the node
                        // shows up twice for ever -- and because adoption then
                        // "succeeds" on every tick, nothing anywhere says why.
                        // Measured against a lab host on an older build: this
                        // loop re-created the identical placeholder every five
                        // seconds and logged a successful adoption each time.
                        // A peer that cannot name itself stays a placeholder.
                        let stated_id = content
                            .get("instance_id")
                            .and_then(|i| i.as_str())
                            .filter(|i| !i.is_empty())
                            .map(str::to_string);
                        // NEVER PEER WITH YOURSELF. A node that lists its own
                        // URL -- which is exactly what happens when one host's
                        // config reaches another -- proxies its own calls back
                        // to itself and re-exports its own boards as remote
                        // ones. Measured: alpha ran with a config naming
                        // alpha, and appeared in its own fleet table.
                        if stated_id.as_deref() == Some(own_id.as_str()) {
                            tracing::warn!(
                                url = %p.mcp_url,
                                "this peer is THIS node; dropping it rather than \
                                 peering with myself"
                            );
                            let _ = pr::forget(&mut reg, &p.instance_id);
                            continue;
                        }
                        if let (Some(name), Some(id)) = (
                            content.get("node").and_then(|n| n.as_str()),
                            stated_id.clone(),
                        ) {
                            let (host, _, _) = conminer_core::peers::client::split_url(&p.mcp_url);
                            let adopted = pr::Advert {
                                instance_id: id,
                                name: name.to_string(),
                                // WHICH BUILD that node runs, asked for at the
                                // same time as its name. A static peer is how
                                // this fleet crosses a subnet that broadcast
                                // cannot, so leaving this empty meant the one
                                // node reachable ONLY by static config could
                                // never report its build -- and `in_sync` stayed
                                // permanently uncertain about exactly the node
                                // hardest to check by hand.
                                version: content
                                    .get("build")
                                    .and_then(|b| b.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                mcp_url: p.mcp_url.clone(),
                                dash_url: String::new(),
                                ser2net_host: host.clone(),
                                ser2net_ports: vec![],
                            };
                            tracing::info!(
                                peer = %name,
                                id = %adopted.instance_id,
                                replacing = %p.instance_id,
                                "adopting a peer's real identity"
                            );
                            pr::adopt_identity(&mut reg, &p.mcp_url, &adopted, Some(&host), now)?;
                        } else if stated_id.is_none() {
                            tracing::warn!(
                                peer = %p.mcp_url,
                                "this peer does not report an instance id (an older build?): it \
                                 stays a placeholder rather than becoming a second entry for one \
                                 node"
                            );
                        }
                    }
                }
                let peers = pr::live(&reg, now, cfg.peers.ttl_s)?;
                match inventory::sync_all_as(
                    &mut reg,
                    &client,
                    &peers,
                    app.config.ser2net.base_port,
                    now,
                    // §P2. Told our own name, so a relayed row describing one of
                    // OUR boards is recognised and dropped rather than imported
                    // as a proxied copy of hardware we hold the tty for.
                    &identity.name,
                ) {
                    Ok(r) if r.rows_added > 0 || r.rows_gone > 0 || r.peers_failed > 0 => {
                        tracing::info!(
                            peers_ok = r.peers_ok,
                            peers_failed = r.peers_failed,
                            added = r.rows_added,
                            gone = r.rows_gone,
                            "fleet inventory changed"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e.message, "inventory sync failed"),
                }
                // §P3. TELL THE PEERS WE CAN REACH WHO WE ARE.
                //
                // Inventory is a pull, so a node that can open no connections
                // learns nothing -- however many peers can open one to IT. The
                // bravo bench sits upstream of a NAT and saw an empty fleet while
                // two nodes were talking to it every five seconds. So the
                // reachable side carries the conversation both ways: having just
                // asked a peer what it owns, tell it what we own.
                //
                // Only to peers that ANSWERED. Announcing into a timeout would
                // double the stall on an unreachable peer for nothing.
                if cfg.peers.announce {
                    let mine = local_inventory(&mut reg, &app)?;
                    for p in pr::all(&reg)?.iter().filter(|p| p.ok) {
                        let payload = serde_json::json!({
                            "node": identity.name,
                            "instance_id": identity.instance_id,
                            "build": conminer_core::build_id(),
                            "host": advertise,
                            "mcp_url": format!("http://{advertise}:{}/mcp", cfg.mcpd.port),
                            "ser2net_host": advertise,
                            "devices": mine,
                        });
                        match client.call_tool(
                            &p.mcp_url,
                            "peer_announce",
                            &payload,
                            Duration::from_secs(10),
                        ) {
                            Ok(_) => {}
                            Err(e) => tracing::debug!(
                                peer = %p.name,
                                error = %e.message,
                                "could not announce to this peer"
                            ),
                        }
                    }
                }
                // §P3. AND OFFER TO RUN THEIR CALLS.
                //
                // Announcing gives the unreachable side a complete view and no
                // way to touch anything. Long-poll workers fix that: each parks
                // on a peer asking for work, runs whatever comes back against
                // our own mcpd, and posts the answer over a second connection in
                // the same working direction.
                //
                // SEVERAL PER PEER, because a worker is not polling while it is
                // running a call. One would make the far node's calls strictly
                // serial, so a `follow` parked for two minutes would also hold
                // up every power and boot_mode behind it -- a hang that would
                // look like broken hardware rather than a queue.
                //
                // Same knob as the announcement, deliberately. A node that
                // publishes boards nobody can drive has published a catalogue,
                // not a bench.
                if cfg.peers.announce {
                    for p in pr::all(&reg)?
                        .iter()
                        .filter(|p| p.ok && !p.mcp_url.is_empty())
                    {
                        for slot in 0..RELAY_WORKERS_PER_PEER {
                            let key = format!("{}#{slot}", p.name);
                            if relay_workers.get(&key).is_some_and(|a| a.load(Relaxed)) {
                                continue;
                            }
                            let live = Arc::new(AtomicBool::new(true));
                            relay_workers.insert(key, live.clone());
                            let peer_url = p.mcp_url.clone();
                            let peer_name = p.name.clone();
                            let own_url = format!("http://127.0.0.1:{}/mcp", cfg.mcpd.port);
                            let me = identity.name.clone();
                            // A plain thread, not a task: every call in the round
                            // trip is blocking HTTP, and a parked thread is
                            // cheaper to reason about than making it async.
                            std::thread::spawn(move || {
                                let client = conminer_core::peers::PeerClient::new(me.clone());
                                let mut misses = 0u32;
                                while misses < 12 {
                                    match conminer_core::peers::relay::serve_once(
                                        &client,
                                        &peer_url,
                                        &own_url,
                                        &me,
                                        Duration::from_secs(20),
                                    ) {
                                        Ok(_) => misses = 0,
                                        Err(e) => {
                                            misses += 1;
                                            tracing::debug!(
                                                peer = %peer_name,
                                                error = %e.message,
                                                "relay poll failed"
                                            );
                                            // Back off rather than spin: a peer that
                                            // went away must not become a busy loop.
                                            std::thread::sleep(Duration::from_secs(5));
                                        }
                                    }
                                }
                                tracing::info!(peer = %peer_name, "relay worker stopping");
                                live.store(false, Relaxed);
                            });
                            tracing::debug!(peer = %p.name, slot, "relay worker started");
                        }
                    }
                }
                let expired = pr::expire(&mut reg, now, cfg.peers.ttl_s, 4)?;
                for id in expired {
                    tracing::info!(peer = %id, "peer expired from the fleet");
                }
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// This node's OWN hardware, in the shape `list_devices {detail:true}` returns.
///
/// §P3. What an announcement carries. Deliberately the same shape the pull uses,
/// so the receiver applies one import path to both and the two cannot drift --
/// and deliberately only rows this node OWNS: relaying somebody else's boards
/// onward is the pull side's job, with its own hop bound.
fn local_inventory(
    reg: &mut conminer_core::store::Registry,
    app: &App,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let present = reg.all_devices()?;
    let cfg = &app.config;
    // WHAT IS PLUGGED IN, not what has ever been seen. This published a
    // controller claim to every peer, so one host's stale attribution became the
    // whole fleet's: alpha told bravo that a Nucleo was Bantam-driven because a
    // Bantam unplugged five days earlier still had a row.
    let names = conminer_core::store::registry::present_on_this_host(&present);
    let mut out = Vec::new();
    for d in present.iter().filter(|d| d.node.is_none() && !d.ignored) {
        let Some(port) = d.ser2net_port else { continue };
        out.push(serde_json::json!({
            "device": d.display_name(),
            "canonical": d.canonical,
            "endpoint": format!("tcp://{}:{}", cfg.ser2net.bind, port),
            "state": d.state,
            // PRESENCE ABOVE, CAPTURE HEALTH HERE. They were one field with
            // two writers; a reader asking "is this console recording" was
            // answered by whichever process wrote last.
            "capture_state": d.capture_state,
            "ignored": d.ignored,
            "label": d.nickname,
            "target": d.target,
            // 0: the announcing node OWNS these. Same convention as the pull
            // side publishes, so one import rule serves both.
            "hops": 0,
            // The owner is the only node that can answer this; see §P2.
            //
            // Built by the SHARED builder the pull side also uses. This was a
            // second, independent copy carrying a comment saying it matched the
            // other one, and it drifted the moment a field was added: the pull
            // side learned to publish the controller's name and this did not,
            // so a node that only ever hears us ANNOUNCE drew our boards headed
            // by a raw by-id path while a node that pulls named them correctly.
            "controls": conminer_core::peers::inventory::controls_for(
                cfg, d, &present, &names,
            ),
        }));
    }
    Ok(out)
}

/// The address peers should use to reach this node.
///
/// Never loopback: an advert saying 127.0.0.1 tells every listener to talk to
/// itself. The guess uses the UDP connect trick -- no packet is sent, the kernel
/// simply picks the source address it would use for an off-host destination.
fn advertise_host(cfg: &conminer_core::config::Config) -> String {
    if !cfg.peers.advertise_host.is_empty() {
        return cfg.peers.advertise_host.clone();
    }
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?; // TEST-NET-1: routable, never answers
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

pub fn discoveryd(config: &Option<PathBuf>, data: &Option<PathBuf>) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    let dev_root =
        PathBuf::from(std::env::var("CONMINER_DEV_ROOT").unwrap_or_else(|_| "/dev".into()));
    let cfg_path = app.config.ser2net.config_path.clone();
    let hz = app.config.discovery.poll_fallback_hz.max(1);
    let period = Duration::from_millis(1000 / hz as u64);
    let debounce = Duration::from_millis(app.config.discovery.hotplug_debounce_ms);
    let removal_grace = Duration::from_millis(app.config.discovery.removal_grace_ms);

    tracing::info!(
        dev_root = %dev_root.display(),
        config = %cfg_path.display(),
        interval_ms = period.as_millis() as u64,
        "discoveryd starting"
    );

    let rt = runtime()?;
    let (tx, mut rx) = shutdown_channel();
    rt.block_on(async move {
        tokio::spawn(wait_for_signal(tx));
        let mut reg = Registry::open(&app.data_dir)?;
        let mut pending = RewritePending::default();

        // Write once at startup, before any change is detected.
        //
        // The debounced write below only fires after the device set *changes*,
        // so a service whose registry is already correct — a restart, a fresh
        // container on an existing volume — would never write the config at
        // all, and ser2net would come up serving nothing.
        if let Err(e) = write_ser2net_config(&reg, &app.config, &cfg_path) {
            tracing::error!(error = %e, "initial ser2net config write failed");
        }

        let mut rebind_tick: u64 = 0;
        loop {
            if *rx.borrow() {
                break;
            }
            // Recover consoles that a driver detach made vanish.
            //
            // Anything claiming an FTDI through usbfs detaches ftdi_sio, and the
            // by-id node goes with it -- a pyftdi EEPROM read did exactly that
            // on this bench and the console stayed gone for hours because
            // nothing put it back. conminer's own hooks rebind on their way out,
            // but this is the safety net for tools we do not own. It only binds
            // interfaces with NO driver, so a genuinely unplugged device stays
            // gone rather than being conjured back.
            rebind_tick = rebind_tick.wrapping_add(1);
            if rebind_tick % 10 == 0 && conminer_core::ftdi::rebind_ftdi_sio() > 0 {
                // Give udev a moment to recreate the by-id symlink before the
                // scan below decides what exists.
                tokio::time::sleep(Duration::from_millis(400)).await;
            }

            let found = discovery::scan(&dev_root).unwrap_or_default();
            let now = app.clock.now_wall_ms();
            match discovery::reconcile(&mut reg, &app.config, &found, now) {
                Ok(r) if r.changed() => {
                    // Cheap hubs bounce enumeration; wait for it to settle
                    // before rewriting a config the whole lab reads.
                    let only_removals = r.added.is_empty() && r.returned.is_empty();
                    pending.note_change(only_removals, std::time::Instant::now());
                    tracing::info!(
                        added = ?r.added, returned = ?r.returned, gone = ?r.gone,
                        ignored = r.ignored.len(),
                        rewrite_after_ms = pending.wait(debounce, removal_grace).as_millis() as u64,
                        "device set changed"
                    );
                }
                Ok(_) => {
                    // Also rewrite if the file has gone missing under us: an
                    // absent config is indistinguishable from a lab with no
                    // consoles, and ser2net would serve nothing until the next
                    // hotplug.
                    if pending.due(std::time::Instant::now(), debounce, removal_grace)
                        || !cfg_path.exists()
                    {
                        pending = RewritePending::default();
                        write_ser2net_config(&reg, &app.config, &cfg_path)?;
                    }
                }
                Err(e) => tracing::error!(error = %e.message, "reconcile failed"),
            }

            tokio::select! {
                _ = tokio::time::sleep(period) => {}
                _ = rx.changed() => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    })
}

/// When a pending ser2net config rewrite may land.
///
/// Additions and returns land after the hotplug debounce. A change that is
/// ONLY removals waits out `removal_grace` instead: a controller that shares
/// its FTDI between console and power control (the Bughopper) detaches that
/// console for the length of every press and it comes straight back, and
/// rewriting the config on each detach meant a SIGHUP, which ser2net 4.x
/// answers by dropping accepters, which meant a full restart that severed every
/// console on the node -- 39 times in 30 minutes on one bench, one of them
/// under a `run_command` mid-echo. ser2net serves an absent path as an open
/// failure and opens it fine on the next attach once it is back (measured,
/// 4.6.4), so an accepter for a console that is briefly gone costs nothing.
#[derive(Default, Debug, Clone, Copy)]
struct RewritePending {
    since: Option<std::time::Instant>,
    only_removals: bool,
}

impl RewritePending {
    fn note_change(&mut self, only_removals: bool, now: std::time::Instant) {
        match self.since {
            None => {
                self.since = Some(now);
                self.only_removals = only_removals;
            }
            // A removal already waiting out its grace must not delay an
            // addition; and a RETURN during the grace restarts the clock as a
            // short debounce, after which the rewrite is a byte-identical no-op
            // and ser2net is never signalled at all.
            Some(_) if !only_removals => {
                self.since = Some(now);
                self.only_removals = false;
            }
            Some(_) => {}
        }
    }

    fn wait(&self, debounce: Duration, removal_grace: Duration) -> Duration {
        if self.only_removals {
            removal_grace
        } else {
            debounce
        }
    }

    fn due(&self, now: std::time::Instant, debounce: Duration, removal_grace: Duration) -> bool {
        self.since
            .is_some_and(|t| now.duration_since(t) >= self.wait(debounce, removal_grace))
    }
}

/// The reopen requests that a ser2net restart could actually satisfy.
///
/// ONLY A PRESENT DEVICE CAN BE WEDGED. An open failure for a path that is not
/// there is an absence, not a wedge -- a shared-FTDI power press detaches the
/// console for seconds -- and ser2net's reply to the client reads the same
/// either way. Restarting the daemon for an absent path served nobody: measured
/// against stock ser2net 4.6.4, an open that failed on a missing path succeeds
/// on the next attach once the path is back, with no restart. So an absent
/// device's request is dropped, with a note, and every other console on the
/// node keeps its session.
/// The consoles ser2net is currently serving, and whether each one's device
/// node is on the bus, read from the generated config.
///
/// The supervisor has no registry handle -- it runs stock ser2net against a
/// file -- but that file lists every `connector: serialdev,<path>` it was told
/// to open, and the connection key is derived from the same path. That is
/// enough to tell "this board is mid-reset" from "ser2net is wedged".
fn managed_consoles(config_text: &str, present: impl Fn(&str) -> bool) -> Vec<(String, bool)> {
    config_text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("connector: serialdev,"))
        .filter_map(|rest| rest.split(',').next())
        .map(|dev| (conminer_core::discovery::yaml_key(dev), present(dev)))
        .collect()
}

/// Is a ser2net open-failure worth restarting ser2net over?
///
/// RESTARTING SER2NET DROPS EVERY CONSOLE ON THE HOST. That is the right price
/// for recovering a wedged open, and far too high for a board that is simply
/// mid-reset: ser2net cannot open a tty that is not there, no restart changes
/// that, and the boards that ARE up lose their consoles for nothing. Seen on
/// the bench as 27 re-attaches in three minutes on one board while every other
/// console attached twice, and felt by an operator as a laggy, chunky console.
///
/// ser2net names the connection in its own message, and the connection key is
/// derived from the device path, so a failure can be attributed. A failure that
/// names a device we know to be ABSENT is not worth a restart. Anything that
/// cannot be attributed still is: an unexplained failure is exactly the wedge
/// this recovery exists for.
fn open_failure_worth_a_restart(lines: &[String], managed: &[(String, bool)]) -> bool {
    if lines.is_empty() {
        // The flag fired without a captured line (older path): behave as before.
        return true;
    }
    lines.iter().any(|line| {
        match managed.iter().find(|(key, _)| line.contains(key.as_str())) {
            Some((key, present)) => {
                if !*present {
                    tracing::info!(
                        connection = %key,
                        "ser2net could not open a device that is not on the bus; it is \
                         mid-reset, not wedged -- no restart, the next attach after it \
                         returns opens it"
                    );
                }
                *present
            }
            // Not attributable to any console we manage: treat as a real wedge.
            None => true,
        }
    })
}

fn reopen_requests_worth_a_restart(
    requested: Vec<conminer_core::recovery::ReopenRequest>,
    present: impl Fn(&str) -> bool,
) -> Vec<conminer_core::recovery::ReopenRequest> {
    requested
        .into_iter()
        .filter(|r| {
            if present(&r.device) {
                tracing::warn!(device = %r.device, reason = %r.reason, "reopen requested");
                true
            } else {
                tracing::info!(
                    device = %r.device, reason = %r.reason,
                    "reopen requested for an absent device; nothing to reopen -- the next \
                     attach after it returns opens it, no restart"
                );
                false
            }
        })
        .collect()
}

fn write_ser2net_config(
    reg: &Registry,
    cfg: &conminer_core::config::Config,
    path: &Path,
) -> Result<()> {
    let devices = reg.all_devices()?;
    let text = discovery::ser2net_config(&devices, cfg);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write-then-rename: ser2net must never read a half-written config.
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    tracing::info!(path = %path.display(), devices = devices.len(), "wrote ser2net config");
    Ok(())
}

// -------------------------------------------------------- ser2net supervisor -

/// Run stock upstream ser2net against the generated config, reloading on change.
///
/// conminer never claims exclusive ownership of a `/dev/tty*`: ser2net fans each
/// port out over TCP so minicom, uart-mcp, labgrid and conminer can all attach to
/// the same console at once (§2).
pub fn ser2net_supervisor(config: &Option<PathBuf>, data: &Option<PathBuf>) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    let cfg_path = app.config.ser2net.config_path.clone();
    let run_dir = app.config.paths.run_dir.clone();
    let binary = std::env::var("CONMINER_SER2NET_BIN").unwrap_or_else(|_| "ser2net".into());

    let rt = runtime()?;
    let (tx, mut rx) = shutdown_channel();
    rt.block_on(async move {
        tokio::spawn(wait_for_signal(tx));

        // Nothing to serve until discoveryd has written a config.
        while !cfg_path.exists() {
            if *rx.borrow() {
                return Ok(());
            }
            tracing::info!(path = %cfg_path.display(), "waiting for the generated config");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = rx.changed() => {}
            }
        }

        // Set by the log relay when ser2net reports it could not open a device.
        let open_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The lines behind the flag, so a failure can be attributed to a device.
        let open_failures: OpenFailureLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut child = spawn_ser2net_logged(
            &binary,
            &cfg_path,
            Some(open_failed.clone()),
            Some(open_failures.clone()),
        )?;
        // A transiently busy tty must not cost a console permanently, but a tty
        // that is genuinely held forever must not cause an endless restart loop
        // either -- so recovery is capped, and the cap resets once a spawn comes
        // up clean.
        let mut open_failure_recoveries: u32 = 0;
        const MAX_OPEN_FAILURE_RECOVERIES: u32 = 5;
        // Hash the CONTENT, not the mtime. A device that disappears and comes
        // straight back regenerates a byte-identical config, and an mtime
        // comparison SIGHUPs on every one of those rewrites -- which is exactly
        // the fd-churn that leaves ser2net with no accepters. If the bytes did
        // not change, ser2net has nothing to reload.
        let mut seen = config_digest(&cfg_path);
        // ~15s of quiet after a spawn before the liveness check may fire.
        const COOLDOWN_TICKS: u64 = 30;
        let mut ticks: u64 = 0;

        loop {
            if *rx.borrow() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = rx.changed() => continue,
            }

            // Restart if it died: a dead ser2net means every console is dark.
            if let Ok(Some(status)) = child.try_wait() {
                tracing::error!(?status, "ser2net exited; restarting");
                child = spawn_ser2net(&binary, &cfg_path)?;
                continue;
            }

            // A reload is not the only way to end up with no listeners: ser2net
            // can come up missing one (measured on a fresh deploy -- three of
            // four ports bound, nothing logged, healthcheck green). So verify
            // continuously, not only after a SIGHUP.
            // Generous grace, and a cooldown after every spawn. A short deadline
            // here does not detect a wedged ser2net faster -- it just kills a
            // healthy one that is still binding, which was measured as a restart
            // loop reporting ports missing that were about to appear.
            ticks = ticks.wrapping_add(1);

            // SECOND TRIGGER: a reopen requested by whoever actually holds the
            // connection. ser2net answers a failed open by serving its failure
            // text to the CLIENT and often logs nothing at all -- measured, the
            // RIDE's AP console delivered 0 bytes through ser2net while the tty
            // produced 102190 in the same window, with an empty ser2net log. So
            // the stderr watcher alone cannot see the wedge, and minerd reports
            // it across the shared run dir instead.
            let requested = reopen_requests_worth_a_restart(
                conminer_core::recovery::take_reopen_requests(&run_dir),
                |dev| std::path::Path::new(dev).exists(),
            );
            // ser2net never retries a failed device open: it serves the failure
            // text to clients forever. Restart it so the port reopens, but only
            // after the cooldown (the tty is often busy for a moment) and only a
            // bounded number of times.
            // A failure naming a device that is not on the bus is a board
            // mid-reset, not a wedged ser2net, and restarting over it drops
            // every healthy console on the host for nothing.
            let saw_open_failure = open_failed.swap(false, std::sync::atomic::Ordering::Relaxed);
            let lines: Vec<String> = open_failures
                .lock()
                .map(|mut v| std::mem::take(&mut *v))
                .unwrap_or_default();
            let managed = std::fs::read_to_string(&cfg_path)
                .map(|t| managed_consoles(&t, |dev| std::path::Path::new(dev).exists()))
                .unwrap_or_default();
            let worth_it = saw_open_failure && open_failure_worth_a_restart(&lines, &managed);
            if ticks >= COOLDOWN_TICKS && (worth_it || !requested.is_empty()) {
                if open_failure_recoveries < MAX_OPEN_FAILURE_RECOVERIES {
                    open_failure_recoveries += 1;
                    tracing::warn!(
                        attempt = open_failure_recoveries,
                        "restarting ser2net to reopen a device whose open failed"
                    );
                    stop_ser2net(&mut child).await;
                    child = spawn_ser2net_logged(
                        &binary,
                        &cfg_path,
                        Some(open_failed.clone()),
                        Some(open_failures.clone()),
                    )?;
                    ticks = 0;
                    continue;
                }
                // Say so plainly rather than retrying into the void: something
                // outside conminer is holding the device.
                tracing::error!(
                    "a device open keeps failing after {MAX_OPEN_FAILURE_RECOVERIES} restarts; \
                     something outside conminer is holding the tty"
                );
            }
            if ticks >= COOLDOWN_TICKS
                && ticks % 60 == 0
                && missing_listeners(&cfg_path, 40).await.is_some()
            {
                stop_ser2net(&mut child).await;
                child = spawn_ser2net(&binary, &cfg_path)?;
                ticks = 0;
                continue;
            }

            // A spawn that has stayed up past the cooldown without a new open
            // failure counts as clean, so a later unrelated blip gets a full
            // budget again.
            if ticks == COOLDOWN_TICKS * 4 {
                open_failure_recoveries = 0;
            }

            let now = config_digest(&cfg_path);
            if now != seen {
                // Debounce: device churn can rewrite this file repeatedly (a
                // flapping by-id symlink was measured regenerating it once a
                // second), and SIGHUPing ser2net on every rewrite is how it ends
                // up with no accepters at all. Wait for the file to hold still.
                let mut settled = now;
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let again = config_digest(&cfg_path);
                    if again == settled {
                        break;
                    }
                    settled = again;
                }
                seen = settled;

                if let Some(pid) = child.id() {
                    tracing::info!(pid, "config changed; reloading ser2net");
                    // SAFETY: `pid` came from a child we spawned and have not reaped.
                    unsafe {
                        libc::kill(pid as i32, libc::SIGHUP);
                    }

                    // A reload that silently drops the accepters is the worst
                    // failure this service has: existing sessions survive, so
                    // capture keeps streaming and every health check passes,
                    // while every NEW attach is refused. Verify the ports came
                    // back, and restart rather than sit there looking healthy.
                    if missing_listeners(&cfg_path, 20).await.is_some() {
                        stop_ser2net(&mut child).await;
                        child = spawn_ser2net(&binary, &cfg_path)?;
                        ticks = 0;
                    }
                }
            }
        }

        let _ = child.start_kill();
        let _ = child.wait().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// TCP ports currently in LISTEN state, from `/proc/net/tcp{,6}`.
///
/// State `0A` is LISTEN; `01` is ESTABLISHED. That distinction is the whole
/// point: after a bad reload ser2net was measured holding four ESTABLISHED
/// sessions and ZERO listeners, so capture kept streaming and looked healthy
/// while every new console attach was refused.
fn listening_ports() -> std::collections::BTreeSet<u16> {
    let mut out = std::collections::BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            out.extend(parse_listeners(&text));
        }
    }
    out
}

/// Parse listening ports out of a `/proc/net/tcp[6]` table.
///
/// Split out from the reader purely so it can be tested against a real kernel
/// table, because the column walk here was wrong in a way no amount of staring
/// caught: the fields are
///
///   sl  local_address rem_address st tx_queue ...
///   0:  0100007F:1389 00000000:0000 0A ...
///
/// so after `nth(1)` yields `local_address`, the *next* item is `rem_address`,
/// not the state. Reading the state from the wrong column made it always
/// `00000000:0000`, never `0A`, so this returned an EMPTY set on a perfectly
/// healthy ser2net -- and the supervisor dutifully "recovered" from that phantom
/// failure by restarting ser2net every 40 seconds, cutting every console on the
/// rig mid-stream. A watchdog that cannot see is worse than no watchdog.
fn parse_listeners(text: &str) -> std::collections::BTreeSet<u16> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines().skip(1) {
        let mut f = line.split_whitespace();
        // local_address is column 1, st is column 3.
        let (Some(local), Some(state)) = (f.nth(1), f.nth(1)) else {
            continue;
        };
        if state != "0A" {
            continue;
        }
        if let Some((_, port)) = local.rsplit_once(':') {
            if let Ok(p) = u16::from_str_radix(port, 16) {
                out.insert(p);
            }
        }
    }
    out
}

/// Ports the generated config says ser2net should be accepting on.
fn configured_ports(cfg: &Path) -> std::collections::BTreeSet<u16> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(text) = std::fs::read_to_string(cfg) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("accepter:") {
            continue;
        }
        // `accepter: telnet(rfc2217=false),tcp,0.0.0.0,5001`
        if let Some(last) = line.rsplit(',').next() {
            if let Ok(p) = last.trim().parse::<u16>() {
                out.insert(p);
            }
        }
    }
    out
}

/// Configured ports with no listener, after allowing `tries` quarter-seconds.
///
/// Returns `None` when everything the config asks for is listening.
async fn missing_listeners(cfg: &Path, tries: u32) -> Option<Vec<u16>> {
    let want = configured_ports(cfg);
    if want.is_empty() {
        return None;
    }
    let mut missing: Vec<u16> = Vec::new();
    for _ in 0..tries.max(1) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let live = listening_ports();
        missing = want.difference(&live).copied().collect();
        if missing.is_empty() {
            return None;
        }
    }
    tracing::error!(
        ?missing,
        "ser2net has no listener on configured ports; restarting"
    );
    Some(missing)
}

/// Content digest of the generated config, or `None` when it cannot be read.
///
/// Deliberately content-based: the supervisor must not react to a rewrite that
/// changed nothing. A flapping by-id symlink regenerates identical bytes, and
/// SIGHUPing on each one is how ser2net ends up with no accepters at all.
fn config_digest(cfg: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let text = std::fs::read(cfg).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    Some(h.finish())
}

/// Stop a running ser2net and wait for the kernel to release its sockets.
///
/// Killing and immediately respawning races the old process, which still owns
/// its TCP accepters: the new one fails to bind with "Address already in use",
/// and the freed sockets then sit in TIME_WAIT, so a spammy respawn wedges the
/// whole console layer for a minute. Ported from the HIL's Ser2netManager,
/// which learned this the hard way.
async fn stop_ser2net(child: &mut tokio::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.start_kill();
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            tracing::warn!("ser2net ignored SIGTERM after 5s; killing");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    // The kernel needs a beat to release the listening sockets even after the
    // process is gone.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// True when a ser2net log line says it could not open a serial device.
///
/// ser2net does not retry a failed open. It keeps the accepter bound and serves
/// the failure text to every client forever, so the console looks alive and
/// answers with `Device open failure: Object was already in use` instead of the
/// board. Measured on the RIDE: an unrelated process held the tty for a few
/// seconds during a diagnostic, and that console stayed dark long after the
/// process exited -- only a ser2net restart brought it back. A transiently busy
/// tty must not cost a console until a human notices.
fn is_device_open_failure(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("device open failure")
        || (l.contains("unable to open") && l.contains("device"))
        || l.contains("could not open device")
}

fn spawn_ser2net(binary: &str, cfg: &Path) -> Result<tokio::process::Child> {
    spawn_ser2net_watched(binary, cfg, None)
}

type OpenFailureLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn spawn_ser2net_watched(
    binary: &str,
    cfg: &Path,
    open_failed: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<tokio::process::Child> {
    spawn_ser2net_logged(binary, cfg, open_failed, None)
}

/// As [`spawn_ser2net_watched`], but also keeping the failure lines themselves.
///
/// The bare flag says "something failed to open" and nothing more, so the
/// supervisor could only restart blindly. ser2net names the connection in its
/// message, so keeping the line is what lets a failure be attributed to a
/// device and an absent board be told from a wedge.
fn spawn_ser2net_logged(
    binary: &str,
    cfg: &Path,
    open_failed: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    failures: Option<OpenFailureLog>,
) -> Result<tokio::process::Child> {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(binary)
        // -n: stay in the foreground so the supervisor owns the lifecycle.
        // -d: log to stderr. Without it ser2net's own diagnostics go nowhere,
        //     which is why a port that failed to open was invisible: the only
        //     line anyone ever saw from ser2net was an unrelated mdns warning.
        .args(["-n", "-d", "-c"])
        .arg(cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start {binary}"))?;

    // Relay ser2net's output into our log. A per-port open failure is exactly
    // the thing an operator needs to see, and it only exists on this stream.
    for (name, pipe) in [
        ("stdout", child.stdout.take().map(EitherPipe::Out)),
        ("stderr", child.stderr.take().map(EitherPipe::Err)),
    ] {
        let flag = open_failed.clone();
        let failure_log = failures.clone();
        if let Some(pipe) = pipe {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = match pipe {
                    EitherPipe::Out(p) => BufReader::new(Box::pin(p) as PinnedRead).lines(),
                    EitherPipe::Err(p) => BufReader::new(Box::pin(p) as PinnedRead).lines(),
                };
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        if is_device_open_failure(&line) {
                            tracing::error!(
                                stream = name,
                                "ser2net could not open a device; scheduling recovery: {line}"
                            );
                            if let Some(l) = &failure_log {
                                if let Ok(mut v) = l.lock() {
                                    // Bounded: a wedged ser2net can repeat this
                                    // line forever and the decision only needs
                                    // the recent ones.
                                    if v.len() >= 32 {
                                        v.remove(0);
                                    }
                                    v.push(line.clone());
                                }
                            }
                            if let Some(f) = &flag {
                                f.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        } else {
                            tracing::info!(stream = name, "ser2net: {line}");
                        }
                    }
                }
            });
        }
    }
    Ok(child)
}

type PinnedRead = std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>;

enum EitherPipe {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

// -------------------------------------------------------------------- minerd -

/// Live capture for every discovered device (§3).
pub fn minerd(config: &Option<PathBuf>, data: &Option<PathBuf>) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    let rt = runtime()?;
    let (tx, mut rx) = shutdown_channel();

    rt.block_on(async move {
        tokio::spawn(wait_for_signal(tx));
        let registry = Arc::new(Mutex::new(Registry::open(&app.data_dir)?));

        // minerd is the SINGLE reader of each device's ser2net connection, so it
        // is also the place that republishes. dashd and mcpd subscribe here
        // instead of each opening their own connection to the same console --
        // which is what made tty contention everyone's problem and nobody's job.
        let hub = conminer_core::broker::Hub::new();
        {
            let sock = conminer_core::broker::socket_path(&app.config.paths.run_dir);
            let h = hub.clone();
            let brx = rx.clone();
            tracing::info!(socket = %sock.display(), "serving console broker");
            tokio::spawn(async move {
                if let Err(e) = conminer_core::broker::serve(h, sock, brx).await {
                    // A broker failure must not take capture down with it:
                    // capture is the system of record, the fan-out is a
                    // convenience for everyone else.
                    tracing::error!(error = %e, "console broker stopped");
                }
            });
        }

        let mut running: HashMap<i64, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut supervised: HashMap<i64, tokio::sync::watch::Sender<bool>> = HashMap::new();
        // When each device FIRST went absent, so a board that is merely
        // power-cycling keeps its capture instead of losing it (see below).
        let mut gone_since: HashMap<i64, std::time::Instant> = HashMap::new();

        loop {
            if *rx.borrow() {
                break;
            }
            let devices = registry.lock().unwrap().all_devices().unwrap_or_default();

            for d in &devices {
                // §P1. NEVER CAPTURE A PEER'S BOARD.
                //
                // A remote row is a pointer, not hardware: the owner runs the
                // only miner for it. Attaching here would open a second store
                // for a console this host cannot see, take that store's writer
                // lock, and then block every OTHER process that touches the row
                // -- measured on the first two-host bring-up, where mcpd stopped
                // answering entirely within seconds of the first inventory sync.
                //
                // The re-exported console still works: ser2net relays it, and
                // nothing on this side needs to mine what the owner already has.
                let attachable = !d.kind.is_remote()
                    && !d.ignored
                    && d.state != "gone"
                    && d.ser2net_port.is_some();
                let alive = running.get(&d.id).is_some_and(|h| !h.is_finished());

                if attachable && !alive {
                    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                    match Capture::open(
                        d.clone(),
                        app.config.clone(),
                        app.profiles.clone(),
                        registry.clone(),
                        app.clock.clone(),
                        &app.data_dir,
                    ) {
                        Ok(cap) => {
                            let cap = cap.with_hub(hub.clone());
                            let name = d.display_name().to_string();
                            tracing::info!(device = %name, "starting capture");
                            let h = tokio::spawn(async move {
                                if let Err(e) = cap.run(stop_rx).await {
                                    tracing::error!(device = %name, error = %e.message, "capture failed");
                                }
                            });
                            running.insert(d.id, h);
                            supervised.insert(d.id, stop_tx);
                        }
                        Err(e) => {
                            // Most often the writer lock: another process owns
                            // this device. Retried on the next pass.
                            tracing::warn!(
                                device = %d.display_name(), error = %e.message,
                                "cannot start capture yet"
                            );
                        }
                    }
                } else if !attachable && alive {
                    // A POWER CYCLE IS NOT A REMOVAL.
                    //
                    // Every power action takes the console's USB away for a few
                    // seconds, which marks the device `gone`. Tearing the
                    // capture down here meant it could only come back after
                    // discoveryd re-detected the device -- measured on the
                    // bench at 9.4s end to end (`device gone; stopping capture`
                    // 22:45:53.695, `starting capture` 22:46:01.145, `attached`
                    // 22:46:03.119) -- and the board printed its entire
                    // bootloader into that hole. The epoch then began mid-line,
                    // NUL-spliced, with the firmware banner missing entirely
                    // (reports #31/#32/#34, and an operator watching the web UI
                    // seeing "zero bootloader messages").
                    //
                    // The capture task already knows how to sit out an absent
                    // console: it re-dials, and it returns to its floor delay
                    // the moment the tty is back. Keeping it alive takes the
                    // discovery round-trip off the critical path entirely, so
                    // the console is being read again the instant ser2net can
                    // open it. Only an absence that outlasts the same grace
                    // discoveryd gives the accepter is a real removal.
                    let since = *gone_since.entry(d.id).or_insert_with(std::time::Instant::now);
                    let grace = Duration::from_millis(app.config.discovery.removal_grace_ms);
                    let stop = should_stop_capture(
                        d.kind.is_remote(),
                        d.ignored,
                        d.ser2net_port.is_some(),
                        &d.state,
                        since.elapsed(),
                        grace,
                    );
                    if !stop {
                        tracing::debug!(
                            device = %d.display_name(),
                            absent_ms = since.elapsed().as_millis() as u64,
                            "console absent; holding capture through the actuation"
                        );
                    } else {
                        tracing::info!(device = %d.display_name(), "device gone; stopping capture");
                        if let Some(s) = supervised.remove(&d.id) {
                            let _ = s.send(true);
                        }
                        running.remove(&d.id);
                        gone_since.remove(&d.id);
                    }
                } else if attachable {
                    // Back, and being read: forget the absence.
                    gone_since.remove(&d.id);
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = rx.changed() => {}
            }
        }

        for (_, s) in supervised {
            let _ = s.send(true);
        }
        for (_, h) in running {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
        Ok::<_, anyhow::Error>(())
    })
}

// ---------------------------------------------------------------------- mcpd -

/// The MCP server (§8). `--stdio` is the `docker exec` form; otherwise it serves
/// streamable HTTP plus `/healthz` and `/metrics`.
pub fn mcpd(config: &Option<PathBuf>, data: &Option<PathBuf>, stdio: bool) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    let mut cfg = app.config.clone();
    cfg.paths.data_dir = app.data_dir.clone();
    let bind: SocketAddr = format!("{}:{}", cfg.mcpd.bind, cfg.mcpd.port)
        .parse()
        .with_context(|| format!("invalid mcpd bind {}:{}", cfg.mcpd.bind, cfg.mcpd.port))?;

    let ctx = Context::open(cfg, app.profiles.clone(), app.clock.clone())?;
    let server = Server::new(ctx);

    runtime()?.block_on(async move {
        if stdio {
            server.serve_stdio().await
        } else {
            server.serve_http(bind).await
        }
    })
}

// ------------------------------------------------------------------- dashd --

/// The human dashboard (§17).
///
/// Its own process and its own port, because it is the one surface that streams
/// a console straight into a browser and can transmit: a host that should serve
/// only agents simply does not run it.
pub fn dashd(config: &Option<PathBuf>, data: &Option<PathBuf>) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    let mut cfg = app.config.clone();
    cfg.paths.data_dir = app.data_dir.clone();
    let bind = cfg.dashboard.bind.clone();

    runtime()?.block_on(async move {
        let (tx, rx) = shutdown_channel();
        tokio::spawn(wait_for_signal(tx));
        let dash = conminer::dash::Dash::new(cfg, app.data_dir.clone());
        // Populate before accepting, so the first page load is already correct.
        if let Err(e) = dash.refresh() {
            tracing::warn!(error = %e, "initial device scan failed");
        }
        conminer::dash::serve(dash, &bind, rx).await
    })
}

// --------------------------------------------------------------- healthcheck -

/// Compose healthcheck (§14.2). Checks the things that would make a service
/// silently useless: unreadable config, unopenable registry, unwritable volume,
/// and — for a network service — a socket that is not actually accepting.
pub fn healthcheck(config: &Option<PathBuf>, data: &Option<PathBuf>, service: &str) -> Result<()> {
    let app = App::load(config.as_deref(), data.as_deref())?;
    check_data_dir(&app.data_dir)?;
    let reg = app.registry()?;
    let devices = reg.all_devices()?;

    match service {
        "mcpd" => {
            let addr = format!("{}:{}", app.config.mcpd.bind, app.config.mcpd.port);
            std::net::TcpStream::connect_timeout(
                &addr
                    .parse()
                    .with_context(|| format!("invalid bind {addr}"))?,
                Duration::from_secs(2),
            )
            .with_context(|| format!("mcpd is not accepting connections on {addr}"))?;
        }
        "dashd" => {
            let addr = &app.config.dashboard.bind;
            let sock: std::net::SocketAddr = addr
                .parse()
                .with_context(|| format!("invalid dashboard.bind {addr}"))?;
            // 0.0.0.0 is a bind address, not a destination: probe loopback.
            let probe = if sock.ip().is_unspecified() {
                std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), sock.port())
            } else {
                sock
            };
            std::net::TcpStream::connect_timeout(&probe, Duration::from_secs(2))
                .with_context(|| format!("dashd is not accepting connections on {probe}"))?;
        }
        "ser2net" => {
            // Only meaningful once discoveryd has produced a config.
            let p = &app.config.ser2net.config_path;
            if p.exists() {
                let text = std::fs::read_to_string(p)?;
                anyhow::ensure!(
                    text.contains("%YAML"),
                    "{} exists but has no ser2net 3.x port lines",
                    p.display()
                );
            }
        }
        "minerd" => {
            // A miner that is up but attached to nothing is worth saying out
            // loud rather than reporting as healthy.
            let attachable = devices
                .iter()
                .filter(|d| !d.ignored && d.state != "gone")
                .count();
            let listening = devices
                .iter()
                // "attached" is a CAPTURE fact; `state` is presence.
                .filter(|d| {
                    matches!(
                        d.capture_state.as_deref().unwrap_or(d.state.as_str()),
                        "listening" | "streaming" | "garbage"
                    )
                })
                .count();
            println!(
                "minerd: ok ({listening}/{attachable} devices attached, {} known)",
                devices.len()
            );
            return Ok(());
        }
        _ => {}
    }

    println!(
        "{service}: ok ({} devices, {} profiles)",
        devices.len(),
        app.profiles.names().len()
    );
    Ok(())
}

fn check_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".conminer-write-probe");
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(&probe)?;
    Ok(())
}

#[cfg(test)]
mod listener_tests {
    use super::*;

    /// A verbatim `/proc/net/tcp` table from the lab host while ser2net was
    /// healthy and accepting on every configured port. 1389..=1393 are hex for
    /// 5001..=5011.
    const REAL_TABLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0B00007F:946F 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000 100 0 0 10 0
   1: 00000000:1389 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000 100 0 0 10 0
   2: 00000000:138E 00000000:0000 0A 00000000:00000000 00000000:00 00000000     0        0 12347 1 0000 100 0 0 10 0
   3: 00000000:1393 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12348 1 0000 100 0 0 10 0
";

    /// The regression that matters: this returned an EMPTY set against a
    /// perfectly healthy ser2net, because the state was read from the
    /// rem_address column and so never equalled "0A". The supervisor treated
    /// that phantom as a wedge and restarted ser2net every 40 seconds, which
    /// severed every console on the rig mid-stream -- the watchdog was the
    /// outage. Parsing a real kernel table is the only way to catch this;
    /// a hand-rolled fixture would have encoded the same misreading.
    #[test]
    fn a_healthy_ser2net_is_not_reported_as_missing_its_listeners() {
        let got = parse_listeners(REAL_TABLE);
        assert!(
            got.contains(&5001) && got.contains(&5006) && got.contains(&5011),
            "listening ports must be found in a real /proc/net/tcp table, got {got:?}"
        );

        // And the whole point: nothing is "missing", so nothing gets restarted.
        let configured: std::collections::BTreeSet<u16> = [5001, 5006, 5011].into_iter().collect();
        let missing: Vec<u16> = configured.difference(&got).copied().collect();
        assert!(
            missing.is_empty(),
            "healthy ser2net reported missing {missing:?}"
        );
    }

    #[test]
    fn only_listening_sockets_count() {
        // st=01 is ESTABLISHED. An accepted connection is not a listener, and
        // counting one would hide a genuinely dead accepter.
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:138E 0100007F:C001 01 00000000:00000000 00:00000000 00000000     0        0 1 1 0000 100 0 0 10 0
";
        assert!(
            parse_listeners(table).is_empty(),
            "an ESTABLISHED socket must not be mistaken for a listener"
        );
    }

    #[test]
    fn the_remote_port_is_never_mistaken_for_a_listening_port() {
        // A listener on 5006 whose peer is on port 0x1F90 (8080). Reading the
        // wrong column could harvest 8080 and report a listener that does not
        // exist -- the same class of error, inverted into a false negative on
        // the restart path.
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:138E 0100007F:1F90 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000 100 0 0 10 0
";
        let got = parse_listeners(table);
        assert!(got.contains(&5006));
        assert!(!got.contains(&8080), "harvested the remote port: {got:?}");
    }
}

#[cfg(test)]
mod open_failure_tests {
    use super::*;

    /// The exact line ser2net served to every client on a wedged console. It
    /// keeps the accepter bound and answers with this text instead of the
    /// board, so the port looks healthy from the outside: `diagnose` reported
    /// "connected but received NOTHING" and the console stayed dark long after
    /// the process that briefly held the tty had exited.
    #[test]
    fn the_real_wedge_line_is_recognised() {
        assert!(is_device_open_failure(
            "Device open failure: Object was already in use"
        ));
    }

    #[test]
    fn other_phrasings_are_recognised() {
        assert!(is_device_open_failure(
            "ser2net: Unable to open device /dev/ttyUSB5"
        ));
        assert!(is_device_open_failure("could not open device"));
        // Case must not matter; ser2net is not consistent about it.
        assert!(is_device_open_failure("DEVICE OPEN FAILURE: busy"));
    }

    /// The benign line that is always present must never trigger a restart --
    /// treating it as a failure would rebuild the exact restart loop this
    /// module already had once.
    #[test]
    fn the_harmless_mdns_warning_does_not_trigger_recovery() {
        assert!(!is_device_open_failure(
            "ser2net: Unable to start mdns: Out of memory"
        ));
        assert!(!is_device_open_failure("ser2net: certificate loaded"));
        assert!(!is_device_open_failure(""));
    }

    // ---- what a shared-FTDI press must NOT cost the rest of the node ----

    fn req(dev: &str) -> conminer_core::recovery::ReopenRequest {
        conminer_core::recovery::ReopenRequest {
            device: dev.to_string(),
            reason: "ser2net served a device-open failure instead of console data".into(),
        }
    }

    #[test]
    fn a_reopen_request_for_an_absent_device_does_not_restart_ser2net() {
        // Measured on bravo: the Bughopper's console detaches for every press,
        // minerd files a reopen, and the supervisor restarted ser2net -- and
        // every other console's session -- for a tty that was simply not there.
        let kept = reopen_requests_worth_a_restart(
            vec![req(
                "/dev/serial/by-id/usb-Arduino_Bughopper_DK0HEVIC-if00-port0",
            )],
            |_| false,
        );
        assert!(
            kept.is_empty(),
            "an absent device has nothing to reopen: {kept:?}"
        );
    }

    #[test]
    fn a_reopen_request_for_a_present_device_still_restarts_ser2net() {
        // The wedge this mechanism was built for: the tty is there, open fails
        // anyway (held by something), and only a restart clears it.
        let kept = reopen_requests_worth_a_restart(
            vec![req("/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0-if00-port0")],
            |_| true,
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_console_that_vanishes_keeps_its_accepter_for_the_grace() {
        use std::time::{Duration, Instant};
        let debounce = Duration::from_millis(500);
        let grace = Duration::from_secs(45);
        let t0 = Instant::now();
        let mut p = RewritePending::default();
        p.note_change(true, t0);
        assert!(
            !p.due(t0 + Duration::from_secs(5), debounce, grace),
            "a removal alone must wait out the grace, not the hotplug debounce"
        );
        assert!(
            p.due(t0 + grace, debounce, grace),
            "and after the grace the removal lands"
        );
    }

    #[test]
    fn a_console_that_returns_within_the_grace_lands_as_a_no_op_after_the_debounce() {
        use std::time::{Duration, Instant};
        let debounce = Duration::from_millis(500);
        let grace = Duration::from_secs(45);
        let t0 = Instant::now();
        let mut p = RewritePending::default();
        p.note_change(true, t0);
        // 6 s later the FTDI is back (a return, not a removal).
        let t1 = t0 + Duration::from_secs(6);
        p.note_change(false, t1);
        assert!(!p.due(t1, debounce, grace));
        assert!(
            p.due(t1 + debounce, debounce, grace),
            "a return is an addition: short debounce, byte-identical rewrite, no SIGHUP"
        );
    }

    #[test]
    fn an_addition_is_never_delayed_by_a_pending_removal() {
        use std::time::{Duration, Instant};
        let debounce = Duration::from_millis(500);
        let grace = Duration::from_secs(45);
        let t0 = Instant::now();
        let mut p = RewritePending::default();
        p.note_change(true, t0);
        let t1 = t0 + Duration::from_secs(1);
        p.note_change(false, t1);
        assert!(p.due(t1 + debounce, debounce, grace));
    }
}

#[cfg(test)]
mod capture_supervisor_tests {
    use super::should_stop_capture;
    use std::time::Duration;

    const GRACE: Duration = Duration::from_millis(45_000);

    /// The regression that cost an operator the firmware boot: a power cycle
    /// marks the console `gone` for a few seconds, and tearing capture down
    /// there put discoveryd's re-detection on the critical path. Measured at
    /// 9.4s of no capture at all, which is most of a boot.
    #[test]
    fn a_console_absent_for_a_power_cycle_keeps_its_capture() {
        assert!(
            !should_stop_capture(false, false, true, "gone", Duration::from_secs(9), GRACE),
            "a console absent for 9s is power-cycling, not removed"
        );
    }

    /// ...but an absence that outlasts the grace discoveryd gives the accepter
    /// is a real removal, and holding a capture on it forever would be a leak.
    #[test]
    fn a_console_absent_past_the_grace_really_is_gone() {
        assert!(should_stop_capture(
            false,
            false,
            true,
            "gone",
            Duration::from_secs(60),
            GRACE
        ));
    }

    /// Everything that is not a transient absence still stops immediately:
    /// there is nothing to wait for.
    #[test]
    fn a_device_that_is_not_merely_absent_stops_at_once() {
        let now = Duration::from_millis(0);
        assert!(
            should_stop_capture(true, false, true, "discovered", now, GRACE),
            "remote consoles are relayed, never captured here"
        );
        assert!(
            should_stop_capture(false, true, true, "gone", now, GRACE),
            "an ignored device is not ours to hold"
        );
        assert!(
            should_stop_capture(false, false, false, "gone", now, GRACE),
            "no ser2net port means nothing to re-dial"
        );
    }
}

#[cfg(test)]
mod ser2net_restart_tests {
    use super::{managed_consoles, open_failure_worth_a_restart};

    const CFG: &str = "connection: &con_dev_serial_by_id_usb_Board_A_if00_port0\n  \
                       accepter: telnet(rfc2217=false),tcp,0.0.0.0,5001\n  \
                       connector: serialdev,/dev/serial/by-id/usb-Board_A-if00-port0,115200n81\n\
                       connection: &con_dev_serial_by_id_usb_Board_B_if00_port0\n  \
                       connector: serialdev,/dev/serial/by-id/usb-Board_B-if00-port0,115200n81\n";

    fn managed(present_a: bool, present_b: bool) -> Vec<(String, bool)> {
        managed_consoles(CFG, |dev| {
            if dev.contains("Board_A") {
                present_a
            } else {
                present_b
            }
        })
    }

    fn failure_for(board: &str) -> Vec<String> {
        vec![format!(
            "ser2net[2261]: dev read error for device on port \
             con_dev_serial_by_id_usb_{board}_if00_port0: Remote end closed connection"
        )]
    }

    #[test]
    fn the_config_names_every_console_and_whether_it_is_on_the_bus() {
        let m = managed(true, false);
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m.iter().any(|(k, p)| k.contains("Board_A") && *p));
        assert!(m.iter().any(|(k, p)| k.contains("Board_B") && !*p));
    }

    /// A board mid-reset is not a wedged ser2net. Restarting drops every OTHER
    /// console on the host, which an operator sees as the console going laggy
    /// and arriving in chunks.
    #[test]
    fn a_failure_naming_an_absent_board_is_not_worth_a_restart() {
        assert!(!open_failure_worth_a_restart(
            &failure_for("Board_B"),
            &managed(true, false)
        ));
    }

    /// ...but a board that IS on the bus and still cannot be opened is exactly
    /// the wedge this recovery exists for.
    #[test]
    fn a_failure_naming_a_present_board_still_restarts() {
        assert!(open_failure_worth_a_restart(
            &failure_for("Board_A"),
            &managed(true, false)
        ));
    }

    /// An unattributable failure keeps the old behaviour: an unexplained one is
    /// the case least safe to ignore.
    #[test]
    fn an_unattributable_failure_still_restarts() {
        assert!(open_failure_worth_a_restart(
            &["ser2net: Unable to open something inscrutable".to_string()],
            &managed(true, true)
        ));
        assert!(
            open_failure_worth_a_restart(&[], &managed(true, true)),
            "no captured line at all must behave as it did before"
        );
    }
}
