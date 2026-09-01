//! Multi-console targets (§15.8).
//!
//! One DUT is often several UARTs — AP plus EC, BMC plus host, secure and normal
//! world on separate ports. Grouping them under one logical target buys three
//! things the individual devices cannot give:
//!
//! * a power event on one member opens epochs on **all** of them, so the
//!   consoles stay comparable;
//! * `get_context` can interleave every member by host-receipt time, which is
//!   what answers "what did the EC see when the AP panicked";
//! * a target is addressable as a unit without pretending it is one device.
//!
//! Interleaving uses **host** receipt time, never target-side timestamps: two
//! boards do not share a clock, and a printk stamp from one is not comparable to
//! a `[  12.3]` from the other (§14.4).

use crate::error::{ErrorCode, Result, ToolError};
use crate::store::{DeviceRow, DeviceStore, Registry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// A logical target: several consoles on one device under test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub members: Vec<String>,
}

/// Every target the registry knows about.
/// The target a device belongs to: its configured name, or its USB topology.
///
/// DERIVED WHEN UNSET, and that is what makes the multi-console tools usable at
/// all. A sweep found `list_targets` empty on a rig with three multi-console
/// boards, so `target_context` and `target_mark` -- the tools for "show me every
/// console of this board around this moment" -- were dead exactly where they
/// matter (IQ10 AP+SAIL, NordAU AP+safety-monitor+4 more). Nobody had written
/// the config, and nobody was going to.
///
/// The consoles of one board already share a USB hub, which is the same signal
/// that binds a controller to the right board. So a board is a target by
/// default, and an explicit `target` in config still wins for a bench wired in
/// some way topology cannot see.
fn target_of(d: &DeviceRow) -> Option<String> {
    if let Some(t) = &d.target {
        return Some(t.clone());
    }
    crate::config::topology_group(d.by_path.as_deref())
}

pub fn list(reg: &Registry) -> Result<Vec<Target>> {
    let mut by_name: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for d in reg.all_devices()? {
        if let Some(t) = target_of(&d) {
            by_name
                .entry(t)
                .or_default()
                .push(d.display_name().to_string());
        }
    }
    Ok(by_name
        .into_iter()
        .map(|(name, members)| Target { name, members })
        .collect())
}

/// The name of the target a selector means, however it was punctuated.
///
/// A DEVICE selector already tolerates this -- `unoq` finds `uno-q` -- and a
/// target selector did not, so the same name worked in `device:` and failed in
/// `target:` on the same board. Reported from a live session, after the device
/// side had been fixed: half a fix reads as no fix.
///
/// Exact first, always. The loose pass only runs when nothing matched exactly,
/// and an ambiguous spelling is an error naming the candidates rather than a
/// guess about which board to power-cycle.
pub fn resolve_name(reg: &Registry, sel: &str) -> Result<String> {
    let names: Vec<String> = list(reg)?.into_iter().map(|t| t.name).collect();
    if names.iter().any(|n| n == sel) {
        return Ok(sel.to_string());
    }
    let squash = |t: &str| -> String {
        t.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let want = squash(sel);
    if want.is_empty() {
        return Ok(sel.to_string());
    }
    let hits: Vec<String> = names.into_iter().filter(|n| squash(n) == want).collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
        0 => Ok(sel.to_string()),
        _ => Err(ToolError::new(
            ErrorCode::AmbiguousDevice,
            format!("{sel:?} matches more than one target: {}", hits.join(", ")),
        )
        .with_hint("name the target exactly")),
    }
}

/// The devices belonging to a target, in a stable order.
pub fn members(reg: &Registry, target: &str) -> Result<Vec<DeviceRow>> {
    let target = &resolve_name(reg, target)?;
    let mut out: Vec<DeviceRow> = reg
        .all_devices()?
        .into_iter()
        .filter(|d| target_of(d).as_deref() == Some(target))
        .collect();
    if out.is_empty() {
        return Err(ToolError::new(
            ErrorCode::UnknownDevice,
            format!("no devices belong to target {target:?}"),
        )
        .with_hint("assign one with tag_device or the `target` field in conminer.toml"));
    }
    out.sort_by(|a, b| a.canonical.cmp(&b.canonical));
    Ok(out)
}

