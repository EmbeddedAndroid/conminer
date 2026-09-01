//! Framing helpers: run a profile over text and get back a inspectable result.
//!
//! Framer tests read best when they assert about *records* — "this 17-line oops
//! is one record, kind=crash, and its lines are exactly these" — rather than
//! about internal state. This is the shape that makes that possible, and the
//! golden snapshots of §12.2 serialize it directly.

use conminer_core::config::FramerConfig;
use conminer_core::framer::{
    FramedRecord, Framer, FramerEvent, FramerInput, ProfileFramer, ProfileSet,
};
use conminer_core::linesplit::split_all;
use conminer_core::store::{RecordKind, Severity};
use serde::Serialize;
use std::sync::Arc;

/// What a profile made of an input, in the form assertions want.
#[derive(Debug, Clone, Serialize)]
pub struct FrameResult {
    pub lines: Vec<String>,
    pub records: Vec<FramedRecord>,
    pub stages: Vec<StageHit>,
    /// Line ids, in order — used for the conservation check.
    pub line_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageHit {
    pub name: String,
    pub profile: String,
    pub is_reset: bool,
    pub stage_changed: bool,
    pub line: i64,
}

impl FrameResult {
    /// The lines a record actually covers, in order.
    pub fn record_lines(&self, r: &FramedRecord) -> Vec<&str> {
        let start = (r.first_line_id - 1) as usize;
        let end = (r.last_line_id - 1) as usize;
        self.lines[start..=end].iter().map(String::as_str).collect()
    }

    pub fn crashes(&self) -> Vec<&FramedRecord> {
        self.records
            .iter()
            .filter(|r| r.kind == RecordKind::Crash)
            .collect()
    }

    pub fn garbage(&self) -> Vec<&FramedRecord> {
        self.records
            .iter()
            .filter(|r| r.kind == RecordKind::Garbage)
            .collect()
    }

    /// The single record containing a line matching `needle`.
    pub fn record_containing(&self, needle: &str) -> &FramedRecord {
        let idx = self
            .lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no input line contains {needle:?}"));
        let id = idx as i64 + 1;
        self.records
            .iter()
            .find(|r| r.first_line_id <= id && id <= r.last_line_id)
            .unwrap_or_else(|| panic!("no record covers the line containing {needle:?}"))
    }

    pub fn stage_names(&self) -> Vec<&str> {
        self.stages.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn max_severity(&self) -> Severity {
        self.records
            .iter()
            .map(|r| r.severity)
            .min()
            .unwrap_or(Severity::Unknown)
    }

    /// Assert the §12.1 conservation property: records tile the line sequence
    /// exactly, with no gaps, duplicates or reordering.
    pub fn assert_conserved(&self) {
        let events: Vec<FramerEvent> = self
            .records
            .iter()
            .cloned()
            .map(|r| FramerEvent::Record(Box::new(r)))
            .collect();
        if let Err(e) = conminer_core::framer::check_conservation(&self.line_ids, &events) {
            panic!("framer conservation violated: {e}");
        }
        let covered: i64 = self.records.iter().map(|r| r.line_count).sum();
        assert_eq!(
            covered as usize,
            self.lines.len(),
            "every line must belong to exactly one record"
        );
    }
}

/// Frame text with one profile (pinned, so a test asserts about the profile it
/// names rather than about stage detection).
pub fn frame_text(profile: &str, text: &str) -> FrameResult {
    frame_with(profile, text, &FramerConfig::default(), true)
}

/// Frame text with stage auto-detection on — the realistic path.
pub fn frame_auto(text: &str) -> FrameResult {
    frame_with("raw", text, &FramerConfig::default(), false)
}

pub fn frame_with(profile: &str, text: &str, cfg: &FramerConfig, pinned: bool) -> FrameResult {
    let set = Arc::new(ProfileSet::builtin().expect("built-in profiles"));
    let mut framer =
        ProfileFramer::new(set, cfg, if pinned { Some(profile) } else { None }).expect("framer");

    let lines: Vec<String> = split_all(
        text.as_bytes(),
        conminer_core::config::LineEndingMode::Auto,
        1 << 20,
    )
    .iter()
    .map(|l| l.lossy().into_owned())
    .collect();

    let mut records = Vec::new();
    let mut stages = Vec::new();
    let mut line_ids = Vec::new();
    // Deterministic synthetic timestamps: 10 ms per line, so DEAD_AIR never
    // fires accidentally and a `tick` test can choose exactly when it does.
    for (i, l) in lines.iter().enumerate() {
        let id = i as i64 + 1;
        line_ids.push(id);
        let ev = framer.push(FramerInput::new(id, l.clone(), i as i64 * 10));
        collect(ev, &mut records, &mut stages);
    }
    let ev = framer.flush();
    collect(ev, &mut records, &mut stages);

    records.sort_by_key(|r| r.first_line_id);
    FrameResult {
        lines,
        records,
        stages,
        line_ids,
    }
}

/// Frame text and then let the clock run past the record timeout, so the
/// DEAD_AIR close path (UEFI/TF-A dead-loops) is exercised.
pub fn frame_then_idle(profile: &str, text: &str, idle_ms: i64) -> FrameResult {
    let set = Arc::new(ProfileSet::builtin().expect("built-in profiles"));
    let cfg = FramerConfig::default();
    let mut framer = ProfileFramer::new(set, &cfg, Some(profile)).expect("framer");

    let lines: Vec<String> = split_all(
        text.as_bytes(),
        conminer_core::config::LineEndingMode::Auto,
        1 << 20,
    )
    .iter()
    .map(|l| l.lossy().into_owned())
    .collect();

    let mut records = Vec::new();
    let mut stages = Vec::new();
    let mut line_ids = Vec::new();
    let mut now = 0i64;
    for (i, l) in lines.iter().enumerate() {
        let id = i as i64 + 1;
        line_ids.push(id);
        now = i as i64 * 10;
        collect(
            framer.push(FramerInput::new(id, l.clone(), now)),
            &mut records,
            &mut stages,
        );
    }
    collect(framer.tick(now + idle_ms), &mut records, &mut stages);
    records.sort_by_key(|r| r.first_line_id);
    FrameResult {
        lines,
        records,
        stages,
        line_ids,
    }
}

fn collect(ev: Vec<FramerEvent>, records: &mut Vec<FramedRecord>, stages: &mut Vec<StageHit>) {
    for e in ev {
        match e {
            FramerEvent::Record(r) => records.push(*r),
            FramerEvent::Stage(s) => stages.push(StageHit {
                name: s.name,
                profile: s.profile,
                is_reset: s.is_reset,
                stage_changed: s.stage_changed,
                line: s.banner_line_id,
            }),
        }
    }
}
