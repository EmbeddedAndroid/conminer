//! §P3. Actuation across a link that only opens one way.
//!
//! Announcing (see `inventory::import_devices` and the `peer_announce` tool)
//! gives a node behind one-way connectivity a complete VIEW of the fleet. It
//! does not let it touch anything: seeing alpha's boards and being able to
//! power one are different problems, and the second one needs a connection in
//! the direction that does not open.
//!
//! So it reuses the direction that does. The reachable side already dials in
//! every few seconds; here it also asks "is there anything for me to run?" and
//! parks on that question. When an agent on the unreachable side calls a tool
//! for a board it does not own, the call is queued here instead of dialled, the
//! parked poller wakes with it, runs it against its OWN mcpd, and posts the
//! answer back over a second connection it opens. The caller's tool call blocks
//! across the whole round trip and returns the owner's answer verbatim -- the
//! same contract as a directly forwarded call, because it IS the same answer.
//!
//! WHY A QUEUE AND NOT A TUNNEL. A general reverse tunnel (SSH -R, a websocket
//! carrying arbitrary TCP) would also work and would be less code, but it moves
//! the trust boundary: whoever holds the tunnel can reach everything the far
//! side can reach. This carries exactly one thing -- a tool name and its
//! arguments, which the far side was already willing to serve to any LAN caller
//! -- so the reverse path grants nothing the forward path would not have.
//!
//! WHAT IS DELIBERATELY NOT HERE. No persistence: a queued call belongs to a
//! caller who is blocked on it right now, and a call that outlives the process
//! that made it has nobody to answer to. No retry: actuating a board is not
//! idempotent, and quietly powering something twice is worse than one honest
//! failure. No fairness beyond FIFO, because a bench is not a datacentre.
//!
//! Trust model matches the rest of peering: LAN-trust. Work is addressed by node
//! NAME, so a node that polls under somebody else's name receives their calls;
//! that is the same exposure as the unauthenticated tool surface it would be
//! calling anyway, and it moves when peering gets signatures (`client`'s origin
//! header is the slot).

use crate::error::{ErrorCode, Result, ToolError};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How many calls may be in flight for the whole fleet at once.
///
/// Each one has a caller blocked on it, so this is really a bound on how many
/// agents can be mid-actuation. Far above any real bench; here so a runaway
/// caller cannot grow the queue without limit.
const MAX_IN_FLIGHT: usize = 64;

/// A node counts as listening for this long after its last poll.
///
/// Comfortably longer than a poll's own wait, so a worker that is mid-round-trip
/// -- parked, or busy running the last call -- is not briefly declared absent
/// and does not have a caller told "nobody is listening" while it works.
pub const POLLER_TTL_MS: i64 = 90_000;

/// The longest a poller may park. Bounded so a proxy or a NAT with an idle
/// timeout sees traffic, and so a worker notices a config change eventually.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(25);

/// One call waiting to be run on the node that owns the hardware.
#[derive(Debug, Clone)]
pub struct RelayCall {
    pub id: u64,
    /// The node expected to run it.
    pub node: String,
    pub tool: String,
    pub args: Value,
    /// `<node>/<agent>`, so the owner leases under the real caller's identity.
    pub origin: String,
    /// The nodes this call has already traversed (§P2), for loop refusal.
    pub path: Vec<String>,
}

impl RelayCall {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "tool": self.tool,
            "args": self.args,
            "origin": self.origin,
            "path": self.path,
        })
    }
}

/// A call on its way out: everything but the id, which the queue assigns.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub node: String,
    pub tool: String,
    pub args: Value,
    pub origin: String,
    pub path: Vec<String>,
}

/// Why a relayed call did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// Nothing from that node has polled recently, so queueing would only stall.
    NoPoller,
    /// Too many calls already waiting.
    Busy,
    /// Queued, and no answer inside the caller's budget. `taken` says whether a
    /// worker ever picked it up, which is the difference between "the far node
    /// is not really there" and "the far node is stuck on it".
    Timeout { taken: bool },
}

