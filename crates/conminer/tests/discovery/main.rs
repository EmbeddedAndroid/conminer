//! Suite `discovery` (§13) — discoveryd, and §3.1 device identity at scale.
//!
//! Edge cases: hotplug add/remove · rapid replug · the same adapter
//! re-enumerating with a new ttyUSBn but the same by-id · two identical adapters
//! (serial-less FTDI clones → positional fallback) · udev unavailable → poll
//! fallback · permission-denied device · symlink churn during a scan.
//!
//! Plus §3.1: nickname survives replug, selector precedence, ambiguous substring
//! → candidates, tag queries, group selectors, nickname collisions, and observed
//! identity.

use conminer_core::config::Config;
use conminer_core::discovery::{reconcile, scan_with, ser2net_config};
use conminer_core::store::{IdentityKind, Registry};
use conminer_core::ErrorCode;
use std::collections::BTreeMap;
use std::path::Path;

/// Build a fake `/dev` tree. Each entry is (by-id name or "", by-path name, tty).
fn fake_dev(entries: &[(&str, Option<&str>, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    rewrite(dir.path(), entries);
    dir
}

fn rewrite(root: &Path, entries: &[(&str, Option<&str>, &str)]) {
    let _ = std::fs::remove_dir_all(root.join("serial"));
    std::fs::create_dir_all(root.join("serial/by-id")).unwrap();
    std::fs::create_dir_all(root.join("serial/by-path")).unwrap();
    for (by_id, by_path, tty) in entries {
        let target = root.join(tty);
        if !target.exists() {
            std::fs::write(&target, b"").unwrap();
        }
        if !by_id.is_empty() {
            std::os::unix::fs::symlink(&target, root.join("serial/by-id").join(by_id)).unwrap();
        }
        if let Some(p) = by_path {
            std::os::unix::fs::symlink(&target, root.join("serial/by-path").join(p)).unwrap();
        }
    }
}

/// Scan the fake `/dev` WITHOUT consulting the host's real `/sys`.
///
/// `scan()` reads `/sys/class/tty/<name>` to learn a tty's USB ids, and the
/// fixtures name their ttys `ttyUSB0`, `ttyUSB1`... which are real device names
/// on any bench host. On bravo, whose `ttyUSB0` is a live `05c6:9008` QDL gadget,
/// every fixture device was classified "a download gadget, not a console" and
/// skipped: the suite passed or failed depending on what was plugged into the
/// machine. `scan_with` is the seam that already existed for this; using it
/// makes the result depend on the fixture alone.
fn scan(root: &Path) -> conminer_core::Result<Vec<conminer_core::discovery::Discovered>> {
    scan_with(root, &|_tty| None)
}

fn rig() -> (Registry, Config) {
    (Registry::open_memory().unwrap(), Config::default())
}

// -------------------------------------------------------------- hotplug -----

#[test]
fn hotplug_add_and_remove() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);

    let added = reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();
    assert_eq!(added.added.len(), 1);
    assert!(added.changed());

    rewrite(dev.path(), &[]);
    let removed = reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 2).unwrap();
    assert_eq!(removed.gone.len(), 1);
    assert_eq!(
        reg.all_devices().unwrap()[0].state,
        "gone",
        "marked, never deleted"
    );
}

#[test]
fn rapid_replug_never_multiplies_the_device() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
    let present = scan(dev.path()).unwrap();
    for i in 0..40 {
        let found = if i % 2 == 0 {
            present.clone()
        } else {
            Vec::new()
        };
        reconcile(&mut reg, &cfg, &found, i).unwrap();
    }
    assert_eq!(reg.all_devices().unwrap().len(), 1);
}

#[test]
fn the_same_adapter_re_enumerating_with_a_new_tty_is_the_same_device() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
    reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();
    let id = reg.all_devices().unwrap()[0].id;
    reg.set_nickname(id, "rb3-ap").unwrap();
    let port = reg.device(id).unwrap().ser2net_port;

    rewrite(dev.path(), &[("usb-FTDI_FT1-if00-port0", None, "ttyUSB9")]);
    reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 2).unwrap();

    let all = reg.all_devices().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].nickname.as_deref(), Some("rb3-ap"));
    assert_eq!(all[0].tty.as_deref(), Some("ttyUSB9"));
    assert_eq!(all[0].ser2net_port, port, "the endpoint does not move");
}

