//! Device discovery and ser2net config generation (§3, §13 `discovery`,
//! `ser2net-gen`).
//!
//! Identity is the whole point here. A plugged-in FTDI cable must be queryable
//! within about a second with zero configuration, and it must still be the *same*
//! device after a replug, a host reboot, or a `ttyUSBn` renumbering. So:
//!
//! * **by-id is used everywhere internally.** Generated ser2net configs open the
//!   `/dev/serial/by-id` symlink, never `/dev/ttyUSBn`; the store persists the
//!   by-id path; and no ttyUSBn name appears in a tool response except as
//!   informational detail in `identify`.
//! * **Serial-less clones fall back to topology.** Two indistinguishable FTDI
//!   clones in adjacent ports are told apart by `/dev/serial/by-path`, and the
//!   caveat is surfaced (`identity: positional`) so a human knows moving the
//!   cable moves the name.

use crate::config::{Config, LineConfig};
use crate::error::Result;
use crate::store::{DeviceRow, IdentityKind, Registry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One device as the filesystem presents it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    /// The canonical id: the `/dev/serial/by-id` path when the adapter has a
    /// serial number, else the `/dev/serial/by-path` position.
    pub canonical: String,
    pub by_path: Option<String>,
    pub identity: IdentityKind,
    /// Informational only.
    pub tty: Option<String>,
}

impl Discovered {
    /// The short name globs in `discovery.include`/`exclude` are matched against.
    pub fn name(&self) -> &str {
        Path::new(&self.canonical)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.canonical)
    }
}

/// Is this by-id entry a serial console we should bridge, or something that
/// merely looks like one?
///
/// Ported from the HIL, which learned this on the same class of hardware. The
/// important case is Qualcomm's Sahara/Firehose endpoint: it appears under
/// /dev/serial/by-id the moment a Qualcomm SoC enters EDL and disappears when
/// it leaves, so every power-cycle into EDL is a device-set change. Bridging it
/// churns the ser2net config on each transition and disconnects live sessions
/// on *other* boards -- and it is not a console anyway; it is a flashing
/// endpoint that qdl/sahara drivers open directly.
fn is_serial_console(by_id_name: &str) -> bool {
    const NON_CONSOLE: &[&str] = &[
        // Sahara / Firehose (EDL). Comes and goes with every EDL entry.
        "qusb_bulk",
        // Qualcomm diagnostic endpoint, likewise not a console.
        "qcom_diag",
    ];
    let lower = by_id_name.to_ascii_lowercase();
    !NON_CONSOLE.iter().any(|p| lower.contains(p))
}

/// Is this USB device a download/flash gadget rather than a console?
///
/// THE NAME TEST ABOVE IS NOT ENOUGH, and the gap is not theoretical. A board in
/// EDL exposes Qualcomm's QDL gadget, and on the IQ8 that gadget carries NO USB
/// serial number -- so it never appears under `/dev/serial/by-id` for the name
/// filter to catch, and arrives instead through the positional by-path branch,
/// which is the one place `is_serial_console` never ran. Measured on the bravo
/// bench: five EDL entries left a `pci-…-usb-0:2:1.0-port0` console on the
/// dashboard, holding a ser2net port, that was never a console at all.
///
/// The vendor and product ids are the authoritative test, and `usb.rs` already
/// knows them because EDL detection depends on the same pair.
pub fn is_download_gadget(vid: u16, pid: u16) -> bool {
    crate::usb::is_qdl_id(vid, pid)
}