impl RelayError {
    pub fn into_tool_error(self, node: &str) -> ToolError {
        match self {
            RelayError::NoPoller => ToolError::new(
                ErrorCode::UnknownPeer,
                format!(
                    "node {node:?} cannot be reached from here, and it is not listening for work"
                ),
            )
            .with_hint(
                "its boards are visible because it announced them; driving them needs it to be \
                 running peerd with peers.announce = true, or a network route back to it",
            ),
            RelayError::Busy => ToolError::new(
                ErrorCode::Internal,
                format!("too many calls already relaying; {node:?} is not keeping up"),
            )
            .with_hint("retry in a moment"),
            RelayError::Timeout { taken } => ToolError::new(
                ErrorCode::HookTimeout,
                if taken {
                    format!("node {node:?} took this call and did not answer in time")
                } else {
                    format!("node {node:?} never collected this call")
                },
            )
            .with_hint(if taken {
                "the owner is running it; raise timeout_s if the operation is genuinely long"
            } else {
                "its poller stopped between the check and the queue; retry"
            })
            .with_detail(json!({"relay": true, "collected": taken})),
        }
    }
}

/// One in-flight call: who it is for, whether anyone took it, and its answer.
struct Slot {
    node: String,
    taken: bool,
    result: Option<Value>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Not yet collected, oldest first.
    queue: VecDeque<RelayCall>,
    /// Every call a caller is still blocked on, collected or not.
    slots: HashMap<u64, Slot>,
    /// node -> wall-clock ms of its last poll.
    pollers: HashMap<String, i64>,
}

/// The reverse-call rendezvous for one node.
///
/// Deliberately per-`Context` rather than a process-wide static: two nodes share
/// a process in the fleet tests, and a shared static would have each one
/// answering the other's work by accident -- a passing test proving nothing.
#[derive(Default)]
pub struct RelayQueue {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl RelayQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is `node` currently asking us for work?
    ///
    /// The one signal that says a reverse call will actually go somewhere. It is
    /// evidence of connectivity in the direction that works, which is exactly
    /// what the router cannot learn from its own failed probes.
    pub fn has_poller(&self, node: &str, now: i64) -> bool {
        let inner = self.inner.lock().expect("relay queue");
        inner
            .pollers
            .get(node)
            .is_some_and(|last| now.saturating_sub(*last) < POLLER_TTL_MS)
    }

