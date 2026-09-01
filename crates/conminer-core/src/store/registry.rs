//! Global registry: device identity, nicknames, tags, leases (§3.1, §15.1).
//!
//! Plugging in N consoles must never create ambiguity about which one an agent is
//! talking to. Identity has four layers and every tool's `device` parameter is a
//! **selector** resolved against all of them:
//!
//! 1. **canonical id** — the `/dev/serial/by-id` path; survives replug, host
//!    reboot and ttyUSBn renumbering. Serial-less clone adapters fall back to
//!    USB topology position (`/dev/serial/by-path`), flagged `positional` so a
//!    human knows moving the cable moves the name.
//! 2. **nickname** — `rb3-ap`, `bench-left`; bound to the canonical id, unique.
//! 3. **tags** — `tag:role=ap-console AND tag:rack=r2`.
//! 4. **observed identity** — what the console has *shown* it is.
//!
//! Resolution never guesses: anything matching ≠ 1 device returns a structured
//! `AMBIGUOUS_DEVICE` carrying the candidates (so the agent can disambiguate in
//! one more step) or `UNKNOWN_DEVICE`.

use super::{migrate, open_sqlite, schema};
use crate::config::LineConfig;
use crate::error::{ErrorCode, Result, ToolError};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    /// Keyed on the adapter's USB serial number: stable across replug.
    ById,
    /// Keyed on USB topology position: "the cable in that physical port".
    Positional,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityKind::ById => "by_id",
            IdentityKind::Positional => "positional",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "positional" => IdentityKind::Positional,
            _ => IdentityKind::ById,
        }
    }
}

/// What a registry row actually is (§P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// A serial port on this host.
    Local,
    /// An ingested file, which has a store and mined data but no hardware.
    File,
    /// A derived store (`#dmesg`): mined content belonging to another row.
    Derived,
    /// A device owned by a peer. No local store, no local capture; every tool
    /// call for it is proxied to the owner.
    Remote,
}

impl DeviceKind {
    /// Classify a canonical id. One implementation, used at row load.
    pub fn of(canonical: &str) -> Self {
        if canonical.starts_with("peer:") {
            Self::Remote
        } else if canonical.starts_with("file:") {
            Self::File
        } else if canonical.contains('#') {
            Self::Derived
        } else {
            Self::Local
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote)
    }

    /// Does this row have a local store and local capture?
    pub fn is_locally_mined(&self) -> bool {
        matches!(self, Self::Local | Self::File | Self::Derived)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::File => "file",
            Self::Derived => "derived",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    pub id: i64,
    /// §P1. WHAT KIND OF ROW THIS IS, decided once at load.
    ///
    /// Five places used to re-derive this from string prefixes (`file:`, `#`,
    /// and now `peer:`), and each answered slightly differently -- which is how
    /// a `file:` log ended up borrowing a board's power hook. A row knows what
    /// it is.
    pub kind: DeviceKind,
    /// The peer that OWNS this device: its name (`alpha`) and the address it
    /// is reachable on. `None` means this node owns it.
    pub node: Option<String>,
    pub node_host: Option<String>,
    /// The canonical id the owner knows this device by. Only a proxied call
    /// needs it, and sending our prefixed id instead is the mistake this field
    /// exists to make impossible.
    pub remote_canonical: Option<String>,
    /// The ser2net port the OWNER serves this console on. Our own
    /// `ser2net_port` is the local re-export; this is what it relays to.
    pub remote_port: Option<u16>,
    /// The peer to FORWARD to when this device is not directly peered.
    ///
    /// `node` says who OWNS the board; this says who to hand the call to. They
    /// are the same thing only when the owner is one hop away, and telling them
    /// apart is the whole of transitive routing: a call addressed to a machine
    /// this node cannot reach goes nowhere, however correct the owner field is.
    pub via: Option<String>,
    /// How far the OWNER is: 1 = directly peered, 2 = one relay, and so on.
    pub hops: u8,
    /// What the OWNER reports about driving this board: its controller's name,
    /// the boot modes it can offer, whether a power hook resolves there.
    ///
    /// Not recomputed locally, and cannot be: the controller profiles match a
    /// by-id name against hardware plugged into somebody else's host.
    pub remote_controls: Option<serde_json::Value>,
    pub canonical: String,
    pub by_path: Option<String>,
    pub identity: IdentityKind,
    /// Informational only — no tool response uses a ttyUSBn name as an address.
    pub tty: Option<String>,
    pub nickname: Option<String>,
    pub pinned_profile: Option<String>,
    pub ser2net_port: Option<u16>,
    pub line: LineConfig,
    pub target: Option<String>,
    pub state: String,
    /// Whether CAPTURE is working, as minerd last observed it -- a different
    /// fact from `state`, which is presence and belongs to discovery.
    ///
    /// They shared a column and two processes wrote it, so each erased the
    /// other: a presence sweep could publish `discovered` over a live
    /// `streaming`, and a capture update could publish `listening` over a
    /// `gone`. Measured on the bench as `capture_state: not_listening` on a
    /// console that was capturing a boot at that moment. `None` means minerd
    /// has not said anything about this device yet.
    pub capture_state: Option<String>,
    pub ignored: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Latest BANNER_VERSION extractions: last U-Boot build tag, kernel version,
    /// Zephyr board name, EC image string (§3.1 layer 4).
    pub observed: serde_json::Value,
    pub tags: BTreeMap<String, String>,
    pub db_file: String,
}

impl DeviceRow {
    /// The name a human sees first.
    /// What this device IS: its physical port. Never a nickname.
    ///
    /// A nickname used to be substituted here, which meant naming a board
    /// erased which port it was -- on the dashboard, in every tool response, in
    /// every log line. "adp-ventuno" does not tell an operator which of four
    /// FTDI interfaces they are looking at, and two people can disagree about
    /// what it refers to; `/dev/serial/by-id/...-if02-port0` cannot be
    /// misunderstood. It also stopped a nickname from ever being wrong: the
    /// label can go stale when boards move, the port path cannot.
    ///
    /// Nicknames remain first-class as SELECTORS (you can still address this
    /// device as "adp-ventuno") and are shown as a label beside the port -- see
    /// [`Self::label`].
    pub fn display_name(&self) -> &str {
        &self.canonical
    }

