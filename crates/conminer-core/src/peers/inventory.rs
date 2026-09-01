//! Turning a peer's device list into rows in this node's registry.
//!
//! SERVER SIDE, ONCE PER INTERVAL. The obvious alternative -- let each browser
//! poll every node for every device -- is what the design this ports from
//! actually did, and it multiplies: N dashboards times M nodes times a refresh
//! tick. Syncing here means one request per peer per interval no matter how many
//! humans and agents are watching, and it means mcpd can resolve a remote
//! selector without any network call at all.
//!
//! A remote row is a POINTER, never a copy of anything mined: no store, no
//! capture, no templates. Everything an agent asks about it is proxied to the
//! owner at the moment of asking, so there is nothing here to go stale except
//! the list of what exists.

use super::client::PeerClient;
use super::registry::{self as peer_registry, PeerRow};
use super::{remote_canonical, remote_target, split_remote, valid_node_name};
use crate::error::Result;
use crate::store::registry::DeviceKind;
use crate::store::{IdentityKind, Registry};
use serde_json::{json, Value};
use std::time::Duration;

/// How long a peer's inventory call may take before it is treated as down.
const SYNC_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a remote row survives after its owner stops listing it.
///
/// Not zero, deliberately. Deleting on the first miss releases the row's
/// ser2net port, and the port is what a human has in a terminal tab and what a
/// script has in a connection string. A board that reboots its owner should not
/// renumber every console on the bench.
pub const GRACE_MS: i64 = 10 * 60 * 1000;

/// How far a board may be and still be re-exported here.
///
/// §P2. A bound, not a ban. The first version refused relaying outright because
/// a cycle between three nodes would materialise devices for ever; owner
/// attribution stops the duplication and this stops the walk. Three is enough
/// for a bench whose far side is reachable only through a middle node, and small
/// enough that a mistake shows up as a missing device rather than a fleet that
/// grows without limit.
pub const MAX_HOPS: u8 = 3;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncReport {
    pub peers_ok: usize,
    pub peers_failed: usize,
    pub rows_added: usize,
    pub rows_updated: usize,
    pub rows_gone: usize,
    pub errors: Vec<String>,
}

/// Pull every live peer's device list and reconcile our remote rows.
pub fn sync_all(
    reg: &mut Registry,
    client: &PeerClient,
    peers: &[PeerRow],
    base_port: u16,
    now: i64,
) -> Result<SyncReport> {
    sync_all_as(reg, client, peers, base_port, now, "")
}

/// As [`sync_all`], but told THIS node's name.
///
/// Needed the moment relaying exists: without it a node cannot recognise its own
/// boards coming back through a peer, and the shortest cycle -- two nodes
/// describing each other -- gives each of them a proxied copy of hardware it is
/// already holding the tty for.
pub fn sync_all_as(
    reg: &mut Registry,
    client: &PeerClient,
    peers: &[PeerRow],
    base_port: u16,
    now: i64,
    this_node: &str,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    for peer in peers {
        // A PEER WE CANNOT NAME CANNOT OWN ANYTHING. Until the handshake lands,
        // a static row's `name` is still the URL it was configured with, and
        // importing devices then attributes them to a "node" called
        // `http://host:8090/mcp` -- rows that no later adoption ever reconciles,
        // because the real node arrives under its real name. Measured on bravo:
        // two of its own boards, owned by a URL.
        if peer.instance_id.starts_with("static:") || peer.instance_id.starts_with("static-id:") {
            continue;
        }
        match sync_one(reg, client, peer, base_port, now, this_node) {
            Ok((added, updated)) => {
                report.peers_ok += 1;
                report.rows_added += added;
                report.rows_updated += updated;
                peer_registry::mark_ok(reg, &peer.instance_id, now)?;
            }
            Err(e) => {
                report.peers_failed += 1;
                report.errors.push(format!("{}: {}", peer.name, e.message));
                peer_registry::mark_failed(reg, &peer.instance_id, &e.message, now)?;
                // Rows stay, marked gone by the sweep below. A peer that is
                // briefly unreachable has not stopped owning its boards.
            }
        }
    }
    report.rows_gone = sweep_stale(reg, now)?;
    Ok(report)
}

fn sync_one(
    reg: &mut Registry,
    client: &PeerClient,
    peer: &PeerRow,
    base_port: u16,
    now: i64,
    this_node: &str,
) -> Result<(usize, usize)> {
    let (reply, _rtt) = client.call_tool(
        &peer.mcp_url,
        "list_devices",
        &json!({"detail": true, "freshness": false}),
        SYNC_TIMEOUT,
    )?;
    let devices = extract_devices(&reply);
    import_devices(reg, peer, &devices, base_port, now, this_node)
}