/// Give a target a name of its own, or clear it back to the topology.
///
/// A target is derived from the USB hub its consoles share, so it arrives called
/// `2.1` -- exact, stable, and impossible to remember on a bench with four
/// boards. The name is stored on the MEMBERS (their `target` column), which is
/// the same field a hand-written `conminer.toml` uses, so a named target and a
/// configured one are the same thing to every tool that reads one.
///
/// Naming does not change membership: whoever was in the group stays in it,
/// controller included. Clearing (`None`) lets topology speak again rather than
/// leaving the board with no target at all.
pub fn rename(reg: &mut Registry, target: &str, name: Option<&str>) -> Result<Vec<String>> {
    if let Some(n) = name {
        let n = n.trim();
        if n.is_empty() {
            return Err(ToolError::invalid_arg(
                "empty target name: pass no name at all to fall back to the USB topology",
            ));
        }
        // `peer:<node>/<target>` is how an imported target is addressed, so a
        // local name containing either would be indistinguishable from somebody
        // else's board.
        if n.contains('/') || n.starts_with("peer:") {
            return Err(ToolError::invalid_arg(format!(
                "target name {n:?} must not contain '/' or start with \"peer:\": that shape is                  reserved for a peer's target"
            )));
        }
        // A name that already belongs to another group would silently merge two
        // boards into one target -- and a power event on one would then open
        // epochs on the other's consoles.
        if let Ok(existing) = members(reg, n) {
            let clash: Vec<String> = existing
                .iter()
                .filter(|d| target_of(d).as_deref() != Some(target))
                .map(|d| d.display_name().to_string())
                .collect();
            if !clash.is_empty() {
                return Err(ToolError::invalid_arg(format!(
                    "target {n:?} already names another group ({}); one name, one board",
                    clash.join(", ")
                )));
            }
        }
    }
    let group = members(reg, target)?;
    let mut named = Vec::new();
    for d in &group {
        reg.set_target(d.id, name)?;
        named.push(d.display_name().to_string());
    }
    Ok(named)
}

/// The members that are actually consoles, plus the names of the ones skipped.
///
/// A target's membership includes its CONTROLLER -- the Bantam or bughopper that
/// drives power -- because that is how conminer knows which controller belongs
/// to which board. But a controller is marked `ignored`: nothing captures it,
/// it has no epochs and no lines. Treating it as a console meant `target_mark`
/// demanded a lease on a pseudo-device that an operator has no reason to lease
/// and gains nothing by leasing, then tried to open an epoch on a store that
/// never receives a byte. Leasing every console of a board and still being told
/// `LEASE_REQUIRED` is how that surfaced.
pub fn console_members(reg: &Registry, target: &str) -> Result<(Vec<DeviceRow>, Vec<String>)> {
    let all = members(reg, target)?;
    let (consoles, skipped): (Vec<_>, Vec<_>) = all.into_iter().partition(|d| !d.ignored);
    if consoles.is_empty() {
        return Err(ToolError::new(
            ErrorCode::UnknownDevice,
            format!(
                "target {target:?} has no capturing consoles: its {} member(s) are all ignored \
                 (controllers or excluded ports)",
                skipped.len()
            ),
        )
        .with_hint("assign a console to this target with tag_device"));
    }
    Ok((
        consoles,
        skipped
            .into_iter()
            .map(|d| d.display_name().to_string())
            .collect(),
    ))
}

/// One line from one member, ready to be merged.
#[derive(Debug, Clone, Serialize)]
pub struct TargetLine {
    pub device: String,
    pub line_id: i64,
    pub ts_wall: i64,
    pub boot_id: Option<i64>,
    pub text: String,
}

/// Interleave every member's console around a moment in host time.
///
/// `around_ts` is a host wall timestamp — typically taken from the record you
/// are investigating — and `window_ms` is how far either side to look.
pub fn interleave(
    reg: &Registry,
    data_dir: &Path,
    target: &str,
    around_ts: i64,
    window_ms: i64,
    per_device_cap: usize,
) -> Result<Vec<TargetLine>> {
    let mut merged = Vec::new();
    for d in members(reg, target)? {
        let store = DeviceStore::open(&data_dir.join(&d.db_file), &d.canonical, false)?;
        for l in
            store.lines_between(around_ts - window_ms, around_ts + window_ms, per_device_cap)?
        {
            merged.push(TargetLine {
                device: d.display_name().to_string(),
                line_id: l.id,
                ts_wall: l.ts_wall,
                boot_id: l.boot_id,
                text: l.lossy(),
            });
        }
    }
    // Host receipt time is the only clock the members share.
    merged.sort_by_key(|l| (l.ts_wall, l.device.clone(), l.line_id));
    Ok(merged)
}