    /// The operator's label for this port, if they set one.
    pub fn label(&self) -> Option<&str> {
        self.nickname.as_deref()
    }

    /// Is the cable still plugged in?
    ///
    /// `state == "gone"` is the ONLY record of a device having been unplugged;
    /// discovery writes it and nothing else does. Rows are never deleted, so
    /// every listing that means "what is on the bench" has to ask this
    /// question, and the ones that forgot to ask showed five-day-old hardware
    /// as though it were present.
    ///
    /// Deliberately NOT about `ignored`: an excluded controller is a cable like
    /// any other. `ignored` says conminer must not OPEN it, which is a separate
    /// fact living in a separate column.
    pub fn is_present(&self) -> bool {
        self.state != "gone"
    }
}

/// The devices a controller may be resolved against: plugged into THIS host,
/// right now.
///
/// Every caller that asks "which controller drives this console" needs the same
/// list, and two of them built it independently as `all_devices()` -- every row
/// ever recorded -- while naming the variable `present`. On alpha that bound a
/// live Nucleo's power buttons to a Bantam that had been unplugged for five and
/// a half days, and the same claim was then published to the peer. One helper so
/// a third caller cannot reintroduce it.
///
/// Two filters, each load-bearing:
///
///  * NOT GONE -- the point of the exercise.
///  * LOCAL ONLY -- a peer's row is not hardware on this host. Their canonical
///    ids carry the owner's device path (`peer:bravo//dev/...Bantam...`), which
///    matches a controller `match` glob just as happily as our own would, so
///    leaving them in lets a local console bind to another machine's controller.
pub fn present_on_this_host(rows: &[DeviceRow]) -> Vec<(String, Option<String>)> {
    rows.iter()
        .filter(|d| d.node.is_none() && d.is_present())
        .map(|d| (d.canonical.clone(), d.by_path.clone()))
        .collect()
}

/// A held reservation (§15.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub device_id: i64,
    pub holder: String,
    pub token: String,
    pub acquired_at: i64,
    pub expires_at: i64,
    pub stolen_from: Option<String>,
}

#[derive(Debug)]
pub struct Registry {
    conn: Connection,
    dir: PathBuf,
}