/// Apply a peer's device list, however it arrived.
///
/// §P3. Split out of the pull because there are now two ways a node learns what
/// a peer owns -- it fetches, or the peer announces -- and every rule that
/// matters lives here: owner attribution, the hop bound, the self-owner guard,
/// serve-only, shortest-path-wins. Two copies of that would drift, and the
/// drift would show up as a board that exists on one node and not another.
pub fn import_devices(
    reg: &mut Registry,
    peer: &PeerRow,
    devices: &[Value],
    base_port: u16,
    now: i64,
    this_node: &str,
) -> Result<(usize, usize)> {
    let devices = devices.to_vec();
    let mut added = 0;
    let mut updated = 0;
    let mut seen: Vec<String> = Vec::new();

    for d in devices {
        let Some(remote_id) = d.get("device").and_then(Value::as_str) else {
            continue;
        };
        // §P2. A PEER'S OWN REMOTE ROWS ARE RELAYED, not refused.
        //
        // This used to `continue` here, because a cycle between three nodes
        // would materialise devices for ever. That hazard is real; refusing the
        // topology is not the only answer to it. What a relayed row needs is to
        // keep naming its true OWNER -- so the same board is one row however
        // many nodes it is heard through -- plus a bound on distance and a
        // refusal to import anything we own ourselves. All three are below.
        let (owner, owner_id, relayed) = match split_remote(remote_id) {
            Some((owner, inner)) => (owner, inner, true),
            None => (peer.name.to_string(), remote_id.to_string(), false),
        };
        // NEVER IMPORT OUR OWN BOARDS BACK. The shortest possible cycle is two
        // nodes describing each other, and it ends here: a row we own is not a
        // remote row, and re-importing it would give this node a second,
        // proxied copy of a board it is holding the tty for.
        if owner == this_node {
            continue;
        }
        // AN OWNER THAT IS NOT A NODE NAME IS NOT AN OWNER. The peer we are
        // talking to may be perfectly well-named and still relay a row whose
        // owner is a placeholder URL, because it has not adopted that node's
        // real identity yet. Storing it makes a row nothing can ever route to or
        // collect -- alpha carried two for months. Refuse it here, where the
        // name crosses the boundary, rather than discovering it later as a
        // device belonging to a node called "http:".
        if !valid_node_name(&owner) {
            tracing::debug!(
                device = %remote_id,
                owner = %owner,
                peer = %peer.name,
                "refusing a device whose owner is not a node name"
            );
            continue;
        }
        // Distance, and its limit. `hops` on the peer's own row is how far the
        // owner is from THEM; one more hop reaches us.
        // Distance to the OWNER, one hop further than the peer told us. A peer
        // that says nothing is assumed to own what it lists, which is what the
        // canonical prefix says anyway; the old default of 1 silently added a
        // hop to every row on the bench and flattened the comparison below.
        let hops = d
            .get("hops")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1)
            .min(u8::MAX as u64) as u8;
        if relayed && hops > MAX_HOPS {
            tracing::debug!(
                device = %remote_id,
                owner = %owner,
                hops,
                "beyond the hop limit; not relaying further"
            );
            continue;
        }
        // Derived stores and ingested files belong to their owner's disk, not to
        // the fleet's hardware pool.
        if DeviceKind::of(&owner_id) != DeviceKind::Local {
            continue;
        }
        // ONLY WHAT THE OWNER ACTUALLY SERVES. A device with no endpoint is one
        // its owner deliberately does not open -- an excluded controller, or a
        // TAC's bit-bang GPIO channels, which enumerate as ttys and are kept out
        // of discovery precisely because opening one writes to the board's power
        // lines. Re-exporting it here claims to serve something nobody serves:
        // measured on the bravo node, two ignored GPIO channels arrived as remote
        // consoles and were handed local ports 5003 and 5004, pointing at
        // nothing.
        let endpoint = d.get("endpoint").and_then(Value::as_str).unwrap_or("");
        if endpoint.is_empty() || d.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // Keyed on the OWNER, never on the node we happened to hear it from.
        // Two peers that both relay the same board must produce ONE row, or the
        // fleet grows a copy per path.
        let canonical = remote_canonical(&owner, &owner_id);
        seen.push(canonical.clone());

        // A shorter path wins. Learning a board directly from its owner must not
        // be overwritten by hearing about it second-hand a moment later.
        if let Some(existing) = reg.device_by_canonical(&canonical)? {
            if existing.hops < hops && existing.state != "gone" {
                continue;
            }
        }

        let existed = reg.device_by_canonical(&canonical)?.is_some();
        let row = reg.upsert_device(&canonical, None, IdentityKind::ById, None, now)?;
        reg.set_remote_route(
            row.id,
            &owner,
            peer.host.as_deref().or(peer.ser2net_host.as_deref()),
            &owner_id,
            // The owner's own console port, parsed out of the endpoint it
            // advertises: `tcp://0.0.0.0:5001` -> 5001.
            endpoint
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok()),
            // Who to forward to. None when the owner is the peer itself, which
            // keeps the one-hop case exactly as it was.
            relayed.then_some(peer.name.as_str()),
            hops,
        )?;
        // The remote console re-exports on a local port, so every existing
        // consumer -- dashboard terminal, `endpoint_for`, a human with telnet --
        // dials it exactly like a local one.
        reg.assign_port(row.id, base_port)?;
        if let Some(nick) = d.get("label").and_then(Value::as_str) {
            // Nicknames are the peer's, prefixed so two nodes may both have an
            // "adp" without one shadowing the other.
            let _ = reg.set_nickname(row.id, &format!("{}/{}", peer.name, nick));
        }
        if let Some(t) = d.get("target").and_then(Value::as_str) {
            let _ = reg.set_target(row.id, Some(&remote_target(&peer.name, t)));
        }
        // WHAT THE OWNER SAYS ABOUT DRIVING IT, carried verbatim. Recomputing it
        // here is impossible: the controller profiles match a by-id name against
        // hardware plugged into the OWNER's host, so every peer's board showed
        // no controller and no power buttons.
        // ONLY WHEN THE SENDER ACTUALLY SAID SOMETHING. An absent key is "I did
        // not tell you", not "there are no controls", and writing NULL for it
        // erases the owner's own answer. With three nodes relaying each other
        // that erasure propagates: one node's blank overwrites another's good
        // value, which the third then relays back as blank.
        if let Some(controls) = d.get("controls") {
            reg.set_remote_controls(row.id, Some(controls))?;
        }
        let state = d
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("listening");
        reg.set_state(row.id, state)?;
        if existed {
            updated += 1;
        } else {
            added += 1;
        }
    }

    // Devices this peer no longer lists go to `gone` (not deleted): the grace
    // sweep decides when they really left.
    for row in reg.devices_learned_via(&peer.name)? {
        if !seen.contains(&row.canonical) && row.state != "gone" {
            reg.set_state(row.id, "gone")?;
        }
    }
    Ok((added, updated))
}