#[test]
fn two_serial_less_clones_stay_distinct_and_are_flagged_positional() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[
        ("", Some("pci-0000:00:14.0-usb-0:1.1:1.0-port0"), "ttyUSB0"),
        ("", Some("pci-0000:00:14.0-usb-0:1.2:1.0-port0"), "ttyUSB1"),
    ]);
    reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();

    let all = reg.all_devices().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.identity == IdentityKind::Positional));
    assert_ne!(all[0].canonical, all[1].canonical);
    assert_ne!(all[0].ser2net_port, all[1].ser2net_port);
}

#[test]
fn a_permission_denied_device_does_not_abort_the_scan() {
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
    // A dangling symlink is what an unreadable or vanished node looks like.
    std::os::unix::fs::symlink(
        dev.path().join("does-not-exist"),
        dev.path().join("serial/by-id/usb-BROKEN-if00"),
    )
    .unwrap();

    let found = scan(dev.path()).unwrap();
    assert_eq!(found.len(), 2, "a broken entry is reported, not fatal");
    assert!(found.iter().any(|d| d.canonical.contains("FT1")));
}

#[test]
fn symlink_churn_during_a_scan_does_not_panic() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-a-if00", None, "ttyUSB0")]);
    for i in 0..10 {
        rewrite(
            dev.path(),
            &[
                ("usb-a-if00", None, "ttyUSB0"),
                ("usb-b-if00", None, "ttyUSB1"),
            ][..if i % 2 == 0 { 1 } else { 2 }],
        );
        let found = scan(dev.path()).unwrap();
        reconcile(&mut reg, &cfg, &found, i).unwrap();
    }
    assert!(reg.all_devices().unwrap().len() <= 2);
}

#[test]
fn the_poll_fallback_is_the_documented_default_interval() {
    // udev netlink availability varies by host distro; polling works everywhere,
    // and 1 Hz meets the "queryable within about a second" contract.
    let c = Config::default();
    assert_eq!(c.discovery.poll_fallback_hz, 1);
    assert_eq!(c.discovery.hotplug_debounce_ms, 500);
}

// ------------------------------------------------------- identity at scale ---

#[test]
fn selector_precedence_puts_an_exact_nickname_ahead_of_a_substring() {
    let (mut reg, _cfg) = rig();
    let a = reg
        .upsert_device("usb-FTDI_AAAA-if00", None, IdentityKind::ById, None, 1)
        .unwrap();
    reg.upsert_device("usb-FTDI_BBBB-if00", None, IdentityKind::ById, None, 1)
        .unwrap();
    // A nickname deliberately colliding with the other device's canonical id.
    reg.set_nickname(a.id, "BBBB").unwrap();
    assert_eq!(reg.resolve("BBBB").unwrap().id, a.id);
}

#[test]
fn an_ambiguous_selector_returns_candidates_rather_than_guessing() {
    let (mut reg, _cfg) = rig();
    for n in ["usb-FTDI_A-if00", "usb-FTDI_B-if00", "usb-FTDI_C-if00"] {
        reg.upsert_device(n, None, IdentityKind::ById, None, 1)
            .unwrap();
    }
    let err = reg.resolve("FTDI").unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousDevice);
    let candidates = err.detail.unwrap();
    assert_eq!(candidates["candidates"].as_array().unwrap().len(), 3);
    // The candidate list is what lets an agent disambiguate in one more step.
    assert!(candidates["candidates"][0]["canonical"].is_string());
}

