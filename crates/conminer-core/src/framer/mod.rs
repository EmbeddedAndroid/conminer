//! Record framing (§5): turning a line stream into *records*.
//!
//! A record is one logical event — a single printk, or an entire 80-line kernel
//! oops, U-Boot exception dump, Zephyr fatal-error backtrace, TF-A panic.
//! Framing runs **before** Drain so a looping panic dedupes as one template
//! rather than eighty.
//!
//! ## The framer contract
//!
//! Framing is a partition, not a filter. Every line belongs to exactly one
//! record, records are contiguous and in stream order, and concatenating all
//! record spans reproduces the input line sequence exactly. That is the §12.1
//! "framer conservation" property, and it is what lets an agent trust that a
//! table of contents is complete.
//!
//! ## Mining keys and the no-masking rule
//!
//! Profiles may extract fields (printk timestamp, severity, cpu, module) from a
//! line. Extraction is **non-destructive**: the raw line is stored untouched and
//! the extracted values are stored beside it in `fields`. A record's `mine_key`
//! is the derived view Drain clusters on — the line with its already-extracted
//! spans removed — which keeps a per-boot timestamp from minting one template per
//! line. Nothing about the stored bytes changes, and `rebuild_templates`
//! reproduces the same keys from the same raw.

pub mod generic;
pub mod profile;
pub mod stage;

pub use profile::{Profile, ProfileFramer, ProfileSet};
pub use stage::StageMachine;

use crate::store::{RecordKind, Severity};
use serde::{Deserialize, Serialize};

/// One line offered to the framer. `line_id` is the store's row id, already
/// persisted — capture never waits on framing.
#[derive(Debug, Clone)]
pub struct FramerInput {
    pub line_id: i64,
    /// UTF-8-lossy display view. The raw bytes stay in the store.
    pub text: String,
    pub raw_len: usize,
    pub ts_wall: i64,
    pub ts_mono: i64,
    /// GARBAGE_BURST was active while this line arrived (§A.10).
    pub garbage: bool,
    /// The port is under an exclusive binary claim (§15.2): capture continues,
    /// interpretation is suspended.
    pub binary: bool,
}

impl FramerInput {
    pub fn new(line_id: i64, text: impl Into<String>, ts_wall: i64) -> Self {
        let text = text.into();
        Self {
            line_id,
            raw_len: text.len(),
            text,
            ts_wall,
            ts_mono: ts_wall * 1_000_000,
            garbage: false,
            binary: false,
        }
    }
}

/// A completed record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FramedRecord {
    pub first_line_id: i64,
    pub last_line_id: i64,
    pub line_count: i64,
    pub kind: RecordKind,
    pub severity: Severity,
    pub profile: String,
    pub stage: Option<String>,
    /// Hit `framer.max_record_lines`, or the stream ended mid-record.
    pub truncated: bool,
    /// Non-destructive extracted fields plus framer flags
    /// (`interleave_suspected`, `closed_by`, `retro_attached`).
    pub fields: serde_json::Value,
    /// What Drain clusters on. `None` means "do not mine" — garbage bursts and
    /// binary spans are quarantined so a baud mismatch never pollutes templates.
    pub mine_key: Option<String>,
    /// Full record text, lines joined with `\n`, for the record-scope index.
    pub text: String,
}

impl FramedRecord {
    pub fn is_crash(&self) -> bool {
        self.kind == RecordKind::Crash
    }
}

/// A boot-stage transition (§5 "Boot-stage tracking").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageTransition {
    pub name: String,
    pub profile: String,
    pub banner_line_id: i64,
    pub at: i64,
    /// The earliest-stage banner reappeared: an involuntary reboot, which is how
    /// boot-loop detection comes for free (§A.0 RESET-MARKER).
    pub is_reset: bool,
    /// The stage name actually changed (a reset in place does not change it).
    pub stage_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FramerEvent {
    Record(Box<FramedRecord>),
    Stage(StageTransition),
}

