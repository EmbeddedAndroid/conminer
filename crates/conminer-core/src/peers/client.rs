//! Talking to another node's mcpd.
//!
//! POOLED ON PURPOSE. The design note this port is based on records a hard
//! lesson from the system it borrows from: an unpooled client capped that fleet
//! at 22 messages a second, because every call paid a fresh TCP handshake. The
//! pool here is deliberately boring -- one keep-alive connection per peer,
//! handed out under a mutex, dropped and reopened on any error -- and there is a
//! regression test that counts connections across many calls.
//!
//! Hand-rolled HTTP for the same reason `push.rs` is: this codebase spends its
//! dependency budget on capture, and what a JSON-RPC POST needs is a request
//! line, four headers and a content-length reader.

use crate::error::{ErrorCode, Result, ToolError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Identifies the node a proxied call came from.
///
/// The slot a signature would go in when this leaves LAN-trust: the owner reads
/// the origin today and would verify `X-Conminer-Signature` over the same bytes
/// tomorrow (`push::signature` already implements the HMAC).
pub const ORIGIN_HEADER: &str = "X-Conminer-Origin";

/// The nodes a call has already passed through, comma separated.
///
/// §P2. Relaying needs this: hops alone cannot tell a long chain from a loop,
/// and a two-node ping-pong stays under any hop limit for ever. A node that
/// finds itself on the path refuses to forward and says so.
pub const PATH_HEADER: &str = "X-Conminer-Path";

/// How long to wait for the far side to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct PeerClient {
    inner: Arc<Mutex<Pool>>,
    /// This node's name, sent as the origin of every proxied call.
    origin: String,
}

#[derive(Default)]
struct Pool {
    /// One live connection per peer base URL.
    conns: HashMap<String, TcpStream>,
    /// Connections opened, ever. The pool regression test reads this.
    opened: HashMap<String, u64>,
}