impl Registry {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut conn = open_sqlite(Some(&dir.join("registry.db")))?;
        migrate(&mut conn, schema::REGISTRY_MIGRATIONS)?;
        Ok(Self {
            conn,
            dir: dir.to_path_buf(),
        })
    }

    pub fn open_memory() -> Result<Self> {
        let mut conn = open_sqlite(None)?;
        migrate(&mut conn, schema::REGISTRY_MIGRATIONS)?;
        Ok(Self {
            conn,
            dir: PathBuf::from("."),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn device_db_path(&self, d: &DeviceRow) -> PathBuf {
        self.dir.join(&d.db_file)
    }

    // ------------------------------------------------------------ upserting --

    /// Register (or refresh) a discovered device.
    ///
    /// Re-enumeration with a new `ttyUSBn` but the same by-id path updates the
    /// informational tty and nothing else: the nickname, tags, port assignment
    /// and history all stay bound to the canonical id.
    pub fn upsert_device(
        &mut self,
        canonical: &str,
        by_path: Option<&str>,
        identity: IdentityKind,
        tty: Option<&str>,
        now: i64,
    ) -> Result<DeviceRow> {
        let db_file = format!("dev-{}.db", super::db_stem(canonical));
        self.conn.execute(
            "INSERT INTO devices(canonical,by_path,identity_kind,tty,first_seen,last_seen,db_file)
             VALUES (?1,?2,?3,?4,?5,?5,?6)
             ON CONFLICT(canonical) DO UPDATE SET
                 by_path=COALESCE(?2,by_path),
                 tty=COALESCE(?4,tty),
                 last_seen=?5",
            params![canonical, by_path, identity.as_str(), tty, now, db_file],
        )?;
        self.device_by_canonical(canonical)?
            .ok_or_else(|| ToolError::new(ErrorCode::Internal, "device vanished after upsert"))
    }

    /// Mark a row as belonging to a peer (§P1).
    ///
    /// The three facts a proxied call needs, written together so a row can never
    /// be half-remote: WHO owns it, WHERE they are, and WHAT they call it. That
    /// last one matters most -- sending our prefixed id to the owner is the
    /// mistake this column exists to prevent.
    pub fn set_remote_origin(
        &mut self,
        device_id: i64,
        node: &str,
        node_host: Option<&str>,
        remote_canonical: &str,
        remote_port: Option<u16>,
    ) -> Result<()> {
        self.set_remote_route(
            device_id,
            node,
            node_host,
            remote_canonical,
            remote_port,
            None,
            1,
        )
    }

    /// As `set_remote_origin`, but also recording HOW to reach the owner.
    ///
    /// `via` is the peer to forward to (None when the owner is directly peered)
    /// and `hops` is how far the owner is. Kept on the device row rather than
    /// derived at call time because the path is what inventory learned, and
    /// re-deriving it during an actuation would mean a route that can change
    /// between the check and the call.
    #[allow(clippy::too_many_arguments)]
    pub fn set_remote_route(
        &mut self,
        device_id: i64,
        node: &str,
        node_host: Option<&str>,
        remote_canonical: &str,
        remote_port: Option<u16>,
        via: Option<&str>,
        hops: u8,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET node=?2, node_host=?3, remote_canonical=?4, remote_port=?5,
                                via=?6, hops=?7
             WHERE id=?1",
            params![
                device_id,
                node,
                node_host,
                remote_canonical,
                remote_port.map(|p| p as i64),
                via,
                hops as i64,
            ],
        )?;
        Ok(())
    }

    /// Every row this node holds on behalf of a peer.
    pub fn remote_devices(&self) -> Result<Vec<DeviceRow>> {
        let sql = format!("{DEVICE_SELECT} WHERE node IS NOT NULL ORDER BY canonical");
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], map_device)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Rows owned by one named peer.
    pub fn devices_for_node(&self, node: &str) -> Result<Vec<DeviceRow>> {
        let sql = format!("{DEVICE_SELECT} WHERE node=?1 ORDER BY canonical");
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![node], map_device)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Record what the owner says about driving one of its boards.
    pub fn set_remote_controls(
        &mut self,
        device_id: i64,
        controls: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET remote_controls=?2 WHERE id=?1",
            params![device_id, controls.map(|c| c.to_string())],
        )?;
        Ok(())
    }

    /// Every row this node learned THROUGH a given peer.
    ///
    /// §P2. Not the same question as `devices_for_node`, which asks who OWNS a
    /// board. When a peer stops listing something, what goes stale is everything
    /// we heard FROM that peer -- including boards owned by nodes further along
    /// the chain, which we have no other way to check on.
    pub fn devices_learned_via(&self, peer: &str) -> Result<Vec<DeviceRow>> {
        let sql = format!(
            "{DEVICE_SELECT} WHERE (via=?1 OR (via IS NULL AND node=?1)) ORDER BY canonical"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![peer], map_device)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The registry's connection, for the fleet tables (§P1).
    ///
    /// Peer bookkeeping lives in `peers::registry` rather than here because it
    /// is soft state with entirely different rules -- TTLs, adverts, liveness --
    /// and mixing it into the device registry's API would blur the one thing
    /// that must stay sharp: a device row is a fact, a peer row is a belief.
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// Assign the next free ser2net port, stable across restarts (§13 `ser2net-gen`).
    pub fn assign_port(&mut self, device_id: i64, base: u16) -> Result<u16> {
        if let Some(p) = self
            .conn
            .query_row(
                "SELECT ser2net_port FROM devices WHERE id=?1",
                params![device_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
        {
            return Ok(p as u16);
        }
        let used: Vec<i64> = {
            let mut st = self
                .conn
                .prepare("SELECT ser2net_port FROM devices WHERE ser2net_port IS NOT NULL")?;
            let v = st
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        let mut port = base as i64;
        while used.contains(&port) {
            port += 1;
        }
        self.conn.execute(
            "UPDATE devices SET ser2net_port=?2 WHERE id=?1",
            params![device_id, port],
        )?;
        Ok(port as u16)
    }

    /// Set, or CLEAR, a device's nickname.
    ///
    /// An empty nickname REMOVES it. Rejecting empty made naming a one-way door:
    /// a name chosen once could never be corrected or withdrawn, and on this rig
    /// a nickname also broke the device's power control, so an operator was
    /// stuck with a board they could not actuate and could not un-name.
    pub fn set_nickname(&mut self, device_id: i64, nickname: &str) -> Result<()> {
        if nickname.trim().is_empty() {
            self.conn.execute(
                "UPDATE devices SET nickname = NULL WHERE id = ?1",
                params![device_id],
            )?;
            return Ok(());
        }
        // Nicknames address devices, so they must not look like the other
        // selector forms or a nickname could shadow a tag query.
        if nickname.contains(':') || nickname.contains('=') || nickname.contains(' ') {
            return Err(ToolError::invalid_arg(
                "nickname must not contain ':', '=' or spaces",
            ));
        }
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM devices WHERE nickname=?1",
                params![nickname],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) if id != device_id => {
                return Err(ToolError::new(
                    ErrorCode::NicknameTaken,
                    format!("nickname {nickname:?} is already bound to another device"),
                )
                .with_detail(serde_json::json!({ "held_by_device_id": id })));
            }
            _ => {}
        }
        self.conn.execute(
            "UPDATE devices SET nickname=?2 WHERE id=?1",
            params![device_id, nickname],
        )?;
        Ok(())
    }

    /// Drop tags by key.
    ///
    /// `set_tags` only ever upserts, so a label could be added and never taken
    /// off -- which makes labelling a one-way door and stops people using it. An
    /// empty VALUE is deliberately not the delete signal: a bare label with no
    /// value ("needs-rma") is the most useful kind, and overloading it would
    /// make the two indistinguishable.
    pub fn remove_tags(&mut self, device_id: i64, keys: &[String]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for k in keys {
            tx.execute(
                "DELETE FROM tags WHERE device_id = ?1 AND key = ?2",
                params![device_id, k],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_tags(&mut self, device_id: i64, tags: &BTreeMap<String, String>) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (k, v) in tags {
            tx.execute(
                "INSERT INTO tags(device_id,key,value) VALUES (?1,?2,?3)
                 ON CONFLICT(device_id,key) DO UPDATE SET value=?3",
                params![device_id, k, v],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_tag(&mut self, device_id: i64, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tags WHERE device_id=?1 AND key=?2",
            params![device_id, key],
        )?;
        Ok(())
    }

    pub fn set_pinned_profile(&mut self, device_id: i64, profile: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET pinned_profile=?2 WHERE id=?1",
            params![device_id, profile],
        )?;
        Ok(())
    }

    pub fn set_line(&mut self, device_id: i64, line: &LineConfig) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET line_json=?2 WHERE id=?1",
            params![
                device_id,
                serde_json::to_string(line).map_err(ToolError::internal)?
            ],
        )?;
        Ok(())
    }

    /// PRESENCE, written by discovery: is this device plugged in and served?
    pub fn set_state(&mut self, device_id: i64, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET state=?2 WHERE id=?1",
            params![device_id, state],
        )?;
        Ok(())
    }

    /// CAPTURE HEALTH, written by minerd: is the console being recorded?
    ///
    /// Deliberately a different column from `state`. They were one, and two
    /// processes wrote it with different meanings, so whichever ran last won and
    /// the other's fact vanished.
    /// The live capture-health column for one device, by id.
    ///
    /// A targeted read for the response envelope, which reports capture health
    /// on EVERY call and must not quote the value the DeviceRow was resolved
    /// with seconds ago -- an actuation that just published `away_in_edl` has
    /// to be visible in the very response that caused it (§W4, the class behind
    /// report #7). One indexed lookup, not the full-row SELECT.
    pub fn capture_state(&self, device_id: i64) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row(
                "SELECT capture_state FROM devices WHERE id=?1",
                params![device_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn set_capture_state(&mut self, device_id: i64, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET capture_state=?2 WHERE id=?1",
            params![device_id, state],
        )?;
        Ok(())
    }

    pub fn set_ignored(&mut self, device_id: i64, ignored: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET ignored=?2 WHERE id=?1",
            params![device_id, ignored as i64],
        )?;
        Ok(())
    }

    pub fn set_target(&mut self, device_id: i64, target: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET target=?2 WHERE id=?1",
            params![device_id, target],
        )?;
        Ok(())
    }

    /// Merge newly observed identity fields (kernel version, U-Boot build tag…).
    pub fn merge_observed(&mut self, device_id: i64, fields: &serde_json::Value) -> Result<()> {
        let cur: String = self.conn.query_row(
            "SELECT observed_json FROM devices WHERE id=?1",
            params![device_id],
            |r| r.get(0),
        )?;
        let mut obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&cur).unwrap_or_default();
        if let Some(m) = fields.as_object() {
            for (k, v) in m {
                obj.insert(k.clone(), v.clone());
            }
        }
        self.conn.execute(
            "UPDATE devices SET observed_json=?2 WHERE id=?1",
            params![device_id, serde_json::Value::Object(obj).to_string()],
        )?;
        Ok(())
    }

    pub fn forget_device(&mut self, device_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM devices WHERE id=?1", params![device_id])?;
        Ok(())
    }

    // ------------------------------------------------------------- reading --

    pub fn all_devices(&self) -> Result<Vec<DeviceRow>> {
        let mut st = self.conn.prepare(&format!("{DEVICE_SELECT} ORDER BY id"))?;
        let rows: Vec<DeviceRow> = st
            .query_map([], map_device)?
            .collect::<std::result::Result<_, _>>()?;
        self.attach_tags(rows)
    }

    pub fn device(&self, id: i64) -> Result<DeviceRow> {
        let row = self
            .conn
            .query_row(
                &format!("{DEVICE_SELECT} WHERE id=?1"),
                params![id],
                map_device,
            )
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownDevice, format!("no device {id}")))?;
        Ok(self.attach_tags(vec![row])?.remove(0))
    }

    pub fn device_by_canonical(&self, canonical: &str) -> Result<Option<DeviceRow>> {
        let row = self
            .conn
            .query_row(
                &format!("{DEVICE_SELECT} WHERE canonical=?1"),
                params![canonical],
                map_device,
            )
            .optional()?;
        Ok(match row {
            Some(r) => Some(self.attach_tags(vec![r])?.remove(0)),
            None => None,
        })
    }

    fn attach_tags(&self, mut rows: Vec<DeviceRow>) -> Result<Vec<DeviceRow>> {
        if rows.is_empty() {
            return Ok(rows);
        }
        let mut st = self
            .conn
            .prepare("SELECT device_id,key,value FROM tags ORDER BY key")?;
        let all: Vec<(i64, String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for row in &mut rows {
            for (id, k, v) in &all {
                if *id == row.id {
                    row.tags.insert(k.clone(), v.clone());
                }
            }
        }
        Ok(rows)
    }

    // ------------------------------------------------- selector resolution --

    /// Resolve a selector to every device it matches, in precedence order.
    ///
    /// Precedence (§3.1): exact nickname → exact canonical id → tag query →
    /// unique unambiguous substring of any of the above. The first layer that
    /// matches anything wins, so a nickname always shadows a substring match.
    pub fn resolve_all(&self, selector: &str) -> Result<Vec<DeviceRow>> {
        let sel = selector.trim();
        if sel.is_empty() {
            return Err(ToolError::invalid_arg("empty device selector"));
        }
        let devices = self.all_devices()?;

        // 1. exact nickname
        let hit: Vec<DeviceRow> = devices
            .iter()
            .filter(|d| d.nickname.as_deref() == Some(sel))
            .cloned()
            .collect();
        if !hit.is_empty() {
            return Ok(hit);
        }

        // 2. exact canonical id
        let hit: Vec<DeviceRow> = devices
            .iter()
            .filter(|d| d.canonical == sel)
            .cloned()
            .collect();
        if !hit.is_empty() {
            return Ok(hit);
        }

        // 3. tag query
        if let Some(terms) = parse_tag_query(sel) {
            return Ok(devices
                .into_iter()
                .filter(|d| {
                    terms
                        .iter()
                        .all(|(k, v)| d.tags.get(*k).map(String::as_str) == Some(*v))
                })
                .collect());
        }

        // 4. unique unambiguous substring of nickname, canonical id, or by-path
        //
        // DERIVED SUB-DEVICES DO NOT COMPETE HERE. `snapshot_dmesg` materialises
        // its capture as `<console>#dmesg` so it can mine without taking the
        // live console's writer lock. That store is a real device to every tool
        // -- and it is not a port anybody selects by habit. Letting it match a
        // substring means the FIRST snapshot on a board silently breaks every
        // script and muscle-memory selector for it: measured on the rig,
        // `AR40BYP4AU-if02` had been unique for weeks and started answering
        // AMBIGUOUS_DEVICE with two candidates.
        //
        // A caller who means the sub-device says so by putting `#` in the
        // selector; exact-name lookups above are untouched either way.
        let derived_ok = sel.contains('#');
        let hit: Vec<DeviceRow> = devices
            .iter()
            .filter(|d| derived_ok || !d.canonical.contains('#'))
            .filter(|d| {
                d.canonical.contains(sel)
                    || d.nickname.as_deref().is_some_and(|n| n.contains(sel))
                    || d.by_path.as_deref().is_some_and(|p| p.contains(sel))
            })
            .cloned()
            .collect();
        if !hit.is_empty() {
            return Ok(hit);
        }

        // 5. the same name, punctuated differently
        //
        // An operator names a board `uno-q` and an agent asks for `unoq`; the
        // layers above are literal, so that is UNKNOWN_DEVICE on a bench where
        // the board is plainly there. Reported from a real session. Hyphens,
        // underscores, dots and spaces are how people happen to have typed a
        // name, not part of what they meant by it -- and case is not either.
        //
        // LAST, and no looser than that. Everything above still wins, so an
        // exact name can never be shadowed by a punctuation coincidence, and an
        // ambiguous match still comes back as AMBIGUOUS_DEVICE with candidates
        // rather than picking one. The port remains the identity; this only
        // decides which port a HUMAN's spelling pointed at.
        let squash = |t: &str| -> String {
            t.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect()
        };
        let want = squash(sel);
        if want.is_empty() {
            return Ok(Vec::new());
        }
        Ok(devices
            .into_iter()
            .filter(|d| derived_ok || !d.canonical.contains('#'))
            .filter(|d| {
                squash(&d.canonical).contains(&want)
                    || d.nickname
                        .as_deref()
                        .is_some_and(|n| squash(n).contains(&want))
                    || d.by_path
                        .as_deref()
                        .is_some_and(|p| squash(p).contains(&want))
            })
            .collect())
    }

    /// Resolve to exactly one device, or fail with candidates attached.
    pub fn resolve(&self, selector: &str) -> Result<DeviceRow> {
        let mut hits = self.resolve_all(selector)?;
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => Err(ToolError::new(
                ErrorCode::UnknownDevice,
                format!("no device matches {selector:?}"),
            )),
            _ => {
                let is_group = parse_tag_query(selector.trim()).is_some();
                let code = if is_group {
                    ErrorCode::GroupSelectorNotAllowed
                } else {
                    ErrorCode::AmbiguousDevice
                };
                Err(
                    ToolError::new(code, format!("{} devices match {selector:?}", hits.len()))
                        .with_detail(serde_json::json!({
                            "candidates": hits.iter().map(|d| serde_json::json!({
                                "canonical": d.canonical,
                                "nickname": d.nickname,
                                "identity": d.identity,
                                "tags": d.tags,
                                "observed": d.observed,
                            })).collect::<Vec<_>>()
                        })),
                )
            }
        }
    }

    /// Group selectors are valid only for explicitly multi-device tools.
    pub fn resolve_group(&self, selector: &str) -> Result<Vec<DeviceRow>> {
        let hits = self.resolve_all(selector)?;
        if hits.is_empty() {
            return Err(ToolError::new(
                ErrorCode::UnknownDevice,
                format!("no device matches {selector:?}"),
            ));
        }
        Ok(hits)
    }

    // -------------------------------------------------------------- leases --

    /// Move a lease's expiry, so a lapsed one can exist on purpose.
    ///
    /// A lease that has run out is a real state with its own reporting rules --
    /// `require_lease` treats it as free, diagnostics must not show it as held --
    /// and there was no way to produce one except by waiting out a TTL, which a
    /// test cannot do and an operator should not have to. It is also the
    /// primitive a "shorten my reservation" tool would use.
    pub fn set_lease_expiry(&mut self, device_id: i64, expires_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE leases SET expires_at=?2 WHERE device_id=?1",
            params![device_id, expires_at],
        )?;
        Ok(())
    }

    pub fn acquire_lease(
        &mut self,
        device_id: i64,
        holder: &str,
        now: i64,
        ttl_s: i64,
        max_s: i64,
        steal: bool,
    ) -> Result<Lease> {
        let ttl = ttl_s.clamp(1, max_s);
        let current = self.lease(device_id)?;
        if let Some(l) = &current {
            if l.expires_at > now && l.holder != holder && !steal {
                return Err(ToolError::new(
                    ErrorCode::LeaseHeld,
                    format!("device is leased by {:?} until {}", l.holder, l.expires_at),
                )
                .with_detail(serde_json::json!({
                    "holder": l.holder,
                    "expires_at": l.expires_at,
                })));
            }
        }
        let stolen_from = current
            .filter(|l| l.expires_at > now && l.holder != holder)
            .map(|l| l.holder);
        let token = format!("{device_id}-{now}-{}", holder.len());
        self.conn.execute(
            "INSERT INTO leases(device_id,holder,token,acquired_at,expires_at,stolen_from)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(device_id) DO UPDATE SET
                holder=?2, token=?3, acquired_at=?4, expires_at=?5, stolen_from=?6",
            params![device_id, holder, token, now, now + ttl * 1000, stolen_from],
        )?;
        self.lease(device_id)?
            .ok_or_else(|| ToolError::new(ErrorCode::Internal, "lease vanished"))
    }

    /// Drop a lease regardless of who holds it.
    ///
    /// A lease whose holder no longer exists must always be reclaimable, or one
    /// crashed viewer strands a device permanently. Measured: a dashboard
    /// console that died left `holder="dashboard"` in place, and the documented
    /// escape hatches did not help -- `release()` is holder-matched, so it
    /// refused with LEASE_HELD, which reads as "you cannot have it" rather than
    /// "you asked the wrong way". Same principle as breaking a stale pid lock.
    pub fn force_release_lease(&mut self, device_id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM leases WHERE device_id=?1", params![device_id])?;
        Ok(n > 0)
    }

    pub fn release_lease(&mut self, device_id: i64, holder: &str) -> Result<()> {
        let n = self.conn.execute(
            "DELETE FROM leases WHERE device_id=?1 AND holder=?2",
            params![device_id, holder],
        )?;
        if n == 0 {
            return Err(
                ToolError::new(ErrorCode::LeaseHeld, "no lease held by that holder")
                    .with_hint("pass force:true to drop a lease whose holder is gone"),
            );
        }
        Ok(())
    }

    pub fn lease(&self, device_id: i64) -> Result<Option<Lease>> {
        Ok(self
            .conn
            .query_row(
                "SELECT device_id,holder,token,acquired_at,expires_at,stolen_from
                 FROM leases WHERE device_id=?1",
                params![device_id],
                |r| {
                    Ok(Lease {
                        device_id: r.get(0)?,
                        holder: r.get(1)?,
                        token: r.get(2)?,
                        acquired_at: r.get(3)?,
                        expires_at: r.get(4)?,
                        stolen_from: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Mutating tools call this first (§15.1: reads are always unrestricted).
    pub fn require_lease(&self, device_id: i64, holder: &str, now: i64) -> Result<()> {
        // NAME THE DEVICE. A multi-console tool checks each member in turn, so an
        // anonymous "requires a lease" left the caller guessing which of a
        // board's six consoles it had missed -- reproduced holding four leases of
        // five and being told only that a lease was required. The one fact the
        // caller needs is the one the error had.
        let named = |verb: &str| match self.device(device_id) {
            Ok(d) => format!("{verb} {:?}", d.display_name()),
            Err(_) => format!("{verb} device id {device_id}"),
        };
        match self.lease(device_id)? {
            Some(l) if l.holder == holder && l.expires_at > now => Ok(()),
            Some(l) if l.expires_at > now => Err(ToolError::new(
                ErrorCode::LeaseHeld,
                format!("{} is leased by {:?}", named("device"), l.holder),
            )),
            _ => Err(ToolError::new(
                ErrorCode::LeaseRequired,
                format!(
                    "this tool mutates {} and requires a lease on it",
                    named("").trim()
                ),
            )
            .with_hint(match self.device(device_id) {
                Ok(d) => format!("acquire({:?})", d.display_name()),
                Err(_) => "acquire the device before calling a mutating tool".into(),
            })),
        }
    }

    // --------------------------------------------------- exclusive claims ---

    /// §15.2 — the miner keeps *capturing* bytes; only interpretation suspends.
    pub fn claim_exclusive(
        &mut self,
        device_id: i64,
        holder: &str,
        protocol: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if let Some((h, _)) = self.exclusive_claim(device_id)? {
            if h != holder {
                return Err(ToolError::new(
                    ErrorCode::ExclusiveClaimed,
                    format!("port is claimed by {h:?}"),
                ));
            }
        }
        self.conn.execute(
            "INSERT INTO exclusive_claims(device_id,holder,protocol,claimed_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(device_id) DO UPDATE SET holder=?2, protocol=?3, claimed_at=?4",
            params![device_id, holder, protocol, now],
        )?;
        Ok(())
    }

    pub fn release_exclusive(&mut self, device_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM exclusive_claims WHERE device_id=?1",
            params![device_id],
        )?;
        Ok(())
    }

    pub fn exclusive_claim(&self, device_id: i64) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT holder,protocol FROM exclusive_claims WHERE device_id=?1",
                params![device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

/// `tag:role=ap-console AND tag:rack=r2` → `[("role","ap-console"),("rack","r2")]`.
/// Returns `None` when the selector is not a tag query at all.
fn parse_tag_query(sel: &str) -> Option<Vec<(&str, &str)>> {
    if !sel.starts_with("tag:") {
        return None;
    }
    let mut terms = Vec::new();
    for part in sel.split(" AND ").map(str::trim) {
        let body = part.strip_prefix("tag:")?;
        let (k, v) = body.split_once('=')?;
        if k.is_empty() {
            return None;
        }
        terms.push((k, v));
    }
    Some(terms)
}

const DEVICE_SELECT: &str = "SELECT id,canonical,by_path,identity_kind,tty,nickname,pinned_profile,
        ser2net_port,line_json,target,state,ignored,first_seen,last_seen,observed_json,db_file,
        node,node_host,remote_canonical,remote_port,via,hops,remote_controls,capture_state
     FROM devices";

fn map_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    let identity: String = r.get(3)?;
    let canonical: String = r.get(1)?;
    let line_json: String = r.get(8)?;
    let observed: String = r.get(14)?;
    Ok(DeviceRow {
        id: r.get(0)?,
        canonical: canonical.clone(),
        by_path: r.get(2)?,
        identity: IdentityKind::parse(&identity),
        tty: r.get(4)?,
        nickname: r.get(5)?,
        pinned_profile: r.get(6)?,
        ser2net_port: r.get::<_, Option<i64>>(7)?.map(|p| p as u16),
        line: serde_json::from_str(&line_json).unwrap_or_default(),
        target: r.get(9)?,
        state: r.get(10)?,
        capture_state: r.get(23)?,
        ignored: r.get::<_, i64>(11)? != 0,
        first_seen: r.get(12)?,
        last_seen: r.get(13)?,
        observed: serde_json::from_str(&observed).unwrap_or(serde_json::Value::Null),
        tags: BTreeMap::new(),
        db_file: r.get(15)?,
        kind: DeviceKind::of(&canonical),
        node: r.get(16)?,
        node_host: r.get(17)?,
        remote_canonical: r.get(18)?,
        remote_port: r.get::<_, Option<i64>>(19)?.map(|p| p as u16),
        via: r.get(20)?,
        hops: r.get::<_, i64>(21)?.max(1) as u8,
        remote_controls: r
            .get::<_, Option<String>>(22)?
            .and_then(|j| serde_json::from_str(&j).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_with_two() -> Registry {
        let mut r = Registry::open_memory().unwrap();
        r.upsert_device(
            "usb-FTDI_TTL232R_FTAAAA-if00-port0",
            Some("pci-0000:00:14.0-usb-0:1.1:1.0"),
            IdentityKind::ById,
            Some("ttyUSB0"),
            100,
        )
        .unwrap();
        r.upsert_device(
            "usb-FTDI_TTL232R_FTBBBB-if00-port0",
            Some("pci-0000:00:14.0-usb-0:1.2:1.0"),
            IdentityKind::ById,
            Some("ttyUSB1"),
            100,
        )
        .unwrap();
        r
    }

    /// A peer's board is not hardware on this host.
    ///
    /// A remote row's canonical id embeds the OWNER's device path
    /// (`peer:bravo//dev/serial/by-id/usb-..._Bantam_...`), which matches a
    /// controller `match` glob exactly as happily as one of ours. Leaving them
    /// in the candidate list lets a local console resolve a controller plugged
    /// into another machine -- and the fallback arm of `controller_port_for`
    /// does not even check topology, so it would bind on the name alone.
    #[test]
    fn controller_candidates_exclude_peer_rows_and_unplugged_ones() {
        let mut r = reg_with_two();
        let rows = r.all_devices().unwrap();
        assert_eq!(
            present_on_this_host(&rows).len(),
            2,
            "precondition: both local cables count"
        );

        // One unplugged.
        let gone = rows[0].id;
        r.set_state(gone, "gone").unwrap();

        // One belonging to a peer, named the way inventory sync names them.
        let remote = r
            .upsert_device(
                "peer:bravo//dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00",
                None,
                IdentityKind::ById,
                None,
                100,
            )
            .unwrap();
        r.set_remote_origin(
            remote.id,
            "bravo",
            Some("192.0.2.5"),
            "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00",
            None,
        )
        .unwrap();

        let rows = r.all_devices().unwrap();
        let names: Vec<String> = present_on_this_host(&rows)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names.len(), 1, "one local cable is still in: got {names:?}");
        assert!(
            names[0].contains("FTBBBB"),
            "the unplugged one must be out and the peer's must never have been \
             in: got {names:?}"
        );
    }

    #[test]
    fn nickname_survives_replug_and_renumbering() {
        let mut r = reg_with_two();
        let d = r
            .device_by_canonical("usb-FTDI_TTL232R_FTAAAA-if00-port0")
            .unwrap()
            .unwrap();
        r.set_nickname(d.id, "rb3-ap").unwrap();
        let port = r.assign_port(d.id, 5001).unwrap();

        // Replug: same by-id, new ttyUSBn.
        r.upsert_device(
            "usb-FTDI_TTL232R_FTAAAA-if00-port0",
            None,
            IdentityKind::ById,
            Some("ttyUSB7"),
            200,
        )
        .unwrap();

        let again = r.resolve("rb3-ap").unwrap();
        assert_eq!(again.id, d.id);
        assert_eq!(again.tty.as_deref(), Some("ttyUSB7"));
        assert_eq!(again.ser2net_port, Some(port), "port assignment is stable");
        assert_eq!(r.assign_port(d.id, 5001).unwrap(), port);
    }

    #[test]
    fn ports_are_distinct_and_stable_across_devices() {
        let mut r = reg_with_two();
        let all = r.all_devices().unwrap();
        let p0 = r.assign_port(all[0].id, 5001).unwrap();
        let p1 = r.assign_port(all[1].id, 5001).unwrap();
        assert_eq!((p0, p1), (5001, 5002));
        assert_eq!(r.assign_port(all[1].id, 5001).unwrap(), 5002);
    }

    #[test]
    fn selector_precedence_nickname_shadows_substring() {
        let mut r = reg_with_two();
        let all = r.all_devices().unwrap();
        // A nickname deliberately chosen to also be a substring of the *other*
        // device's canonical id.
        r.set_nickname(all[0].id, "FTBBBB").unwrap();
        let hit = r.resolve("FTBBBB").unwrap();
        assert_eq!(hit.id, all[0].id, "exact nickname must win over substring");
    }

    #[test]
    fn ambiguous_substring_returns_candidates() {
        let r = reg_with_two();
        let err = r.resolve("FTDI").unwrap_err();
        assert_eq!(err.code, ErrorCode::AmbiguousDevice);
        let c = err.detail.unwrap();
        assert_eq!(c["candidates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn unknown_selector_is_structured() {
        let r = reg_with_two();
        assert_eq!(
            r.resolve("nope").unwrap_err().code,
            ErrorCode::UnknownDevice
        );
    }

    #[test]
    fn tag_query_resolution_including_multi_tag_and() {
        let mut r = reg_with_two();
        let all = r.all_devices().unwrap();
        r.set_tags(
            all[0].id,
            &BTreeMap::from([
                ("role".into(), "ap-console".into()),
                ("rack".into(), "r2".into()),
            ]),
        )
        .unwrap();
        r.set_tags(
            all[1].id,
            &BTreeMap::from([
                ("role".into(), "ap-console".into()),
                ("rack".into(), "r3".into()),
            ]),
        )
        .unwrap();

        assert_eq!(r.resolve_all("tag:rack=r2").unwrap().len(), 1);
        assert_eq!(r.resolve_all("tag:role=ap-console").unwrap().len(), 2);
        assert_eq!(
            r.resolve_all("tag:role=ap-console AND tag:rack=r3")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            r.resolve("tag:role=ap-console AND tag:rack=r3").unwrap().id,
            all[1].id
        );
    }

    #[test]
    fn group_selector_is_rejected_on_single_device_tools() {
        let mut r = reg_with_two();
        let all = r.all_devices().unwrap();
        for d in &all {
            r.set_tags(d.id, &BTreeMap::from([("role".into(), "console".into())]))
                .unwrap();
        }
        let err = r.resolve("tag:role=console").unwrap_err();
        assert_eq!(err.code, ErrorCode::GroupSelectorNotAllowed);
        // …but the multi-device form is fine.
        assert_eq!(r.resolve_group("tag:role=console").unwrap().len(), 2);
    }

    #[test]
    fn nickname_collision_is_a_structured_error() {
        let mut r = reg_with_two();
        let all = r.all_devices().unwrap();
        r.set_nickname(all[0].id, "bench-left").unwrap();
        let err = r.set_nickname(all[1].id, "bench-left").unwrap_err();
        assert_eq!(err.code, ErrorCode::NicknameTaken);
        // Re-setting the same device's own nickname is idempotent.
        r.set_nickname(all[0].id, "bench-left").unwrap();
    }

    #[test]
    fn nicknames_cannot_impersonate_other_selector_forms() {
        let mut r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;
        for bad in ["tag:role=x", "has space", "a=b"] {
            assert_eq!(
                r.set_nickname(id, bad).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn positional_identity_is_recorded_for_serialless_clones() {
        let mut r = Registry::open_memory().unwrap();
        // Two indistinguishable clones in adjacent ports: identity falls back to
        // topology, and the caveat is visible.
        let a = r
            .upsert_device(
                "pci-0000:00:14.0-usb-0:1.1:1.0",
                Some("pci-0000:00:14.0-usb-0:1.1:1.0"),
                IdentityKind::Positional,
                Some("ttyUSB0"),
                1,
            )
            .unwrap();
        let b = r
            .upsert_device(
                "pci-0000:00:14.0-usb-0:1.2:1.0",
                Some("pci-0000:00:14.0-usb-0:1.2:1.0"),
                IdentityKind::Positional,
                Some("ttyUSB1"),
                1,
            )
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.identity, IdentityKind::Positional);
        assert_eq!(r.resolve("usb-0:1.2").unwrap().id, b.id);
    }

    #[test]
    fn observed_identity_merges_and_persists() {
        let mut r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;
        r.merge_observed(id, &serde_json::json!({"kernel": "Linux 6.12.9"}))
            .unwrap();
        r.merge_observed(id, &serde_json::json!({"uboot": "U-Boot 2026.01"}))
            .unwrap();
        let d = r.device(id).unwrap();
        assert_eq!(d.observed["kernel"], "Linux 6.12.9");
        assert_eq!(d.observed["uboot"], "U-Boot 2026.01");
    }

    #[test]
    fn lease_blocks_others_until_expiry_and_can_be_stolen() {
        let mut r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;

        r.acquire_lease(id, "agent-a", 1_000, 900, 14_400, false)
            .unwrap();
        r.require_lease(id, "agent-a", 1_000).unwrap();
        assert_eq!(
            r.require_lease(id, "agent-b", 1_000).unwrap_err().code,
            ErrorCode::LeaseHeld
        );
        assert_eq!(
            r.acquire_lease(id, "agent-b", 1_100, 900, 14_400, false)
                .unwrap_err()
                .code,
            ErrorCode::LeaseHeld
        );

        // Steal is explicit, and records who it was taken from.
        let l = r
            .acquire_lease(id, "agent-b", 1_200, 900, 14_400, true)
            .unwrap();
        assert_eq!(l.stolen_from.as_deref(), Some("agent-a"));

        // After expiry, anyone may take it without stealing.
        let after = l.expires_at + 1;
        r.acquire_lease(id, "agent-c", after, 900, 14_400, false)
            .unwrap();
    }

    #[test]
    fn lease_ttl_is_clamped_to_max() {
        let mut r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;
        let l = r.acquire_lease(id, "a", 0, 99_999, 100, false).unwrap();
        assert_eq!(l.expires_at, 100 * 1000);
    }

    #[test]
    fn mutating_without_a_lease_is_refused() {
        let r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;
        assert_eq!(
            r.require_lease(id, "anyone", 0).unwrap_err().code,
            ErrorCode::LeaseRequired
        );
    }

    #[test]
    fn exclusive_claim_is_exclusive() {
        let mut r = reg_with_two();
        let id = r.all_devices().unwrap()[0].id;
        r.claim_exclusive(id, "flasher", Some("sahara"), 1).unwrap();
        assert_eq!(
            r.claim_exclusive(id, "other", None, 2).unwrap_err().code,
            ErrorCode::ExclusiveClaimed
        );
        assert_eq!(
            r.exclusive_claim(id).unwrap().unwrap().1.as_deref(),
            Some("sahara")
        );
        r.release_exclusive(id).unwrap();
        r.claim_exclusive(id, "other", None, 3).unwrap();
    }
}