/// `list_devices` answers either shape depending on `detail`; take both.
fn extract_devices(reply: &Value) -> Vec<Value> {
    let content = reply
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .or_else(|| reply.get("structuredContent"))
        .cloned()
        .unwrap_or(Value::Null);
    content
        .get("devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Delete remote rows that have been `gone` longer than the grace window.
fn sweep_stale(reg: &mut Registry, now: i64) -> Result<usize> {
    // WHO IS STILL IN THE FLEET. A remote row exists because some peer told us
    // about it; when that peer is gone from the table entirely, nothing will
    // ever mark the row `gone`, because the sweep that does so runs per peer.
    // Those rows then sit on the dashboard for ever, describing boards on a node
    // nobody is talking to. Measured after a bad config made two nodes peer with
    // themselves: four rows for boards the host was already holding the tty for,
    // left behind when the bogus peers were dropped.
    let known: std::collections::BTreeSet<String> = super::registry::all(reg)?
        .into_iter()
        .map(|p| p.name)
        .collect();
    let mut gone = 0;
    for row in reg.remote_devices()? {
        // A ROW WHOSE OWNER IS NOT A NODE NAME CAN NEVER BE ROUTED TO.
        //
        // These arrive over a relay from a peer that has not yet adopted a third
        // node's real identity, and they persist: the sweep below asks whether
        // the RELAY is still in the fleet, and the relay is perfectly healthy.
        // Import now refuses them, and this collects the ones already stored --
        // two of them on alpha, describing bravo's boards under the owner
        // "http:", one hop past the node that knew better.
        if row.node.as_deref().is_some_and(|n| !valid_node_name(n)) {
            tracing::info!(
                device = %row.canonical,
                "this row's owner is not a node name; forgetting it"
            );
            reg.forget_device(row.id)?;
            gone += 1;
            continue;
        }
        // The node we would have to ask about this row: the relay if there is
        // one, else the owner.
        let source = row.via.clone().or_else(|| row.node.clone());
        let orphaned = source.is_some_and(|n| !known.contains(&n));
        if orphaned {
            tracing::info!(
                device = %row.canonical,
                "no peer left to vouch for this row; forgetting it"
            );
            reg.forget_device(row.id)?;
            gone += 1;
            continue;
        }
        if row.state == "gone" && now - row.last_seen > GRACE_MS {
            reg.forget_device(row.id)?;
            gone += 1;
        }
    }
    Ok(gone)
}
