//! §P1. Fleet peering: several conminer instances, one pool of hardware.
//!
//! THE OWNER MINES; EVERYONE ELSE PROXIES. The node a board is physically cabled
//! to runs the only capture, the only store and the only lease table for it.
//! Other nodes carry a registry ROW for the device and forward every tool call
//! to the owner. The alternative -- mirroring the console to a second miner --
//! buys nothing and costs the one property this whole tool is built on: a single
//! agreed history for each board. Two stores that disagree about a boot are
//! worse than one store you have to ask over the network.
//!
//! What that decision buys, concretely:
//!   * epochs, templates, baselines and leases have exactly one writer
//!   * a proxied answer is the owner's answer, verbatim -- no merge semantics
//!   * an agent on either node reads the same numbers for the same board
//!
//! The pieces here are deliberately small and independent:
//!   * `identity`  -- who this node is, stable across restarts and renames
//!   * `client`    -- pooled HTTP to a peer's mcpd
//!   * `registry`  -- TTL soft-state table of live peers, in registry.db
//!   * `beacon`    -- UDP broadcast advert + listener
//!   * `inventory` -- pull each peer's device list, materialise remote rows
//!   * `relay`     -- reverse call channel, for actuation across a one-way link
//!
//! Trust model for v1 is LAN-trust, matching the rest of the deployment (the
//! dashboard is already LAN-open by deliberate choice). Requests carry an origin
//! header so a signature can be added later without changing the shape; the slot
//! is named in `client::ORIGIN_HEADER` and the HMAC helper already exists in
//! `push::signature`.

pub mod beacon;
pub mod client;
pub mod identity;
pub mod inventory;
pub mod registry;
pub mod relay;

pub use client::PeerClient;
pub use identity::Identity;
pub use registry::{PeerRow, PeerSource};
pub use relay::RelayQueue;

/// Is this a usable node name?
///
/// §P3. THE ID SCHEME DEPENDS ON THIS. A remote device is `peer:<node>/<remote>`
/// and is split back on the first `/`, so a node name containing one does not
/// round-trip: `peer:http://host:8090/mcp//dev/ttyX` reads back as the node
/// `"http:"`, which is not a node, is in nobody's peer table, and can never be
/// routed to.
///
/// Measured on alpha, which held two such rows for months. They came in over a
/// RELAY: charlie still knew bravo by the placeholder name peerd invents from
/// `[peers] nodes` (a bare URL), and relayed that name onward as the owner. The
/// placeholder guard on the peer table could not see it -- the peer alpha
/// talked to was perfectly well-named -- so the bad name was laundered by one
/// honest hop into a row that no sweep would ever collect.
pub fn valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains('/')
        && !name.contains(':')
        && !name.chars().any(char::is_whitespace)
}

/// The canonical id a remote device gets on this node.
///
/// Deliberately a string prefix, following the `file:` and `#dmesg` precedent
/// that already exists in the registry: uniqueness, `db_stem` and every existing
/// query keep working untouched, and one glance at an id says where it lives.
pub fn remote_canonical(node: &str, remote: &str) -> String {
    format!("peer:{node}/{remote}")
}

/// Split `peer:<node>/<remote>` back into its parts.
pub fn split_remote(canonical: &str) -> Option<(String, String)> {
    let rest = canonical.strip_prefix("peer:")?;
    let (node, remote) = rest.split_once('/')?;
    if node.is_empty() || remote.is_empty() {
        return None;
    }
    Some((node.to_string(), remote.to_string()))
}

/// A target name as it appears on a remote node: `<node>:<target>`.
///
/// Targets are derived from USB topology on every node, so `3.2` exists on all
/// of them and means something different on each. Namespacing is not cosmetic:
/// without it, `selftest {target: "3.2"}` on a two-node fleet is a coin flip.
pub fn remote_target(node: &str, target: &str) -> String {
    format!("{node}:{target}")
}

pub fn split_remote_target(name: &str) -> Option<(String, String)> {
    let (node, target) = name.split_once(':')?;
    if node.is_empty() || target.is_empty() || node.contains('/') {
        return None;
    }
    Some((node.to_string(), target.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_ids_round_trip() {
        let c = remote_canonical("alpha", "/dev/serial/by-id/usb-FTDI_X-if00-port0");
        assert_eq!(c, "peer:alpha//dev/serial/by-id/usb-FTDI_X-if00-port0");
        let (node, remote) = split_remote(&c).expect("splits");
        assert_eq!(node, "alpha");
        assert_eq!(remote, "/dev/serial/by-id/usb-FTDI_X-if00-port0");
    }

    #[test]
    fn a_local_id_is_never_mistaken_for_a_remote_one() {
        assert!(split_remote("/dev/serial/by-id/usb-FTDI_X-if00-port0").is_none());
        assert!(split_remote("file:/tmp/x.log").is_none());
        assert!(split_remote("peer:").is_none());
        assert!(split_remote("peer:node-with-no-device/").is_none());
    }

    #[test]
    fn a_url_is_not_a_node_name() {
        assert!(valid_node_name("alpha"));
        assert!(valid_node_name("charlie"));
        // The placeholder peerd invents from `[peers] nodes`, which split_remote
        // would read back as the node "http:".
        assert!(!valid_node_name("http://192.168.10.11:8090/mcp"));
        assert!(!valid_node_name("http:"));
        assert!(!valid_node_name(""));
        assert!(!valid_node_name("two words"));
    }

    #[test]
    fn targets_are_namespaced_per_node() {
        assert_eq!(remote_target("alpha", "3.2"), "alpha:3.2");
        assert_eq!(
            split_remote_target("alpha:3.2"),
            Some(("alpha".into(), "3.2".into()))
        );
        // A bare target is local and must not be read as a remote one.
        assert_eq!(split_remote_target("3.2"), None);
    }
}