/// The USB ids behind a tty, read from sysfs.
///
/// `/sys/class/tty/ttyUSB2/device` points at the USB *interface*; the ids live
/// on the parent device, so this walks up until it finds them.
fn sysfs_usb_ids(tty: &str) -> Option<(u16, u16)> {
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{tty}/device")).ok()?;
    for _ in 0..6 {
        let vid = std::fs::read_to_string(dir.join("idVendor")).ok();
        let pid = std::fs::read_to_string(dir.join("idProduct")).ok();
        if let (Some(v), Some(p)) = (vid, pid) {
            return Some((
                u16::from_str_radix(v.trim(), 16).ok()?,
                u16::from_str_radix(p.trim(), 16).ok()?,
            ));
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Scan `/dev/serial/by-id` and `/dev/serial/by-path`.
///
/// `root` lets the §12.6 e2e harness point this at a faked `/dev`, which is how
/// hotplug is simulated without hardware.
pub fn scan(root: &Path) -> Result<Vec<Discovered>> {
    scan_with(root, &sysfs_usb_ids)
}

/// As [`scan`], but with the USB-id lookup injected.
///
/// The e2e harness fakes `/dev`; it cannot fake `/sys`, so without this seam the
/// download-gadget rule would be untestable and therefore untested.
pub fn scan_with(
    root: &Path,
    usb_ids: &dyn Fn(&str) -> Option<(u16, u16)>,
) -> Result<Vec<Discovered>> {
    let by_id = root.join("serial/by-id");
    let by_path = root.join("serial/by-path");

    // tty → by-path, so a serial-less adapter can borrow its topology.
    let mut topology: BTreeMap<PathBuf, String> = BTreeMap::new();
    if by_path.is_dir() {
        for e in std::fs::read_dir(&by_path)?.flatten() {
            if let Ok(target) = std::fs::canonicalize(e.path()) {
                topology.insert(target, e.path().to_string_lossy().into_owned());
            }
        }
    }

    let mut out: Vec<Discovered> = Vec::new();
    let mut claimed: std::collections::BTreeSet<PathBuf> = Default::default();

    if by_id.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&by_id)?.flatten().collect();
        // Deterministic order, so the device set does not appear to change
        // merely because readdir returned a different permutation.
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let link = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_serial_console(&name) {
                tracing::debug!(device = %name, "not a serial console; skipping");
                continue;
            }
            let target = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
            if let Some(tty) = target.file_name().and_then(|s| s.to_str()) {
                if let Some((vid, pid)) = usb_ids(tty) {
                    if is_download_gadget(vid, pid) {
                        tracing::info!(
                            device = %name,
                            id = format!("{vid:04x}:{pid:04x}"),
                            "a download/flash gadget, not a console; skipping"
                        );
                        continue;
                    }
                }
            }
            // One physical device can be exposed under several by-id names.
            // Listing it twice makes the set look like it changed whenever the
            // duplicate comes and goes.
            if claimed.contains(&target) {
                continue;
            }
            claimed.insert(target.clone());
            out.push(Discovered {
                canonical: link.to_string_lossy().into_owned(),
                by_path: topology.get(&target).cloned(),
                identity: IdentityKind::ById,
                tty: target
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
            });
        }
    }

    // Adapters with no usable serial number never appear under by-id. They are
    // still real consoles, so key them on position and say so.
    for (target, path) in &topology {
        if claimed.contains(target) {
            continue;
        }
        // The branch the EDL gadget actually arrives on: no serial number means
        // no by-id entry, so nothing above ever looked at it.
        if let Some(tty) = target.file_name().and_then(|s| s.to_str()) {
            if let Some((vid, pid)) = usb_ids(tty) {
                if is_download_gadget(vid, pid) {
                    tracing::info!(
                        device = %path,
                        id = format!("{vid:04x}:{pid:04x}"),
                        "a download/flash gadget, not a console; skipping"
                    );
                    continue;
                }
            }
        }
        out.push(Discovered {
            canonical: path.clone(),
            by_path: Some(path.clone()),
            identity: IdentityKind::Positional,
            tty: target
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string),
        });
    }

    out.sort_by(|a, b| a.canonical.cmp(&b.canonical));
    Ok(out)
}

/// What one reconciliation pass changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciled {
    pub added: Vec<String>,
    pub returned: Vec<String>,
    pub gone: Vec<String>,
    pub ignored: Vec<String>,
    pub unchanged: usize,
}

impl Reconciled {
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.returned.is_empty() || !self.gone.is_empty()
    }
}

