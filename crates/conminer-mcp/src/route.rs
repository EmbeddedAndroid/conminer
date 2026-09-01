//! §P1. One seam that federates all eighty tools.
//!
//! A tool call names a device. If that device belongs to a peer, the call is not
//! answered here: it is forwarded to the node the hardware is cabled to, and
//! that node's answer is returned VERBATIM. No merging, no local caching, no
//! second opinion -- the owner's store is the only store for its boards, so its
//! reply is the fact.
//!
//! Doing it at the dispatcher rather than inside each tool is the whole point.
//! Eighty tools already resolve a selector and act; making each one peer-aware
//! would be eighty chances to forget, and the ones written next year would start
//! out wrong. Here there is one place to be right, and one place to test.
//!
//! What gets rewritten on the way out is exactly one thing: the selector. The
//! owner knows the board as `/dev/serial/by-id/...`, not as
//! `peer:alpha//dev/serial/by-id/...`, and sending our prefixed id would have
//! it resolve nothing. The reply comes back untouched except for a `via` field
//! saying which node answered and how long it took -- observability that costs
//! one object and turns "why is this slow" into a number.

use crate::state::Context;
use crate::tools::Tool;
use conminer_core::error::{ErrorCode, Result, ToolError};
use conminer_core::peers::{self, client::PeerClient, registry as peer_registry};
use conminer_core::store::registry::DeviceRow;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;
use std::time::Duration;

/// The selector arguments a tool may carry. Both are resolved the same way.
const SELECTORS: [&str; 2] = ["device", "target"];

/// Tools that must never leave this node, whatever selector they are given.
///
/// `ingest_file` reads a path on the LOCAL disk; federating it would read a file
/// on the owner's disk with the caller's filename, which is either nothing or
/// the wrong thing. `peers` is about this node's view of the fleet by
/// definition. The `peer_*` three are node-to-node plumbing: forwarding one
/// would have a node relay the very machinery that decides where to relay.
const NEVER_FEDERATED: [&str; 6] = [
    "ingest_file",
    "peers",
    "list_profiles",
    "peer_announce",
    "peer_poll",
    "peer_result",
];

/// The shared client, so a fleet keeps one pooled connection per peer.
fn client(ctx: &Context) -> &'static PeerClient {
    static CLIENT: OnceLock<PeerClient> = OnceLock::new();
    CLIENT.get_or_init(|| PeerClient::new(ctx.node_name()))
}

/// Run a tool locally, or on the node that owns the device it names.
///
/// Both the JSON-RPC handler and `selftest` call this rather than `(tool.call)`
/// directly: selftest bypassing the handler is precisely how a federated
/// selftest would have silently run against the wrong node.
pub fn route_or_call(ctx: &Context, tool: &Tool, args: &Map<String, Value>) -> Result<Value> {
    match plan(ctx, tool, args)? {
        Route::Local => (tool.call)(ctx, args),
        Route::Remote { node, url, args } => forward(ctx, tool, &node, &url, args),
        Route::Reverse { node, args } => relay(ctx, tool, &node, args),
    }
}

enum Route {
    Local,
    Remote {
        node: String,
        url: String,
        args: Map<String, Value>,
    },
    /// §P3. The next node cannot be dialled from here, but it is asking us for
    /// work; hand the call to it over the connection IT opened.
    Reverse {
        node: String,
        args: Map<String, Value>,
    },
}

