//! The UDP beacon: "I am here, and this is where to reach me."
//!
//! One socket does both jobs -- broadcast out, listen in -- because two sockets
//! on one port is a portability problem nobody needs. A node drops its OWN
//! adverts by instance id rather than by source address: a host with several
//! interfaces hears itself from an address it does not recognise as itself, and
//! a node that peers with itself materialises its own devices as remote ones.
//!
//! Encoding and decoding are pure functions, so the wire format is tested
//! without opening a socket at all.

use super::registry::Advert;
use crate::error::{ErrorCode, Result, ToolError};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

/// Adverts larger than this are dropped unread.
///
/// A beacon is a postcard: id, name, a couple of URLs. Anything bigger is either
/// a bug or someone else's protocol on our port, and parsing it would be the
/// only place in this daemon where a stranger's bytes size an allocation.
pub const MAX_ADVERT_BYTES: usize = 2048;

pub struct Beacon {
    sock: UdpSocket,
    port: u16,
    /// Our own id, so we can ignore ourselves.
    self_id: String,
}

impl Beacon {
    /// Bind the beacon socket.
    ///
    /// `bind_addr` is 0.0.0.0 in production; tests pass 127.0.0.1 so a suite run
    /// never sprays adverts across the office LAN.
    pub fn bind(bind_addr: Ipv4Addr, port: u16, self_id: impl Into<String>) -> Result<Self> {
        let sock = UdpSocket::bind((bind_addr, port)).map_err(|e| {
            ToolError::new(
                ErrorCode::Internal,
                format!("cannot bind the peer beacon on {bind_addr}:{port}: {e}"),
            )
            .with_hint(
                "another conminer (or another program) already holds this UDP port; peerd needs \
                 host networking and a port of its own",
            )
        })?;
        sock.set_broadcast(true).ok();
        sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
        Ok(Self {
            sock,
            port,
            self_id: self_id.into(),
        })
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    /// Broadcast one advert.
    pub fn announce(&self, advert: &Advert) -> Result<()> {
        let bytes = encode(advert);
        if bytes.len() > MAX_ADVERT_BYTES {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!(
                    "advert is {} bytes, over the {MAX_ADVERT_BYTES} cap",
                    bytes.len()
                ),
            ));
        }
        self.sock
            .send_to(&bytes, (Ipv4Addr::BROADCAST, self.port))
            .map_err(|e| ToolError::new(ErrorCode::Internal, format!("beacon send failed: {e}")))?;
        Ok(())
    }

    /// Send an advert to one specific address (used by tests and by targeted
    /// re-announce when a peer is known but quiet).
    pub fn announce_to(&self, advert: &Advert, to: SocketAddr) -> Result<()> {
        self.sock
            .send_to(&encode(advert), to)
            .map_err(|e| ToolError::new(ErrorCode::Internal, format!("beacon send failed: {e}")))?;
        Ok(())
    }

    /// Receive the next advert that is not ours, or `None` on timeout.
    pub fn recv(&self) -> Option<(Advert, SocketAddr)> {
        let mut buf = [0u8; MAX_ADVERT_BYTES];
        let (n, from) = self.sock.recv_from(&mut buf).ok()?;
        if n >= MAX_ADVERT_BYTES {
            // Truncated: refuse rather than half-parse.
            return None;
        }
        let advert = decode(&buf[..n])?;
        if advert.instance_id == self.self_id {
            return None;
        }
        Some((advert, from))
    }
}

pub fn encode(advert: &Advert) -> Vec<u8> {
    serde_json::to_vec(advert).unwrap_or_default()
}

/// Parse an advert. Anything malformed is dropped silently: the beacon port is
/// a public surface on a LAN and a parse error is not an event worth logging
/// once per second.
pub fn decode(bytes: &[u8]) -> Option<Advert> {
    if bytes.len() > MAX_ADVERT_BYTES {
        return None;
    }
    let advert: Advert = serde_json::from_slice(bytes).ok()?;
    if advert.instance_id.is_empty() || advert.name.is_empty() || advert.mcp_url.is_empty() {
        return None;
    }
    Some(advert)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert(id: &str, name: &str) -> Advert {
        Advert {
            instance_id: id.into(),
            name: name.into(),
            version: "0.2.0".into(),
            mcp_url: "http://192.168.10.10:8090/mcp".into(),
            dash_url: "http://192.168.10.10:8080".into(),
            ser2net_host: "192.168.10.10".into(),
            ser2net_ports: vec![5001, 5002],
        }
    }

    #[test]
    fn the_wire_format_round_trips() {
        let a = advert("id-a", "alpha");
        let back = decode(&encode(&a)).expect("decodes");
        assert_eq!(a, back);
    }

    #[test]
    fn junk_on_the_port_is_dropped_not_parsed() {
        assert!(decode(b"").is_none());
        assert!(decode(b"not json at all").is_none());
        // Well-formed JSON that is not an advert.
        assert!(decode(br#"{"hello":"world"}"#).is_none());
        // An advert with no way to reach it is not an advert.
        assert!(decode(
            br#"{"instance_id":"x","name":"y","version":"1","mcp_url":"",
                           "dash_url":"","ser2net_host":""}"#
        )
        .is_none());
        // Oversized input never sizes an allocation.
        let big = vec![b'x'; MAX_ADVERT_BYTES + 1];
        assert!(decode(&big).is_none());
    }

    #[test]
    fn a_node_does_not_peer_with_itself() {
        // Loopback round trip on an odd port: send our own advert and a peer's,
        // and see only the peer's come back.
        let port = 49_337;
        let me = Beacon::bind(Ipv4Addr::LOCALHOST, port, "id-self").expect("bind");
        let addr = me.local_addr().expect("addr");

        me.announce_to(&advert("id-self", "me"), addr)
            .expect("send");
        me.announce_to(&advert("id-other", "them"), addr)
            .expect("send");

        let mut heard = Vec::new();
        for _ in 0..4 {
            if let Some((a, _)) = me.recv() {
                heard.push(a.instance_id);
            }
        }
        assert_eq!(
            heard,
            vec!["id-other".to_string()],
            "a node that hears itself and believes it would materialise its own boards as \
             remote ones"
        );
    }
}