/// Open an epoch on every member of a target.
///
/// A power event affects the whole DUT, so leaving the EC console on its old
/// epoch would make the two consoles incomparable exactly when correlating them
/// matters most.
pub fn open_epoch_on_all(
    reg: &Registry,
    data_dir: &Path,
    target: &str,
    opened_by: &str,
    label: Option<&str>,
    now: i64,
) -> Result<Value> {
    let mut opened = Vec::new();
    let (consoles, skipped) = console_members(reg, target)?;
    for d in consoles {
        let mut store = DeviceStore::open(&data_dir.join(&d.db_file), &d.canonical, false)?;
        let session = store.latest_session()?.map(|s| s.id);
        let boot = store.open_boot(opened_by, label, now, session)?;
        opened.push(json!({
            "device": d.display_name(),
            "boot_id": boot.id,
            "boot_seq": boot.seq,
            "cursor": store.head_cursor().encode(),
        }));
    }
    // Name what was left out rather than silently narrowing the target: a
    // caller that asked for "every console on this board" deserves to see that
    // the controller was not one of them.
    Ok(json!({"target": target, "opened": opened, "skipped_not_consoles": skipped}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::IdentityKind;

    fn rig() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::open(dir.path()).unwrap();
        (dir, reg)
    }

    fn add(reg: &mut Registry, canonical: &str, nickname: &str, target: Option<&str>) -> DeviceRow {
        let d = reg
            .upsert_device(canonical, None, IdentityKind::ById, None, 1)
            .unwrap();
        reg.set_nickname(d.id, nickname).unwrap();
        reg.set_target(d.id, target).unwrap();
        reg.device(d.id).unwrap()
    }

    #[test]
    fn devices_group_into_targets_and_ungrouped_ones_are_left_alone() {
        let (_d, mut reg) = rig();
        add(&mut reg, "usb-ap", "rb3-ap", Some("rb3"));
        add(&mut reg, "usb-ec", "rb3-ec", Some("rb3"));
        add(&mut reg, "usb-other", "bench-left", None);

        let targets = list(&reg).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "rb3");
        assert_eq!(targets[0].members.len(), 2);
        assert_eq!(members(&reg, "rb3").unwrap().len(), 2);
    }

    #[test]
    fn an_unknown_target_is_a_structured_error_with_a_hint() {
        let (_d, reg) = rig();
        let err = members(&reg, "nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownDevice);
        assert!(err.hint.contains("target"));
    }

    #[test]
    fn a_power_event_opens_an_epoch_on_every_member() {
        let (dir, mut reg) = rig();
        add(&mut reg, "usb-ap", "rb3-ap", Some("rb3"));
        add(&mut reg, "usb-ec", "rb3-ec", Some("rb3"));

        let out =
            open_epoch_on_all(&reg, dir.path(), "rb3", "power", Some("cycle"), 1_000).unwrap();
        let opened = out["opened"].as_array().unwrap();
        assert_eq!(
            opened.len(),
            2,
            "the whole DUT was power-cycled, not half of it"
        );
        for o in opened {
            assert!(o["boot_id"].as_i64().unwrap() > 0);
            assert!(o["cursor"].as_str().unwrap().contains(':'));
        }
    }

    #[test]
    fn consoles_interleave_by_host_receipt_time_not_by_target_timestamps() {
        let (dir, mut reg) = rig();
        let ap = add(&mut reg, "usb-ap", "rb3-ap", Some("rb3"));
        let ec = add(&mut reg, "usb-ec", "rb3-ec", Some("rb3"));

        // Two boards, two unrelated target clocks, one host clock.
        for (dev, lines) in [
            (
                &ap,
                vec![(1_000, "[ 9999.0] AP: Kernel panic - not syncing")],
            ),
            (
                &ec,
                vec![(999, "[    1.0] EC: Watchdog! reset cause: AP hang")],
            ),
        ] {
            let mut store =
                DeviceStore::open(&dir.path().join(&dev.db_file), &dev.canonical, false).unwrap();
            let sid = store
                .begin_session(crate::store::SessionSource::Live, 0, None, None, None)
                .unwrap();
            for (ts, text) in lines {
                let bytes = text.as_bytes();
                store
                    .append_lines(
                        sid,
                        None,
                        &[crate::store::PendingLine {
                            bytes,
                            terminator: crate::linesplit::Terminator::Lf,
                            truncated: false,
                            continuation: false,
                            ts_mono: ts * 1_000_000,
                            ts_wall: ts,
                            stage_id: None,
                        }],
                    )
                    .unwrap();
            }
        }

        let merged = interleave(&reg, dir.path(), "rb3", 1_000, 500, 100).unwrap();
        assert_eq!(merged.len(), 2);
        // Named by its port, like everywhere else: an interleaved view of six
        // consoles is exactly where "which one said this?" must be unambiguous.
        assert_eq!(
            merged[0].device, "usb-ec",
            "the EC line arrived first in *host* time, even though its own \
             timestamp is smaller by four orders of magnitude"
        );
        assert!(merged[0].text.contains("Watchdog"));
        assert!(merged[1].text.contains("Kernel panic"));
    }

    #[test]
    fn the_interleave_window_excludes_what_is_outside_it() {
        let (dir, mut reg) = rig();
        let ap = add(&mut reg, "usb-ap", "rb3-ap", Some("rb3"));
        let mut store =
            DeviceStore::open(&dir.path().join(&ap.db_file), &ap.canonical, false).unwrap();
        let sid = store
            .begin_session(crate::store::SessionSource::Live, 0, None, None, None)
            .unwrap();
        for ts in [100i64, 1_000, 50_000] {
            store
                .append_lines(
                    sid,
                    None,
                    &[crate::store::PendingLine {
                        bytes: b"line",
                        terminator: crate::linesplit::Terminator::Lf,
                        truncated: false,
                        continuation: false,
                        ts_mono: ts,
                        ts_wall: ts,
                        stage_id: None,
                    }],
                )
                .unwrap();
        }
        let merged = interleave(&reg, dir.path(), "rb3", 1_000, 500, 100).unwrap();
        assert_eq!(merged.len(), 1, "only the line inside ±500 ms");
        assert_eq!(merged[0].ts_wall, 1_000);
    }
}
