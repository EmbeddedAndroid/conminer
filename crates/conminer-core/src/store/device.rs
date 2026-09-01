//! Per-device store: sessions, raw lines, records, templates, stages, epochs.
//!
//! Everything an agent can ask about one console lives in one SQLite file, so a
//! device's history is a single unit to back up, copy to a bug report, or delete.

use super::{migrate, open_sqlite, schema, Cursor, Severity};
use crate::drain::{Drain, DrainConfig, Template, TokenizerRules};
use crate::error::{ErrorCode, Result, ToolError};
use crate::linesplit::Terminator;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

/// Magic that marks a `conminer export_session` archive (§14.7).
pub const EXPORT_MAGIC: &str = "CONMINER-EXPORT-1";

// ------------------------------------------------------------------ rows -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    Live,
    File,
    Pstore,
    Lava,
}

impl SessionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionSource::Live => "live",
            SessionSource::File => "file",
            SessionSource::Pstore => "pstore",
            SessionSource::Lava => "lava",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "live" => SessionSource::Live,
            "file" => SessionSource::File,
            "pstore" => SessionSource::Pstore,
            "lava" => SessionSource::Lava,
            other => {
                return Err(ToolError::invalid_arg(format!(
                    "unknown session source {other:?}"
                )))
            }
        })
    }
}

/// What a record represents. `Garbage` and `Binary` spans are quarantined rather
/// than mined, so a baud mismatch or a Sahara transfer never pollutes templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Line,
    Crash,
    Garbage,
    Binary,
}

