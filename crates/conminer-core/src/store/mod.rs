//! Persistence (§7).
//!
//! SQLite in WAL mode: one database file per device on the shared volume, plus a
//! small global registry database. Zero-ops, fits Alpine, handles an 800 MB
//! ingest with batched transactions, and a volume snapshot is a complete backup.
//!
//! Two structural commitments:
//!
//! * **Raw is the source of truth.** `templates`, `occurrences`, `records`,
//!   `stages` and the FTS indexes are derived views. `rebuild_templates` and
//!   `rebuild_index` regenerate them from `raw_lines` at any time, which is also
//!   how a similarity threshold is re-tuned retroactively.
//! * **Identity is positional.** Every raw line carries its absolute byte offset
//!   in the device's append-only stream. Byte-identical boot iterations are
//!   distinct rows at distinct offsets; storage never dedupes (§8.4).

pub mod device;
pub mod lock;
pub mod registry;
pub mod schema;

pub use device::{
    BaselineRow, BootRow, DeviceStore, LineRef, LineRow, PendingLine, PendingRecord, PromptRow,
    RecordKind, RecordRow, SessionRow, SessionSource, StageRow, Stats, TemplateOrder,
    TemplateQuery, TemplateRow, Verdict, VerdictRow, WatchHit, WatchRow,
};
pub use lock::DeviceLock;
pub use registry::{DeviceRow, IdentityKind, Registry};

use crate::error::{ErrorCode, Result, ToolError};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// syslog severities plus an explicit "not classified" so an unknown line is
/// never silently reported as `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Emerg = 0,
    Alert = 1,
    Crit = 2,
    Err = 3,
    Warn = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
    Unknown = 8,
}

impl Severity {
    pub fn from_i64(v: i64) -> Self {
        use Severity::*;
        match v {
            0 => Emerg,
            1 => Alert,
            2 => Crit,
            3 => Err,
            4 => Warn,
            5 => Notice,
            6 => Info,
            7 => Debug,
            _ => Unknown,
        }
    }

    /// True for anything at least as severe as `err` — the default cut for
    /// "show me what went wrong".
    pub fn is_error(self) -> bool {
        (self as i64) <= (Severity::Err as i64)
    }
}

/// An opaque, monotonic position in a device's line stream (§8.2).
///
/// The token half binds a cursor to the database that issued it, so a cursor from
/// another device is rejected as `INVALID_CURSOR` rather than silently returning
/// somebody else's lines. The offset half is the absolute byte offset, which is
/// what makes cursors survive reconnects and stay meaningful across sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub token: String,
    pub offset: u64,
}

impl Cursor {
    pub fn new(token: impl Into<String>, offset: u64) -> Self {
        Self {
            token: token.into(),
            offset,
        }
    }

    pub fn encode(&self) -> String {
        format!("{}:{:016x}", self.token, self.offset)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let (token, off) = s.rsplit_once(':').ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidCursor, format!("malformed cursor {s:?}"))
        })?;
        let offset = u64::from_str_radix(off, 16).map_err(|_| {
            ToolError::new(ErrorCode::InvalidCursor, format!("malformed cursor {s:?}"))
        })?;
        if token.is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidCursor,
                format!("malformed cursor {s:?}"),
            ));
        }
        Ok(Self {
            token: token.to_string(),
            offset,
        })
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