/// Bring the registry into line with what is physically present.
///
/// Devices that vanish are marked `gone`, never deleted: their nickname, tags,
/// port assignment and entire capture history must survive an unplugged cable.
pub fn reconcile(
    reg: &mut Registry,
    cfg: &Config,
    found: &[Discovered],
    now: i64,
) -> Result<Reconciled> {
    let mut r = Reconciled::default();
    let present: std::collections::BTreeSet<&str> =
        found.iter().map(|d| d.canonical.as_str()).collect();

    for d in found {
        // The exclude list is the important one: lab hosts carry modems, UPS
        // serials and debug-probe aux ports conminer must be keepable off.
        if !cfg.device_included(d.name()) {
            let row = reg.upsert_device(
                &d.canonical,
                d.by_path.as_deref(),
                d.identity,
                d.tty.as_deref(),
                now,
            )?;
            reg.set_ignored(row.id, true)?;
            reg.set_state(row.id, "ignored")?;
            r.ignored.push(d.canonical.clone());
            continue;
        }

        let existed = reg.device_by_canonical(&d.canonical)?;
        let row = reg.upsert_device(
            &d.canonical,
            d.by_path.as_deref(),
            d.identity,
            d.tty.as_deref(),
            now,
        )?;
        reg.set_ignored(row.id, false)?;
        reg.assign_port(row.id, cfg.ser2net.base_port)?;

        match existed {
            None => {
                // First sighting: apply the layered line defaults (§3.2).
                reg.set_line(row.id, &cfg.line_for(row.display_name()))?;
                reg.set_state(row.id, "discovered")?;
                r.added.push(d.canonical.clone());
            }
            Some(prev) if prev.state == "gone" => {
                reg.set_state(row.id, "discovered")?;
                r.returned.push(d.canonical.clone());
            }
            Some(_) => r.unchanged += 1,
        }
    }

    for known in reg.all_devices()? {
        // §P1. A PEER'S BOARD IS NOT MISSING JUST BECAUSE IT IS NOT OURS.
        //
        // This sweep answers one question -- "is this cable still plugged into
        // THIS host?" -- and a remote row was never plugged in here at all.
        // Marking it gone is not a harmless label: `gone` rows are dropped from
        // the ser2net config, so the remote console stops being re-exported, and
        // every listing shows the peer's hardware as dead. Measured on the first
        // two-host bring-up, where all thirteen of the lab host's consoles read
        // `gone` on the other node while the lab host itself reported them
        // listening. Their lifecycle belongs to inventory sync, which asks the
        // owner rather than the local /dev tree.
        if known.kind.is_remote() {
            continue;
        }
        // AN EXCLUDED CONTROLLER IS STILL A CABLE.
        //
        // This skipped every `ignored` row, so a device conminer is configured
        // not to OPEN was also never recorded as having been UNPLUGGED. Both
        // Bantams on alpha sat at `state=ignored` five and a half days after
        // their cables came out, and every surface that trusts the registry
        // drew them as the rig's control panel -- one of them as the resolved
        // power controller for a Nucleo that is merely on the same USB hub.
        //
        // Nothing is lost by marking them: `ignored` is its own column, so the
        // exclusion survives, and the loop above writes `state=ignored` back the
        // moment the cable returns.
        //
        // Sibling stores stay out. `<port>#dmesg` is an internal handle that was
        // never plugged into anything, so "gone" would be a claim about hardware
        // for a row that describes none.
        if known.canonical.contains('#') || present.contains(known.canonical.as_str()) {
            continue;
        }
        if known.state != "gone" {
            // Marked, never deleted: history and identity outlive the cable.
            reg.set_state(known.id, "gone")?;
            r.gone.push(known.canonical.clone());
        }
    }

    Ok(r)
}