/// Decide where a call belongs, and rewrite the selector if it leaves.
fn plan(ctx: &Context, tool: &Tool, args: &Map<String, Value>) -> Result<Route> {
    if NEVER_FEDERATED.contains(&tool.name) {
        return Ok(Route::Local);
    }
    for key in SELECTORS {
        let Some(sel) = args.get(key).and_then(Value::as_str) else {
            continue;
        };
        let Some((node, remote_sel)) = owner_of(ctx, key, sel)? else {
            continue;
        };
        // §P2. WHO OWNS IT versus WHERE TO SEND IT.
        //
        // A directly peered owner is both. A relayed one is not: the call goes
        // to the neighbour that can reach it, carrying a selector that names the
        // owner so the neighbour's own router carries it the rest of the way.
        // Addressing the owner directly here is exactly the bug transitive
        // routing exists to avoid -- a perfectly correct owner name and no route
        // to it.
        let hop = next_hop(ctx, &node, key, sel)?;
        let mut out = args.clone();
        out.insert(key.into(), json!(hop.selector.unwrap_or(remote_sel)));
        return Ok(match hop.url {
            Some(url) => Route::Remote {
                node: hop.node,
                url,
                args: out,
            },
            None => Route::Reverse {
                node: hop.node,
                args: out,
            },
        });
    }
    Ok(Route::Local)
}

/// Where to actually send a call for a device owned by `owner`.
struct Hop {
    /// The node being dialled -- the owner, or the neighbour relaying to it.
    node: String,
    /// Where to dial it. `None` means the forward direction does not open and
    /// the call goes out over the reverse channel instead (§P3).
    url: Option<String>,
    /// The selector to send, when relaying: `<owner>/<id>`, which the next node
    /// resolves with the same rules. `None` means "send the plain remote id",
    /// which is the direct case.
    selector: Option<String>,
}

/// Pick the direction for one hop: dial it, or hand it the call to run.
///
/// §P3. WHICH WAY DOES THE WIRE GO. A peer row records two independent facts:
/// an address, and whether our last probe of that address answered. Neither one
/// alone decides this. On the bravo bench -- one host upstream of a mesh NAT --
/// the address is present and correct and completely undialable, so trusting it
/// buys a full-budget hang and an error naming the wrong cause.
///
/// The signal that a call will actually arrive is that the far node is asking us
/// for work right now. That is proof of connectivity in the direction that
/// works, which is exactly what our own failed probes cannot tell us.
fn dial(ctx: &Context, p: &conminer_core::peers::PeerRow, selector: Option<String>) -> Result<Hop> {
    if !p.ok && ctx.relay().has_poller(&p.name, ctx.now()) {
        return Ok(Hop {
            node: p.name.clone(),
            url: None,
            selector,
        });
    }
    if p.mcp_url.is_empty() {
        return Err(ToolError::new(
            ErrorCode::UnknownPeer,
            format!("node {:?} published no address to call it on", p.name),
        )
        .with_hint("it announced itself here; it can be driven from a node it can reach"));
    }
    // Known unreachable, sourced from ITS side, and not listening: dialling
    // would spend the caller's whole budget to learn what the peer row already
    // says. A peer we chose to configure still gets dialled -- an `ok: false`
    // there is as likely to be one missed probe as a real partition.
    if !p.ok && p.source.is_push() {
        return Err(ToolError::new(
            ErrorCode::UnknownPeer,
            format!(
                "node {:?} reached this one, but cannot be reached back",
                p.name
            ),
        )
        .with_hint(
            "its boards are visible because it announced them; driving them needs peerd running \
             there with peers.announce = true, or a network route back to it",
        )
        .with_detail(json!({"peer": p.name, "url": p.mcp_url, "reachable": false})));
    }
    Ok(Hop {
        node: p.name.clone(),
        url: Some(p.mcp_url.clone()),
        selector,
    })
}