/// Open a SQLite database with the pragmas every conminer store depends on.
pub(crate) fn open_sqlite(path: Option<&Path>) -> Result<Connection> {
    let conn = match path {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            Connection::open(p)?
        }
        None => Connection::open_in_memory()?,
    };
    // BUSY_TIMEOUT FIRST, BEFORE ANYTHING THAT CAN CONTEND.
    //
    // Switching to WAL takes a brief exclusive lock, and this was set AFTER
    // that switch -- so the one operation most likely to collide was the one
    // with no timeout configured yet. Four daemons opening the same fresh
    // registry at once is the normal case on a new node, and the loser died
    // with "database is locked" rather than waiting its turn. Caught by the
    // eight-way open gate once a longer migration widened the window.
    conn.pragma_update(None, "busy_timeout", 10_000)?;
    // WAL: concurrent readers during live capture and during an 800 MB ingest.
    // NORMAL synchronous bounds a host power cut to the batch window documented
    // as `capture.commit_interval_ms`, rather than fsyncing every line.
    //
    // AND `busy_timeout` DOES NOT COVER THIS ONE. Changing the journal mode
    // needs every other connection out of the file, and SQLite answers that
    // contention with an immediate SQLITE_BUSY instead of invoking the busy
    // handler, so the timeout set above is not consulted. Four daemons coming
    // up together on a fresh node is the normal case, and the losers died on
    // the pragma that was supposed to be protected. Retry it here, and treat a
    // file another opener has already switched as the success it is.
    let mut waited = std::time::Duration::from_millis(0);
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => break,
            Err(e) => {
                let mode: String = conn
                    .pragma_query_value(None, "journal_mode", |r| r.get(0))
                    .unwrap_or_default();
                if mode.eq_ignore_ascii_case("wal") {
                    break;
                }
                if waited >= std::time::Duration::from_secs(10) {
                    return Err(e.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
                waited += std::time::Duration::from_millis(25);
            }
        }
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // A 64 MB page cache: an 800 MB ingest touches several large indexes at
    // once, and the 2 MB default makes it thrash on every insert.
    //
    // BUT KNOW WHAT IT MULTIPLIES BY. A page cache is per CONNECTION, and
    // minerd holds a store per device, so on an 18-device bench this line reads
    // "64 MB" and means "up to 1.15 GB, filled as pages are touched". That is
    // fine -- it is a cache doing its job, and the memory buys speed -- but only
    // when the container limits clear it with room to spare. They did not: mcpd
    // ran at 128 MB and was OOM-killed mid-call, and minerd was at 97% of 512 MB
    // when nobody was looking.
    //
    // So the number stays, the ARITHMETIC is written down, and
    // `mcpd_has_room_for_the_answers_it_is_asked_to_build` holds the limits
    // above it. Cache is not the thing to economise on here; being surprised is.
    conn.pragma_update(None, "cache_size", -65_536)?;
    // Checkpoint far less often than the 4 MB default, so a long ingest is not
    // interrupted by a checkpoint every few thousand lines.
    conn.pragma_update(None, "wal_autocheckpoint", 20_000)?;
    conn.pragma_update(None, "journal_size_limit", 256 * 1024 * 1024)?;
    Ok(conn)
}

/// Apply every migration above the database's current `user_version`.
///
/// Each step runs in its own transaction with the version bump inside it, so an
/// interrupted upgrade leaves a consistent database at the last completed
/// version rather than a half-applied one.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[schema::Migration]) -> Result<i32> {
    let mut current: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for m in migrations {
        if m.version <= current {
            continue;
        }
        if m.version != current + 1 {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!(
                    "migration gap: database at v{current}, next available is v{} ({})",
                    m.version, m.name
                ),
            ));
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // RE-READ THE VERSION INSIDE THE WRITE LOCK.
        //
        // The version was read before this transaction existed, so between the
        // two another process may have applied the very same step: minerd, mcpd,
        // dashd and peerd all open the registry as they start, and on a node
        // whose data directory is new they do it at the same instant. Both then
        // ran migration v1 and the loser died with "table meta already exists" --
        // a daemon that refuses to start on a fresh install, which is the worst
        // moment for it. Measured under `cargo test --workspace`, which starts
        // the same processes the same way.
        let locked: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if locked >= m.version {
            drop(tx);
            current = locked;
            continue;
        }
        tx.execute_batch(m.sql).map_err(|e| {
            ToolError::new(
                ErrorCode::Internal,
                format!("migration v{} ({}) failed: {e}", m.version, m.name),
            )
        })?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        current = m.version;
    }
    Ok(current)
}

/// The schema version this build writes.
pub fn device_schema_version() -> i32 {
    schema::DEVICE_MIGRATIONS.last().map_or(0, |m| m.version)
}