/// Render a ser2net YAML config from the registry (§3).
///
/// Every connection opens the **by-id** path, so a renumbered `ttyUSBn` cannot
/// silently point an endpoint at a different board.
pub fn ser2net_config(devices: &[DeviceRow], cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str(
        "# Generated by conminer discoveryd. Do not edit: regenerated whenever the\n\
         # device registry changes, and reloaded with SIGHUP.\n\
         #\n\
         # ser2net 4.x YAML. The runtime image is Debian for this: Alpine stable\n\
         # has only 3.x, which cannot read this and serves one client per port.\n\
         # The connector option list is COMMA separated — space-separated words\n\
         # are 3.x syntax and are silently wrong here.\n\
         %YAML 1.1\n---\n\n",
    );

    // Say what is being left out, and why. A device dropped here has no console
    // at all, and silence about it is how this rig served three consoles where
    // four were expected with nothing reporting it -- no log line, no unhealthy
    // service, no tool error. It was found by counting accepter lines by hand.
    for d in devices.iter() {
        let why = if d.ignored {
            "ignored by config"
        } else if d.state == "gone" {
            "state=gone (node absent, or latched gone and never recovered)"
        } else if d.ser2net_port.is_none() {
            "no ser2net port assigned"
        } else {
            continue;
        };
        tracing::warn!(
            device = %d.canonical,
            port = ?d.ser2net_port,
            state = %d.state,
            reason = why,
            "device excluded from ser2net config: it will have no console"
        );
    }

    let mut sorted: Vec<&DeviceRow> = devices
        .iter()
        .filter(|d| !d.ignored && d.state != "gone" && d.ser2net_port.is_some())
        .collect();
    sorted.sort_by_key(|d| d.ser2net_port);

    for d in sorted {
        let port = d.ser2net_port.expect("filtered");

        // §P1. A REMOTE CONSOLE RE-EXPORTS ON A LOCAL PORT.
        //
        // The device is cabled to another node, so there is no serialdev here to
        // open. ser2net dials the owner's port instead and relays -- proven
        // against stock ser2net 4.x before any of this was wired -- which means
        // the dashboard terminal, `endpoint_for`, and a human with telnet all
        // work on a remote board with no idea that it is remote. The alternative
        // was teaching every consumer a second dial path.
        //
        // No `max-connections` here: the fan-out that matters happens on the
        // OWNER's port, where the miner and the humans already share one
        // connector. Stacking a second limit on the relay would cap the fleet at
        // whichever number is smaller for no reason anyone could see.
        if d.kind.is_remote() {
            let Some(host) = d.node_host.as_deref().filter(|h| !h.is_empty()) else {
                tracing::warn!(
                    device = %d.canonical,
                    node = ?d.node,
                    "remote device has no host to relay to: its console will be absent until \
                     the peer advertises an address"
                );
                continue;
            };
            let Some(remote_port) = d.remote_port else {
                tracing::warn!(
                    device = %d.canonical,
                    node = ?d.node,
                    "remote device has no console port on its owner: nothing to relay to"
                );
                continue;
            };
            out.push_str(&format!(
                "connection: &{name}\n  \
                 accepter: telnet(rfc2217=false),tcp,{bind},{port}\n  \
                 enable: on\n  \
                 connector: tcp,{host},{remote_port}\n  \
                 options:\n    \
                 kickolduser: false\n\n",
                name = yaml_key(d.display_name()),
                bind = cfg.ser2net.bind,
            ));
            continue;
        }

        let line = if d.line == LineConfig::default() {
            cfg.line_for(d.display_name())
        } else {
            d.line.clone()
        };
        // `telnet(rfc2217=false)` matches the lab's proven ser2net 4.x config.
        // A plain `tcp` accepter was tried and made the second consumer fail
        // with "Device open failure: Object was already in use" — 4.x shares one
        // connector across clients of a telnet accepter, which is exactly the
        // multi-consumer behaviour `max-connections` is meant to provide.
        // rfc2217 stays off so a client cannot renegotiate the line settings
        // conminer configured.
        out.push_str(&format!(
            "connection: &{name}\n  \
             accepter: telnet(rfc2217=false),tcp,{bind},{port}\n  \
             enable: on\n  \
             connector: serialdev,{dev},{opts}\n  \
             options:\n    \
             max-connections: {maxconn}\n    \
             kickolduser: false\n\n",
            name = yaml_key(d.display_name()),
            bind = cfg.ser2net.bind,
            port = port,
            dev = d.canonical,
            opts = line.ser2net_options(),
            maxconn = cfg.ser2net.max_connections,
        ));
    }
    out
}