fn next_hop(ctx: &Context, owner: &str, key: &str, selector: &str) -> Result<Hop> {
    let reg = ctx.registry();
    // Directly peered? Then the owner IS the hop, and nothing changes.
    if let Some(p) = peer_registry::by_name(&reg, owner)? {
        return dial(ctx, &p, None);
    }
    // Otherwise the device row records which neighbour we learned it through.
    let rows = match key {
        "target" => conminer_core::target::members(&reg, selector).unwrap_or_default(),
        _ => reg.resolve_all(selector).unwrap_or_default(),
    };
    let via = rows
        .iter()
        .filter(|r| r.node.as_deref() == Some(owner))
        .find_map(|r| r.via.clone());
    if let Some(via) = via {
        if let Some(p) = peer_registry::by_name(&reg, &via)? {
            let inner = rows
                .iter()
                .find(|r| r.node.as_deref() == Some(owner))
                .and_then(|r| r.remote_canonical.clone())
                .unwrap_or_else(|| selector.to_string());
            // Node-scoped, so the relay's own router resolves it onward. The
            // reverse channel applies to a relay hop exactly as it does to a
            // direct one: what matters is whether THIS hop can be dialled.
            return dial(ctx, &p, Some(format!("{owner}/{inner}")));
        }
    }
    Err(ToolError::new(
        ErrorCode::UnknownPeer,
        format!("device {selector:?} belongs to node {owner:?}, which this node cannot reach"),
    )
    .with_hint("no direct peering and no relay knows the way there")
    .with_detail(json!({
        "known": peer_registry::all(&reg)
            .unwrap_or_default()
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
    })))
}

/// Which node owns what this selector names, and what that node calls it.
///
/// Two forms resolve to a peer:
///   * `alpha/<anything>` -- explicit node scoping, which works even when the
///     device is not yet in our inventory (a fresh peer, a race with sync)
///   * any selector that resolves to a remote ROW, including nicknames and
///     substrings, so `power {device: "adp-ventuno"}` federates without the
///     caller having to know where the board lives
fn owner_of(ctx: &Context, key: &str, selector: &str) -> Result<Option<(String, String)>> {
    // Explicit `<node>/<selector>` beats everything, including a local device of
    // the same name: it is the caller saying which node they mean.
    if let Some((node, rest)) = selector.split_once('/') {
        if !node.is_empty()
            && !rest.is_empty()
            && !selector.starts_with('/')
            && peer_registry::by_name(&ctx.registry(), node)?.is_some()
        {
            return Ok(Some((node.to_string(), rest.to_string())));
        }
    }
    // A namespaced target: `alpha:3.2`.
    if key == "target" {
        if let Some((node, target)) = peers::split_remote_target(selector) {
            if peer_registry::by_name(&ctx.registry(), &node)?.is_some() {
                return Ok(Some((node, target)));
            }
        }
    }
    // Otherwise: does this selector resolve to a row we hold on a peer's behalf?
    let rows = match key {
        "target" => conminer_core::target::members(&ctx.registry(), selector).unwrap_or_default(),
        _ => ctx.registry().resolve_all(selector).unwrap_or_default(),
    };
    let remote: Vec<&DeviceRow> = rows.iter().filter(|r| r.kind.is_remote()).collect();
    if remote.is_empty() {
        return Ok(None);
    }
    // A selector that matches BOTH a local and a remote device is ambiguous, and
    // guessing would eventually actuate the wrong board on the wrong host. Say
    // so, and name both, instead.
    if remote.len() < rows.len() {
        return Err(ToolError::new(
            ErrorCode::AmbiguousDevice,
            format!("{selector:?} matches devices on more than one node"),
        )
        .with_hint("scope it with <node>/<selector>, or use the canonical id")
        .with_detail(json!({
            "candidates": rows.iter().map(|r| json!({
                "device": r.canonical,
                "node": r.node.clone().unwrap_or_else(|| "local".into()),
            })).collect::<Vec<_>>(),
        })));
    }
    if remote.len() > 1 {
        // Several remote rows: only fine if they are all on one node AND the
        // tool takes a group (targets do). Otherwise the tool's own resolution
        // will complain with better words than we can.
        let nodes: std::collections::BTreeSet<&str> =
            remote.iter().filter_map(|r| r.node.as_deref()).collect();
        if nodes.len() > 1 {
            return Err(ToolError::new(
                ErrorCode::AmbiguousDevice,
                format!(
                    "{selector:?} matches devices on {} different nodes",
                    nodes.len()
                ),
            )
            .with_hint("scope it with <node>/<selector>"));
        }
    }
    let row = remote[0];
    let node = row.node.clone().unwrap_or_default();
    let remote_sel = match key {
        // A target federates by its own name on the owner, without our prefix.
        "target" => peers::split_remote_target(selector)
            .map(|(_, t)| t)
            .unwrap_or_else(|| selector.to_string()),
        _ => row
            .remote_canonical
            .clone()
            .or_else(|| peers::split_remote(&row.canonical).map(|(_, r)| r))
            .unwrap_or_else(|| row.canonical.clone()),
    };
    Ok(Some((node, remote_sel)))
}