    /// Queue a call for `node` and block until it answers or the budget runs out.
    ///
    /// Returns the owner's raw JSON-RPC `result` object, so the caller unwraps it
    /// with the same code that unwraps a directly forwarded reply -- a relayed
    /// error must arrive as that tool's error, not as a transport failure.
    pub fn submit(
        &self,
        out: Outgoing,
        budget: Duration,
        now: i64,
    ) -> std::result::Result<Value, RelayError> {
        let deadline = Instant::now() + budget;
        let id = {
            let mut inner = self.inner.lock().expect("relay queue");
            if !inner
                .pollers
                .get(&out.node)
                .is_some_and(|last| now.saturating_sub(*last) < POLLER_TTL_MS)
            {
                return Err(RelayError::NoPoller);
            }
            if inner.slots.len() >= MAX_IN_FLIGHT {
                return Err(RelayError::Busy);
            }
            inner.next_id += 1;
            let id = inner.next_id;
            inner.slots.insert(
                id,
                Slot {
                    node: out.node.clone(),
                    taken: false,
                    result: None,
                },
            );
            inner.queue.push_back(RelayCall {
                id,
                node: out.node,
                tool: out.tool,
                args: out.args,
                origin: out.origin,
                path: out.path,
            });
            id
        };
        self.cv.notify_all();

        let mut inner = self.inner.lock().expect("relay queue");
        loop {
            if let Some(slot) = inner.slots.get(&id) {
                if let Some(result) = slot.result.clone() {
                    inner.slots.remove(&id);
                    return Ok(result);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Give up the slot AND the queue entry: nobody is waiting for
                // this answer any more, and a call collected after its caller
                // left would actuate a board on behalf of no one.
                let taken = inner.slots.remove(&id).is_some_and(|s| s.taken);
                inner.queue.retain(|c| c.id != id);
                return Err(RelayError::Timeout { taken });
            }
            let (guard, _) = self
                .cv
                .wait_timeout(inner, left.min(Duration::from_millis(500)))
                .expect("relay queue");
            inner = guard;
        }
    }

    /// A worker asking for work on behalf of `node`, parking up to `wait`.
    ///
    /// Records the poll whether or not there is anything to hand back: an empty
    /// poll is still proof that the reverse direction is open, and that proof is
    /// what lets the router choose this path instead of failing.
    pub fn take(&self, node: &str, wait: Duration, now: i64) -> Option<RelayCall> {
        let deadline = Instant::now() + wait.min(MAX_POLL_WAIT);
        let mut inner = self.inner.lock().expect("relay queue");
        inner.pollers.insert(node.to_string(), now);
        loop {
            if let Some(pos) = inner.queue.iter().position(|c| c.node == node) {
                let call = inner.queue.remove(pos).expect("position just found");
                if let Some(slot) = inner.slots.get_mut(&call.id) {
                    slot.taken = true;
                }
                return Some(call);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(inner, left.min(Duration::from_millis(250)))
                .expect("relay queue");
            inner = guard;
        }
    }

    /// A worker posting the answer. False if nobody is waiting for it any more.
    pub fn complete(&self, id: u64, node: &str, result: Value) -> bool {
        let mut inner = self.inner.lock().expect("relay queue");
        // The node check is not ceremony: without it a node could answer another
        // node's call, and the caller would get a reply from hardware it never
        // addressed.
        let ok = match inner.slots.get_mut(&id) {
            Some(slot) if slot.node == node => {
                slot.result = Some(result);
                true
            }
            _ => false,
        };
        drop(inner);
        if ok {
            self.cv.notify_all();
        }
        ok
    }

    /// What is outstanding, for the `peers` tool and the dashboard.
    pub fn stats(&self, now: i64) -> Value {
        let inner = self.inner.lock().expect("relay queue");
        let listening: Vec<&str> = inner
            .pollers
            .iter()
            .filter(|(_, last)| now.saturating_sub(**last) < POLLER_TTL_MS)
            .map(|(n, _)| n.as_str())
            .collect();
        json!({
            "queued": inner.queue.len(),
            "in_flight": inner.slots.len(),
            "listening": listening,
        })
    }
}

/// How long to wait on a relayed call before calling it a transport failure.
///
/// Deliberately the same shape as the router's own `call_budget`: an actuation
/// on the owner can legitimately run for a minute or more (hook, settle, verify,
/// escalate, verify again), and a ceiling under that turns a board being
/// power-cycled into "the node did not answer".
fn hop_budget(tool: &str, args: &Value) -> Duration {
    let floor = match tool {
        "power" | "boot_mode" => 300,
        "flash" | "selftest" | "transfer_file" | "push_file" | "pull_file" => 900,
        _ => 45,
    };
    let asked = args
        .get("timeout_s")
        .and_then(Value::as_u64)
        .map(|s| s + 20)
        .unwrap_or(0);
    Duration::from_secs(asked.max(floor))
}

/// One poll-run-answer round against a peer, exactly as peerd runs it.
///
/// Lives here rather than in the daemon so the fleet tests drive the SAME code
/// the deployment does; a hand-rolled test double for this loop would be the one
/// piece of the reverse path never actually exercised.
///
/// Returns whether a call was handled. `own_mcp_url` is this node's OWN mcpd:
/// the relayed call is dispatched through the front door, so it takes the same
/// routing, lease and mutation path as any other caller's, rather than a private
/// side entrance that could drift from it.
pub fn serve_once(
    client: &crate::peers::PeerClient,
    peer_mcp_url: &str,
    own_mcp_url: &str,
    my_node: &str,
    wait: Duration,
) -> Result<bool> {
    let poll_budget = wait.min(MAX_POLL_WAIT) + Duration::from_secs(10);
    let (reply, _) = client.call_tool(
        peer_mcp_url,
        "peer_poll",
        &json!({"node": my_node, "wait_ms": wait.min(MAX_POLL_WAIT).as_millis() as u64}),
        poll_budget,
    )?;
    let call = reply
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .and_then(|c| c.get("call"))
        .cloned()
        .unwrap_or(Value::Null);
    let Some(call) = call.as_object() else {
        return Ok(false);
    };
    let id = call.get("id").and_then(Value::as_u64).unwrap_or_default();
    let tool = call
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = call.get("args").cloned().unwrap_or(json!({}));
    let origin = call
        .get("origin")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let path: Vec<String> = call
        .get("path")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if tool.is_empty() {
        return Ok(false);
    }

    // Budget: the same rule the router uses for a forward hop, because this IS
    // the hop -- just in the other direction. A shorter one here would kill a
    // working actuation at the last leg and report it as the owner failing to
    // answer, which is the exact confusion the tool-shaped floor exists to stop.
    let budget = hop_budget(&tool, &args);
    let result =
        match client.call_tool_via(own_mcp_url, &tool, &args, budget, &origin, &path.join(",")) {
            Ok((reply, _)) => reply.get("result").cloned().unwrap_or_else(|| {
                json!({"isError": true, "structuredContent": {"error": {
                "code": "INTERNAL", "message": "the owning node returned no result"}}})
            }),
            // A LOCAL FAILURE MUST STILL BE ANSWERED. Dropping it here would leave
            // the caller parked for its whole budget and then reported as "took the
            // call and never answered", which points at the wrong node.
            Err(e) => json!({"isError": true, "structuredContent": {"error": {
                "code": "INTERNAL",
                "message": format!("{my_node} could not run the relayed call: {e}"),
            }}}),
        };
    client.call_tool(
        peer_mcp_url,
        "peer_result",
        &json!({"id": id, "node": my_node, "result": result}),
        Duration::from_secs(20),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn call(node: &str, tool: &str) -> Outgoing {
        Outgoing {
            node: node.into(),
            tool: tool.into(),
            args: json!({}),
            origin: "a/x".into(),
            path: vec![],
        }
    }

    #[test]
    fn a_call_is_refused_immediately_when_nobody_is_listening() {
        let q = RelayQueue::new();
        let err = q
            .submit(call("nodeb", "power"), Duration::from_secs(5), 1_000)
            .expect_err("no poller");
        assert_eq!(err, RelayError::NoPoller);
    }

    #[test]
    fn a_poll_registers_the_listener_even_with_no_work() {
        let q = RelayQueue::new();
        assert!(!q.has_poller("nodeb", 1_000));
        assert!(q.take("nodeb", Duration::from_millis(1), 1_000).is_none());
        assert!(q.has_poller("nodeb", 1_000));
        // …and it lapses.
        assert!(!q.has_poller("nodeb", 1_000 + POLLER_TTL_MS));
    }

    #[test]
    fn a_call_travels_out_and_its_answer_comes_back() {
        let q = Arc::new(RelayQueue::new());
        q.take("nodeb", Duration::from_millis(1), 1_000);
        let worker = {
            let q = q.clone();
            std::thread::spawn(move || {
                let call = q
                    .take("nodeb", Duration::from_secs(5), 1_000)
                    .expect("work arrives");
                assert_eq!(call.tool, "power");
                assert_eq!(call.origin, "nodea/agent");
                assert!(q.complete(call.id, "nodeb", json!({"structuredContent": {"ok": true}})));
            })
        };
        let out = q
            .submit(
                Outgoing {
                    node: "nodeb".into(),
                    tool: "power".into(),
                    args: json!({"device": "x"}),
                    origin: "nodea/agent".into(),
                    path: vec!["nodea".into()],
                },
                Duration::from_secs(10),
                1_000,
            )
            .expect("answered");
        assert_eq!(out["structuredContent"]["ok"], true);
        worker.join().unwrap();
    }

    #[test]
    fn one_node_cannot_answer_another_nodes_call() {
        let q = Arc::new(RelayQueue::new());
        q.take("nodeb", Duration::from_millis(1), 1_000);
        let q2 = q.clone();
        let worker = std::thread::spawn(move || {
            let call = q2
                .take("nodeb", Duration::from_secs(5), 1_000)
                .expect("work");
            // A different node tries to answer it, and is refused.
            assert!(!q2.complete(call.id, "nodec", json!({"structuredContent": {"ok": true}})));
            assert!(q2.complete(call.id, "nodeb", json!({"structuredContent": {"ok": true}})));
        });
        assert!(q
            .submit(call("nodeb", "power"), Duration::from_secs(10), 1_000)
            .is_ok());
        worker.join().unwrap();
    }

    #[test]
    fn a_call_nobody_collects_times_out_saying_so() {
        let q = RelayQueue::new();
        q.take("nodeb", Duration::from_millis(1), 1_000);
        let err = q
            .submit(call("nodeb", "power"), Duration::from_millis(150), 1_000)
            .expect_err("times out");
        assert_eq!(err, RelayError::Timeout { taken: false });
        // …and it is not left behind for a later poller to run on nobody's behalf.
        assert!(q.take("nodeb", Duration::from_millis(1), 1_000).is_none());
    }
}