impl RecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::Line => "line",
            RecordKind::Crash => "crash",
            RecordKind::Garbage => "garbage",
            RecordKind::Binary => "binary",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "crash" => RecordKind::Crash,
            "garbage" => RecordKind::Garbage,
            "binary" => RecordKind::Binary,
            _ => RecordKind::Line,
        }
    }

    /// Only ordinary and crash records feed the miner.
    pub fn is_minable(self) -> bool {
        matches!(self, RecordKind::Line | RecordKind::Crash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: i64,
    pub source: SessionSource,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub label: Option<String>,
    pub content_sha: Option<String>,
    pub source_path: Option<String>,
    pub bytes: i64,
    pub lines: i64,
    pub records: i64,
}

#[derive(Debug, Clone)]
pub struct LineRow {
    pub id: i64,
    pub session_id: i64,
    pub boot_id: Option<i64>,
    pub stream_offset: u64,
    pub ts_mono: i64,
    pub ts_wall: i64,
    pub stage_id: Option<i64>,
    pub bytes: Vec<u8>,
    pub terminator: Terminator,
    pub truncated: bool,
    pub continuation: bool,
}

impl LineRow {
    pub fn lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// The cursor that sits immediately *after* this line.
    pub fn end_offset(&self) -> u64 {
        self.stream_offset + self.bytes.len() as u64 + self.terminator.raw().len() as u64
    }
}

#[derive(Debug, Clone)]
pub struct RecordRow {
    pub id: i64,
    pub session_id: i64,
    pub boot_id: Option<i64>,
    pub first_line_id: i64,
    pub last_line_id: i64,
    pub line_count: i64,
    pub stage_id: Option<i64>,
    pub profile: String,
    pub severity: Severity,
    pub kind: RecordKind,
    pub template_id: Option<i64>,
    pub truncated: bool,
    pub fields: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateRow {
    pub id: i64,
    pub stage: Option<String>,
    pub profile: Option<String>,
    pub text: String,
    pub tokens: Vec<String>,
    pub head_only: bool,
    pub severity: Severity,
    pub first_seen_session: i64,
    pub first_seen_boot: Option<i64>,
    pub first_seen_ts: i64,
    pub total_count: i64,
    /// Filled by session/boot-scoped queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_first_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_last_ts: Option<i64>,
    /// The agent's standing judgement, when one has been recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_ticket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRow {
    pub id: i64,
    pub session_id: i64,
    pub boot_id: Option<i64>,
    pub name: String,
    pub profile: String,
    pub entered_ts: i64,
    pub banner_line_id: Option<i64>,
    pub exited_ts: Option<i64>,
}

/// A timeline event: `(id, at, stream_offset, kind, data)`.
///
/// Named rather than left as a bare tuple so the shape is documented where it is
/// used, not re-derived at every call site.
pub type EventRow = (i64, i64, u64, String, serde_json::Value);

/// A row from `images`: `(id, bound_at, name, git_sha, image_hash)`.
type BoundImage = (i64, i64, Option<String>, Option<String>, Option<String>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRow {
    pub id: i64,
    pub pattern: String,
    pub kind: String,
    /// `profile` | `configured` | `learned`
    pub provenance: String,
    pub stage: Option<String>,
    pub observations: i64,
    pub last_seen: Option<i64>,
}

/// An agent's standing judgement about a template.
///
/// Deliberately a small closed set rather than free-form labels: the point is
/// that `list_templates` and `evaluate_policy` can *act* on it (hide it, waive
/// it, fail on it), which open-ended tagging cannot support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Known noise. Hidden from the table of contents by default and waived by
    /// the regression gate.
    Benign,
    /// A real defect that is already understood; it should not read as novel.
    KnownBad,
    /// Actively being chased.
    Investigating,
    /// Worth surfacing even though it is not an error.
    Interesting,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Benign => "benign",
            Verdict::KnownBad => "known_bad",
            Verdict::Investigating => "investigating",
            Verdict::Interesting => "interesting",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "benign" => Ok(Verdict::Benign),
            "known_bad" => Ok(Verdict::KnownBad),
            "investigating" => Ok(Verdict::Investigating),
            "interesting" => Ok(Verdict::Interesting),
            other => Err(ToolError::invalid_arg(format!(
                "unknown verdict {other:?}; expected benign, known_bad, investigating or interesting"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRow {
    pub template_id: i64,
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineRow {
    pub name: String,
    pub boot_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub set_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchRow {
    pub id: i64,
    pub name: String,
    pub predicate: Value,
    pub created_at: i64,
    pub scanned_to: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_polled: Option<i64>,
    pub active: bool,
    /// §K4. Push configuration and what has actually been delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Value>,
}

/// A watch with push armed (§K4).
#[derive(Debug, Clone)]
pub struct ArmedWatch {
    pub name: String,
    pub url: String,
    pub secret: Option<String>,
    /// Firings inside this window arrive as ONE post.
    pub min_interval_s: i64,
    pub last_delivery_at: i64,
    /// Consecutive failed attempts since the last success (§L1). The retry
    /// backoff is computed from this instead of being slept through inline.
    pub failed_streak: i64,
}

/// One firing of a watch predicate, durable so it survives the agent being away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchHit {
    pub at: i64,
    pub stream_offset: u64,
    pub matched: String,
    pub evidence: Value,
}

/// (template_id, slot, agg, unit) — see [`DeviceStore::metric`].
pub type PinnedMetric = (i64, i64, String, Option<String>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootRow {
    pub id: i64,
    pub seq: i64,
    pub session_id: Option<i64>,
    pub label: Option<String>,
    pub opened_by: String,
    pub opened_at: i64,
    pub opened_offset: u64,
    pub closed_at: Option<i64>,
    pub bytes: i64,
    pub fingerprint: Option<String>,
    pub outcome: Option<String>,
    pub image_id: Option<i64>,
    /// Ties epochs opened by ONE action across a board's consoles (§F1).
    pub group_id: Option<String>,
}

// -------------------------------------------------------------- pending ------

/// A line on its way into the store, still owning nothing.
#[derive(Debug, Clone, Copy)]
pub struct PendingLine<'a> {
    pub bytes: &'a [u8],
    pub terminator: Terminator,
    pub truncated: bool,
    pub continuation: bool,
    pub ts_mono: i64,
    pub ts_wall: i64,
    pub stage_id: Option<i64>,
}

impl<'a> PendingLine<'a> {
    pub fn from_line(l: &'a crate::linesplit::Line, ts_mono: i64, ts_wall: i64) -> Self {
        Self {
            bytes: &l.bytes,
            terminator: l.terminator,
            truncated: l.truncated,
            continuation: l.continuation,
            ts_mono,
            ts_wall,
            stage_id: None,
        }
    }

    pub fn consumed(&self) -> u64 {
        self.bytes.len() as u64 + self.terminator.raw().len() as u64
    }
}

/// Where a line landed: its row id and its absolute stream offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRef {
    pub id: i64,
    pub stream_offset: u64,
}

#[derive(Debug, Clone)]
pub struct PendingRecord {
    pub session_id: i64,
    pub boot_id: Option<i64>,
    pub first_line_id: i64,
    pub last_line_id: i64,
    pub line_count: i64,
    pub stage_id: Option<i64>,
    pub profile: String,
    pub severity: Severity,
    pub kind: RecordKind,
    pub template_id: Option<i64>,
    pub truncated: bool,
    pub fields: serde_json::Value,
    /// Full record text, used for the record-scope FTS index (§8.1).
    pub text: String,
}

// --------------------------------------------------------------- queries -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TemplateOrder {
    #[default]
    Count,
    FirstSeen,
    LastSeen,
    Severity,
}

#[derive(Debug, Clone, Default)]
pub struct TemplateQuery {
    pub session_id: Option<i64>,
    pub boot_id: Option<i64>,
    pub stage: Option<String>,
    pub min_count: Option<i64>,
    pub min_severity: Option<Severity>,
    /// `new_only=true` = first seen in the scoped session (§8).
    pub new_only: bool,
    /// Only templates that did *not* fire in this epoch: "new versus the last
    /// boot that worked", which is the question `new_only` cannot answer once a
    /// device has more than one session of history.
    pub not_in_boot: Option<i64>,
    /// Keep only these verdicts (empty = no restriction).
    pub only_verdicts: Vec<Verdict>,
    /// Drop these verdicts. The tool layer defaults this to `[Benign]` so
    /// triaged noise leaves the table of contents entirely.
    pub hide_verdicts: Vec<Verdict>,
    pub order: TemplateOrder,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub sessions: i64,
    pub lines: i64,
    pub records: i64,
    pub templates: i64,
    pub bytes: i64,
    /// lines ÷ distinct templates — the number that says "you can read the table
    /// of contents instead of the log".
    pub compression_ratio: f64,
    pub fragmentation_ratio: f64,
    pub stream_offset: u64,
    pub pruned_before_offset: u64,
    pub db_bytes: i64,
    pub fts_enabled: bool,
}

// ----------------------------------------------------------------- store -----

/// Handle to one device's database.
#[derive(Debug)]
pub struct DeviceStore {
    conn: Connection,
    /// `None` for an in-memory store, which has nothing to lock.
    path: Option<std::path::PathBuf>,
    canonical: String,
    cursor_token: String,
    stream_offset: u64,
    pruned_before: u64,
    fts: bool,
}

impl DeviceStore {
    /// Open (creating and migrating if needed) the store for `canonical`.
    pub fn open(path: &Path, canonical: &str, fts: bool) -> Result<Self> {
        Self::open_inner(Some(path), canonical, fts)
    }

    /// In-memory store — used by tests and by `conminer profile test`.
    pub fn open_memory(canonical: &str) -> Result<Self> {
        Self::open_inner(None, canonical, true)
    }

    fn open_inner(path: Option<&Path>, canonical: &str, fts: bool) -> Result<Self> {
        let mut conn = open_sqlite(path)?;
        migrate(&mut conn, schema::DEVICE_MIGRATIONS)?;

        let existing: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='canonical'", [], |r| {
                r.get(0)
            })
            .optional()?;

        match existing {
            Some(c) if c != canonical => {
                return Err(ToolError::new(
                    ErrorCode::Internal,
                    format!("store belongs to device {c:?}, not {canonical:?}"),
                ));
            }
            Some(_) => {}
            None => {
                use sha2::{Digest, Sha256};
                let token = hex::encode(&Sha256::digest(canonical.as_bytes())[..6]);
                let init = [
                    ("canonical", canonical.to_string()),
                    ("cursor_token", token),
                    ("stream_offset", "0".into()),
                    ("pruned_before_offset", "0".into()),
                ];
                for (k, v) in init {
                    conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2)", params![k, v])?;
                }
            }
        }

        let cursor_token = meta_get(&conn, "cursor_token")?.unwrap_or_default();
        let stream_offset = meta_get(&conn, "stream_offset")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let pruned_before = meta_get(&conn, "pruned_before_offset")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        Ok(Self {
            conn,
            path: path.map(Path::to_path_buf),
            canonical: canonical.to_string(),
            cursor_token,
            stream_offset,
            pruned_before,
            fts,
        })
    }

    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Take the device's writer lock. One writer per device, always (§3): two
    /// pipelines on one device would each hold their own miner and allocate
    /// colliding template ids.
    ///
    /// Re-reads the stream offset and retention horizon afterwards, because a
    /// handle opened *before* the lock was granted is looking at whatever the
    /// previous writer had appended since.
    pub fn lock_for_writing(&mut self) -> Result<Option<super::DeviceLock>> {
        let lock = match &self.path {
            Some(p) => super::DeviceLock::acquire(p).map(Some)?,
            None => None,
        };
        self.reload_counters()?;
        Ok(lock)
    }

    /// Re-read the append-only counters from the database.
    pub fn reload_counters(&mut self) -> Result<()> {
        self.stream_offset = meta_get(&self.conn, "stream_offset")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.pruned_before = meta_get(&self.conn, "pruned_before_offset")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(())
    }

    pub fn fts_enabled(&self) -> bool {
        self.fts
    }

    /// Record the unterminated line the console is currently sitting on.
    ///
    /// Written by the capture loop, read by `console::derive` in another
    /// process: a prompt has no terminator, so this is the ONLY place a live
    /// prompt exists until dead air closes the record ten seconds later. See
    /// `LineSplitter::pending_text` for the four-round bug this closes.
    ///
    /// Empty clears it. That matters as much as setting it: once the operator
    /// presses enter the prompt becomes a real line, and a stale copy here would
    /// have `console_state` reporting a prompt at a console that has moved on.
    pub fn set_pending_tail(&mut self, text: &str, ts_wall: i64) -> Result<()> {
        if text.is_empty() {
            self.conn.execute(
                "DELETE FROM meta WHERE key IN ('pending_tail','pending_tail_ts')",
                [],
            )?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('pending_tail',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![text],
        )?;
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('pending_tail_ts',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![ts_wall.to_string()],
        )?;
        Ok(())
    }

    /// The unterminated line and when it was last touched, if any.
    pub fn pending_tail(&self) -> Result<Option<(String, i64)>> {
        let Some(text) = meta_get(&self.conn, "pending_tail")? else {
            return Ok(None);
        };
        if text.is_empty() {
            return Ok(None);
        }
        let ts = meta_get(&self.conn, "pending_tail_ts")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(Some((text, ts)))
    }

    /// A cursor at the current head of the stream.
    pub fn head_cursor(&self) -> Cursor {
        Cursor::new(&self.cursor_token, self.stream_offset)
    }

    pub fn cursor_at(&self, offset: u64) -> Cursor {
        Cursor::new(&self.cursor_token, offset)
    }

    /// Validate a cursor belongs to this device and still points inside the
    /// retained window (§8.2 `CURSOR_EXPIRED`).
    pub fn resolve_cursor(&self, c: &Cursor) -> Result<u64> {
        if c.token != self.cursor_token {
            return Err(ToolError::new(
                ErrorCode::InvalidCursor,
                "cursor was issued by a different device",
            ));
        }
        if c.offset < self.pruned_before {
            return Err(ToolError::new(
                ErrorCode::CursorExpired,
                format!(
                    "cursor at offset {} is behind the retention horizon {}",
                    c.offset, self.pruned_before
                ),
            )
            .with_detail(serde_json::json!({
                "earliest_offset": self.pruned_before,
                "head": self.head_cursor().encode(),
            })));
        }
        Ok(c.offset)
    }

    pub fn stream_offset(&self) -> u64 {
        self.stream_offset
    }

    pub fn pruned_before_offset(&self) -> u64 {
        self.pruned_before
    }

    // ------------------------------------------------------------ sessions ---

    #[allow(clippy::too_many_arguments)]
    pub fn begin_session(
        &mut self,
        source: SessionSource,
        started_at: i64,
        label: Option<&str>,
        content_sha: Option<&str>,
        source_path: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions(source, started_at, label, content_sha, source_path)
             VALUES (?1,?2,?3,?4,?5)",
            params![source.as_str(), started_at, label, content_sha, source_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn end_session(&mut self, id: i64, at: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE sessions SET ended_at=?2 WHERE id=?1 AND ended_at IS NULL",
            params![id, at],
        )?;
        if n == 0 && self.session(id).is_err() {
            return Err(ToolError::new(
                ErrorCode::UnknownSession,
                format!("no session {id}"),
            ));
        }
        Ok(())
    }

    /// Recorded after the stream is consumed: we cannot know a file's hash until
    /// we have read it, so duplicate detection is honestly a post-ingest answer.
    pub fn set_session_content_sha(&mut self, id: i64, sha: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET content_sha=?2 WHERE id=?1",
            params![id, sha],
        )?;
        Ok(())
    }

    pub fn session(&self, id: i64) -> Result<SessionRow> {
        self.conn
            .query_row(
                "SELECT id,source,started_at,ended_at,label,content_sha,source_path,bytes,lines,records
                 FROM sessions WHERE id=?1",
                params![id],
                map_session,
            )
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownSession, format!("no session {id}")))
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,source,started_at,ended_at,label,content_sha,source_path,bytes,lines,records
             FROM sessions ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], map_session)?;
        let out = rows.collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn latest_session(&self) -> Result<Option<SessionRow>> {
        Ok(self.list_sessions(1)?.into_iter().next())
    }

    /// Duplicate detection for `ingest_file` (§13 `ingest`): re-ingesting the
    /// same bytes creates a new session but the caller is warned.
    pub fn session_with_sha(&self, sha: &str) -> Result<Option<SessionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,source,started_at,ended_at,label,content_sha,source_path,bytes,lines,records
                 FROM sessions WHERE content_sha=?1 ORDER BY id LIMIT 1",
                params![sha],
                map_session,
            )
            .optional()?)
    }

    // --------------------------------------------------------------- lines ---

    /// Start a write batch. Every table a block of input touches is written in
    /// one transaction, which *is* the durability window: a caller honouring
    /// `capture.commit_interval_ms` loses at most that much on a host power cut,
    /// and nothing a committed batch returned.
    pub fn begin_batch(&mut self) -> Result<Batch<'_>> {
        // Never start below what is already stored. `stream_offset` is a cached
        // cursor, and a cache that drifts below the table kills capture
        // outright: `raw_lines.stream_offset` is UNIQUE, so a reused offset
        // fails the insert and takes the read loop with it. Measured on the
        // Bughopper, whose FTDI reconnects on every power action -- minerd
        // attached, died with "UNIQUE constraint failed: raw_lines.stream_offset",
        // reattached, and looped every ~4s while the board was visibly booting.
        // The device looked DEAD (bytes_this_boot stayed 0) and every judgement
        // built on that signal was wrong.
        //
        // THE LAST ROW, NOT A MAX() OVER EVERY ROW.
        //
        // `MAX(stream_offset + length(bytes) + length(terminator))` aggregates a
        // COMPUTED expression, which no index can answer, so SQLite scans the
        // whole table -- once per batch, which is once per chunk of console.
        // That is free on a fresh store and ruinous on a used one: measured on
        // the bench at 4,909,488 rows / 2.08 GB, this query took **1879 ms**
        // while the same answer from the last row took **0.4 ms**. It put the
        // miner at ~1 line/sec, minutes behind the live console, and it is not
        // a property of that board -- it is a property of store SIZE, so every
        // board reaches it once somebody develops on it.
        //
        // Offsets only ever grow, and rows are appended in offset order, so the
        // greatest offset is by definition the newest row. `ORDER BY id DESC
        // LIMIT 1` reads exactly one row through the primary key and is just as
        // self-healing as the scan was.
        // ...and the terminator is measured in BYTES, not in the length of its
        // name. `raw_lines.terminator` stores "lf"/"crlf"/"cr"/"none", so the
        // obvious `length(terminator)` in SQL yields 2/4/2/4 where the board
        // actually sent 1/2/1/0. That one-byte-per-line overcount is not
        // cosmetic: the healed cursor lands PAST the real end of the stream, so
        // the next line is written at an offset with a phantom gap in front of
        // it, and the drift compounds. Measured on the bench: every heal
        // reported exactly `drift=1` on an LF console, and the device's
        // `stream_offset` had run 9.35 MB ahead of its actual 227 MB of bytes.
        let stored: u64 = self
            .conn
            .query_row(
                "SELECT stream_offset, length(bytes), terminator \
                 FROM raw_lines ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()
            .map(|(off, len, label)| {
                off.max(0) as u64
                    + len.max(0) as u64
                    + crate::linesplit::Terminator::from_label(&label).raw().len() as u64
            })
            .unwrap_or(0);
        if stored > self.stream_offset {
            tracing::warn!(
                cached = self.stream_offset,
                stored,
                "stream offset cursor was behind the store; advancing to avoid a collision"
            );
            self.stream_offset = stored;
        }
        let offset = self.stream_offset;
        let fts = self.fts;
        Ok(Batch {
            // §L2. IMMEDIATE, not the deferred default.
            //
            // `busy_timeout` does NOT cover a transaction that starts by reading and then
            // tries to UPGRADE to a write: SQLite returns SQLITE_BUSY straight away there,
            // because waiting could deadlock two upgraders against each other. That is the
            // one path where a busy database surfaces as a raw "database is locked" instead
            // of a pause -- measured three times in one session, as `actuation.power_on`,
            // `actuation.reset` and `mining.follow_size` failing with the bare SQLite error
            // as their result.
            //
            // Taking the write lock up front makes the wait ordinary, so `busy_timeout`
            // applies and a contended store costs a moment rather than an error.
            tx: self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?,
            fts,
            offset,
            counters: Default::default(),
            boot_bytes: Default::default(),
        })
    }

    /// Record the offset a committed batch ended at.
    pub fn finish_batch(&mut self, new_offset: u64) {
        self.stream_offset = new_offset;
    }

    /// Convenience wrapper for one-shot callers (tests, the CLI). The live and
    /// ingest paths use `begin_batch` so a whole block commits once.
    pub fn append_lines(
        &mut self,
        session_id: i64,
        boot_id: Option<i64>,
        lines: &[PendingLine<'_>],
    ) -> Result<Vec<LineRef>> {
        let mut b = self.begin_batch()?;
        let refs = b.append_lines(session_id, boot_id, lines)?;
        let off = b.commit()?;
        self.finish_batch(off);
        Ok(refs)
    }

    pub fn line(&self, id: i64) -> Result<LineRow> {
        self.conn
            .query_row(LINE_SELECT_BY_ID, params![id], map_line)
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownLine, format!("no line {id}")))
    }

    /// ±N verbatim lines around any line (§8 `get_context`).
    pub fn context(&self, line_id: i64, before: usize, after: usize) -> Result<Vec<LineRow>> {
        let anchor = self.line(line_id)?;
        let mut out = Vec::new();
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut pre: Vec<LineRow> = st
            .query_map(params![line_id, before as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        pre.reverse();
        out.extend(pre);
        out.push(anchor);
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        out.extend(
            st.query_map(params![line_id, after as i64], map_line)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        Ok(out)
    }

    /// Tail of the stream (§8 `get_recent`, uart-mcp `get_recent_logs` parity).
    pub fn recent_lines(&self, n: usize) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines ORDER BY id DESC LIMIT ?1",
        )?;
        let mut v: Vec<LineRow> = st
            .query_map(params![n as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        v.reverse();
        Ok(v)
    }

    /// Lines strictly after a cursor, in stream order. The tailing primitive
    /// behind `follow` (§8.2).
    pub fn lines_after(&self, offset: u64, limit: usize) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE stream_offset >= ?1 ORDER BY stream_offset ASC LIMIT ?2",
        )?;
        let out = st
            .query_map(params![offset as i64, limit as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn lines_for_session(&self, session_id: i64) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE session_id=?1 ORDER BY id",
        )?;
        let out = st
            .query_map(params![session_id], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Tier-2 regex scan over raw lines (§8.1). Rust's `regex` crate is
    /// linear-time, so a hostile pattern from an agent cannot pin a lab host.
    ///
    /// `limit` bounds the *results*, not the scan; `scanned` reports the real
    /// cost so a response can say `scan=true` honestly.
    pub fn search_regex(
        &self,
        re: &regex::Regex,
        session_id: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<LineRow>, usize)> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE (?1 IS NULL OR session_id=?1) ORDER BY id",
        )?;
        let rows = st.query_map(params![session_id], map_line)?;
        let mut hits = Vec::new();
        let mut scanned = 0usize;
        for row in rows {
            let l = row?;
            scanned += 1;
            if re.is_match(&String::from_utf8_lossy(&l.bytes)) {
                hits.push(l);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok((hits, scanned))
    }

    /// Lines received in a host wall-time window — the basis for interleaving
    /// several consoles of one target (§15.8).
    pub fn lines_between(&self, from_ms: i64, to_ms: i64, limit: usize) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE ts_wall BETWEEN ?1 AND ?2 ORDER BY ts_wall, id LIMIT ?3",
        )?;
        let out = st
            .query_map(params![from_ms, to_ms, limit as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn line_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM raw_lines", [], |r| r.get(0))?)
    }

    /// How many occurrences a template needs before a burst rate means anything.
    ///
    /// Five, because one or two lines have no meaningful span between them and
    /// the resulting rate is an artefact of the clock, not a property of the
    /// board.
    const BURST_MIN_SAMPLES: i64 = 5;

    /// What is FLOODING the console right now, by rate (§F11).
    ///
    /// `list_templates` answers "what has this board ever said, and how often" --
    /// a lifetime count, which cannot distinguish a message that fired 6,000
    /// times during a boot two days ago from one firing four times a second at
    /// this moment. An agent watching a board that has started crash-looping
    /// needs the second question, and needs it before it decides to read
    /// anything raw.
    ///
    /// Rate is measured over the window the caller names, from the occurrence
    /// rollups that already exist -- no new capture, no log scan.
    pub fn noisiest_templates(
        &self,
        since_ts: i64,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let mut st = self.conn.prepare(
            "SELECT o.template_id, SUM(o.count) AS n, MIN(o.first_ts), MAX(o.last_ts),
                    t.template_text, t.severity
               FROM occurrences o JOIN templates t ON t.id = o.template_id
              WHERE o.last_ts >= ?1
              GROUP BY o.template_id
              ORDER BY n DESC
              LIMIT ?2",
        )?;
        let window_ms = (now_ms - since_ts).max(1) as f64;
        let rows = st
            .query_map(params![since_ts, limit as i64], |r| {
                let raw_n: i64 = r.get(1)?;
                let first: i64 = r.get(2)?;
                let last: i64 = r.get(3)?;
                // OCCURRENCES ARE ROLLED UP PER EPOCH, not per line, so a rollup
                // that STARTED before the window still carries its whole count.
                // Measured on the ADP: 171 occurrences reported inside a window
                // holding 144 lines -- a 118% share, which is not a rounding
                // error, it is counting the wrong thing.
                //
                // Prorating over the rollup's own span assumes the occurrences
                // were spread evenly across it. That is an ESTIMATE, and the
                // response says so rather than presenting it as a measurement:
                // conminer keeps counts, not per-occurrence timestamps, and that
                // compression is the whole point of the tool.
                let span = (last - first).max(0) as f64;
                let (n, approximate) = if first >= since_ts || span == 0.0 {
                    (raw_n, false)
                } else {
                    let inside = ((last - since_ts).max(0) as f64 / span).clamp(0.0, 1.0);
                    (((raw_n as f64) * inside).round() as i64, true)
                };
                // Two rates, because they answer different questions. `per_min`
                // is share-of-the-window: what this message costs over the span
                // asked about. `burst_per_min` is how fast it comes when it
                // comes -- 200 lines in 200 ms is a flood whether you ask about
                // the last minute or the last day, and an agent that asked a
                // wide window must not be told "nothing is flooding" about a
                // message arriving four times a second.
                // A BURST NEEDS SAMPLES. One occurrence has no span: dividing by
                // the 1 ms floor turned a single line into "60,000 per minute"
                // and the advice told an agent to go muting -- measured on the
                // IQ10 the moment this shipped. Below the floor the honest
                // answer is "no burst rate", not a number that big.
                let span_ms = (last - first).max(0) as f64;
                let burst = if n >= Self::BURST_MIN_SAMPLES && span_ms >= 1.0 {
                    Some(((n as f64) / (span_ms / 60_000.0) * 10.0).round() / 10.0)
                } else {
                    None
                };
                Ok(serde_json::json!({
                    "template_id": r.get::<_, i64>(0)?,
                    "count_in_window": n,
                    "per_min": ((n as f64) / (window_ms / 60_000.0) * 10.0).round() / 10.0,
                    "burst_per_min": burst,
                    // True when this template's rollup straddles the start of
                    // the window and the count above was prorated.
                    "approximate": approximate,
                    "first_ts": first,
                    "last_ts": last,
                    "text": r.get::<_, String>(4)?,
                    // Stored as an integer rank, not a name.
                    "severity": r.get::<_, Option<i64>>(5)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many lines arrived since an instant, for the share-of-output figure.
    pub fn lines_since_ts(&self, since_ts: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM raw_lines WHERE ts_wall >= ?1",
            params![since_ts],
            |r| r.get(0),
        )?)
    }

    /// Which template a raw line belongs to, if it was mined into one (§F11).
    pub fn template_of_line(&self, line_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT template_id FROM records
                  WHERE ?1 BETWEEN first_line_id AND last_line_id
                  ORDER BY id LIMIT 1",
                params![line_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Templates an operator has already judged benign — the durable mute.
    pub fn muted_templates(&self) -> Result<Vec<i64>> {
        let mut st = self
            .conn
            .prepare("SELECT template_id FROM template_verdicts WHERE verdict = 'benign'")?;
        let out = st
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------- metrics (F3) ---

    /// Pin a number that lives in a template slot, by name.
    pub fn pin_metric(
        &mut self,
        name: &str,
        template_id: i64,
        slot: i64,
        agg: &str,
        unit: Option<&str>,
        at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metrics(name,template_id,slot,agg,unit,pinned_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(name) DO UPDATE SET
                 template_id = excluded.template_id, slot = excluded.slot,
                 agg = excluded.agg, unit = excluded.unit, pinned_at = excluded.pinned_at",
            params![name, template_id, slot, agg, unit, at],
        )?;
        Ok(())
    }

    pub fn unpin_metric(&mut self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM metrics WHERE name=?1", params![name])?
            > 0)
    }

    /// A pinned metric's definition: (template_id, slot, agg, unit).
    ///
    /// Aliased because clippy is right that four anonymous fields at a call site
    /// is a puzzle, and this one is read in two places.
    ///
    /// (name, template_id, slot, agg, unit, pinned_at)
    #[allow(clippy::type_complexity)]
    pub fn metrics(&self) -> Result<Vec<(String, i64, i64, String, Option<String>, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT name,template_id,slot,agg,unit,pinned_at FROM metrics ORDER BY name",
        )?;
        let out = st
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub fn metric(&self, name: &str) -> Result<Option<PinnedMetric>> {
        Ok(self
            .conn
            .query_row(
                "SELECT template_id,slot,agg,unit FROM metrics WHERE name=?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?)
    }

    /// Records of a template within one epoch, oldest first — the raw material
    /// a metric aggregates.
    pub fn records_of_template_in_boot(
        &self,
        template_id: i64,
        boot_id: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT text FROM records
              WHERE template_id = ?1 AND boot_id = ?2
              ORDER BY id LIMIT ?3",
        )?;
        let out = st
            .query_map(params![template_id, boot_id, limit as i64], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------------- records ---

    pub fn append_record(&mut self, rec: &PendingRecord) -> Result<i64> {
        let mut b = self.begin_batch()?;
        let id = b.append_record(rec)?;
        let off = b.commit()?;
        self.finish_batch(off);
        Ok(id)
    }

    pub fn record(&self, id: i64) -> Result<RecordRow> {
        self.conn
            .query_row(RECORD_SELECT, params![id], map_record)
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownRecord, format!("no record {id}")))
    }

    /// Verbatim raw records for a template (§8 `get_records`).
    pub fn records_for_template(
        &self,
        template_id: i64,
        session_id: Option<i64>,
        boot_id: Option<i64>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RecordRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,first_line_id,last_line_id,line_count,stage_id,profile,
                    severity,kind,template_id,truncated,fields_json
             FROM records
             WHERE template_id=?1
               AND (?2 IS NULL OR session_id=?2)
               AND (?3 IS NULL OR boot_id=?3)
             ORDER BY id LIMIT ?4 OFFSET ?5",
        )?;
        let out = st
            .query_map(
                params![
                    template_id,
                    session_id,
                    boot_id,
                    limit as i64,
                    offset as i64
                ],
                map_record,
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Every raw line belonging to a record, verbatim and in order.
    pub fn record_lines(&self, record_id: i64) -> Result<Vec<LineRow>> {
        let r = self.record(record_id)?;
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                    truncated,continuation
             FROM raw_lines WHERE id BETWEEN ?1 AND ?2 ORDER BY id",
        )?;
        let out = st
            .query_map(params![r.first_line_id, r.last_line_id], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn record_text(&self, record_id: i64) -> Result<String> {
        Ok(self
            .record_lines(record_id)?
            .iter()
            .map(LineRow::lossy)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Records of a given kind inside one epoch — the evidence `boot_report`
    /// attaches to a `crashed` or `garbage` verdict.
    pub fn records_in_boot(
        &self,
        boot_id: i64,
        kind: Option<RecordKind>,
        limit: usize,
    ) -> Result<Vec<RecordRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,first_line_id,last_line_id,line_count,stage_id,profile,
                    severity,kind,template_id,truncated,fields_json
             FROM records WHERE boot_id=?1 AND (?2 IS NULL OR kind=?2) ORDER BY id LIMIT ?3",
        )?;
        let out = st
            .query_map(
                params![boot_id, kind.map(|k| k.as_str()), limit as i64],
                map_record,
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Give THIS connection a much bigger page cache, for a bulk ingest.
    ///
    /// An ingest walks several large indexes at once and is the one operation
    /// that can use far more cache than the steady-state 64 MB. It is asked for
    /// on the connection doing the work, for as long as it is doing it, rather
    /// than being every store's permanent floor.
    pub fn use_bulk_cache(&self) -> Result<()> {
        self.conn.pragma_update(None, "cache_size", -262_144)?;
        Ok(())
    }

    /// The connection's page-cache setting, negative meaning KiB (SQLite's own
    /// convention). Exposed so the size can be asserted rather than assumed.
    pub fn cache_size(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "cache_size", |r| r.get(0))?)
    }

    /// The last record of a kind in a boot, by stream order.
    ///
    /// `records_in_boot` takes the FIRST n and is therefore the wrong tool for
    /// "did anything happen after this?": with a cap of five, span six is
    /// invisible and the answer silently flips.
    pub fn last_record_in_boot(&self, boot_id: i64, kind: RecordKind) -> Result<Option<RecordRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,first_line_id,last_line_id,line_count,stage_id,profile,
                    severity,kind,template_id,truncated,fields_json
             FROM records WHERE boot_id=?1 AND kind=?2 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = st.query_map(params![boot_id, kind.as_str()], map_record)?;
        rows.next().transpose().map_err(Into::into)
    }

    pub fn last_line_in_boot(&self, boot_id: i64) -> Result<Option<LineRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,
                        terminator,truncated,continuation
                 FROM raw_lines WHERE boot_id=?1 ORDER BY id DESC LIMIT 1",
                params![boot_id],
                map_line,
            )
            .optional()?)
    }

    /// The last lines of an epoch, newest first.
    ///
    /// Enough to answer "did this boot reach a prompt?" without paging the whole
    /// epoch: a prompt is the last thing a board prints before it waits.
    /// The newest `limit` lines from `min_boot_id` onwards, newest first.
    ///
    /// A FLOOR, because an epoch is not a boot: `session` and `mark` epochs open
    /// without the board restarting, so "this boot's output" spans every epoch
    /// from the last actuation to now.
    /// The epoch whose recorded output covers this stream position.
    ///
    /// BY OFFSET, NOT BY ID. An actuation epoch is back-dated to the stream mark
    /// taken when the button was pressed, so once a capture reconnect opens a
    /// `session` epoch during the hook, the epoch that OWNS a position can have a
    /// higher id and a lower offset than the markers around it. Measured on the
    /// Uno-Q: epoch 631 is an empty `session` marker at offset 5605020, and the
    /// boot covering it is 633 at 5605018 -- two ids LATER. Anything that walks
    /// backwards by id to find "the epoch before this one" looks the wrong way.
    ///
    /// Only epochs that actually recorded something can cover a position; an
    /// empty marker covers nothing.
    pub fn boot_covering_offset(&self, offset: i64) -> Result<Option<BootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                        fingerprint,outcome,image_id,group_id
                   FROM boots
                  WHERE bytes > 0 AND opened_offset IS NOT NULL AND opened_offset <= ?1
                  ORDER BY opened_offset DESC, id DESC LIMIT 1",
                params![offset],
                map_boot,
            )
            .optional()?)
    }

    /// The wall time of a boot's FIRST recorded line, or None if it recorded
    /// nothing. This is when the board actually began speaking in that epoch --
    /// distinct from `opened_at`, which is when the actuation TOOL finished (up
    /// to ~10 s later on a slow off). Used to tell a partial that belongs to
    /// this boot from one a later cold boot left stale (report #21).
    pub fn boot_first_line_ts(&self, boot_id: i64) -> Result<Option<i64>> {
        let v: Option<i64> = self.conn.query_row(
            "SELECT MIN(ts_wall) FROM raw_lines WHERE boot_id = ?1",
            params![boot_id],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    pub fn tail_since_boot(&self, min_boot_id: i64, limit: usize) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,
                    terminator,truncated,continuation
             FROM raw_lines WHERE boot_id >= ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows: Vec<LineRow> = st
            .query_map(params![min_boot_id, limit as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn tail_of_boot(&self, boot_id: i64, limit: usize) -> Result<Vec<LineRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,
                    terminator,truncated,continuation
             FROM raw_lines WHERE boot_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows: Vec<LineRow> = st
            .query_map(params![boot_id, limit as i64], map_line)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Templates whose very first sighting was inside this epoch — "what is new
    /// in this boot?", which is the crash you have not seen before.
    pub fn templates_first_seen_in_boot(&self, boot_id: i64, limit: usize) -> Result<Vec<Value>> {
        let mut st = self.conn.prepare(
            "SELECT t.id, t.template_text, t.severity, o.count
             FROM templates t JOIN occurrences o
               ON o.template_id = t.id AND o.boot_id = ?1
             WHERE t.first_seen_boot = ?1
             ORDER BY t.severity ASC, o.count DESC LIMIT ?2",
        )?;
        let out = st
            .query_map(params![boot_id, limit as i64], |r| {
                Ok(serde_json::json!({
                    "template_id": r.get::<_, i64>(0)?,
                    "text": r.get::<_, String>(1)?,
                    "severity": Severity::from_i64(r.get(2)?),
                    "count": r.get::<_, i64>(3)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ----------------------------------------------------------- templates ---

    /// Persist the effect of mining one record's key line.
    ///
    /// `created` comes from the miner, which owns id allocation; the store never
    /// invents template ids, so the in-memory miner and the database cannot drift.
    #[allow(clippy::too_many_arguments)]
    pub fn note_template(
        &mut self,
        t: &Template,
        created: bool,
        changed: bool,
        session_id: i64,
        boot_id: Option<i64>,
        ts: i64,
        stage: Option<&str>,
        profile: &str,
        severity: Severity,
    ) -> Result<()> {
        let mut b = self.begin_batch()?;
        b.note_template(
            t, created, changed, session_id, boot_id, ts, stage, profile, severity,
        )?;
        let off = b.commit()?;
        self.finish_batch(off);
        Ok(())
    }

    pub fn template(&self, id: i64) -> Result<TemplateRow> {
        self.conn
            .query_row(TEMPLATE_SELECT, params![id], map_template)
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownTemplate, format!("no template {id}")))
    }

    /// The table of contents (§8 `list_templates`).
    pub fn list_templates(&self, q: &TemplateQuery) -> Result<Vec<TemplateRow>> {
        let (core, order) = template_core_sql(q);
        let sql = format!("{core} ORDER BY {order} LIMIT ?7 OFFSET ?8");
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params![
                q.session_id,
                q.boot_id,
                q.stage,
                q.min_severity.map(|s| s as i64),
                q.new_only as i64,
                q.min_count,
                q.limit as i64,
                q.offset as i64,
                q.not_in_boot,
            ],
            |r| {
                let mut t = map_template(r)?;
                t.scoped_count = Some(r.get("cnt")?);
                t.scoped_first_ts = r.get("first_ts")?;
                t.scoped_last_ts = r.get("last_ts")?;
                t.verdict = r
                    .get::<_, Option<String>>("verdict")?
                    .and_then(|s| Verdict::parse(&s).ok());
                t.verdict_note = r.get("vnote")?;
                t.verdict_ticket = r.get("vticket")?;
                Ok(t)
            },
        )?;
        let out = rows.collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// How many templates a query matches, ignoring `limit`/`offset`.
    ///
    /// Called twice by the tool layer — once with the verdict filter and once
    /// without — so a response can say exactly how many rows a `benign` verdict
    /// removed. Hiding rows silently would make the table of contents lie by
    /// omission, which is worse than being long.
    pub fn count_templates(&self, q: &TemplateQuery) -> Result<i64> {
        let (core, _) = template_core_sql(q);
        let sql = format!("SELECT count(*) FROM ({core})");
        Ok(self.conn.query_row(
            &sql,
            params![
                q.session_id,
                q.boot_id,
                q.stage,
                q.min_severity.map(|s| s as i64),
                q.new_only as i64,
                q.min_count,
                // Unused by the count, but the placeholders must still bind.
                q.limit as i64,
                q.offset as i64,
                q.not_in_boot,
            ],
            |r| r.get(0),
        )?)
    }

    pub fn template_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM templates", [], |r| r.get(0))?)
    }

    /// Timeline buckets for `template_detail`: `buckets` equal-width slots across
    /// the scope's time span, each holding the number of hits.
    pub fn template_timeline(
        &self,
        template_id: i64,
        session_id: Option<i64>,
        buckets: usize,
    ) -> Result<Vec<(i64, i64)>> {
        let buckets = buckets.max(1);
        let mut st = self.conn.prepare(
            "SELECT r.id, l.ts_wall FROM records r
             JOIN raw_lines l ON l.id = r.first_line_id
             WHERE r.template_id=?1 AND (?2 IS NULL OR r.session_id=?2)
             ORDER BY l.ts_wall",
        )?;
        let times: Vec<i64> = st
            .query_map(params![template_id, session_id], |r| r.get::<_, i64>(1))?
            .collect::<std::result::Result<_, _>>()?;
        if times.is_empty() {
            return Ok(Vec::new());
        }
        let (lo, hi) = (times[0], *times.last().unwrap());
        let span = (hi - lo).max(1);
        let mut out = vec![0i64; buckets];
        for t in &times {
            let idx = (((t - lo) as i128 * buckets as i128) / span as i128) as usize;
            out[idx.min(buckets - 1)] += 1;
        }
        Ok(out
            .into_iter()
            .enumerate()
            .map(|(i, c)| (lo + (span * i as i64) / buckets as i64, c))
            .collect())
    }

    /// Load every template into a miner, so a restarted process keeps stable ids.
    pub fn load_drain(&self, cfg: DrainConfig, rules: TokenizerRules) -> Result<Drain> {
        let mut st = self
            .conn
            .prepare("SELECT id,tokens_json,total_count,head_only FROM templates ORDER BY id")?;
        let rows = st.query_map([], |r| {
            let tokens_json: String = r.get(1)?;
            Ok(Template {
                id: r.get::<_, i64>(0)? as u64,
                tokens: serde_json::from_str(&tokens_json).unwrap_or_default(),
                count: r.get::<_, i64>(2)? as u64,
                head_only: r.get::<_, i64>(3)? != 0,
            })
        })?;
        let templates: Vec<Template> = rows.collect::<std::result::Result<_, _>>()?;
        Ok(Drain::from_templates(cfg, rules, templates))
    }

    /// Regenerate every template from the stored records (§6).
    ///
    /// This is how a `mine.similarity` change is applied retroactively, and it is
    /// the executable proof that templates are a derived view: the raw store is
    /// not touched, and the result is a pure function of it.
    pub fn rebuild_templates(
        &mut self,
        cfg: DrainConfig,
        profiles: &crate::framer::ProfileSet,
    ) -> Result<usize> {
        // Key line of each minable record, in stream order.
        let mut st = self.conn.prepare(
            "SELECT r.id, r.session_id, r.boot_id, r.stage_id, r.profile, r.severity,
                    l.bytes, l.ts_wall
             FROM records r JOIN raw_lines l ON l.id = r.first_line_id
             WHERE r.kind IN ('line','crash')
             ORDER BY r.id",
        )?;
        struct Item {
            record_id: i64,
            session_id: i64,
            boot_id: Option<i64>,
            stage: Option<i64>,
            profile: String,
            severity: i64,
            text: String,
            ts: i64,
        }
        let items: Vec<Item> = st
            .query_map([], |r| {
                let bytes: Vec<u8> = r.get(6)?;
                Ok(Item {
                    record_id: r.get(0)?,
                    session_id: r.get(1)?,
                    boot_id: r.get(2)?,
                    stage: r.get(3)?,
                    profile: r.get(4)?,
                    severity: r.get(5)?,
                    text: String::from_utf8_lossy(&bytes).into_owned(),
                    ts: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(st);

        let stage_names = self.stage_names()?;
        let mut drain = Drain::new(cfg);

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Carry human judgements and learned expectations across the re-mint.
        //
        // Rebuild deletes and re-mints every template row, and two tables key on
        // template_id: `template_verdicts` (a human's standing judgement) and
        // `expectations` (what a baseline learned to expect). Deleting templates
        // under them failed outright with "FOREIGN KEY constraint failed", which
        // made rebuild impossible on any device that had ever been triaged --
        // and rebuild is the RECOVERY PATH for a miner improvement, so that also
        // meant an improved tokenizer could never be applied to captures already
        // on disk.
        //
        // TEXT is the stable identity across a re-mine; ids are not. So the
        // annotations are re-attached by matching the new template whose text is
        // the same. A verdict that finds no home is REPORTED, never dropped in
        // silence: a lost `known_bad` is a real regression, not housekeeping.
        struct Carried {
            text: String,
            verdict: String,
            note: Option<String>,
            ticket: Option<String>,
            author: Option<String>,
            updated_at: i64,
        }
        let carried: Vec<Carried> = {
            let mut q = tx.prepare(
                "SELECT t.template_text, v.verdict, v.note, v.ticket, v.author, v.updated_at
                 FROM template_verdicts v JOIN templates t ON t.id = v.template_id",
            )?;
            let rows = q.query_map([], |r| {
                Ok(Carried {
                    text: r.get(0)?,
                    verdict: r.get(1)?,
                    note: r.get(2)?,
                    ticket: r.get(3)?,
                    author: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        // PINNED METRICS ARE CARRIED THE SAME WAY, and for the same reason.
        //
        // §F3's `pin_metric` added a fifth table referencing templates(id) and
        // this rebuild was never taught about it, so `DELETE FROM templates`
        // hit the constraint and took the WHOLE rebuild down. Measured on the
        // ADP, which had one metric pinned: `rebuild_templates` returned
        // INTERNAL "FOREIGN KEY constraint failed" and no rebuild was possible
        // on that board at all, while the IQ10 (nothing pinned) rebuilt fine --
        // so the failure looked like a property of the store rather than of the
        // feature that had been used on it.
        //
        // The slot is carried too: a metric names a wildcard by TOKEN INDEX, and
        // a re-mine can move it. Re-attaching to the same text at the old index
        // would silently start reading a different number.
        struct CarriedMetric {
            name: String,
            text: String,
            slot: i64,
            agg: String,
            unit: Option<String>,
            pinned_at: i64,
        }
        let carried_metrics: Vec<CarriedMetric> = {
            let mut q = tx.prepare(
                "SELECT m.name, t.template_text, m.slot, m.agg, m.unit, m.pinned_at
                 FROM metrics m JOIN templates t ON t.id = m.template_id",
            )?;
            let rows = q.query_map([], |r| {
                Ok(CarriedMetric {
                    name: r.get(0)?,
                    text: r.get(1)?,
                    slot: r.get(2)?,
                    agg: r.get(3)?,
                    unit: r.get(4)?,
                    pinned_at: r.get(5)?,
                })
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        tx.execute("DELETE FROM occurrences", [])?;
        tx.execute("UPDATE records SET template_id=NULL", [])?;
        // Dependents first, or the delete below hits the same constraint. These
        // are re-attached after re-mining.
        tx.execute("DELETE FROM template_verdicts", [])?;
        tx.execute("DELETE FROM metrics", [])?;
        // Expectations are derived from a baseline over template ids that are
        // about to stop existing; they are rebuilt by re-running the baseline,
        // and keeping stale rows would silently attach one template's history to
        // another.
        tx.execute("DELETE FROM expectations", [])?;
        tx.execute("DELETE FROM templates", [])?;

        // §G5. BACKFILL THE VERSION BANNERS while we are walking every record.
        //
        // Extraction happens at mining time, so a store captured before a banner
        // pattern existed -- or before the extraction ran at all -- keeps the
        // text and knows nothing about it. Measured on the ADP: `UEFI Ver :
        // 6.0.260212...KODIAKLA-1` and `QC_IMAGE_VERSION_STRING=...` sat in the
        // store as ordinary lines while provenance reported nothing running,
        // and no amount of re-reading would have found them.
        //
        // A rebuild is exactly the moment to fix that: it already has every
        // record's text in hand, and it is the operation that exists to say
        // "derived views are a pure function of the raw". Versions are a derived
        // view too.
        let banners: Vec<crate::framer::profile::VersionBanner> = profiles
            .all()
            .iter()
            .flat_map(|p| p.version_banners.iter().cloned())
            .collect();
        // Only epochs that still EXIST. A record can carry a boot_id whose row
        // is gone -- measured on the ADP, where this backfill failed the whole
        // rebuild with `FOREIGN KEY constraint failed` on the first such record.
        // Skipping those is right: there is no epoch left to attribute the
        // banner to, and taking the rebuild down with it would cost the caller
        // every template to salvage a version string.
        let live_boots: std::collections::HashSet<i64> = {
            let mut st = tx.prepare("SELECT id FROM boots")?;
            let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut versions_found = 0usize;
        for it in &items {
            let Some(boot) = it.boot_id.filter(|b| live_boots.contains(b)) else {
                continue;
            };
            for line in it.text.lines() {
                for vb in &banners {
                    if let Some((version, detail)) = vb.extract(line) {
                        tx.execute(
                            "INSERT INTO epoch_versions(boot_id,component,version,detail_json,
                                                        line_id,ts_wall)
                             VALUES(?1,?2,?3,?4,NULL,?5)
                             ON CONFLICT(boot_id,component,ts_wall) DO UPDATE SET
                                 version = excluded.version,
                                 detail_json = excluded.detail_json",
                            params![boot, vb.component, version, detail.to_string(), it.ts],
                        )?;
                        versions_found += 1;
                    }
                }
            }
        }
        let _ = versions_found;

        for it in &items {
            // Mine what the live path mined: the profile's derived key, with its
            // already-extracted prefixes removed. Re-mining the raw line instead
            // would produce a different template set from the same raw, and
            // "templates are a derived view" would stop being true.
            let profile = profiles.get(&it.profile);
            let (key, rules) = match &profile {
                Some(p) => (p.mine_key(&it.text), p.tokenizer.clone()),
                None => (it.text.clone(), TokenizerRules::default()),
            };
            let tokens = rules.tokenize(&key);
            if tokens.is_empty() {
                continue;
            }
            let m = drain.add_tokens(&tokens);
            let t = drain.template(m.template_id).expect("just mined").clone();
            let tokens = serde_json::to_string(&t.tokens).unwrap_or_else(|_| "[]".into());
            let stage = it.stage.and_then(|s| stage_names.get(&s).cloned());
            if m.created {
                tx.execute(
                    "INSERT INTO templates(id,stage,profile,template_text,tokens_json,head_only,
                                           severity,first_seen_session,first_seen_boot,first_seen_ts,
                                           total_count)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1)",
                    params![
                        t.id as i64,
                        stage,
                        it.profile,
                        t.text(),
                        tokens,
                        t.head_only as i64,
                        it.severity,
                        it.session_id,
                        it.boot_id,
                        it.ts
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE templates SET template_text=?2, tokens_json=?3, head_only=?4,
                            severity=min(severity,?5), total_count=total_count+1 WHERE id=?1",
                    params![
                        t.id as i64,
                        t.text(),
                        tokens,
                        t.head_only as i64,
                        it.severity
                    ],
                )?;
            }
            tx.execute(
                "INSERT INTO occurrences(template_id,session_id,boot_id,count,first_ts,last_ts)
                 VALUES (?1,?2,?3,1,?4,?4)
                 ON CONFLICT(template_id,session_id,boot_id)
                 DO UPDATE SET count=count+1, last_ts=?4",
                params![t.id as i64, it.session_id, it.boot_id.unwrap_or(0), it.ts],
            )?;
            tx.execute(
                "UPDATE records SET template_id=?2 WHERE id=?1",
                params![it.record_id, t.id as i64],
            )?;
        }

        // Re-attach the carried judgements by TEXT, and account for any that
        // could not be placed. A template whose text changed because the miner
        // improved (the whole point of a rebuild) may legitimately have no
        // successor -- but that must be visible, not silent.
        let mut reattached = 0usize;
        let mut orphaned: Vec<String> = Vec::new();
        for c in &carried {
            let id: Option<i64> = tx
                .query_row(
                    "SELECT id FROM templates WHERE template_text = ?1 LIMIT 1",
                    params![c.text],
                    |r| r.get(0),
                )
                .optional()?;
            match id {
                Some(id) => {
                    tx.execute(
                        "INSERT INTO template_verdicts(template_id,verdict,note,ticket,author,updated_at)
                         VALUES (?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(template_id) DO UPDATE SET
                           verdict=?2, note=?3, ticket=?4, author=?5, updated_at=?6",
                        params![id, c.verdict, c.note, c.ticket, c.author, c.updated_at],
                    )?;
                    reattached += 1;
                }
                None => orphaned.push(c.text.clone()),
            }
        }

        // The same for pinned metrics, with the SLOT re-derived rather than
        // trusted: the re-mine may have moved which token is a wildcard, and a
        // metric that keeps its old index would quietly start reporting a
        // different number. When the old slot is no longer a wildcard the
        // metric is orphaned and said so, which is the honest outcome.
        let mut metrics_orphaned: Vec<String> = Vec::new();
        for m in &carried_metrics {
            let found: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, tokens_json FROM templates WHERE template_text = ?1 LIMIT 1",
                    params![m.text],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((id, tokens_json)) = found else {
                metrics_orphaned.push(m.name.clone());
                continue;
            };
            let tokens: Vec<String> = serde_json::from_str(&tokens_json).unwrap_or_default();
            let still_a_slot = tokens
                .get(m.slot as usize)
                .is_some_and(|t| t == crate::drain::WILDCARD);
            if !still_a_slot {
                metrics_orphaned.push(m.name.clone());
                continue;
            }
            tx.execute(
                "INSERT INTO metrics(name,template_id,slot,agg,unit,pinned_at)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(name) DO UPDATE SET
                   template_id=?2, slot=?3, agg=?4, unit=?5",
                params![m.name, id, m.slot, m.agg, m.unit, m.pinned_at],
            )?;
        }
        if !carried_metrics.is_empty() {
            tracing::info!(
                carried = carried_metrics.len(),
                orphaned = metrics_orphaned.len(),
                "rebuild re-attached pinned metrics"
            );
        }

        tx.commit()?;
        if !carried.is_empty() {
            tracing::info!(
                carried = carried.len(),
                reattached,
                orphaned = orphaned.len(),
                "rebuild re-attached template verdicts"
            );
        }
        for text in &orphaned {
            // Loud, and one line each: a judgement someone made by hand has just
            // lost its subject, and they are the only one who can re-place it.
            tracing::warn!(template = %text, "verdict could not be re-attached after rebuild");
        }
        Ok(drain.len())
    }

    /// Records whose first line is at or after `from_line_id` — the record view
    /// of a `follow` increment.
    pub fn records_after_line(&self, from_line_id: i64, limit: usize) -> Result<Vec<RecordRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,first_line_id,last_line_id,line_count,stage_id,profile,
                    severity,kind,template_id,truncated,fields_json
             FROM records WHERE first_line_id >= ?1 ORDER BY id LIMIT ?2",
        )?;
        let out = st
            .query_map(params![from_line_id, limit as i64], map_record)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Stage transitions whose banner is at or after `from_line_id`.
    pub fn stages_after_line(&self, from_line_id: i64) -> Result<Vec<Value>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,profile,entered_ts,banner_line_id,boot_id
             FROM stages WHERE banner_line_id >= ?1 ORDER BY id",
        )?;
        let out = st
            .query_map(params![from_line_id], |r| {
                Ok(serde_json::json!({
                    "stage_id": r.get::<_, i64>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "profile": r.get::<_, String>(2)?,
                    "entered_ts": r.get::<_, i64>(3)?,
                    "banner_line_id": r.get::<_, Option<i64>>(4)?,
                    "boot_id": r.get::<_, Option<i64>>(5)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Epochs opened at or after a stream offset — how `follow` reports that a
    /// reset happened while it was waiting.
    pub fn boots_after_offset(&self, offset: u64) -> Result<Vec<i64>> {
        let mut st = self
            .conn
            .prepare("SELECT id FROM boots WHERE opened_offset >= ?1 ORDER BY seq")?;
        let out = st
            .query_map(params![offset as i64], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // -------------------------------------------------------------- stages ---

    pub fn append_stage(
        &mut self,
        session_id: i64,
        boot_id: Option<i64>,
        name: &str,
        profile: &str,
        entered_ts: i64,
        banner_line_id: Option<i64>,
    ) -> Result<i64> {
        let mut b = self.begin_batch()?;
        let id = b.append_stage(
            session_id,
            boot_id,
            name,
            profile,
            entered_ts,
            banner_line_id,
        )?;
        let off = b.commit()?;
        self.finish_batch(off);
        Ok(id)
    }

    pub fn stages(&self, session_id: Option<i64>, boot_id: Option<i64>) -> Result<Vec<StageRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,session_id,boot_id,name,profile,entered_ts,banner_line_id,exited_ts
             FROM stages
             WHERE (?1 IS NULL OR session_id=?1) AND (?2 IS NULL OR boot_id=?2)
             ORDER BY id",
        )?;
        let out = st
            .query_map(params![session_id, boot_id], |r| {
                Ok(StageRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    boot_id: r.get(2)?,
                    name: r.get(3)?,
                    profile: r.get(4)?,
                    entered_ts: r.get(5)?,
                    banner_line_id: r.get(6)?,
                    exited_ts: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    fn stage_names(&self) -> Result<std::collections::HashMap<i64, String>> {
        let mut st = self.conn.prepare("SELECT id,name FROM stages")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let out = rows.collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // --------------------------------------------------------------- boots ---

    pub fn open_boot(
        &mut self,
        opened_by: &str,
        label: Option<&str>,
        at: i64,
        session_id: Option<i64>,
    ) -> Result<BootRow> {
        self.open_boot_at(opened_by, label, at, session_id, None)
    }

    /// Open an epoch that starts at a KNOWN OFFSET rather than at "now".
    ///
    /// The board does not wait for the tool. A reset takes effect the moment the
    /// controller pulls the line, and the banner is on the wire while the hook
    /// is still returning -- so stamping the boundary when the call finishes puts
    /// the new boot's first words in the OLD epoch. Measured on the bench: a
    /// banner captured at offset 235299 was assigned to epoch 26 because epoch
    /// 27 was opened at 235333, thirty-four bytes later; a follow from the
    /// boundary saw nothing and a search of the new epoch found nothing.
    ///
    /// So an actuation notes where the stream stood BEFORE it acted, and the
    /// epoch begins there.
    pub fn open_boot_at(
        &mut self,
        opened_by: &str,
        label: Option<&str>,
        at: i64,
        session_id: Option<i64>,
        at_offset: Option<u64>,
    ) -> Result<BootRow> {
        let mut b = self.begin_batch()?;
        let id = b.open_boot_after(opened_by, label, at, session_id, None, at_offset, true)?;
        let off = b.commit()?;
        self.finish_batch(off);
        self.boot(id)
    }

    /// What was running in an epoch: the live entry per component (§F2).
    pub fn versions_in_boot(&self, boot_id: i64) -> Result<Vec<(String, serde_json::Value)>> {
        let mut st = self.conn.prepare(
            "SELECT component,version,detail_json,line_id,ts_wall,superseded
               FROM epoch_versions WHERE boot_id = ?1 AND superseded = 0
              ORDER BY component",
        )?;
        let rows = st
            .query_map(params![boot_id], |r| {
                let component: String = r.get(0)?;
                let detail: String = r.get(2)?;
                let mut v = serde_json::json!({
                    "version": r.get::<_, String>(1)?,
                    "line_id": r.get::<_, Option<i64>>(3)?,
                    "ts": r.get::<_, i64>(4)?,
                });
                if let (Some(o), Ok(serde_json::Value::Object(d))) = (
                    v.as_object_mut(),
                    serde_json::from_str::<serde_json::Value>(&detail),
                ) {
                    for (k, val) in d {
                        o.insert(k, val);
                    }
                }
                Ok((component, v))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Build-fingerprint tokens this epoch PRINTED (`build=`/`fp=`).
    ///
    /// A board can announce what it is running without any profile knowing how
    /// to parse its banner. The Uno-Q prints
    ///
    ///   Sirocco version 0.1.0-sirocco-unoq-appsdk (mojo ...) build=271e11b419aa852e
    ///   BUILD fp=271e11b419aa852e
    ///
    /// and no component was extracted from either, so `provenance` compared the
    /// bound image against the FIRMWARE strings it did have -- chip, uefi, xbl --
    /// and called a correctly flashed board a mismatch. The fingerprint is the
    /// part that identifies a build, and it is greppable without teaching the
    /// framer a new banner for every board on the bench.
    /// The build identities this epoch PRINTED.
    ///
    /// A board names its build in more than one shape. `build=` and `fp=` are
    /// key=value; a kernel banner writes `git f1fb57060680` with the key as a
    /// separate word. Reading only the first shape made a board that announced
    /// the exact commit look silent about it, and provenance called the epoch a
    /// mismatch against the very SHA that was bound (report #24).
    ///
    /// Conservative on purpose: a hex run counts only when an identity KEY
    /// introduces it, so addresses, hashes and timestamps elsewhere on the line
    /// are not mistaken for a build.
    pub fn build_fingerprints_in_boot(&self, boot_id: i64, limit: usize) -> Result<Vec<String>> {
        const ID_KEYS: [&str; 6] = ["build", "fp", "git", "commit", "sha", "rev"];
        // KEY=VALUE, or the key as its OWN WORD. Never a bare substring:
        // `%build %` matches "rebuild gitless", `%git %` matches "digit 5", and
        // a kernel log is full of both -- the row limit then filled with noise
        // and crowded out the line that actually carried the fingerprint.
        // Matching
        // `%build%`/`%sha%` anywhere pulled in most of a kernel log: the row
        // limit then filled with noise and crowded out the line that actually
        // carried the fingerprint.
        let mut st = self.conn.prepare(
            "SELECT bytes FROM raw_lines
              WHERE boot_id = ?1 AND (CAST(bytes AS TEXT) LIKE '%build=%'
                                   OR CAST(bytes AS TEXT) LIKE '%fp=%'
                                   OR CAST(bytes AS TEXT) LIKE '%git=%'
                                   OR CAST(bytes AS TEXT) LIKE '% git %'
                                   OR CAST(bytes AS TEXT) LIKE '%commit=%'
                                   OR CAST(bytes AS TEXT) LIKE '% commit %'
                                   OR CAST(bytes AS TEXT) LIKE '%sha=%'
                                   OR CAST(bytes AS TEXT) LIKE '% sha %'
                                   OR CAST(bytes AS TEXT) LIKE '%rev=%'
                                   OR CAST(bytes AS TEXT) LIKE '% rev %')
              ORDER BY id LIMIT ?2",
        )?;
        // LOSSY, DELIBERATELY. These are raw console bytes: a board mid-reset
        // emits NULs and half-formed UTF-8, and asking rusqlite for a `String`
        // made the whole call fail with "invalid utf-8 sequence" -- taking
        // boot_report down with it (reports #29, #30). A corrupted byte must
        // cost that byte, never the report.
        let rows = st
            .query_map(params![boot_id, limit as i64], |r| {
                r.get::<_, Vec<u8>>(0)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let is_id = |v: &str| v.len() >= 12 && v.bytes().all(|b| b.is_ascii_hexdigit());
        let mut out = Vec::new();
        for line in rows {
            // The previous word, so `git <sha>` is read the same as `git=<sha>`.
            let mut prev_key: Option<String> = None;
            for tok in line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '=')) {
                if tok.is_empty() {
                    continue;
                }
                let lower = tok.to_ascii_lowercase();
                match lower.split_once('=') {
                    Some((k, v)) => {
                        if ID_KEYS.contains(&k) && is_id(v) {
                            out.push(v.to_string());
                        }
                        prev_key = None;
                    }
                    None => {
                        if prev_key.as_deref().is_some_and(|k| ID_KEYS.contains(&k))
                            && is_id(&lower)
                        {
                            out.push(lower.clone());
                        }
                        prev_key = Some(lower);
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The epoch this store has for a given action group, if any (§F1).
    /// The epoch immediately before this one, by sequence.
    ///
    /// §G5. A board's firmware banners do not respect epoch boundaries: an
    /// epoch opens when conminer's hook runs, while the board's early output
    /// (bl2, bl31, OP-TEE, UEFI) may already have gone past. Measured on the
    /// rig, ONE boot's chain arrived split -- kernel and machine in the power
    /// epoch, everything below them in the epoch before it -- so `provenance`
    /// on either epoch showed half a chain and no way to tell that was why.
    pub fn previous_boot(&self, boot_id: i64) -> Result<Option<BootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                        fingerprint,outcome,image_id,group_id
                 FROM boots
                WHERE seq < (SELECT seq FROM boots WHERE id=?1)
                ORDER BY seq DESC LIMIT 1",
                params![boot_id],
                map_boot,
            )
            .optional()?)
    }

    /// Every epoch opened by the same action (§F1), newest first.
    pub fn boots_in_group(&self, group_id: &str) -> Result<Vec<BootRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                    fingerprint,outcome,image_id,group_id
             FROM boots WHERE group_id=?1 ORDER BY seq DESC",
        )?;
        let rows = st.query_map(params![group_id], map_boot)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn boot_in_group(&self, group_id: &str) -> Result<Option<BootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                        fingerprint,outcome,image_id,group_id
                 FROM boots WHERE group_id=?1 ORDER BY seq DESC LIMIT 1",
                params![group_id],
                map_boot,
            )
            .optional()?)
    }

    /// Tie this epoch to the sibling epochs opened by the same action (§F1).
    ///
    /// Set as its own statement rather than threaded through `open_boot`,
    /// because the group is only known once every console's epoch exists and
    /// the caller can name them together.
    pub fn set_boot_group(&mut self, boot_id: i64, group_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE boots SET group_id=?2 WHERE id=?1",
            params![boot_id, group_id],
        )?;
        Ok(())
    }

    pub fn boot(&self, id: i64) -> Result<BootRow> {
        self.conn
            .query_row(BOOT_SELECT, params![id], map_boot)
            .optional()?
            .ok_or_else(|| ToolError::new(ErrorCode::UnknownBoot, format!("no boot {id}")))
    }

    pub fn latest_boot(&self) -> Result<Option<BootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                        fingerprint,outcome,image_id,group_id
                 FROM boots ORDER BY seq DESC LIMIT 1",
                [],
                map_boot,
            )
            .optional()?)
    }

    /// Drop every extracted version, leaving the raw lines untouched.
    ///
    /// The state of a store filled before a banner pattern existed. Exposed so
    /// §G5's backfill can be tested against that shape rather than asserted.
    pub fn forget_versions_for_test(&self) -> Result<()> {
        self.conn.execute("DELETE FROM epoch_versions", [])?;
        Ok(())
    }

    pub fn list_boots(&self, limit: usize) -> Result<Vec<BootRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,
                    fingerprint,outcome,image_id,group_id
             FROM boots ORDER BY seq DESC LIMIT ?1",
        )?;
        let out = st
            .query_map(params![limit as i64], map_boot)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn set_boot_summary(
        &mut self,
        boot_id: i64,
        fingerprint: Option<&str>,
        outcome: Option<&str>,
    ) -> Result<()> {
        let mut b = self.begin_batch()?;
        b.set_boot_summary(boot_id, fingerprint, outcome)?;
        let off = b.commit()?;
        self.finish_batch(off);
        Ok(())
    }

    /// The ordered template-id sequence and stage timeline an epoch produced —
    /// the input to its semantic fingerprint (§8.4).
    pub fn boot_signature(&self, boot_id: i64) -> Result<(Vec<i64>, Vec<String>)> {
        let mut st = self.conn.prepare(
            "SELECT template_id FROM records
             WHERE boot_id=?1 AND template_id IS NOT NULL ORDER BY id",
        )?;
        let templates: Vec<i64> = st
            .query_map(params![boot_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let stages = self
            .stages(None, Some(boot_id))?
            .into_iter()
            .map(|s| s.name)
            .collect();
        Ok((templates, stages))
    }

    // ------------------------------------------------------------- prompts ---

    /// Record a prompt expectation (§8.5). `provenance` is `profile`,
    /// `configured` or `learned`; observation counts accumulate so a prompt the
    /// device has actually settled at outranks one merely declared.
    pub fn learn_prompt(
        &mut self,
        pattern: &str,
        kind: &str,
        provenance: &str,
        stage: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO prompts(pattern,kind,provenance,stage,observations,last_seen)
             VALUES (?1,?2,?3,?4,1,?5)
             ON CONFLICT(pattern,stage) DO UPDATE SET
                kind=?2, provenance=?3, observations=observations+1, last_seen=?5",
            params![pattern, kind, provenance, stage, now],
        )?;
        Ok(())
    }

    /// Prompt expectations, most-observed first.
    /// Record that a prompt pattern was CONFIRMED on this device (§F7).
    ///
    /// Called when `run_command` asserts a prompt and the board answers, which
    /// is the strongest evidence there is: not "this pattern looks like a
    /// prompt" but "this device sat at it and took a command". A pattern that
    /// came from a profile is materialised as a device-scoped `learned` row, so
    /// the knowledge belongs to the board rather than to the guess that found
    /// it -- and `console_state` reads the same table, which is the whole point:
    /// two subsystems used to own prompt knowledge separately, and the one that
    /// asserted prompts successfully never told the one that reported them.
    pub fn observe_prompt(
        &mut self,
        pattern: &str,
        kind: &str,
        stage: Option<&str>,
        at: i64,
        boot_id: Option<i64>,
    ) -> Result<()> {
        // UPDATE first, INSERT only if nothing matched. `ON CONFLICT(pattern,
        // stage)` cannot be used here: SQLite treats NULLs as DISTINCT in a
        // UNIQUE index, so a stage-less prompt never conflicts with itself and
        // every confirmation would insert another row -- the counter would sit
        // at 1 forever while the table filled with duplicates.
        let updated = self.conn.execute(
            "UPDATE prompts
                SET observations = observations + 1, last_seen = ?3, last_boot_id = ?4
              WHERE pattern = ?1 AND stage IS ?2",
            params![pattern, stage, at, boot_id],
        )?;
        if updated == 0 {
            self.conn.execute(
                "INSERT INTO prompts(pattern,kind,provenance,stage,observations,last_seen,
                                     last_boot_id)
                 VALUES(?1,?2,'learned',?3,1,?4,?5)",
                params![pattern, kind, stage, at, boot_id],
            )?;
        }
        Ok(())
    }

    /// Lines stored since a wall-clock instant — the console's recent talkativeness.
    pub fn lines_since(&self, ts_wall: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM raw_lines WHERE ts_wall >= ?1",
            params![ts_wall],
            |r| r.get(0),
        )?)
    }

    pub fn prompts(&self, stage: Option<&str>) -> Result<Vec<PromptRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,pattern,kind,provenance,stage,observations,last_seen
             FROM prompts WHERE (?1 IS NULL OR stage = ?1 OR stage IS NULL)
             ORDER BY observations DESC, id",
        )?;
        let out = st
            .query_map(params![stage], |r| {
                Ok(PromptRow {
                    id: r.get(0)?,
                    pattern: r.get(1)?,
                    kind: r.get(2)?,
                    provenance: r.get(3)?,
                    stage: r.get(4)?,
                    observations: r.get(5)?,
                    last_seen: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Attach a derived annotation to a record (§15.5). Never touches raw.
    pub fn annotate(&mut self, record_id: i64, kind: &str, data: &Value, now: i64) -> Result<i64> {
        self.record(record_id)?;
        self.conn.execute(
            "INSERT INTO annotations(record_id,kind,data_json,made_at) VALUES (?1,?2,?3,?4)",
            params![record_id, kind, data.to_string(), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn annotations(&self, record_id: i64) -> Result<Vec<(String, Value)>> {
        let mut st = self
            .conn
            .prepare("SELECT kind,data_json FROM annotations WHERE record_id=?1 ORDER BY id")?;
        let out = st
            .query_map(params![record_id], |r| {
                let raw: String = r.get(1)?;
                Ok((
                    r.get::<_, String>(0)?,
                    serde_json::from_str(&raw).unwrap_or(Value::Null),
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------------ verdicts ---

    /// Record (or clear) an agent's standing judgement about a template.
    ///
    /// This is the memory that stops the table of contents from being re-triaged
    /// from scratch every session. Passing `None` for the verdict removes it, so
    /// a mistaken call is reversible without a second tool.
    pub fn set_verdict(
        &mut self,
        template_id: i64,
        verdict: Option<Verdict>,
        note: Option<&str>,
        ticket: Option<&str>,
        author: Option<&str>,
        now: i64,
    ) -> Result<()> {
        // Fails loudly on an unknown id rather than storing a verdict about a
        // template that does not exist.
        self.template(template_id)?;
        match verdict {
            None => {
                self.conn.execute(
                    "DELETE FROM template_verdicts WHERE template_id=?1",
                    params![template_id],
                )?;
            }
            Some(v) => {
                self.conn.execute(
                    "INSERT INTO template_verdicts
                       (template_id,verdict,note,ticket,author,updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(template_id) DO UPDATE SET
                       verdict=excluded.verdict, note=excluded.note,
                       ticket=excluded.ticket, author=excluded.author,
                       updated_at=excluded.updated_at",
                    params![template_id, v.as_str(), note, ticket, author, now],
                )?;
            }
        }
        Ok(())
    }

    pub fn verdict(&self, template_id: i64) -> Result<Option<VerdictRow>> {
        let mut st = self.conn.prepare(
            "SELECT template_id,verdict,note,ticket,author,updated_at
             FROM template_verdicts WHERE template_id=?1",
        )?;
        let mut rows = st.query_map(params![template_id], map_verdict)?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }

    /// Every stored verdict, newest first.
    pub fn verdicts(&self, only: Option<Verdict>) -> Result<Vec<VerdictRow>> {
        let mut st = self.conn.prepare(
            "SELECT template_id,verdict,note,ticket,author,updated_at
             FROM template_verdicts
             WHERE (?1 IS NULL OR verdict = ?1)
             ORDER BY updated_at DESC, template_id ASC",
        )?;
        let out = st
            .query_map(params![only.map(|v| v.as_str())], map_verdict)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Template ids carrying a given verdict — the allowlist `evaluate_policy`
    /// no longer has to be handed on every call.
    pub fn templates_with_verdict(&self, v: Verdict) -> Result<BTreeSet<i64>> {
        let mut st = self
            .conn
            .prepare("SELECT template_id FROM template_verdicts WHERE verdict=?1")?;
        let out = st
            .query_map(params![v.as_str()], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ----------------------------------------------------------- baselines ---

    /// Bless an epoch as the reference point for "what is new".
    pub fn set_baseline(
        &mut self,
        name: &str,
        boot_id: i64,
        note: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.boot(boot_id)?;
        self.conn.execute(
            "INSERT INTO baselines(name,boot_id,note,set_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(name) DO UPDATE SET
               boot_id=excluded.boot_id, note=excluded.note, set_at=excluded.set_at",
            params![name, boot_id, note, now],
        )?;
        Ok(())
    }

    pub fn baseline(&self, name: &str) -> Result<Option<BaselineRow>> {
        let mut st = self
            .conn
            .prepare("SELECT name,boot_id,note,set_at FROM baselines WHERE name=?1")?;
        let mut rows = st.query_map(params![name], map_baseline)?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }

    pub fn baselines(&self) -> Result<Vec<BaselineRow>> {
        let mut st = self
            .conn
            .prepare("SELECT name,boot_id,note,set_at FROM baselines ORDER BY name")?;
        let out = st
            .query_map([], map_baseline)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn clear_baseline(&mut self, name: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM baselines WHERE name=?1", params![name])?;
        Ok(n > 0)
    }

    /// The distinct template ids that fired in an epoch.
    pub fn templates_in_boot(&self, boot_id: i64) -> Result<BTreeSet<i64>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT template_id FROM occurrences WHERE boot_id=?1")?;
        let out = st
            .query_map(params![boot_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------------- watches ---

    /// Create or replace a durable watch, starting from `from_offset`.
    pub fn create_watch(
        &mut self,
        name: &str,
        predicate: &Value,
        from_offset: u64,
        now: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO watches(name,predicate_json,created_at,scanned_to,active)
             VALUES (?1,?2,?3,?4,1)
             ON CONFLICT(name) DO UPDATE SET
               predicate_json=excluded.predicate_json,
               created_at=excluded.created_at,
               scanned_to=excluded.scanned_to,
               active=1",
            params![name, predicate.to_string(), now, from_offset as i64],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM watches WHERE name=?1", params![name], |r| {
                r.get(0)
            })?)
    }

    /// §K4. Arm (or disarm) push delivery for a watch.
    pub fn set_watch_notify(
        &mut self,
        name: &str,
        url: Option<&str>,
        secret: Option<&str>,
        min_interval_s: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE watches SET notify_url=?2, notify_secret=?3, notify_min_interval_s=?4
             WHERE name=?1",
            params![name, url, secret, min_interval_s],
        )?;
        Ok(())
    }

    /// Watches with push armed, for the delivery sweep.
    pub fn watches_to_deliver(&self) -> Result<Vec<ArmedWatch>> {
        let mut st = self.conn.prepare(
            "SELECT name, notify_url, notify_secret, notify_min_interval_s, delivered_to,
                    COALESCE(last_delivery_at, 0), COALESCE(delivery_fail_streak, 0)
               FROM watches
              WHERE active=1 AND notify_url IS NOT NULL",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ArmedWatch {
                name: r.get(0)?,
                url: r.get(1)?,
                secret: r.get(2)?,
                min_interval_s: r.get(3)?,
                last_delivery_at: r.get(5)?,
                failed_streak: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Firings this watch has not yet had ACKNOWLEDGED by its receiver.
    ///
    /// Deliberately keyed on its own high-water mark rather than on
    /// `poll_watch`'s: push is best-effort notification and polling is the
    /// source of truth, so neither may consume the other's work.
    pub fn undelivered_hits(&self, name: &str, limit: usize) -> Result<Vec<Value>> {
        let mut st = self.conn.prepare(
            "SELECT h.id, h.at, h.matched, h.evidence_json
               FROM watch_hits h JOIN watches w ON w.id = h.watch_id
              WHERE w.name = ?1 AND h.id > w.delivered_to
              ORDER BY h.id LIMIT ?2",
        )?;
        let rows = st.query_map(params![name, limit as i64], |r| {
            let ev: String = r.get(3)?;
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "at": r.get::<_, i64>(1)?,
                "matched": r.get::<_, Option<String>>(2)?,
                "evidence": serde_json::from_str::<Value>(&ev).unwrap_or(Value::Null),
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Record the outcome of one delivery attempt.
    ///
    /// A firing is marked delivered ONLY on a 2xx. Anything else leaves it in
    /// the queue: a receiver that was down must not cost the operator the
    /// evidence, and `poll_watch` still returns every firing regardless.
    pub fn note_delivery(
        &mut self,
        name: &str,
        status: Option<u16>,
        up_to_hit: Option<i64>,
        now: i64,
    ) -> Result<()> {
        let ok = status.is_some_and(|s| (200..300).contains(&s));
        if ok {
            self.conn.execute(
                "UPDATE watches SET delivered = delivered + 1, last_status = ?2,
                        last_delivery_at = ?3, delivered_to = MAX(delivered_to, ?4),
                        delivery_fail_streak = 0
                  WHERE name = ?1",
                params![name, status.map(|s| s as i64), now, up_to_hit.unwrap_or(0)],
            )?;
        } else {
            self.conn.execute(
                "UPDATE watches SET delivery_failed = delivery_failed + 1, last_status = ?2,
                        last_delivery_at = ?3,
                        delivery_fail_streak = COALESCE(delivery_fail_streak, 0) + 1
                  WHERE name = ?1",
                params![name, status.map(|s| s as i64), now],
            )?;
        }
        Ok(())
    }

    pub fn watch(&self, name: &str) -> Result<WatchRow> {
        let mut st = self.conn.prepare(
            "SELECT id,name,predicate_json,created_at,scanned_to,last_polled,active,
                    notify_url,notify_secret,notify_min_interval_s,delivered,delivery_failed,
                    last_status,last_delivery_at
             FROM watches WHERE name=?1",
        )?;
        let mut rows = st.query_map(params![name], map_watch)?;
        match rows.next() {
            Some(r) => Ok(r?),
            None => Err(ToolError::new(
                ErrorCode::UnknownWatch,
                format!("no watch named {name:?}"),
            )),
        }
    }

    pub fn watches(&self) -> Result<Vec<WatchRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,predicate_json,created_at,scanned_to,last_polled,active,
                    notify_url,notify_secret,notify_min_interval_s,delivered,delivery_failed,
                    last_status,last_delivery_at
             FROM watches ORDER BY id",
        )?;
        let out = st
            .query_map([], map_watch)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn delete_watch(&mut self, name: &str) -> Result<bool> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM watches WHERE name=?1", params![name], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(id) = id else { return Ok(false) };
        self.conn
            .execute("DELETE FROM watch_hits WHERE watch_id=?1", params![id])?;
        self.conn
            .execute("DELETE FROM watches WHERE id=?1", params![id])?;
        Ok(true)
    }

    /// Record newly observed firings and advance the watch's scan position, in
    /// one transaction. If this fails, the watch re-scans the same span rather
    /// than skipping it: a duplicate hit is recoverable, a lost one is not.
    pub fn record_watch_hits(
        &mut self,
        watch_id: i64,
        hits: &[WatchHit],
        scanned_to: u64,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut st = tx.prepare(
                "INSERT INTO watch_hits(watch_id,at,stream_offset,matched,evidence_json)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            for h in hits {
                st.execute(params![
                    watch_id,
                    h.at,
                    h.stream_offset as i64,
                    h.matched,
                    h.evidence.to_string()
                ])?;
            }
        }
        tx.execute(
            "UPDATE watches SET scanned_to=?2, last_polled=?3 WHERE id=?1",
            params![watch_id, scanned_to as i64, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Undelivered hits, oldest first. `mark` also flips them to delivered, so a
    /// poll is a consuming read and an agent never sees the same firing twice.
    /// Everything this watch has EVER fired, delivered or not (§F6).
    ///
    /// `pending` answers "is there something to read?"; this answers "did
    /// anything happen at all since I set this up?", which is the cheap
    /// morning-after question a soak run wants.
    pub fn watch_fired_total(&self, watch_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM watch_hits WHERE watch_id=?1",
            params![watch_id],
            |r| r.get(0),
        )?)
    }

    /// Undelivered hits for a NAMED watch, without consuming them (§F6).
    ///
    /// Read-only on purpose: `follow` uses this to decide whether to return, and
    /// delivery stays with `poll_watch` so a firing cannot be silently eaten by
    /// a follow that happened to be parked.
    pub fn peek_watch_hits(&self, name: &str, limit: usize) -> Result<Vec<WatchHit>> {
        let Some(id) = self
            .conn
            .query_row("SELECT id FROM watches WHERE name=?1", params![name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        else {
            return Ok(Vec::new());
        };
        let mut st = self.conn.prepare(
            "SELECT id,at,stream_offset,matched,evidence_json
             FROM watch_hits WHERE watch_id=?1 AND delivered=0
             ORDER BY id LIMIT ?2",
        )?;
        let out = st
            .query_map(params![id, limit as i64], |r| {
                Ok(WatchHit {
                    at: r.get(1)?,
                    stream_offset: r.get::<_, i64>(2)? as u64,
                    matched: r.get(3)?,
                    evidence: serde_json::from_str(&r.get::<_, String>(4)?)
                        .unwrap_or(serde_json::Value::Null),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub fn take_watch_hits(
        &mut self,
        watch_id: i64,
        limit: usize,
        mark: bool,
    ) -> Result<(Vec<WatchHit>, usize)> {
        let pending: i64 = self.conn.query_row(
            "SELECT count(*) FROM watch_hits WHERE watch_id=?1 AND delivered=0",
            params![watch_id],
            |r| r.get(0),
        )?;
        let mut st = self.conn.prepare(
            "SELECT id,at,stream_offset,matched,evidence_json
             FROM watch_hits WHERE watch_id=?1 AND delivered=0
             ORDER BY id LIMIT ?2",
        )?;
        let rows: Vec<(i64, WatchHit)> = st
            .query_map(params![watch_id, limit as i64], |r| {
                let raw: String = r.get(4)?;
                Ok((
                    r.get(0)?,
                    WatchHit {
                        at: r.get(1)?,
                        stream_offset: r.get::<_, i64>(2)? as u64,
                        matched: r.get(3)?,
                        evidence: serde_json::from_str(&raw).unwrap_or(Value::Null),
                    },
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        if mark {
            for (id, _) in &rows {
                self.conn
                    .execute("UPDATE watch_hits SET delivered=1 WHERE id=?1", params![id])?;
            }
        }
        let remaining = (pending as usize).saturating_sub(rows.len());
        Ok((rows.into_iter().map(|(_, h)| h).collect(), remaining))
    }

    // -------------------------------------------------------- expectations ---

    /// Every template that fired in an epoch, with when it first did.
    ///
    /// Distinct per template on purpose: a line printed forty times in one boot
    /// is one observation of "this boot contained it", not forty.
    pub fn template_first_ts_in_boot(&self, boot_id: i64) -> Result<Vec<(i64, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT template_id, MIN(first_ts) FROM occurrences
             WHERE boot_id=?1 GROUP BY template_id",
        )?;
        let out = st
            .query_map(params![boot_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Replace the learned skeleton wholesale.
    ///
    /// Learning is always a fresh fit over a chosen reference set, never an
    /// incremental update: merging a new fit into an old one would quietly mix
    /// two different definitions of "normal", which is the one thing an absence
    /// report cannot survive.
    pub fn replace_expectations(
        &mut self,
        rows: &[(i64, i64, Option<i64>)],
        reference_boots: i64,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM expectations", [])?;
        {
            let mut st = tx.prepare(
                "INSERT INTO expectations
                   (template_id,stage,seen_in,reference_boots,median_ordinal,
                    median_offset_ms,learned_at)
                 SELECT ?1, t.stage, ?2, ?3, NULL, ?4, ?5 FROM templates t WHERE t.id=?1",
            )?;
            for (tid, seen_in, median) in rows {
                st.execute(params![tid, seen_in, reference_boots, median, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn expectations(&self) -> Result<Vec<crate::absence::Expectation>> {
        let mut st = self.conn.prepare(
            "SELECT e.template_id, t.template_text, e.stage, e.seen_in, e.reference_boots,
                    e.median_offset_ms
             FROM expectations e JOIN templates t ON t.id = e.template_id
             ORDER BY e.seen_in DESC, e.template_id",
        )?;
        let out = st
            .query_map([], |r| {
                let seen_in: i64 = r.get(3)?;
                let reference_boots: i64 = r.get(4)?;
                Ok(crate::absence::Expectation {
                    template_id: r.get(0)?,
                    text: r.get(1)?,
                    stage: r.get(2)?,
                    seen_in,
                    reference_boots,
                    reliability: if reference_boots > 0 {
                        seen_in as f64 / reference_boots as f64
                    } else {
                        0.0
                    },
                    median_offset_ms: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------------- bisect ----

    pub fn create_bisect(
        &mut self,
        name: &str,
        candidates: &[String],
        predicate: &Value,
        now: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO bisects(name,candidates_json,predicate_json,started_at,state)
             VALUES (?1,?2,?3,?4,'running')
             ON CONFLICT(name) DO UPDATE SET
               candidates_json=excluded.candidates_json,
               predicate_json=excluded.predicate_json,
               started_at=excluded.started_at,
               finished_at=NULL, culprit=NULL, state='running'",
            params![
                name,
                serde_json::to_string(candidates).unwrap_or_default(),
                predicate.to_string(),
                now
            ],
        )?;
        let id: i64 =
            self.conn
                .query_row("SELECT id FROM bisects WHERE name=?1", params![name], |r| {
                    r.get(0)
                })?;
        // A re-started bisect must not inherit the previous run's verdicts.
        self.conn
            .execute("DELETE FROM bisect_results WHERE bisect_id=?1", params![id])?;
        Ok(id)
    }

    pub fn bisect(&self, name: &str) -> Result<crate::bisect::Bisect> {
        let mut st = self.conn.prepare(
            "SELECT id,name,candidates_json,predicate_json,started_at,finished_at,culprit,state
             FROM bisects WHERE name=?1",
        )?;
        let mut rows = st.query_map(params![name], |r| {
            let cands: String = r.get(2)?;
            let pred: String = r.get(3)?;
            Ok(crate::bisect::Bisect {
                id: r.get(0)?,
                name: r.get(1)?,
                candidates: serde_json::from_str(&cands).unwrap_or_default(),
                predicate: serde_json::from_str(&pred).unwrap_or(Value::Null),
                started_at: r.get(4)?,
                finished_at: r.get(5)?,
                culprit: r.get(6)?,
                state: r.get(7)?,
                results: Vec::new(),
            })
        })?;
        let mut b = match rows.next() {
            Some(r) => r?,
            None => {
                return Err(ToolError::new(
                    ErrorCode::UnknownBisect,
                    format!("no bisect named {name:?}"),
                ))
            }
        };
        drop(rows);
        drop(st);
        b.results = self.bisect_results(b.id)?;
        Ok(b)
    }

    pub fn bisects(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT name FROM bisects ORDER BY started_at DESC")?;
        let out = st
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    fn bisect_results(&self, id: i64) -> Result<Vec<crate::bisect::BisectResult>> {
        let mut st = self.conn.prepare(
            "SELECT idx,verdict,boot_id,at,note FROM bisect_results
             WHERE bisect_id=?1 ORDER BY idx",
        )?;
        let out = st
            .query_map(params![id], |r| {
                Ok(crate::bisect::BisectResult {
                    idx: r.get::<_, i64>(0)? as usize,
                    verdict: r.get(1)?,
                    boot_id: r.get(2)?,
                    at: r.get(3)?,
                    note: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    pub fn record_bisect_result(
        &mut self,
        id: i64,
        idx: usize,
        verdict: &str,
        boot_id: Option<i64>,
        note: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bisect_results(bisect_id,idx,verdict,boot_id,at,note)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(bisect_id,idx) DO UPDATE SET
               verdict=excluded.verdict, boot_id=excluded.boot_id,
               at=excluded.at, note=excluded.note",
            params![id, idx as i64, verdict, boot_id, now, note],
        )?;
        Ok(())
    }

    pub fn finish_bisect(
        &mut self,
        id: i64,
        state: &str,
        culprit: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE bisects SET state=?2, culprit=?3, finished_at=?4 WHERE id=?1",
            params![id, state, culprit, now],
        )?;
        Ok(())
    }

    // ----------------------------------------------------------- evidence ----

    /// Timeline events in stream order, optionally filtered by kind prefix.
    ///
    /// Ascending, unlike [`Self::events`]: a timeline read backwards is not a
    /// timeline.
    pub fn timeline_events(
        &self,
        boot_id: Option<i64>,
        kind_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,at,stream_offset,kind,data_json FROM events
             WHERE (?1 IS NULL OR boot_id=?1)
               AND (?2 IS NULL OR kind LIKE ?2 || '%')
             ORDER BY at ASC, id ASC LIMIT ?3",
        )?;
        let out = st
            .query_map(params![boot_id, kind_prefix, limit as i64], |r| {
                let raw: String = r.get(4)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get(3)?,
                    serde_json::from_str(&raw).unwrap_or(Value::Null),
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// The epoch whose span contains `at`, so evidence from another tool lands
    /// on the right boot without the caller having to work it out.
    pub fn boot_at(&self, at: i64) -> Result<Option<BootRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,
                    bytes,fingerprint,outcome,image_id,group_id
             FROM boots
             WHERE opened_at <= ?1 AND (closed_at IS NULL OR closed_at >= ?1)
             ORDER BY opened_at DESC LIMIT 1",
        )?;
        let mut rows = st.query_map(params![at], map_boot)?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }

    /// The build that is *supposed* to be on the board, and how conminer knows.
    ///
    /// Two paths reach the same claim and both count: a flash hook reporting
    /// what it pushed, and an operator binding a build with `set_image`. Reading
    /// only the hook would make provenance silently uncheckable on every lab
    /// that flashes by hand, which is most of them during bring-up.
    ///
    /// The most recent claim wins, whichever path produced it.
    pub fn intended_image(&self) -> Result<Option<Value>> {
        let hook: Option<(i64, i64, Value)> = self
            .conn
            .query_row(
                "SELECT id,at,data_json FROM events WHERE kind='flash'
                 ORDER BY at DESC, id DESC LIMIT 1",
                [],
                |r| {
                    let raw: String = r.get(2)?;
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        serde_json::from_str(&raw).unwrap_or(Value::Null),
                    ))
                },
            )
            .optional()?;

        let bound: Option<BoundImage> = self
            .conn
            .query_row(
                "SELECT id,bound_at,name,git_sha,image_hash FROM images
                 ORDER BY bound_at DESC, id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;

        let hook_at = hook.as_ref().map(|(_, at, _)| *at).unwrap_or(i64::MIN);
        let bound_at = bound.as_ref().map(|(_, at, ..)| *at).unwrap_or(i64::MIN);

        Ok(match (hook, bound) {
            (None, None) => None,
            (Some((id, at, data)), _) if hook_at >= bound_at => Some(json!({
                "via": "flash_hook",
                // The hook records what it was asked to push.
                "ref": data.get("image").and_then(Value::as_str),
                "at": at, "event_id": id, "detail": data,
            })),
            (_, Some((id, at, name, sha, hash))) => Some(json!({
                "via": "set_image",
                "ref": name.clone().or_else(|| sha.clone()).or_else(|| hash.clone()),
                "at": at, "image_id": id,
                "detail": {"name": name, "git_sha": sha, "image_hash": hash},
            })),
            _ => None,
        })
    }

    pub fn image(&self, id: i64) -> Result<Value> {
        let mut st = self.conn.prepare(
            "SELECT id,name,git_sha,image_hash,source,meta_json,bound_at FROM images WHERE id=?1",
        )?;
        let mut rows = st.query_map(params![id], |r| {
            let meta: String = r.get(5)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, Option<String>>(1)?,
                "git_sha": r.get::<_, Option<String>>(2)?,
                "image_hash": r.get::<_, Option<String>>(3)?,
                "source": r.get::<_, String>(4)?,
                "meta": serde_json::from_str::<Value>(&meta).unwrap_or(Value::Null),
                "bound_at": r.get::<_, i64>(6)?,
            }))
        })?;
        match rows.next() {
            Some(r) => Ok(r?),
            None => Err(ToolError::new(
                ErrorCode::UnknownArgument,
                format!("no image {id}"),
            )),
        }
    }

    // -------------------------------------------------------------- images ---

    /// Bind a build identity, deduplicating on (name, sha, hash) (§15.4).
    pub fn bind_image(
        &mut self,
        name: Option<&str>,
        git_sha: Option<&str>,
        image_hash: Option<&str>,
        source: &str,
        meta: &Value,
        now: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO images(name,git_sha,image_hash,source,meta_json,bound_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(name,git_sha,image_hash) DO UPDATE SET bound_at=?6",
            params![name, git_sha, image_hash, source, meta.to_string(), now],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM images WHERE name IS ?1 AND git_sha IS ?2 AND image_hash IS ?3",
            params![name, git_sha, image_hash],
            |r| r.get(0),
        )?)
    }

    pub fn set_boot_image(&mut self, boot_id: i64, image_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE boots SET image_id=?2 WHERE id=?1",
            params![boot_id, image_id],
        )?;
        Ok(())
    }

    /// Resolve a build reference — a name, a git sha, or an image hash — to the
    /// epochs it produced. Ambiguity is an error, never a guess.
    pub fn boots_for_image(&self, reference: &str) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare(
            "SELECT b.id FROM boots b JOIN images i ON i.id = b.image_id
             WHERE i.name = ?1 OR i.git_sha = ?1 OR i.image_hash = ?1
                OR i.git_sha LIKE ?1 || '%'
             ORDER BY b.seq",
        )?;
        let out = st
            .query_map(params![reference], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    /// Template counts aggregated across a set of epochs.
    pub fn template_counts_for_boots(
        &self,
        boots: &[i64],
    ) -> Result<std::collections::BTreeMap<i64, (String, i64)>> {
        let mut out = std::collections::BTreeMap::new();
        for b in boots {
            let mut st = self.conn.prepare_cached(
                "SELECT o.template_id, t.template_text, o.count
                 FROM occurrences o JOIN templates t ON t.id = o.template_id
                 WHERE o.boot_id = ?1",
            )?;
            let rows = st.query_map(params![b], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (id, text, n) = row?;
                let e = out.entry(id).or_insert((text, 0i64));
                e.1 += n;
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------- events ---

    pub fn append_event(
        &mut self,
        session_id: Option<i64>,
        boot_id: Option<i64>,
        at: i64,
        kind: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events(session_id,boot_id,at,stream_offset,kind,data_json)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                session_id,
                boot_id,
                at,
                self.stream_offset as i64,
                kind,
                data.to_string()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn events(
        &self,
        boot_id: Option<i64>,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,at,stream_offset,kind,data_json FROM events
             WHERE (?1 IS NULL OR boot_id=?1) AND (?2 IS NULL OR kind=?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let out = st
            .query_map(params![boot_id, kind, limit as i64], |r| {
                let raw: String = r.get(4)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get(3)?,
                    serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null),
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(out)
    }

    // ------------------------------------------------------------ retention --

    /// Prune raw lines below an offset. Templates and rollups are kept forever —
    /// they are tiny, and they are the whole point.
    ///
    /// Returns the number of lines removed. Cursors that pointed into the pruned
    /// region now fail as `CURSOR_EXPIRED` rather than silently returning less.
    pub fn prune_before(&mut self, offset: u64) -> Result<usize> {
        if offset <= self.pruned_before {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ids: Vec<i64> = {
            let mut st =
                tx.prepare("SELECT id FROM raw_lines WHERE stream_offset < ?1 ORDER BY id")?;
            let v = st
                .query_map(params![offset as i64], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        if !ids.is_empty() {
            let last = ids[ids.len() - 1];
            // Records and stages that referenced pruned lines lose their raw
            // backing. The template, its counts and the stage *timeline* survive,
            // which is exactly the retention contract of §7: raw ages out, the
            // table of contents does not.
            tx.execute(
                "UPDATE stages SET banner_line_id=NULL WHERE banner_line_id <= ?1",
                params![last],
            )?;
            tx.execute(
                "DELETE FROM records WHERE first_line_id <= ?1 OR last_line_id <= ?1",
                params![last],
            )?;
            if self.fts {
                let mut del = tx.prepare(
                    "INSERT INTO raw_fts(raw_fts,rowid,text) VALUES('delete',?1,
                        (SELECT CAST(bytes AS TEXT) FROM raw_lines WHERE id=?1))",
                )?;
                for id in &ids {
                    // Best-effort: a contentless index that has already lost the
                    // row is not an error worth failing a prune over.
                    let _ = del.execute(params![id]);
                }
            }
            tx.execute(
                "DELETE FROM raw_lines WHERE stream_offset < ?1",
                params![offset as i64],
            )?;
        }
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key='pruned_before_offset'",
            params![offset.to_string()],
        )?;
        tx.commit()?;
        self.pruned_before = offset;
        Ok(ids.len())
    }

    /// The oldest offset that must survive a prune (§F9).
    ///
    /// Baseline epochs and exported sessions are evidence somebody deliberately
    /// kept; ageing them out silently would break the comparison they exist for.
    pub fn protected_offset(&self) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT MIN(b.opened_offset) FROM boots b
                  WHERE b.id IN (SELECT boot_id FROM baselines)",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(v.map(|o| o as u64))
    }

    /// The stream offset at a wall-clock instant, for age-based pruning.
    pub fn offset_at_ts(&self, ts_wall: i64) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT MIN(stream_offset) FROM raw_lines WHERE ts_wall >= ?1",
                params![ts_wall],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(v.map(|o| o as u64))
    }

    /// Bytes of raw currently stored.
    pub fn raw_bytes(&self) -> Result<u64> {
        let v: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(length(bytes)),0) FROM raw_lines",
            [],
            |r| r.get(0),
        )?;
        Ok(v as u64)
    }

    /// The oldest wall-clock timestamp still backed by raw bytes.
    pub fn oldest_raw_ts(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MIN(ts_wall) FROM raw_lines", [], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// Enforce a size cap by pruning the oldest lines until the raw store fits.
    pub fn enforce_size_cap(&mut self, cap_bytes: u64) -> Result<usize> {
        let live: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(length(bytes)),0) FROM raw_lines",
            [],
            |r| r.get(0),
        )?;
        if (live as u64) <= cap_bytes {
            return Ok(0);
        }
        let excess = live as u64 - cap_bytes;
        // Find the offset that drops at least `excess` bytes.
        let mut st = self
            .conn
            .prepare("SELECT stream_offset, length(bytes) FROM raw_lines ORDER BY stream_offset")?;
        let mut acc = 0u64;
        let mut cut = self.pruned_before;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (off, len) = row?;
            acc += len;
            cut = off + len;
            if acc >= excess {
                break;
            }
        }
        drop(st);
        self.prune_before(cut)
    }

    // --------------------------------------------------------------- stats ---

    pub fn stats(&self, session_id: Option<i64>) -> Result<Stats> {
        let (lines, bytes): (i64, i64) = self.conn.query_row(
            "SELECT count(*), COALESCE(SUM(length(bytes)),0) FROM raw_lines
             WHERE (?1 IS NULL OR session_id=?1)",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let records: i64 = self.conn.query_row(
            "SELECT count(*) FROM records WHERE (?1 IS NULL OR session_id=?1)",
            params![session_id],
            |r| r.get(0),
        )?;
        let templates: i64 = match session_id {
            Some(s) => self.conn.query_row(
                "SELECT count(DISTINCT template_id) FROM occurrences WHERE session_id=?1",
                params![s],
                |r| r.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT count(*) FROM templates", [], |r| r.get(0))?,
        };
        let sessions: i64 = self
            .conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?;
        let db_bytes: i64 = self.conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )?;

        let drain = self.load_drain(DrainConfig::default(), TokenizerRules::default())?;
        Ok(Stats {
            sessions,
            lines,
            records,
            templates,
            bytes,
            compression_ratio: if templates > 0 {
                lines as f64 / templates as f64
            } else {
                0.0
            },
            fragmentation_ratio: drain.fragmentation_ratio(),
            stream_offset: self.stream_offset,
            pruned_before_offset: self.pruned_before,
            db_bytes,
            fts_enabled: self.fts,
        })
    }

    /// Re-extract version banners from stored raw, touching nothing else (§K5b).
    ///
    /// The cheap half of a rebuild. `rebuild_templates` also refreshes these,
    /// but it re-mints every template and every id along with them -- a heavy
    /// price, and one an operator should not have to pay to answer "what was
    /// running on that boot last month". This walks the raw lines once, applies
    /// ONLY the version extractors, and writes `epoch_versions`. No template is
    /// created, altered or renumbered.
    ///
    /// Epochs whose raw has been pruned contribute nothing and are REPORTED as
    /// such: the answer "there is no longer anything to read there" is different
    /// from "that boot printed no version", and a caller who cannot tell them
    /// apart will re-run this forever waiting for a different result.
    pub fn backfill_versions(
        &mut self,
        banners: &[crate::framer::profile::VersionBanner],
    ) -> Result<serde_json::Value> {
        let horizon = self.pruned_before;
        // Every epoch, with the offset window its raw occupies.
        let boots: Vec<(i64, u64)> = {
            let mut st = self
                .conn
                .prepare("SELECT id, opened_offset FROM boots ORDER BY seq")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut skipped_pruned: Vec<i64> = Vec::new();
        let mut scanned = 0usize;
        let mut found = 0usize;
        let mut touched: BTreeSet<i64> = BTreeSet::new();

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut st = tx.prepare(
                "SELECT l.id, l.boot_id, l.bytes, l.ts_wall, l.stream_offset
                   FROM raw_lines l
                  WHERE l.boot_id IS NOT NULL
                  ORDER BY l.id",
            )?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let line_id: i64 = r.get(0)?;
                let boot_id: i64 = r.get(1)?;
                let bytes: Vec<u8> = r.get(2)?;
                let ts: i64 = r.get(3)?;
                scanned += 1;
                let text = String::from_utf8_lossy(&bytes);
                for vb in banners {
                    if let Some((version, detail)) = vb.extract(&text) {
                        tx.execute(
                            "INSERT INTO epoch_versions(boot_id,component,version,detail_json,
                                                        line_id,ts_wall)
                             VALUES(?1,?2,?3,?4,?5,?6)
                             ON CONFLICT(boot_id,component,ts_wall) DO UPDATE SET
                                 version = excluded.version,
                                 detail_json = excluded.detail_json,
                                 line_id = excluded.line_id",
                            params![
                                boot_id,
                                vb.component,
                                version,
                                detail.to_string(),
                                line_id,
                                ts
                            ],
                        )?;
                        found += 1;
                        touched.insert(boot_id);
                    }
                }
            }
        }
        tx.commit()?;

        // An epoch that begins before the prune horizon has lost the bytes this
        // would have read.
        for (id, off) in &boots {
            if horizon > 0 && *off < horizon && !touched.contains(id) {
                skipped_pruned.push(*id);
            }
        }
        Ok(serde_json::json!({
            "lines_scanned": scanned,
            "versions_written": found,
            "epochs_filled": touched.len(),
            "epochs": touched.iter().copied().collect::<Vec<_>>(),
            "skipped_pruned": skipped_pruned,
            "note": "version extractions only: no template was created, altered or renumbered",
        }))
    }

    /// Export a session as a single portable archive (§14.7).
    ///
    /// Format is deliberately trivial so it survives being emailed, attached to
    /// a bug report, or read by a human with `zcat`:
    ///
    /// ```text
    /// CONMINER-EXPORT-1 {"session":…,"device":…,"source":…}\n
    /// <the session's raw bytes, verbatim>
    /// ```
    ///
    /// gzip-compressed. `ingest_file` recognises the magic and restores the
    /// metadata, so export → import is a byte-exact round trip.
    pub fn export_session(&self, session_id: i64, w: &mut impl std::io::Write) -> Result<u64> {
        let s = self.session(session_id)?;
        let header = serde_json::json!({
            "session": s.id,
            "device": self.canonical,
            "source": s.source,
            "label": s.label,
            "started_at": s.started_at,
            "ended_at": s.ended_at,
            "lines": s.lines,
            "bytes": s.bytes,
            "schema": super::device_schema_version(),
        });
        let mut gz = flate2::write::GzEncoder::new(w, flate2::Compression::default());
        writeln!(gz, "{EXPORT_MAGIC} {header}")?;
        let mut n = 0u64;
        for l in self.lines_for_session(session_id)? {
            gz.write_all(&l.bytes)?;
            gz.write_all(l.terminator.raw())?;
            n += l.bytes.len() as u64 + l.terminator.raw().len() as u64;
        }
        gz.finish()?;
        Ok(n)
    }

    /// Cap the database size in pages.
    ///
    /// Operationally this is a guard that turns "the volume filled up" into a
    /// loud `STORAGE_FULL` at a threshold we choose, rather than a surprise at
    /// 100%. It is also how the §13 `store` suite exercises the disk-full path
    /// without filling a real disk.
    pub fn limit_pages(&self, pages: i64) -> Result<()> {
        self.conn.pragma_update(None, "max_page_count", pages)?;
        Ok(())
    }

    pub fn page_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?)
    }

    /// `PRAGMA integrity_check` — the crash-safety assertion of §12.4.
    pub fn integrity_check(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?)
    }

    /// Checkpoint the WAL so a volume snapshot is a complete backup (§14.10).
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Escape hatch for the search tiers and tests. Read-only by convention.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

// ----------------------------------------------------------------- batch -----

/// A write batch: one SQLite transaction covering every table a block of input
/// touches.
///
/// This is the difference between an 800 MB ingest taking seconds and taking
/// hours. Committing per record means one fsync-class operation per line; the
/// batch turns a whole read block into a single commit, which is exactly what
/// `capture.commit_interval_ms` describes as the durability window.
pub struct Batch<'a> {
    tx: rusqlite::Transaction<'a>,
    fts: bool,
    offset: u64,
    /// (session_id, bytes, lines, records) accumulated for one UPDATE at commit.
    counters: std::collections::BTreeMap<i64, (i64, i64, i64)>,
    boot_bytes: std::collections::BTreeMap<i64, i64>,
}

impl<'a> Batch<'a> {
    /// Record what a version banner said about a component this epoch (§F2).
    ///
    /// The newest wins and earlier ones are marked `superseded`: an epoch can
    /// legitimately see a component twice (kexec, or the boundary lag that puts
    /// firmware output in the previous epoch), and "what is running" means the
    /// last one, not the first.
    pub fn note_version(
        &mut self,
        boot_id: i64,
        component: &str,
        version: &str,
        detail: &serde_json::Value,
        line_id: Option<i64>,
        ts_wall: i64,
    ) -> Result<()> {
        self.tx.execute(
            "UPDATE epoch_versions SET superseded = 1
              WHERE boot_id = ?1 AND component = ?2 AND ts_wall < ?3",
            params![boot_id, component, ts_wall],
        )?;
        self.tx.execute(
            "INSERT INTO epoch_versions(boot_id,component,version,detail_json,line_id,ts_wall)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(boot_id,component,ts_wall) DO UPDATE SET
                 version = excluded.version, detail_json = excluded.detail_json",
            params![
                boot_id,
                component,
                version,
                detail.to_string(),
                line_id,
                ts_wall
            ],
        )?;
        Ok(())
    }

    fn count(&mut self, session_id: i64, bytes: i64, lines: i64, records: i64) {
        let e = self.counters.entry(session_id).or_insert((0, 0, 0));
        e.0 += bytes;
        e.1 += lines;
        e.2 += records;
    }

    /// Append raw lines verbatim, assigning stream offsets.
    pub fn append_lines(
        &mut self,
        session_id: i64,
        boot_id: Option<i64>,
        lines: &[PendingLine<'_>],
    ) -> Result<Vec<LineRef>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let mut refs = Vec::with_capacity(lines.len());
        let mut bytes_added: i64 = 0;
        {
            let mut ins = self.tx.prepare_cached(
                "INSERT INTO raw_lines(session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,
                                       bytes,terminator,truncated,continuation)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            let mut fts = if self.fts {
                Some(
                    self.tx
                        .prepare_cached("INSERT INTO raw_fts(rowid,text) VALUES (?1,?2)")?,
                )
            } else {
                None
            };
            for l in lines {
                ins.execute(params![
                    session_id,
                    boot_id,
                    self.offset as i64,
                    l.ts_mono,
                    l.ts_wall,
                    l.stage_id,
                    l.bytes,
                    l.terminator.as_str(),
                    l.truncated as i64,
                    l.continuation as i64,
                ])?;
                let id = self.tx.last_insert_rowid();
                if let Some(f) = fts.as_mut() {
                    // Indexed lossily; the bytes stay findable through the
                    // tier-2/3 raw scan even when they are not valid UTF-8 (§8.1).
                    f.execute(params![id, String::from_utf8_lossy(l.bytes)])?;
                }
                refs.push(LineRef {
                    id,
                    stream_offset: self.offset,
                });
                let consumed = l.consumed();
                self.offset += consumed;
                bytes_added += consumed as i64;
            }
        }
        self.count(session_id, bytes_added, lines.len() as i64, 0);
        if let Some(b) = boot_id {
            *self.boot_bytes.entry(b).or_insert(0) += bytes_added;
        }
        Ok(refs)
    }

    pub fn append_record(&mut self, rec: &PendingRecord) -> Result<i64> {
        {
            let mut ins = self.tx.prepare_cached(
                "INSERT INTO records(session_id,boot_id,first_line_id,last_line_id,line_count,
                                     stage_id,profile,severity,kind,template_id,truncated,fields_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            ins.execute(params![
                rec.session_id,
                rec.boot_id,
                rec.first_line_id,
                rec.last_line_id,
                rec.line_count,
                rec.stage_id,
                rec.profile,
                rec.severity as i64,
                rec.kind.as_str(),
                rec.template_id,
                rec.truncated as i64,
                serde_json::to_string(&rec.fields).unwrap_or_else(|_| "{}".into()),
            ])?;
        }
        let id = self.tx.last_insert_rowid();
        if self.fts {
            let mut f = self
                .tx
                .prepare_cached("INSERT INTO record_fts(rowid,text) VALUES (?1,?2)")?;
            f.execute(params![id, rec.text])?;
        }
        self.count(rec.session_id, 0, 0, 1);
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn note_template(
        &mut self,
        t: &Template,
        created: bool,
        changed: bool,
        session_id: i64,
        boot_id: Option<i64>,
        ts: i64,
        stage: Option<&str>,
        profile: &str,
        severity: Severity,
    ) -> Result<()> {
        if created {
            let tokens = serde_json::to_string(&t.tokens).unwrap_or_else(|_| "[]".into());
            self.tx.prepare_cached(
                "INSERT INTO templates(id,stage,profile,template_text,tokens_json,head_only,severity,
                                       first_seen_session,first_seen_boot,first_seen_ts,total_count)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1)",
            )?.execute(params![
                t.id as i64, stage, profile, t.text(), tokens,
                t.head_only as i64, severity as i64, session_id, boot_id, ts
            ])?;
        } else if changed {
            // The template gained a wildcard: rewrite its text and tokens.
            let tokens = serde_json::to_string(&t.tokens).unwrap_or_else(|_| "[]".into());
            self.tx
                .prepare_cached(
                    "UPDATE templates
                    SET template_text=?2, tokens_json=?3, head_only=?4,
                        severity=min(severity, ?5), total_count=total_count+1
                  WHERE id=?1",
                )?
                .execute(params![
                    t.id as i64,
                    t.text(),
                    tokens,
                    t.head_only as i64,
                    severity as i64
                ])?;
        } else {
            // The overwhelmingly common case: a line matched an existing
            // template unchanged. Serialising the token list and rewriting the
            // row for every one of them is pure waste on a hot path that runs
            // once per line of an 800 MB ingest.
            self.tx
                .prepare_cached(
                    "UPDATE templates SET severity=min(severity, ?2), total_count=total_count+1
                  WHERE id=?1",
                )?
                .execute(params![t.id as i64, severity as i64])?;
        }
        self.tx
            .prepare_cached(
                "INSERT INTO occurrences(template_id,session_id,boot_id,count,first_ts,last_ts)
             VALUES (?1,?2,?3,1,?4,?4)
             ON CONFLICT(template_id,session_id,boot_id)
             DO UPDATE SET count=count+1, last_ts=?4",
            )?
            .execute(params![t.id as i64, session_id, boot_id.unwrap_or(0), ts])?;
        Ok(())
    }

    /// Append a stage transition. `close_prev` is the stage row this one
    /// supersedes; passing it avoids re-finding the open stage, which on a board
    /// that has looped thousands of times is the difference between a linear and
    /// a quadratic ingest.
    #[allow(clippy::too_many_arguments)]
    pub fn append_stage_after(
        &mut self,
        session_id: i64,
        boot_id: Option<i64>,
        name: &str,
        profile: &str,
        entered_ts: i64,
        banner_line_id: Option<i64>,
        close_prev: Option<i64>,
    ) -> Result<i64> {
        match close_prev {
            Some(id) => {
                self.tx
                    .prepare_cached("UPDATE stages SET exited_ts=?2 WHERE id=?1")?
                    .execute(params![id, entered_ts])?;
            }
            None => {
                self.tx.execute(
                    "UPDATE stages SET exited_ts=?2 WHERE session_id=?1 AND exited_ts IS NULL",
                    params![session_id, entered_ts],
                )?;
            }
        }
        self.tx
            .prepare_cached(
                "INSERT INTO stages(session_id,boot_id,name,profile,entered_ts,banner_line_id)
             VALUES (?1,?2,?3,?4,?5,?6)",
            )?
            .execute(params![
                session_id,
                boot_id,
                name,
                profile,
                entered_ts,
                banner_line_id
            ])?;
        Ok(self.tx.last_insert_rowid())
    }

    pub fn append_stage(
        &mut self,
        session_id: i64,
        boot_id: Option<i64>,
        name: &str,
        profile: &str,
        entered_ts: i64,
        banner_line_id: Option<i64>,
    ) -> Result<i64> {
        self.tx.execute(
            "UPDATE stages SET exited_ts=?2 WHERE session_id=?1 AND exited_ts IS NULL",
            params![session_id, entered_ts],
        )?;
        self.tx.execute(
            "INSERT INTO stages(session_id,boot_id,name,profile,entered_ts,banner_line_id)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                session_id,
                boot_id,
                name,
                profile,
                entered_ts,
                banner_line_id
            ],
        )?;
        Ok(self.tx.last_insert_rowid())
    }

    /// Open a new epoch inside the batch (§8.4). `close_prev` names the epoch
    /// being superseded; without it the close has to find the open epoch by
    /// scanning, which is quadratic on a looping board.
    #[allow(clippy::too_many_arguments)]
    pub fn open_boot_after(
        &mut self,
        opened_by: &str,
        label: Option<&str>,
        at: i64,
        session_id: Option<i64>,
        close_prev: Option<i64>,
        // `at_offset`: where the epoch really starts. Defaults to the batch
        // head, but a reset discovered mid-block starts at its *banner*, not at
        // the end of the block the banner happened to arrive in.
        at_offset: Option<u64>,
        // Re-stamp the lines that fall inside the new epoch's range?
        //
        // ONLY THE ACTUATION PATH WANTS THIS. The pipeline opens mid-block
        // epochs and then reassigns their tail itself, so claiming here as well
        // moved the same lines twice and debited the previous epoch twice --
        // which the epochs suite caught as "epoch 1 recorded no bytes". An
        // epoch opened by `power` has no such follow-up: nobody else is going to
        // give it the lines the board produced while the hook ran.
        claim_range: bool,
    ) -> Result<i64> {
        let next_seq: i64 = self
            .tx
            .prepare_cached("SELECT COALESCE(MAX(seq),0)+1 FROM boots")?
            .query_row([], |r| r.get(0))?;
        match close_prev {
            Some(id) => {
                self.tx
                    .prepare_cached("UPDATE boots SET closed_at=?2 WHERE id=?1")?
                    .execute(params![id, at])?;
            }
            None => {
                self.tx.execute(
                    "UPDATE boots SET closed_at=?1 WHERE closed_at IS NULL",
                    params![at],
                )?;
            }
        }
        self.tx
            .prepare_cached(
                "INSERT INTO boots(seq,session_id,label,opened_by,opened_at,opened_offset)
             VALUES (?1,?2,?3,?4,?5,?6)",
            )?
            .execute(params![
                next_seq,
                session_id,
                label,
                opened_by,
                at,
                at_offset.unwrap_or(self.offset) as i64
            ])?;
        let new_id = self.tx.last_insert_rowid();

        // AN EPOCH THAT STARTS BEHIND THE HEAD MUST CLAIM WHAT IT CONTAINS.
        //
        // Moving the boundary back is only half of the attribution: every line
        // already written carries the `boot_id` it was stamped with as it
        // arrived, so the banner the reset caused still pointed at the epoch the
        // reset ENDED. Reported from the bench after the boundary fix landed:
        // the cursor path worked, and an epoch-35 filtered search still returned
        // zero because the records said 34.
        //
        // So the lines, records and stages that fall inside the new epoch's
        // range are re-stamped to it -- and only those: strictly at or after the
        // boundary, and only from the epoch that was open when it moved.
        if let Some(start) = at_offset.filter(|o| claim_range && *o < self.offset) {
            // EVERY EPOCH THAT OVERLAPS THE RANGE, NOT JUST THE NEWEST ONE.
            //
            // This used to claim only from `prev` -- the single most recent
            // epoch -- on the assumption that nothing else could have opened
            // while the hook ran. A capture reconnect opens a `session` epoch,
            // and a power hook is not brief: the Bughopper holds its line for
            // seconds and verification follows, so a `power` call can span more
            // than a minute. Anything that reconnects in that window opens an
            // epoch, and now two of them hold lines the actuation is entitled
            // to.
            //
            // Measured on the Uno-Q, epochs 542-547: 546 (`power`) marked the
            // head at 5027650 before its hook; while the hook ran, session
            // epochs 544 (17,355 bytes -- the boot 546 had just caused) and 545
            // opened. When 546 finally opened, `prev` was 545, which was empty,
            // so NOTHING matched and 546 recorded 0 bytes. The boot then fell to
            // 547, the NEXT power call, whose provenance reported a fingerprint
            // from a boot it never caused. Across this one device that left 41
            // epochs starting behind their predecessor and 165 sharing an
            // offset.
            //
            // The offset bound is the real guard and it is sufficient: `start`
            // is the head at the moment the button was pressed, so every line at
            // or after it arrived afterwards and belongs to this actuation, no
            // matter which epoch was current when it was written.
            // THE EXACT BYTES OF THE LINES THAT ACTUALLY MOVE.
            //
            // `boots.bytes` is a counter incremented as lines are written to
            // whichever epoch was current, so re-stamping without adjusting it
            // leaves the new epoch claiming 433 lines and zero bytes -- reported
            // from the bench as one response saying `bytes_this_boot: 0` beside
            // 17,742 bytes of that boot's own output.
            //
            // Measured per line rather than as the offset span: the span from
            // the boundary to the head also covers anything in that range that
            // did NOT move (a line belonging to an older epoch), and debiting
            // the previous epoch for those emptied it -- which the epochs suite
            // caught as "epoch 1 recorded no bytes".
            // Per DONOR, because each one has to be debited for exactly what it
            // loses. A single total was enough when there could only be one.
            let mut donors: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
            let mut moved_bytes: i64 = 0;
            {
                let mut q = self.tx.prepare(
                    "SELECT boot_id, bytes, terminator FROM raw_lines
                     WHERE stream_offset >= ?1 AND (boot_id IS NULL OR boot_id <> ?2)",
                )?;
                let rows = q.query_map(params![start as i64, new_id], |r| {
                    let owner: Option<i64> = r.get(0)?;
                    let bytes: Vec<u8> = r.get(1)?;
                    let term: String = r.get(2)?;
                    Ok((
                        owner,
                        bytes.len() as i64 + parse_terminator(&term).raw().len() as i64,
                    ))
                })?;
                for row in rows {
                    let (owner, n) = row?;
                    moved_bytes += n;
                    if let Some(o) = owner {
                        *donors.entry(o).or_insert(0) += n;
                    }
                }
            }
            let moved = self.tx.execute(
                "UPDATE raw_lines SET boot_id=?1
                 WHERE stream_offset >= ?2 AND (boot_id IS NULL OR boot_id <> ?1)",
                params![new_id, start as i64],
            )?;
            if moved > 0 {
                for (donor, n) in &donors {
                    *self.boot_bytes.entry(*donor).or_insert(0) -= *n;
                }
                *self.boot_bytes.entry(new_id).or_insert(0) += moved_bytes;
                // Records and stages hang off those lines; leaving them behind
                // would make `boot_report` and `list_templates` disagree with a
                // raw search of the same epoch.
                self.tx.execute(
                    "UPDATE records SET boot_id=?1 WHERE first_line_id IN
                     (SELECT id FROM raw_lines WHERE boot_id=?1)",
                    params![new_id],
                )?;
                self.tx.execute(
                    "UPDATE stages SET boot_id=?1 WHERE banner_line_id IN
                     (SELECT id FROM raw_lines WHERE boot_id=?1)",
                    params![new_id],
                )?;
            }
        }
        Ok(new_id)
    }

    /// Open a new epoch inside the batch (§8.4).
    pub fn open_boot(
        &mut self,
        opened_by: &str,
        label: Option<&str>,
        at: i64,
        session_id: Option<i64>,
    ) -> Result<i64> {
        let next_seq: i64 =
            self.tx
                .query_row("SELECT COALESCE(MAX(seq),0)+1 FROM boots", [], |r| r.get(0))?;
        self.tx.execute(
            "UPDATE boots SET closed_at=?1 WHERE closed_at IS NULL",
            params![at],
        )?;
        self.tx.execute(
            "INSERT INTO boots(seq,session_id,label,opened_by,opened_at,opened_offset)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                next_seq,
                session_id,
                label,
                opened_by,
                at,
                self.offset as i64
            ],
        )?;
        Ok(self.tx.last_insert_rowid())
    }

    pub fn set_boot_summary(
        &mut self,
        boot_id: i64,
        fingerprint: Option<&str>,
        outcome: Option<&str>,
    ) -> Result<()> {
        self.tx.execute(
            "UPDATE boots SET fingerprint=COALESCE(?2,fingerprint), outcome=COALESCE(?3,outcome)
             WHERE id=?1",
            params![boot_id, fingerprint, outcome],
        )?;
        Ok(())
    }

    /// Move the tail of a session's lines into a newly opened epoch.
    ///
    /// Raw lines are persisted *before* they are framed (capture never waits on
    /// interpretation), so a reset discovered mid-block would otherwise leave
    /// every line of that block attributed to the previous epoch — and
    /// `boot_report` would then say `no_output` about a boot that produced
    /// plenty. The reset's own banner line is the boundary.
    pub fn reassign_tail_to_boot(
        &mut self,
        session_id: i64,
        from_line_id: i64,
        old_boot: Option<i64>,
        new_boot: i64,
    ) -> Result<i64> {
        let from_offset = self.offset_of_line(from_line_id)?;
        let from_offset = from_offset as i64;
        // Offsets are contiguous, so the moved span is exactly this wide.
        let moved = self.offset as i64 - from_offset;
        self.tx
            .prepare_cached("UPDATE raw_lines SET boot_id=?3 WHERE session_id=?1 AND id>=?2")?
            .execute(params![session_id, from_line_id, new_boot])?;
        if let Some(ob) = old_boot {
            *self.boot_bytes.entry(ob).or_insert(0) -= moved;
        }
        *self.boot_bytes.entry(new_boot).or_insert(0) += moved;
        Ok(moved)
    }

    /// Read the ordered template/stage signature of an epoch from inside the
    /// batch, so a fingerprint reflects the records this batch just wrote.
    pub fn boot_signature(&self, boot_id: i64) -> Result<(Vec<i64>, Vec<String>)> {
        let templates: Vec<i64> = {
            let mut st = self.tx.prepare_cached(
                "SELECT template_id FROM records
                 WHERE boot_id=?1 AND template_id IS NOT NULL ORDER BY id",
            )?;
            let v = st
                .query_map(params![boot_id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        let stages: Vec<String> = {
            let mut st = self
                .tx
                .prepare_cached("SELECT name FROM stages WHERE boot_id=?1 ORDER BY id")?;
            let v = st
                .query_map(params![boot_id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        Ok((templates, stages))
    }

    pub fn stream_offset(&self) -> u64 {
        self.offset
    }

    /// Where a line sits in the append-only stream.
    pub fn offset_of_line(&self, line_id: i64) -> Result<u64> {
        let o: i64 = self
            .tx
            .prepare_cached("SELECT stream_offset FROM raw_lines WHERE id=?1")?
            .query_row(params![line_id], |r| r.get(0))?;
        Ok(o as u64)
    }

    /// Flush the accumulated counters and commit. Returns the new stream offset.
    pub fn commit(self) -> Result<u64> {
        for (session_id, (bytes, lines, records)) in &self.counters {
            self.tx.execute(
                "UPDATE sessions SET bytes=bytes+?2, lines=lines+?3, records=records+?4
                 WHERE id=?1",
                params![session_id, bytes, lines, records],
            )?;
        }
        for (boot_id, bytes) in &self.boot_bytes {
            // MAX(0): an adjustment must never drive a count below nothing. A
            // negative byte total is not a number anybody can act on, and it
            // would propagate into every freshness envelope that reads it.
            self.tx.execute(
                "UPDATE boots SET bytes=MAX(0, bytes+?2) WHERE id=?1",
                params![boot_id, bytes],
            )?;
        }
        self.tx.execute(
            "UPDATE meta SET value=?1 WHERE key='stream_offset'",
            params![self.offset.to_string()],
        )?;
        let offset = self.offset;
        self.tx.commit()?;
        Ok(offset)
    }
}

// ------------------------------------------------------------- row mapping ---

const LINE_SELECT_BY_ID: &str =
    "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
            truncated,continuation
     FROM raw_lines WHERE id=?1";

const RECORD_SELECT: &str =
    "SELECT id,session_id,boot_id,first_line_id,last_line_id,line_count,stage_id,profile,severity,
            kind,template_id,truncated,fields_json
     FROM records WHERE id=?1";

const TEMPLATE_SELECT: &str =
    "SELECT id,stage,profile,template_text,tokens_json,head_only,severity,first_seen_session,
            first_seen_boot,first_seen_ts,total_count
     FROM templates WHERE id=?1";

const BOOT_SELECT: &str =
    "SELECT id,seq,session_id,label,opened_by,opened_at,opened_offset,closed_at,bytes,fingerprint,
            outcome,image_id,group_id
     FROM boots WHERE id=?1";

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn parse_terminator(s: &str) -> Terminator {
    match s {
        "lf" => Terminator::Lf,
        "crlf" => Terminator::CrLf,
        "cr" => Terminator::Cr,
        _ => Terminator::None,
    }
}

fn map_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let src: String = r.get(1)?;
    Ok(SessionRow {
        id: r.get(0)?,
        source: SessionSource::parse(&src).unwrap_or(SessionSource::File),
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        label: r.get(4)?,
        content_sha: r.get(5)?,
        source_path: r.get(6)?,
        bytes: r.get(7)?,
        lines: r.get(8)?,
        records: r.get(9)?,
    })
}

/// Row mapper exposed for the search tiers, which build their own queries over
/// `raw_lines` but must decode rows identically.
pub fn map_line_pub(r: &rusqlite::Row<'_>) -> rusqlite::Result<LineRow> {
    map_line(r)
}

fn map_line(r: &rusqlite::Row<'_>) -> rusqlite::Result<LineRow> {
    let term: String = r.get(8)?;
    Ok(LineRow {
        id: r.get(0)?,
        session_id: r.get(1)?,
        boot_id: r.get(2)?,
        stream_offset: r.get::<_, i64>(3)? as u64,
        ts_mono: r.get(4)?,
        ts_wall: r.get(5)?,
        stage_id: r.get(6)?,
        bytes: r.get(7)?,
        terminator: parse_terminator(&term),
        truncated: r.get::<_, i64>(9)? != 0,
        continuation: r.get::<_, i64>(10)? != 0,
    })
}

fn map_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecordRow> {
    let kind: String = r.get(9)?;
    let fields: Option<String> = r.get(12)?;
    Ok(RecordRow {
        id: r.get(0)?,
        session_id: r.get(1)?,
        boot_id: r.get(2)?,
        first_line_id: r.get(3)?,
        last_line_id: r.get(4)?,
        line_count: r.get(5)?,
        stage_id: r.get(6)?,
        profile: r.get(7)?,
        severity: Severity::from_i64(r.get(8)?),
        kind: RecordKind::parse(&kind),
        template_id: r.get(10)?,
        truncated: r.get::<_, i64>(11)? != 0,
        fields: fields
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or(serde_json::Value::Null),
    })
}

fn map_template(r: &rusqlite::Row<'_>) -> rusqlite::Result<TemplateRow> {
    let tokens_json: String = r.get(4)?;
    Ok(TemplateRow {
        id: r.get(0)?,
        stage: r.get(1)?,
        profile: r.get(2)?,
        text: r.get(3)?,
        tokens: serde_json::from_str(&tokens_json).unwrap_or_default(),
        head_only: r.get::<_, i64>(5)? != 0,
        severity: Severity::from_i64(r.get(6)?),
        first_seen_session: r.get(7)?,
        first_seen_boot: r.get(8)?,
        first_seen_ts: r.get(9)?,
        total_count: r.get(10)?,
        scoped_count: None,
        scoped_first_ts: None,
        scoped_last_ts: None,
        verdict: None,
        verdict_note: None,
        verdict_ticket: None,
    })
}

/// The shared body of every template query: the projection, joins and filters,
/// without `ORDER BY`/`LIMIT`.
///
/// Extracted so that counting how many templates a query matches uses byte-for-
/// byte the same predicate as listing them. Two hand-kept copies would drift,
/// and the count exists precisely so an agent can trust that what it was not
/// shown was really filtered rather than merely paged.
fn template_core_sql(q: &TemplateQuery) -> (String, &'static str) {
    let scoped = q.session_id.is_some() || q.boot_id.is_some();
    let order = match q.order {
        TemplateOrder::Count => "cnt DESC, t.id ASC",
        TemplateOrder::FirstSeen => "first_ts ASC, t.id ASC",
        TemplateOrder::LastSeen => "last_ts DESC, t.id ASC",
        TemplateOrder::Severity => "t.severity ASC, cnt DESC, t.id ASC",
    };

    // Verdict names come from a closed enum, never from caller text, so
    // splicing them is safe where a bound parameter cannot express `IN`.
    let list = |vs: &[Verdict]| {
        vs.iter()
            .map(|v| format!("'{}'", v.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    };
    let only_clause = if q.only_verdicts.is_empty() {
        String::new()
    } else {
        format!(" AND v.verdict IN ({})", list(&q.only_verdicts))
    };
    let hide_clause = if q.hide_verdicts.is_empty() {
        String::new()
    } else {
        format!(
            " AND (v.verdict IS NULL OR v.verdict NOT IN ({}))",
            list(&q.hide_verdicts)
        )
    };

    let core = format!(
        "SELECT t.id,t.stage,t.profile,t.template_text,t.tokens_json,t.head_only,t.severity,
                t.first_seen_session,t.first_seen_boot,t.first_seen_ts,t.total_count,
                {cnt} AS cnt, {first_ts} AS first_ts, {last_ts} AS last_ts,
                v.verdict AS verdict, v.note AS vnote, v.ticket AS vticket
         FROM templates t
         LEFT JOIN template_verdicts v ON v.template_id = t.id
         {join}
         WHERE (?3 IS NULL OR t.stage = ?3)
           AND (?4 IS NULL OR t.severity <= ?4)
           AND (?5 = 0 OR t.first_seen_session = ?1)
           AND (?9 IS NULL OR t.id NOT IN
                 (SELECT template_id FROM occurrences WHERE boot_id = ?9))
           {only_clause}{hide_clause}
         GROUP BY t.id
         HAVING (?6 IS NULL OR cnt >= ?6)",
        cnt = if scoped {
            "COALESCE(SUM(o.count),0)"
        } else {
            "t.total_count"
        },
        first_ts = if scoped {
            "MIN(o.first_ts)"
        } else {
            "t.first_seen_ts"
        },
        last_ts = if scoped {
            "MAX(o.last_ts)"
        } else {
            "t.first_seen_ts"
        },
        join = if scoped {
            "JOIN occurrences o ON o.template_id = t.id
               AND (?1 IS NULL OR o.session_id = ?1)
               AND (?2 IS NULL OR o.boot_id = ?2)"
        } else {
            ""
        },
    );
    (core, order)
}

fn map_verdict(r: &rusqlite::Row<'_>) -> rusqlite::Result<VerdictRow> {
    let raw: String = r.get(1)?;
    Ok(VerdictRow {
        template_id: r.get(0)?,
        // The CHECK constraint on the column already guarantees the set, so an
        // unparseable value would mean a hand-edited database; `Benign` is the
        // safe read (it hides, it never fails a gate on a corrupt row).
        verdict: Verdict::parse(&raw).unwrap_or(Verdict::Benign),
        note: r.get(2)?,
        ticket: r.get(3)?,
        author: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

fn map_baseline(r: &rusqlite::Row<'_>) -> rusqlite::Result<BaselineRow> {
    Ok(BaselineRow {
        name: r.get(0)?,
        boot_id: r.get(1)?,
        note: r.get(2)?,
        set_at: r.get(3)?,
    })
}

fn map_watch(r: &rusqlite::Row<'_>) -> rusqlite::Result<WatchRow> {
    let raw: String = r.get(2)?;
    Ok(WatchRow {
        id: r.get(0)?,
        name: r.get(1)?,
        predicate: serde_json::from_str(&raw).unwrap_or(Value::Null),
        created_at: r.get(3)?,
        scanned_to: r.get::<_, i64>(4)? as u64,
        last_polled: r.get(5)?,
        active: r.get::<_, i64>(6)? != 0,
        // §K4. The secret is NEVER echoed back -- only whether one is set. A
        // response that returns it turns every log of a tool call into a
        // credential leak.
        notify: r.get::<_, Option<String>>(7)?.map(|url| {
            serde_json::json!({
                "url": url,
                "signed": r.get::<_, Option<String>>(8).ok().flatten().is_some(),
                "min_interval_s": r.get::<_, i64>(9).unwrap_or(60),
            })
        }),
        delivery: Some(serde_json::json!({
            "delivered": r.get::<_, i64>(10).unwrap_or(0),
            "failed": r.get::<_, i64>(11).unwrap_or(0),
            "last_status": r.get::<_, Option<i64>>(12).ok().flatten(),
            "last_at": r.get::<_, Option<i64>>(13).ok().flatten(),
        })),
    })
}

fn map_boot(r: &rusqlite::Row<'_>) -> rusqlite::Result<BootRow> {
    Ok(BootRow {
        id: r.get(0)?,
        seq: r.get(1)?,
        session_id: r.get(2)?,
        label: r.get(3)?,
        opened_by: r.get(4)?,
        opened_at: r.get(5)?,
        opened_offset: r.get::<_, i64>(6)? as u64,
        closed_at: r.get(7)?,
        bytes: r.get(8)?,
        fingerprint: r.get(9)?,
        outcome: r.get(10)?,
        image_id: r.get(11)?,
        group_id: r.get(12).ok().flatten(),
    })
}