impl FramerEvent {
    pub fn as_record(&self) -> Option<&FramedRecord> {
        match self {
            FramerEvent::Record(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_stage(&self) -> Option<&StageTransition> {
        match self {
            FramerEvent::Stage(s) => Some(s),
            _ => None,
        }
    }
}

/// The framer interface. Declarative profiles and native plugins both implement
/// it, which is what makes the two tiers of §5 interchangeable to the pipeline.
pub trait Framer: Send + std::fmt::Debug {
    fn name(&self) -> &str;

    /// The stage currently in force, if the framer tracks stages.
    fn stage(&self) -> Option<&str> {
        None
    }

    /// Offer a line. Records complete asynchronously: a line may produce no event
    /// (it joined an open record) or several (it closed one and opened another).
    fn push(&mut self, line: FramerInput) -> Vec<FramerEvent>;

    /// Wall-clock tick: closes an open record that has gone silent past
    /// `framer.record_timeout_s`. This is the DEAD_AIR path that UEFI and TF-A
    /// depend on, since they dead-loop rather than printing a terminator.
    fn tick(&mut self, _now_ms: i64) -> Vec<FramerEvent> {
        Vec::new()
    }

    /// End of stream: close whatever is open, flagged `truncated`.
    fn flush(&mut self) -> Vec<FramerEvent>;
}

/// Assert the §12.1 framer-conservation property over a run of events.
///
/// Exposed (not test-only) because the live pipeline runs it as a debug
/// assertion: a framer bug that silently drops a crash record is the single
/// worst failure this system could have.
pub fn check_conservation(line_ids: &[i64], events: &[FramerEvent]) -> Result<(), String> {
    // Indexed rather than scanned: this is meant to be affordable as a live
    // debug assertion, and a linear search per record would make it quadratic in
    // exactly the case that matters — a long capture.
    let index: std::collections::HashMap<i64, usize> = line_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();

    // Records must tile the sequence: each one starts where the last ended.
    let mut next = 0usize;
    for e in events {
        let FramerEvent::Record(r) = e else { continue };
        if r.first_line_id > r.last_line_id {
            return Err(format!(
                "record spans backwards: {}..{}",
                r.first_line_id, r.last_line_id
            ));
        }
        let start = *index
            .get(&r.first_line_id)
            .ok_or_else(|| format!("record starts at unknown line {}", r.first_line_id))?;
        let end = *index
            .get(&r.last_line_id)
            .ok_or_else(|| format!("record ends at unknown line {}", r.last_line_id))?;
        if start != next {
            return Err(format!(
                "records do not tile the line sequence in order: expected the next \
                 record to start at index {next}, got {start}"
            ));
        }
        next = end + 1;
    }
    if next > line_ids.len() {
        return Err("records cover more lines than were offered".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(first: i64, last: i64) -> FramerEvent {
        FramerEvent::Record(Box::new(FramedRecord {
            first_line_id: first,
            last_line_id: last,
            line_count: last - first + 1,
            kind: RecordKind::Line,
            severity: Severity::Unknown,
            profile: "raw".into(),
            stage: None,
            truncated: false,
            fields: serde_json::Value::Null,
            mine_key: None,
            text: String::new(),
        }))
    }

    #[test]
    fn conservation_accepts_a_contiguous_tiling() {
        let ids = vec![1, 2, 3, 4, 5];
        let ev = vec![rec(1, 1), rec(2, 4), rec(5, 5)];
        assert!(check_conservation(&ids, &ev).is_ok());
    }

    #[test]
    fn conservation_rejects_a_gap() {
        let ids = vec![1, 2, 3, 4];
        let ev = vec![rec(1, 1), rec(3, 4)];
        assert!(check_conservation(&ids, &ev).is_err());
    }

    #[test]
    fn conservation_rejects_an_overlap_or_reorder() {
        let ids = vec![1, 2, 3, 4];
        assert!(check_conservation(&ids, &[rec(1, 2), rec(2, 4)]).is_err());
        assert!(check_conservation(&ids, &[rec(3, 4), rec(1, 2)]).is_err());
    }

    #[test]
    fn conservation_allows_a_trailing_open_record() {
        // Lines 4..5 are still inside an open record and have produced no event
        // yet; that is not a conservation failure, only an incomplete prefix.
        let ids = vec![1, 2, 3, 4, 5];
        assert!(check_conservation(&ids, &[rec(1, 3)]).is_ok());
    }
}