#[test]
fn tag_queries_resolve_including_multi_tag_and() {
    let (mut reg, _cfg) = rig();
    let a = reg
        .upsert_device("usb-a", None, IdentityKind::ById, None, 1)
        .unwrap();
    let b = reg
        .upsert_device("usb-b", None, IdentityKind::ById, None, 1)
        .unwrap();
    reg.set_tags(
        a.id,
        &BTreeMap::from([
            ("role".into(), "ap-console".into()),
            ("rack".into(), "r2".into()),
        ]),
    )
    .unwrap();
    reg.set_tags(
        b.id,
        &BTreeMap::from([
            ("role".into(), "ap-console".into()),
            ("rack".into(), "r3".into()),
        ]),
    )
    .unwrap();

    assert_eq!(reg.resolve_all("tag:role=ap-console").unwrap().len(), 2);
    assert_eq!(
        reg.resolve("tag:role=ap-console AND tag:rack=r3")
            .unwrap()
            .id,
        b.id
    );
}

#[test]
fn a_group_selector_is_refused_by_single_device_tools() {
    let (mut reg, _cfg) = rig();
    for n in ["usb-a", "usb-b"] {
        let d = reg
            .upsert_device(n, None, IdentityKind::ById, None, 1)
            .unwrap();
        reg.set_tags(d.id, &BTreeMap::from([("role".into(), "console".into())]))
            .unwrap();
    }
    assert_eq!(
        reg.resolve("tag:role=console").unwrap_err().code,
        ErrorCode::GroupSelectorNotAllowed
    );
    assert_eq!(reg.resolve_group("tag:role=console").unwrap().len(), 2);
}

#[test]
fn a_nickname_collision_is_a_structured_error() {
    let (mut reg, _cfg) = rig();
    let a = reg
        .upsert_device("usb-a", None, IdentityKind::ById, None, 1)
        .unwrap();
    let b = reg
        .upsert_device("usb-b", None, IdentityKind::ById, None, 1)
        .unwrap();
    reg.set_nickname(a.id, "bench-left").unwrap();
    let err = reg.set_nickname(b.id, "bench-left").unwrap_err();
    assert_eq!(err.code, ErrorCode::NicknameTaken);
    assert_eq!(err.detail.unwrap()["held_by_device_id"], a.id);
}

#[test]
fn observed_identity_accumulates_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = "usb-observed";
    {
        let mut reg = Registry::open(dir.path()).unwrap();
        let d = reg
            .upsert_device(canonical, None, IdentityKind::ById, None, 1)
            .unwrap();
        reg.merge_observed(d.id, &serde_json::json!({"kernel": "Linux 6.12.9"}))
            .unwrap();
        reg.merge_observed(d.id, &serde_json::json!({"uboot": "U-Boot 2026.01"}))
            .unwrap();
    }
    let reg = Registry::open(dir.path()).unwrap();
    let d = reg.device_by_canonical(canonical).unwrap().unwrap();
    assert_eq!(d.observed["kernel"], "Linux 6.12.9");
    assert_eq!(d.observed["uboot"], "U-Boot 2026.01");
}

// -------------------------------------------------------- ser2net-gen suite --

#[test]
fn config_generation_covers_zero_one_and_thirty_two_devices() {
    let (mut reg, cfg) = rig();
    assert!(!ser2net_config(&[], &cfg).contains("connection:"));

    for n in 0..32 {
        let d = reg
            .upsert_device(
                &format!("/dev/serial/by-id/usb-{n:02}"),
                None,
                IdentityKind::ById,
                None,
                1,
            )
            .unwrap();
        reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
    }
    let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
    assert_eq!(text.matches("connection: &").count(), 32);
    assert!(text.starts_with("# Generated by conminer"));
    assert!(text.contains("%YAML"));
}

#[test]
fn port_numbers_are_stable_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let mut ports = Vec::new();
    {
        let mut reg = Registry::open(dir.path()).unwrap();
        for n in 0..5 {
            let d = reg
                .upsert_device(&format!("usb-{n}"), None, IdentityKind::ById, None, 1)
                .unwrap();
            ports.push(reg.assign_port(d.id, cfg.ser2net.base_port).unwrap());
        }
    }
    let mut reg = Registry::open(dir.path()).unwrap();
    for (n, want) in ports.iter().enumerate() {
        let d = reg
            .device_by_canonical(&format!("usb-{n}"))
            .unwrap()
            .unwrap();
        assert_eq!(reg.assign_port(d.id, cfg.ser2net.base_port).unwrap(), *want);
    }
}