/// Forward the call and return the owner's answer.
fn forward(
    ctx: &Context,
    tool: &Tool,
    node: &str,
    url: &str,
    args: Map<String, Value>,
) -> Result<Value> {
    let budget = call_budget(tool.name, &args);
    // WHO is asking, in the fleet's terms. The owner uses this as the lease
    // holder, so two agents on two nodes are two identities.
    let origin = format!("{}/{}", ctx.node_name(), ctx.holder());

    // §P2. A CALL MUST NOT BE ABLE TO CIRCLE FOREVER.
    //
    // Relaying means a node forwards on behalf of another, and a fleet whose
    // rows disagree even briefly -- mid-deploy, mid-rename -- can hand a call
    // back to a node that already saw it. The path travels with the call: a node
    // that finds itself already on it refuses, and says where the loop was.
    // Bounding hops alone would not do: a two-node ping-pong stays under any
    // hop count while never terminating.
    let path = crate::tools::call_path();
    if path.iter().any(|n| n == node) {
        return Err(loop_refusal(node, &path));
    }
    let mut hops: Vec<String> = path.clone();
    hops.push(ctx.node_name().to_string());
    let (reply, rtt) = client(ctx).call_tool_via(
        url,
        tool.name,
        &Value::Object(args),
        budget,
        &origin,
        &hops.join(","),
    )?;
    let result = reply.get("result").cloned().unwrap_or(reply.clone());
    unwrap_reply(node, result, rtt)
}

/// §P3. Hand the call to the owner over the connection the OWNER opened.
///
/// Everything except the transport is identical to `forward`: same budget, same
/// caller identity, same loop refusal, same unwrapping. That is the point -- the
/// reverse channel is a different wire, not different semantics, and a relayed
/// `power` must be indistinguishable from a forwarded one to the agent that
/// asked for it.
fn relay(ctx: &Context, tool: &Tool, node: &str, args: Map<String, Value>) -> Result<Value> {
    let budget = call_budget(tool.name, &args);
    let origin = format!("{}/{}", ctx.node_name(), ctx.holder());
    let path = crate::tools::call_path();
    if path.iter().any(|n| n == node) {
        return Err(loop_refusal(node, &path));
    }
    let mut hops: Vec<String> = path.clone();
    hops.push(ctx.node_name().to_string());
    let started = std::time::Instant::now();
    let result = ctx
        .relay()
        .submit(
            conminer_core::peers::relay::Outgoing {
                node: node.to_string(),
                tool: tool.name.to_string(),
                args: Value::Object(args),
                origin,
                path: hops,
            },
            budget,
            ctx.now(),
        )
        .map_err(|e| e.into_tool_error(node))?;
    unwrap_reply(node, result, started.elapsed().as_millis() as u64)
}

/// The read timeout for one hop. Has to clear the CALLER's own budget: a
/// `follow` parked for 120 s on the owner must not be cut off at 5 s by the
/// transport underneath it.
///
/// A CEILING SHORTER THAN THE OPERATION IS A LIE ABOUT THE NETWORK. Measured on
/// alpha: `power on` for a board whose console stays silent runs the hook,
/// watches for a boot, escalates to a cycle and watches again -- 74 seconds,
/// working correctly the whole time. Over a flat 30 s hop that came back as
/// PEER_UNREACHABLE, "check the peer is up", pointing an operator at a network
/// that was fine while the board was being power-cycled in front of them.
///
/// So the floor follows the TOOL. These are not estimates of how long the work
/// takes; they are how long we are willing to wait before calling it a transport
/// failure, and they must sit above anything the owner can legitimately spend.
/// An explicit `timeout_s` still wins when it asks for longer.
fn call_budget(tool: &str, args: &Map<String, Value>) -> Duration {
    let floor = match tool {
        // Hook, settle, verify, escalate, verify again -- all on the owner's
        // clock, and every one of them a window in ITS config that we cannot
        // see from here.
        "power" | "boot_mode" => 300,
        // Whole-image operations and the built-in gauntlet: minutes by nature.
        "flash" | "selftest" | "transfer_file" | "push_file" | "pull_file" => 900,
        _ => 30,
    };
    let asked = args
        .get("timeout_s")
        .and_then(Value::as_u64)
        .map(|s| s + 15)
        .unwrap_or(0);
    Duration::from_secs(asked.max(floor))
}