/// ser2net anchors must be bare YAML keys; port paths are full of `/`, `.` and
/// `-`, so they are folded to underscores.
///
/// Derived from the PORT, never from a label. A label used to feed this, which
/// meant renaming a board rewrote its ser2net anchor -- a config change, a
/// reload, and every console on that port dropped, all because somebody typed a
/// friendlier name. Ports do not move when opinions do.
pub fn yaml_key(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("con_{}", s.trim_matches('_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `/dev` tree: `serial/by-id/<name> -> ../../<tty>`.
    fn fake_dev(entries: &[(&str, Option<&str>, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("serial/by-id")).unwrap();
        std::fs::create_dir_all(root.join("serial/by-path")).unwrap();
        for (by_id, by_path, tty) in entries {
            let target = root.join(tty);
            std::fs::write(&target, b"").unwrap();
            if !by_id.is_empty() {
                std::os::unix::fs::symlink(&target, root.join("serial/by-id").join(by_id)).unwrap();
            }
            if let Some(p) = by_path {
                std::os::unix::fs::symlink(&target, root.join("serial/by-path").join(p)).unwrap();
            }
        }
        dir
    }

    /// FOUND ON THE bravo BENCH, after five EDL entries.
    ///
    /// A board in EDL exposes Qualcomm's QDL gadget. On the IQ8 that gadget has
    /// NO USB serial number, so it never appears under `/dev/serial/by-id` where
    /// the name filter (`qusb_bulk`, `qcom_diag`) would have caught it -- it
    /// arrives through the POSITIONAL by-path branch, the one place that filter
    /// never ran. The result was a `pci-…-usb-0:2:1.0-port0` console sitting on
    /// the dashboard holding a ser2net port, that was never a console.
    #[test]
    fn a_download_gadget_is_not_discovered_as_a_console() {
        let dev = fake_dev(&[
            // A real console, named.
            (
                "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0",
                Some("pci-0000:04:00.3-usb-0:4:1.0-port0"),
                "ttyUSB0",
            ),
            // The QDL gadget: no by-id entry at all, only a topology path.
            ("", Some("pci-0000:04:00.3-usb-0:2:1.0-port0"), "ttyUSB9"),
        ]);
        let ids = |tty: &str| -> Option<(u16, u16)> {
            match tty {
                // 05c6:9008 -- Qualcomm QDL.
                "ttyUSB9" => Some((0x05c6, 0x9008)),
                _ => Some((0x0403, 0x6011)),
            }
        };
        let found = scan_with(dev.path(), &ids).unwrap();
        assert_eq!(
            found.len(),
            1,
            "the QDL gadget must not be discovered: {:?}",
            found.iter().map(|d| &d.canonical).collect::<Vec<_>>()
        );
        assert!(found[0].canonical.contains("RIDE_MICRO"));

        // And the rule is about the IDS, not the position: the same path with an
        // ordinary FTDI behind it is a console and must still be found.
        let ordinary = |_: &str| Some((0x0403u16, 0x6011u16));
        assert_eq!(scan_with(dev.path(), &ordinary).unwrap().len(), 2);
    }

    /// A host where the ids cannot be read must not lose its consoles: unknown
    /// is not a synonym for "download gadget".
    #[test]
    fn an_unreadable_usb_id_keeps_the_device() {
        let dev = fake_dev(&[(
            "usb-FTDI_Thing_AAAA-if00-port0",
            Some("pci-0000:00:14.0-usb-0:1:1.0-port0"),
            "ttyUSB0",
        )]);
        assert_eq!(scan_with(dev.path(), &|_| None).unwrap().len(), 1);
    }

    #[test]
    fn the_download_gadget_rule_names_the_right_ids() {
        assert!(is_download_gadget(0x05c6, 0x9008)); // QDL / Sahara
        assert!(is_download_gadget(0x05c6, 0x900e));
        assert!(!is_download_gadget(0x0403, 0x6011)); // FT4232H
        assert!(!is_download_gadget(0x05c6, 0x1000)); // a Qualcomm that is not QDL
    }

    fn reg_cfg() -> (Registry, Config) {
        (Registry::open_memory().unwrap(), Config::default())
    }

    #[test]
    fn a_plugged_in_cable_is_discovered_with_its_by_id_identity() {
        let dev = fake_dev(&[(
            "usb-FTDI_TTL232R_FT1234-if00-port0",
            Some("pci-0000:00:14.0-usb-0:1.1:1.0-port0"),
            "ttyUSB0",
        )]);
        let found = scan(dev.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity, IdentityKind::ById);
        assert!(found[0].canonical.contains("usb-FTDI_TTL232R_FT1234"));
        assert_eq!(found[0].tty.as_deref(), Some("ttyUSB0"));
        assert!(found[0].by_path.is_some());
    }

    #[test]
    fn serial_less_clones_in_adjacent_ports_stay_distinct_via_topology() {
        // The FTDI-clone problem: neither adapter appears under by-id.
        let dev = fake_dev(&[
            ("", Some("pci-0000:00:14.0-usb-0:1.1:1.0-port0"), "ttyUSB0"),
            ("", Some("pci-0000:00:14.0-usb-0:1.2:1.0-port0"), "ttyUSB1"),
        ]);
        let found = scan(dev.path()).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|d| d.identity == IdentityKind::Positional));
        assert_ne!(found[0].canonical, found[1].canonical);
    }

    #[test]
    fn positional_identity_follows_the_port_not_the_cable_and_says_so() {
        let (mut reg, cfg) = reg_cfg();
        let dev = fake_dev(&[("", Some("pci-0000:00:14.0-usb-0:1.1:1.0-port0"), "ttyUSB0")]);
        reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();

        let d = &reg.all_devices().unwrap()[0];
        assert_eq!(
            d.identity,
            IdentityKind::Positional,
            "the caveat has to be visible: moving the cable moves the name"
        );
    }

    #[test]
    fn re_enumeration_with_a_new_ttyusbn_keeps_the_same_device() {
        let (mut reg, cfg) = reg_cfg();
        let a = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
        reconcile(&mut reg, &cfg, &scan(a.path()).unwrap(), 1).unwrap();
        let id = reg.all_devices().unwrap()[0].id;
        reg.set_nickname(id, "rb3-ap").unwrap();
        let port = reg.all_devices().unwrap()[0].ser2net_port;

        // Same adapter, new tty number after a replug.
        let b = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB7")]);
        let mut found = scan(b.path()).unwrap();
        // The by-id path is what persists; rewrite it to the first tree's form.
        found[0].canonical = reg.all_devices().unwrap()[0].canonical.clone();
        found[0].tty = Some("ttyUSB7".into());
        reconcile(&mut reg, &cfg, &found, 2).unwrap();

        let all = reg.all_devices().unwrap();
        assert_eq!(all.len(), 1, "not a second device");
        assert_eq!(all[0].nickname.as_deref(), Some("rb3-ap"));
        assert_eq!(all[0].ser2net_port, port, "port assignment is stable");
        assert_eq!(all[0].tty.as_deref(), Some("ttyUSB7"));
    }

    #[test]
    fn an_unplugged_device_is_marked_gone_not_deleted() {
        let (mut reg, cfg) = reg_cfg();
        let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
        let found = scan(dev.path()).unwrap();
        reconcile(&mut reg, &cfg, &found, 1).unwrap();
        let id = reg.all_devices().unwrap()[0].id;
        reg.set_nickname(id, "bench-left").unwrap();

        let r = reconcile(&mut reg, &cfg, &[], 2).unwrap();
        assert_eq!(r.gone.len(), 1);
        let d = reg.device(id).unwrap();
        assert_eq!(d.state, "gone");
        assert_eq!(
            d.nickname.as_deref(),
            Some("bench-left"),
            "history and identity outlive the cable"
        );

        // Plugging it back in is a return, not a new device.
        let r = reconcile(&mut reg, &cfg, &found, 3).unwrap();
        assert_eq!(r.returned, vec![found[0].canonical.clone()]);
        assert_eq!(reg.all_devices().unwrap().len(), 1);
    }

    /// An excluded controller is still a cable.
    ///
    /// The sweep skipped every `ignored` row, so a Bantam that had been
    /// unplugged for five and a half days sat at `state=ignored` and every
    /// surface drew it as the rig's control panel.
    #[test]
    fn an_unplugged_controller_is_marked_gone_like_any_other_cable() {
        let (mut reg, cfg) = reg_cfg();
        // `Config::default()` carries the real bantam profile, so this goes
        // through the actual exclusion path rather than a hand-set flag.
        let dev = fake_dev(&[(
            "usb-Microchip_Technology_Inc._Bantam_IQ10RRDXXBANTAMTDC000034VG8-if00",
            Some("pci-0000:00:14.0-usb-0:3.2.1:1.0-port0"),
            "ttyACM0",
        )]);
        let found = scan(dev.path()).unwrap();
        reconcile(&mut reg, &cfg, &found, 1).unwrap();

        let row = &reg.all_devices().unwrap()[0];
        assert!(
            row.ignored,
            "a Bantam is excluded from discovery by profile"
        );
        assert_eq!(row.state, "ignored", "and says so while it is plugged in");
        let id = row.id;

        // Cable out.
        let r = reconcile(&mut reg, &cfg, &[], 2).unwrap();
        let d = reg.device(id).unwrap();
        assert_eq!(
            d.state, "gone",
            "an unplugged controller must be recorded as gone; leaving it at \
             `ignored` is what made absent Bantams render as a control panel"
        );
        assert!(
            r.gone.contains(&d.canonical),
            "and it must be REPORTED gone, not only stored that way"
        );
        assert!(
            d.ignored,
            "while staying excluded: `ignored` is its own column and must survive"
        );
        assert!(!d.is_present());

        // Cable back in: excluded again, and present again.
        reconcile(&mut reg, &cfg, &found, 3).unwrap();
        let d = reg.device(id).unwrap();
        assert_eq!(d.state, "ignored");
        assert!(d.is_present(), "a returning controller is present again");
    }

    /// The exception to the rule above, which is itself worth a gate.
    ///
    /// `snapshot_dmesg` mints `<port>#dmesg` so it can mine without taking the
    /// live console's writer lock. It is an internal handle, not a cable, so
    /// "gone" would be a claim about hardware for a row that describes none.
    #[test]
    fn an_internal_sibling_store_is_never_called_gone() {
        let (mut reg, cfg) = reg_cfg();
        let sibling = "/dev/serial/by-id/usb-FTDI_FT1-if00-port0#dmesg";
        reg.upsert_device(sibling, None, IdentityKind::ById, None, 1)
            .unwrap();
        let id = reg.device_by_canonical(sibling).unwrap().unwrap().id;

        reconcile(&mut reg, &cfg, &[], 2).unwrap();

        assert_ne!(
            reg.device(id).unwrap().state,
            "gone",
            "a sibling store was never plugged in, so it cannot be unplugged"
        );
    }

    #[test]
    fn rapid_replug_does_not_multiply_devices() {
        let (mut reg, cfg) = reg_cfg();
        let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
        let found = scan(dev.path()).unwrap();
        for i in 0..20 {
            reconcile(&mut reg, &cfg, if i % 2 == 0 { &found } else { &[] }, i).unwrap();
        }
        assert_eq!(reg.all_devices().unwrap().len(), 1);
    }

    #[test]
    fn an_excluded_device_is_listed_ignored_and_never_opened() {
        let (mut reg, mut cfg) = reg_cfg();
        cfg.discovery.exclude = vec!["*Quectel*".into()];
        let dev = fake_dev(&[
            ("usb-FTDI_FT1-if00-port0", None, "ttyUSB0"),
            ("usb-Quectel_RM520N_Modem-if02", None, "ttyUSB1"),
        ]);
        let r = reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.ignored.len(), 1);

        let modem = reg
            .all_devices()
            .unwrap()
            .into_iter()
            .find(|d| d.canonical.contains("Quectel"))
            .unwrap();
        assert!(modem.ignored);
        assert!(
            modem.ser2net_port.is_none(),
            "an ignored device gets no endpoint, so nothing can open it"
        );
        assert!(!ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("Quectel"));
    }

    #[test]
    fn a_missing_dev_tree_is_not_an_error_just_an_empty_scan() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(dir.path()).unwrap().is_empty());
    }

    // -------------------------------------------------- ser2net-gen suite ----

    #[test]
    fn config_generation_handles_zero_one_and_many_devices() {
        let (mut reg, cfg) = reg_cfg();
        assert!(!ser2net_config(&[], &cfg).contains("connection:"));

        for n in 0..32 {
            let d = reg
                .upsert_device(
                    &format!("/dev/serial/by-id/usb-FTDI_FT{n:02}-if00-port0"),
                    None,
                    IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
        }
        let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
        // One `<port>:raw:0:<device>:<options>` line per device (ser2net 3.x).
        assert_eq!(text.matches("connection: &").count(), 32);
        for p in 5001..5033 {
            assert!(text.contains(&format!(",{p}\n")), "port {p} missing");
        }
    }

    #[test]
    fn generated_connectors_open_the_by_id_path_never_a_tty_number() {
        let (mut reg, cfg) = reg_cfg();
        let d = reg
            .upsert_device(
                "/dev/serial/by-id/usb-FTDI_FT1-if00-port0",
                Some("/dev/serial/by-path/pci-0:1.1"),
                IdentityKind::ById,
                Some("ttyUSB3"),
                1,
            )
            .unwrap();
        reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
        let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
        assert!(
            text.contains("serialdev,/dev/serial/by-id/usb-FTDI_FT1-if00-port0"),
            "{text}"
        );
        assert!(
            !text.contains("ttyUSB3"),
            "a renumbering must not be able to repoint an endpoint"
        );
    }

    #[test]
    fn the_generated_config_reflects_per_device_line_settings() {
        let (mut reg, cfg) = reg_cfg();
        let d = reg
            .upsert_device("/dev/serial/by-id/usb-a", None, IdentityKind::ById, None, 1)
            .unwrap();
        reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
        reg.set_line(
            d.id,
            &LineConfig {
                baud: 921_600,
                data_bits: 7,
                parity: crate::config::Parity::Even,
                ..Default::default()
            },
        )
        .unwrap();
        let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
        assert!(text.contains("921600e71"), "{text}");
    }

    #[test]
    fn port_numbers_are_stable_across_restarts() {
        let (mut reg, cfg) = reg_cfg();
        let mut ports = Vec::new();
        for n in 0..4 {
            let d = reg
                .upsert_device(
                    &format!("/dev/serial/by-id/usb-{n}"),
                    None,
                    IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            ports.push(reg.assign_port(d.id, cfg.ser2net.base_port).unwrap());
        }
        // A "restart" re-runs assignment; nothing may move.
        for (n, want) in ports.iter().enumerate() {
            let d = reg
                .device_by_canonical(&format!("/dev/serial/by-id/usb-{n}"))
                .unwrap()
                .unwrap();
            assert_eq!(reg.assign_port(d.id, cfg.ser2net.base_port).unwrap(), *want);
        }
    }

    #[test]
    fn a_malformed_nickname_cannot_produce_broken_yaml() {
        let (mut reg, cfg) = reg_cfg();
        let d = reg
            .upsert_device("/dev/serial/by-id/usb-a", None, IdentityKind::ById, None, 1)
            .unwrap();
        reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
        // A nickname with punctuation must not be able to break the config.
        reg.set_nickname(d.id, "rack2.bench-left").unwrap();
        let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
        // Dots and hyphens are not valid in a YAML anchor, and an invalid
        // anchor makes ser2net reject the whole config.
        let anchor = text
            .lines()
            .find_map(|l| l.strip_prefix("connection: &"))
            .expect("an anchor");
        assert!(
            anchor
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "anchor {anchor:?} is not a bare YAML key"
        );
    }

    #[test]
    fn a_device_removed_while_a_consumer_is_attached_drops_out_of_the_config() {
        let (mut reg, cfg) = reg_cfg();
        let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
        reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();
        assert!(ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("connection: &"));

        reconcile(&mut reg, &cfg, &[], 2).unwrap();
        let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
        assert!(
            !text.contains("connection: &"),
            "a gone device must not keep an endpoint open"
        );
    }

    #[test]
    fn reconcile_reports_whether_anything_changed_so_reloads_are_not_gratuitous() {
        let (mut reg, cfg) = reg_cfg();
        let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
        let found = scan(dev.path()).unwrap();
        assert!(reconcile(&mut reg, &cfg, &found, 1).unwrap().changed());
        let second = reconcile(&mut reg, &cfg, &found, 2).unwrap();
        assert!(!second.changed(), "a no-op scan must not trigger a SIGHUP");
        assert_eq!(second.unchanged, 1);
    }
}