#[test]
fn a_device_removed_while_a_consumer_is_attached_leaves_the_config() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
    reconcile(&mut reg, &cfg, &scan(dev.path()).unwrap(), 1).unwrap();
    assert!(ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("connection: &"));

    reconcile(&mut reg, &cfg, &[], 2).unwrap();
    assert!(!ser2net_config(&reg.all_devices().unwrap(), &cfg).contains("connection: &"));
}

#[test]
fn a_malformed_name_cannot_produce_broken_yaml() {
    let (mut reg, cfg) = rig();
    // A port path is itself full of characters YAML will not take as a bare
    // key: slashes, dots, hyphens.
    let d = reg
        .upsert_device(
            "/dev/serial/by-id/usb-Vendor_Board.v2-if00-port0",
            None,
            IdentityKind::ById,
            None,
            1,
        )
        .unwrap();
    reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
    // A label with its own awkward characters must not reach the anchor AT ALL:
    // the anchor is derived from the port, so renaming a board cannot rewrite
    // the ser2net config and bounce every console on it.
    reg.set_nickname(d.id, "rack2.bench-left").unwrap();
    let text = ser2net_config(&reg.all_devices().unwrap(), &cfg);
    assert!(
        !text.contains("rack2"),
        "a label must not appear in the ser2net config at all: {text}"
    );
    assert!(text.contains("connection: &con_dev_serial_by_id"), "{text}");
    // Every anchor is a bare YAML key.
    for line in text.lines().filter(|l| l.starts_with("connection: &")) {
        let anchor = line.trim_start_matches("connection: &");
        assert!(
            anchor
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{anchor:?}"
        );
    }
}

#[test]
fn a_reload_is_only_triggered_when_something_actually_changed() {
    let (mut reg, cfg) = rig();
    let dev = fake_dev(&[("usb-FTDI_FT1-if00-port0", None, "ttyUSB0")]);
    let found = scan(dev.path()).unwrap();
    assert!(reconcile(&mut reg, &cfg, &found, 1).unwrap().changed());
    let second = reconcile(&mut reg, &cfg, &found, 2).unwrap();
    assert!(
        !second.changed(),
        "a no-op scan must not SIGHUP the whole lab"
    );
    assert_eq!(second.unchanged, 1);
}

/// Qualcomm's Sahara/EDL endpoint appears under /dev/serial/by-id the moment a
/// SoC enters EDL and vanishes when it leaves. Bridging it means every
/// power-cycle into EDL is a device-set change that churns the ser2net config
/// and disconnects live sessions on OTHER boards. It is a flashing endpoint,
/// not a console. Ported from the HIL.
#[test]
fn edl_and_diag_endpoints_are_not_treated_as_consoles() {
    let dev = fake_dev(&[
        (
            "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0",
            None,
            "ttyUSB2",
        ),
        (
            "usb-Qualcomm__Incorporated_QUSB_BULK_CDEF0123-if00",
            None,
            "ttyUSB9",
        ),
    ]);
    let found = scan(dev.path()).unwrap();
    let names: Vec<&str> = found.iter().map(|d| d.canonical.as_str()).collect();

    assert!(
        names.iter().any(|n| n.contains("if02-port0")),
        "the real console must still be discovered: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.to_ascii_lowercase().contains("qusb_bulk")),
        "the EDL endpoint must not be bridged: {names:?}"
    );
}

/// One physical device can be exposed under several by-id names. Listing it
/// twice makes the device set look like it changed whenever the duplicate comes
/// and goes -- the same churn that leaves ser2net with no accepters.
#[test]
fn one_tty_behind_two_by_id_names_is_discovered_once() {
    let dev = fake_dev(&[
        ("usb-Vendor_Board_AAAA-if00-port0", None, "ttyUSB0"),
        ("usb-Vendor_Board_other_name-if00-port0", None, "ttyUSB0"),
    ]);
    let found = scan(dev.path()).unwrap();
    assert_eq!(found.len(), 1, "same tty behind two names: {found:?}");
}