fn loop_refusal(node: &str, path: &[String]) -> ToolError {
    ToolError::new(
        ErrorCode::Internal,
        format!("refusing to forward to {node:?}: it is already on this call's path"),
    )
    .with_hint("a relay loop; the fleet's routes disagree about who reaches whom")
    .with_detail(json!({"path": path, "next": node}))
}

/// Unwrap the JSON-RPC envelope; a tool error from the owner is still that
/// tool's error and must arrive as one, not as a transport failure.
///
/// Shared by both transports on purpose. Two copies would drift, and the drift
/// would show up as the same failure reading differently depending on which
/// direction the wire happened to open.
fn unwrap_reply(node: &str, result: Value, rtt: u64) -> Result<Value> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let detail = result
            .get("structuredContent")
            .and_then(|c| c.get("error"))
            .cloned()
            .unwrap_or(Value::Null);
        let message = detail
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the owning node refused the call")
            .to_string();
        let code = detail
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("INTERNAL")
            .to_string();
        return Err(ToolError::new(parse_code(&code), message)
            .with_hint(format!("this device is owned by node {node:?}"))
            .with_detail(json!({"via": node, "remote_error": detail})));
    }
    let mut content = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(o) = content.as_object_mut() {
        // Which node actually answered, and what it cost. One object, and it
        // turns "why did that take a second" into a number instead of a guess.
        o.insert("via".into(), json!({"node": node, "rtt_ms": rtt}));
    }
    Ok(content)
}

fn parse_code(code: &str) -> ErrorCode {
    match code {
        "UNKNOWN_DEVICE" => ErrorCode::UnknownDevice,
        "AMBIGUOUS_DEVICE" => ErrorCode::AmbiguousDevice,
        "DEVICE_GONE" => ErrorCode::DeviceGone,
        "LEASE_REQUIRED" => ErrorCode::LeaseRequired,
        "LEASE_HELD" => ErrorCode::LeaseHeld,
        "INVALID_ARGUMENT" => ErrorCode::InvalidArgument,
        "HOOK_FAILED" => ErrorCode::HookFailed,
        "HOOK_TIMEOUT" => ErrorCode::HookTimeout,
        "ACTUATION_IN_FLIGHT" => ErrorCode::ActuationInFlight,
        "AWAY_IN_EDL" => ErrorCode::AwayInEdl,
        _ => ErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_codes_that_route_branches_on_round_trip() {
        // parse_code maps a peer node's wire code back to an ErrorCode so a
        // proxied refusal keeps its identity across the fleet. Every code it
        // branches on must round-trip through as_str(); AWAY_IN_EDL (report
        // #23) proxies from the owner node that is doing the flashing, so it
        // must survive the hop like the actuation codes do.
        for c in [
            ErrorCode::UnknownDevice,
            ErrorCode::AmbiguousDevice,
            ErrorCode::DeviceGone,
            ErrorCode::LeaseRequired,
            ErrorCode::LeaseHeld,
            ErrorCode::InvalidArgument,
            ErrorCode::HookFailed,
            ErrorCode::HookTimeout,
            ErrorCode::ActuationInFlight,
            ErrorCode::AwayInEdl,
        ] {
            assert_eq!(
                parse_code(c.as_str()),
                c,
                "{c:?} does not round-trip via parse_code"
            );
        }
    }
}
