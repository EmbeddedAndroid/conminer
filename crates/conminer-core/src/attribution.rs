//! Which USB devices may be attributed to a board as ITS devices.
//!
//! A board's `usb_identity` and `usb_ports` tags are learned causally: whatever
//! enumerates in the window after the board is actuated. That window cannot
//! tell a board's own gadget (EDL, fastboot, its OS composite) from an
//! unrelated cable plugged in at the same moment, and a debug harness for
//! another board is exactly such a cable: its UART and controller would be
//! learned as the actuated board's gadgets, and that board's EDL scope would
//! then point at the other board's hub.
//!
//! The registry already knows which USB devices are debug adapters: every
//! console and controller row is one. A device that carries a known console
//! or controller is never a board gadget, whichever board was actuated. This
//! module is that one rule, applied when learning, when recording, when
//! resolving, and by discovery to heal tags written before the rule existed.

use crate::error::Result;
use crate::store::{DeviceRow, Registry};
use crate::usb::Identified;
use std::collections::BTreeMap;

/// Shortest serial worth matching textually. Shorter strings occur inside
/// unrelated by-id names by accident.
const MIN_SERIAL: usize = 4;

/// The serial as udev writes it into a by-id name.
///
/// udev collapses whitespace runs to one `_` and replaces every character
/// outside `0-9A-Za-z#+-.:=@_` with `_`.
fn udev_serial(s: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push('_');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if ch.is_ascii_alphanumeric() || "#+-.:=@_".contains(ch) {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Rows describing hardware cabled to THIS host: not a peer's, not an
/// internal sibling store.
fn local(rows: &[DeviceRow]) -> impl Iterator<Item = &DeviceRow> {
    rows.iter()
        .filter(|d| !d.kind.is_remote() && !d.canonical.contains('#'))
}

/// `pci-0000:00:14.0-usb-0:2.2.2:1.0-port0` -> `2.2.2`.
pub fn by_path_chain(by_path: &str) -> Option<&str> {
    let after = by_path
        .split("-usb-0:")
        .nth(1)
        .or_else(|| by_path.split("-usbv2-0:").nth(1))?;
    after.split(':').next().filter(|c| !c.is_empty())
}

/// `1-2.2.2` -> `2.2.2`. The bus number is dropped because a by-path name
/// does not carry it.
fn port_chain(port: &str) -> Option<&str> {
    port.split_once('-')
        .map(|(_, c)| c)
        .filter(|c| !c.is_empty())
}

/// Does a by-id name of a console or controller known here carry this USB
/// serial? Gone rows count: an adapter unplugged for a while is still an
/// adapter when it comes back.
pub fn serial_is_adapter(serial: &str, rows: &[DeviceRow]) -> bool {
    let s = udev_serial(serial);
    if s.len() < MIN_SERIAL {
        return false;
    }
    let mid = format!("_{s}-if");
    let end = format!("_{s}");
    local(rows).any(|d| {
        let Some(name) = d.canonical.strip_prefix("/dev/serial/by-id/") else {
            return false;
        };
        name.contains(&mid) || name.ends_with(&end)
    })
}

/// Is a console or controller that is plugged in right now hanging off this
/// USB port? Only present rows: a gone row's `by_path` is where it USED to
/// be, and another board may sit there now.
pub fn port_is_adapter(port: &str, rows: &[DeviceRow]) -> bool {
    let Some(chain) = port_chain(port) else {
        return false;
    };
    local(rows)
        .filter(|d| d.is_present())
        .filter_map(|d| d.by_path.as_deref().and_then(by_path_chain))
        .any(|c| c == chain)
}

/// The serial (or product string) part of a learned identity
/// `vvvv:pppp:<serial>`. The tail may itself contain `:`.
fn identity_tail(identity: &str) -> Option<&str> {
    identity.splitn(3, ':').nth(2)
}

/// Is a learned identity one of this host's debug adapters?
pub fn identity_is_adapter(identity: &str, rows: &[DeviceRow]) -> bool {
    identity_tail(identity).is_some_and(|s| serial_is_adapter(s, rows))
}

/// Is this device on the bus a known console or controller adapter?
pub fn is_known_adapter(dev: &Identified, rows: &[DeviceRow]) -> bool {
    dev.serial
        .as_deref()
        .is_some_and(|s| serial_is_adapter(s, rows))
        || dev
            .port_path
            .as_deref()
            .is_some_and(|p| port_is_adapter(p, rows))
}

fn split(v: Option<&String>) -> Vec<String> {
    v.map(|t| {
        t.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// Remove adapter identities and adapter ports from every local row's
/// learned `usb_identity` / `usb_ports`. Returns the rows it changed.
///
/// Configured `usb_ports` live in conminer.toml, not in tags, so an operator's
/// explicit declaration is never touched here.
pub fn heal(reg: &mut Registry) -> Result<Vec<String>> {
    let rows = reg.all_devices()?;
    let mut changed = Vec::new();
    for d in local(&rows) {
        let ids = split(d.tags.get("usb_identity"));
        let ports = split(d.tags.get("usb_ports"));
        let keep_ids: Vec<String> = ids
            .iter()
            .filter(|i| !identity_is_adapter(i, &rows))
            .cloned()
            .collect();
        let keep_ports: Vec<String> = ports
            .iter()
            .filter(|p| !port_is_adapter(p, &rows))
            .cloned()
            .collect();
        if keep_ids.len() == ids.len() && keep_ports.len() == ports.len() {
            continue;
        }
        let mut set = BTreeMap::new();
        let mut drop = Vec::new();
        for (key, keep) in [("usb_identity", &keep_ids), ("usb_ports", &keep_ports)] {
            if keep.is_empty() {
                drop.push(key.to_string());
            } else {
                set.insert(key.to_string(), keep.join(","));
            }
        }
        if !set.is_empty() {
            reg.set_tags(d.id, &set)?;
        }
        reg.remove_tags(d.id, &drop)?;
        tracing::info!(
            device = %d.canonical,
            usb_identity = %keep_ids.join(","),
            usb_ports = %keep_ports.join(","),
            "dropped another board's debug adapters from a learned USB attribution"
        );
        changed.push(d.canonical.clone());
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udev_serial_matches_how_by_id_names_are_written() {
        assert_eq!(udev_serial("UARTA001"), "UARTA001");
        assert_eq!(udev_serial("  a  b/c "), "a_b_c");
        assert_eq!(udev_serial("x:y=z@1"), "x:y=z@1");
    }

    #[test]
    fn chains_drop_the_bus_and_the_interface() {
        assert_eq!(
            by_path_chain("pci-0000:00:14.0-usb-0:2.2.2:1.0-port0"),
            Some("2.2.2")
        );
        assert_eq!(
            by_path_chain("pci-0000:00:14.0-usbv2-0:2.4.4:1.0"),
            Some("2.4.4")
        );
        assert_eq!(by_path_chain("platform-foo"), None);
        assert_eq!(port_chain("1-2.2.2"), Some("2.2.2"));
        assert_eq!(port_chain("usb1"), None);
    }

    #[test]
    fn identity_tail_keeps_colons_inside_a_qdl_product_string() {
        assert_eq!(
            identity_tail("05c6:9008:F10_CID:0001_SN:0C0FFEE1"),
            Some("F10_CID:0001_SN:0C0FFEE1")
        );
        assert_eq!(identity_tail("05c6:9008"), None);
    }
}