pub fn registry_schema_version() -> i32 {
    schema::REGISTRY_MIGRATIONS.last().map_or(0, |m| m.version)
}

/// Short, filesystem-safe, collision-resistant stem for a canonical device id.
pub fn db_stem(canonical: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canonical.as_bytes());
    let safe: String = canonical
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let head: String = safe.chars().take(48).collect();
    format!("{head}-{}", hex::encode(&digest[..4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FOUR DAEMONS, ONE EMPTY DATA DIRECTORY, ONE WINNER PER STEP.
    ///
    /// minerd, mcpd, dashd and peerd all open the registry as they come up, and
    /// on a node installed a moment ago they do it together. Reading the version
    /// outside the write lock let two of them decide to apply the same migration;
    /// the second died with "table meta already exists" and the daemon behind it
    /// never started. Every opener must come out of this with a migrated
    /// database and no error.
    #[test]
    fn opening_a_fresh_registry_from_several_processes_at_once_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let start = start.clone();
            handles.push(std::thread::spawn(move || {
                // All eight hit the file in the same instant, which is the only
                // way to reproduce it: staggered opens always pass.
                start.wait();
                Registry::open(&path).map(|_| ())
            }));
        }
        let errs: Vec<String> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().err())
            .map(|e| e.message)
            .collect();
        assert!(errs.is_empty(), "concurrent opens failed: {errs:?}");
        // ...and the database is fully migrated, not merely un-crashed.
        let reg = Registry::open(&path).unwrap();
        assert!(reg.all_devices().is_ok());
    }

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let c = Cursor::new("abc123", 4096);
        let s = c.encode();
        assert_eq!(Cursor::decode(&s).unwrap(), c);

        for bad in ["", "nocolon", "tok:zzz", ":10"] {
            assert_eq!(
                Cursor::decode(bad).unwrap_err().code,
                ErrorCode::InvalidCursor,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn cursors_sort_by_offset_as_strings() {
        // The encoding is zero-padded hex specifically so lexical order matches
        // stream order — an agent comparing two cursors gets the right answer.
        let a = Cursor::new("t", 9).encode();
        let b = Cursor::new("t", 4096).encode();
        assert!(a < b);
    }

    #[test]
    fn db_stem_is_safe_and_distinguishes_similar_ids() {
        let a = db_stem("usb-FTDI_TTL232R_FT1-if00-port0");
        let b = db_stem("usb-FTDI_TTL232R_FT2-if00-port0");
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn severity_ordering_matches_syslog() {
        assert!(Severity::Emerg < Severity::Err);
        assert!(Severity::Err.is_error());
        assert!(!Severity::Warn.is_error());
        assert!(!Severity::Unknown.is_error());
    }

    #[test]
    fn migrations_apply_in_order_and_are_idempotent() {
        let mut c = open_sqlite(None).unwrap();
        let v = migrate(&mut c, schema::DEVICE_MIGRATIONS).unwrap();
        assert_eq!(v, device_schema_version());
        // Running again is a no-op.
        assert_eq!(migrate(&mut c, schema::DEVICE_MIGRATIONS).unwrap(), v);
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
    }

    /// A NEW MIGRATION RUNS AGAINST LIVE DATA. This one adds the reports table,
    /// and it will meet registries that already hold devices and peers on three
    /// nodes. A migration that throws there does not degrade a feature, it stops
    /// every service from starting.
    #[test]
    fn the_registry_upgrades_cleanly_over_an_existing_fleet() {
        let mut c = open_sqlite(None).unwrap();
        // A registry as it stands before the reports table, with rows in it.
        let before = schema::REGISTRY_MIGRATIONS
            .iter()
            .position(|m| m.version == 8)
            .expect("migration 8");
        migrate(&mut c, &schema::REGISTRY_MIGRATIONS[..before]).unwrap();
        c.execute(
            "INSERT INTO devices (canonical, identity_kind, state, first_seen, last_seen, db_file)
             VALUES ('usb-live', 'by_id', 'discovered', 1, 1, 'usb-live')",
            [],
        )
        .unwrap();

        migrate(&mut c, schema::REGISTRY_MIGRATIONS).unwrap();
        // Idempotent, because a node that restarts runs this again.
        migrate(&mut c, schema::REGISTRY_MIGRATIONS).unwrap();

        let devices: i64 = c
            .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(devices, 1, "existing rows survive the upgrade");
        c.query_row("SELECT count(*) FROM reports", [], |r| r.get::<_, i64>(0))
            .expect("the reports table exists");
        c.query_row("SELECT count(*) FROM report_seen", [], |r| {
            r.get::<_, i64>(0)
        })
        .expect("and its sightings table");
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
    }

    /// Presence must not be left holding capture health after the upgrade.
    ///
    /// Migration 6 gave health its own column but left the old value standing in
    /// `state`, and discovery only writes on a transition -- so every row on
    /// both live nodes read `state: not_listening`, including consoles that were
    /// capturing at that moment. The dashboard decided aliveness from that
    /// column, so this was one discovery sweep away from every lamp on the rack
    /// going dark.
    #[test]
    fn upgrading_gives_presence_its_column_back() {
        let mut c = open_sqlite(None).unwrap();
        // A database as migration 6 left it.
        let upto6 = schema::REGISTRY_MIGRATIONS
            .iter()
            .position(|m| m.version == 6)
            .expect("migration 6")
            + 1;
        migrate(&mut c, &schema::REGISTRY_MIGRATIONS[..upto6]).unwrap();
        let now = 1_000i64;
        for (canonical, state, port, ignored) in [
            ("usb-capturing", "streaming", Some(5001), 0),
            ("usb-quiet", "not_listening", Some(5002), 0),
            ("usb-excluded", "open_failed", None::<i64>, 1),
            ("usb-departed", "gone", None, 0),
        ] {
            c.execute(
                "INSERT INTO devices (canonical, identity_kind, state, ser2net_port, ignored,
                                      first_seen, last_seen, db_file)
                 VALUES (?1, 'by_id', ?2, ?3, ?4, ?5, ?5, ?1)",
                rusqlite::params![canonical, state, port, ignored, now],
            )
            .unwrap();
        }
        // Backfill as migration 6 does, so this starts from the real shape.
        c.execute(
            "UPDATE devices SET capture_state = state WHERE state IN
             ('listening','streaming','garbage','open_failed','away_in_edl','not_listening')",
            [],
        )
        .unwrap();

        migrate(&mut c, schema::REGISTRY_MIGRATIONS).unwrap();

        let row = |canonical: &str| -> (String, Option<String>) {
            c.query_row(
                "SELECT state, capture_state FROM devices WHERE canonical = ?1",
                [canonical],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        // Presence is presence again...
        assert_eq!(row("usb-capturing").0, "discovered");
        assert_eq!(row("usb-quiet").0, "discovered");
        assert_eq!(row("usb-excluded").0, "ignored");
        // ...and health is untouched, so nothing loses its answer in the upgrade.
        assert_eq!(row("usb-capturing").1.as_deref(), Some("streaming"));
        assert_eq!(row("usb-quiet").1.as_deref(), Some("not_listening"));
        assert_eq!(row("usb-excluded").1.as_deref(), Some("open_failed"));
        // A row that already held presence is left exactly as it was.
        assert_eq!(row("usb-departed"), ("gone".into(), None));
    }

    #[test]
    fn partial_migration_state_upgrades_forward() {
        // A database written by an older build (v1 only) must upgrade cleanly.
        let mut c = open_sqlite(None).unwrap();
        migrate(&mut c, &schema::DEVICE_MIGRATIONS[..1]).unwrap();
        assert_eq!(
            c.pragma_query_value::<i32, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            1
        );
        let v = migrate(&mut c, schema::DEVICE_MIGRATIONS).unwrap();
        assert_eq!(v, device_schema_version());
        // The v2/v3 objects are really there.
        c.query_row("SELECT count(*) FROM boots", [], |r| r.get::<_, i64>(0))
            .unwrap();
        c.query_row("SELECT count(*) FROM raw_fts", [], |r| r.get::<_, i64>(0))
            .unwrap();
    }
}