impl PeerClient {
    pub fn new(origin: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Pool::default())),
            origin: origin.into(),
        }
    }

    /// How many TCP connections this client has opened to a peer, ever.
    pub fn connections_opened(&self, base: &str) -> u64 {
        self.inner
            .lock()
            .map(|p| p.opened.get(base).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Call a tool on a peer and return its result envelope verbatim.
    ///
    /// `timeout` is the caller's own budget: a `follow` parked for 120 s needs a
    /// read timeout past that, and a `list_devices` must not wait two minutes to
    /// find out a node is gone.
    pub fn call_tool(
        &self,
        base: &str,
        tool: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<(Value, u64)> {
        self.call_tool_as(base, tool, args, timeout, &self.origin)
    }

    /// As `call_tool`, but stating WHO is asking.
    ///
    /// The origin is `<node>/<agent>`, and the owner uses it as the effective
    /// lease holder. Without it every proxied call arrives as the owner's own
    /// default holder, and two agents on two nodes silently share one identity:
    /// the first one's lease looks like the second one's, `LEASE_HELD` names
    /// nobody useful, and a steal cannot be attributed.
    pub fn call_tool_as(
        &self,
        base: &str,
        tool: &str,
        args: &Value,
        timeout: Duration,
        origin: &str,
    ) -> Result<(Value, u64)> {
        self.call_tool_via(base, tool, args, timeout, origin, "")
    }

    /// As `call_tool_as`, but also carrying the path the call has taken.
    #[allow(clippy::too_many_arguments)]
    pub fn call_tool_via(
        &self,
        base: &str,
        tool: &str,
        args: &Value,
        timeout: Duration,
        origin: &str,
        path: &str,
    ) -> Result<(Value, u64)> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        })
        .to_string();
        let started = Instant::now();
        // One retry, and only on a POOLED connection: a keep-alive socket the
        // far side closed while idle is the normal case, not an error worth
        // showing anyone. A fresh connection that fails is a real failure.
        let mut attempt = 0;
        loop {
            attempt += 1;
            let pooled = self.take(base)?;
            let fresh = pooled.is_none();
            let mut stream = match pooled {
                Some(s) => s,
                None => self.connect(base)?,
            };
            match self.exchange(&mut stream, base, &body, timeout, origin, path) {
                Ok(v) => {
                    self.give_back(base, stream);
                    return Ok((v, started.elapsed().as_millis() as u64));
                }
                Err(e) => {
                    if fresh || attempt > 1 {
                        return Err(e);
                    }
                    // Drop the dead socket and try once with a new one.
                }
            }
        }
    }

    fn take(&self, base: &str) -> Result<Option<TcpStream>> {
        let mut pool = self.inner.lock().map_err(|_| poisoned())?;
        Ok(pool.conns.remove(base))
    }

    fn give_back(&self, base: &str, stream: TcpStream) {
        if let Ok(mut pool) = self.inner.lock() {
            pool.conns.insert(base.to_string(), stream);
        }
    }

    fn connect(&self, base: &str) -> Result<TcpStream> {
        let (host, port, _) = split_url(base);
        let addr = format!("{host}:{port}");
        let mut last = None;
        let addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
            .map_err(|e| unreachable(base, &format!("cannot resolve {addr}: {e}")))?
            .collect();
        for a in addrs {
            match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    if let Ok(mut pool) = self.inner.lock() {
                        *pool.opened.entry(base.to_string()).or_insert(0) += 1;
                    }
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(unreachable(
            base,
            &last
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".into()),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn exchange(
        &self,
        stream: &mut TcpStream,
        base: &str,
        body: &str,
        timeout: Duration,
        origin: &str,
        path: &str,
    ) -> Result<Value> {
        let (host, port, url_path) = split_url(base);
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
        let req = format!(
            "POST {url_path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n{ORIGIN_HEADER}: {}\r\n\
             {PATH_HEADER}: {}\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            origin,
            path,
            body.len()
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| unreachable_io(base, &e))?;
        let text = read_response(stream, base)?;
        // The endpoint may answer as SSE; take the last JSON object either way.
        let payload = text
            .lines()
            .map(|l| l.trim_start_matches("data:").trim())
            .rfind(|l| l.starts_with('{'))
            .ok_or_else(|| unreachable(base, "no JSON in the reply"))?;
        serde_json::from_str(payload).map_err(|e| unreachable(base, &format!("bad JSON: {e}")))
    }
}

/// Read one HTTP response, honouring Content-Length so the connection can be
/// reused. Reading to EOF -- which the unpooled version did -- is exactly what
/// makes keep-alive impossible.
fn read_response(stream: &mut TcpStream, base: &str) -> Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find_headers_end(&buf) {
            break i;
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|e| unreachable_io(base, &e))?;
        if n == 0 {
            return Err(unreachable(base, "connection closed before the headers"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buf.len() < body_start + len {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| unreachable_io(base, &e))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf[body_start..]).to_string())
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub fn split_url(url: &str) -> (String, u16, String) {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .unwrap_or(url.trim_end_matches('/'));
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/mcp"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(8090)),
        None => (hostport.to_string(), 8090),
    };
    (host, port, path.to_string())
}

/// A peer that cannot be reached is reported, never guessed around.
///
/// Fail-open belongs in discovery, not in data: an empty answer for a device
/// that exists is indistinguishable from "the board said nothing", which is the
/// one confusion this project has spent ten rounds removing.
fn unreachable(base: &str, why: &str) -> ToolError {
    ToolError::new(
        ErrorCode::PeerUnreachable,
        format!("peer at {base} did not answer: {why}"),
    )
    .with_hint(
        "the owner of this device is unreachable; its data is not cached here and will not be \
         invented. Check the peer is up and that its mcpd is bound where it advertises.",
    )
}

/// The same, for an I/O failure whose KIND we still have.
///
/// NOT REACHING A HOST AND WAITING TOO LONG ARE DIFFERENT FAULTS, and one hint
/// cannot serve both. A read that expires means the socket connected and the far
/// node took the request: it is working, or it is stuck, but it is THERE.
/// Telling an operator to check whether the peer is up sends them to the wrong
/// machine -- measured while a board was being power-cycled in front of them for
/// seventy-four seconds behind a thirty-second ceiling.
///
/// Branching on `ErrorKind` rather than on the message, because the message is
/// not a signal: "Connection refused (os error 111)" CONTAINS "os error 11", and
/// the first draft of this function classified every refused connection as a
/// timeout for exactly that reason.
fn unreachable_io(base: &str, e: &std::io::Error) -> ToolError {
    use std::io::ErrorKind::*;
    if !matches!(e.kind(), WouldBlock | TimedOut) {
        return unreachable(base, &e.to_string());
    }
    ToolError::new(
        ErrorCode::PeerUnreachable,
        format!("peer at {base} accepted the call and did not answer in time: {e}"),
    )
    .with_hint(
        "the owner is reachable but took longer than this call allowed. Actuation on a board \
         that does not respond can legitimately run for a minute; raise `timeout_s`, or look at \
         the owner rather than the network.",
    )
}

fn poisoned() -> ToolError {
    ToolError::new(ErrorCode::Internal, "peer client pool is poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_host_port_path() {
        assert_eq!(
            split_url("http://192.168.10.10:8090/mcp"),
            ("192.168.10.10".to_string(), 8090, "/mcp".to_string())
        );
        // A bare host:port is the common config form; the path is where mcpd
        // actually listens.
        assert_eq!(
            split_url("http://alpha:8090"),
            ("alpha".to_string(), 8090, "/mcp".to_string())
        );
        assert_eq!(split_url("alpha").1, 8090);
    }
}
