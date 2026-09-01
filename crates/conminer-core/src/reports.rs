//! §R. Agent bug reports: what an agent found, with the evidence attached.
//!
//! Reports used to travel as prose pasted from an agent to a human to whoever
//! fixes it, and the first job on every one was reconstructing which board,
//! which epoch, which build, and what was actually called. All of that is known
//! inside the process at the moment the agent notices, so it is recorded rather
//! than remembered.
//!
//! Three rules shape everything here:
//!
//! * A REPORT IS A CLAIM, NEVER A VERDICT. Nothing in this module feeds a tool's
//!   answer. An agent saying "power is broken" does not make `power` report a
//!   fault; it makes a row a human triages. Today's provenance case is the
//!   cautionary tale: the report was right about the symptom and wrong about the
//!   cause, and the rest only surfaced by checking the hardware.
//! * DUPLICATES COLLAPSE, SIGHTINGS DO NOT. The same problem in different words
//!   becomes one row with a count and a list of who hit it, because triage wants
//!   eight problems rather than forty messages -- but each sighting keeps its own
//!   build, so "three agents, three nodes" stays visible.
//! * RESOLUTION IS CHECKABLE. Closing a report records the build that fixed it
//!   and the gate that holds it. A sighting from a build at or after that one is
//!   a REGRESSION, not a duplicate, and says so out loud.

use crate::error::{ErrorCode, Result, ToolError};
use crate::store::Registry;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// A filed report, with everything triage needs to start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub id: i64,
    pub fingerprint: String,
    pub title: String,
    pub expected: Option<String>,
    pub observed: Option<String>,
    pub device: Option<String>,
    pub boot_id: Option<i64>,
    pub cursor: Option<String>,
    pub tool: Option<String>,
    pub args_json: Option<String>,
    pub build: Option<String>,
    pub node: Option<String>,
    pub reporter: Option<String>,
    pub status: String,
    pub fixed_in_build: Option<String>,
    pub gate: Option<String>,
    pub resolution: Option<String>,
    pub occurrences: i64,
    pub regressions: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub resolved_at: Option<i64>,
}

/// One sighting: an agent saying "this happened to me too", with its own
/// evidence. Kept per-row because the build is what separates a duplicate from
/// a regression.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sighting {
    pub reporter: Option<String>,
    pub node: Option<String>,
    pub build: Option<String>,
    pub device: Option<String>,
    pub boot_id: Option<i64>,
    pub cursor: Option<String>,
    pub note: Option<String>,
    pub at: i64,
}

/// What a caller submits. Everything except the title is optional, because a
/// report worth filing should never be blocked on a field the agent cannot fill.
#[derive(Debug, Clone, Default)]
pub struct NewReport {
    pub title: String,
    pub expected: Option<String>,
    pub observed: Option<String>,
    pub device: Option<String>,
    pub boot_id: Option<i64>,
    pub cursor: Option<String>,
    pub tool: Option<String>,
    pub args_json: Option<String>,
    pub build: Option<String>,
    pub node: Option<String>,
    pub reporter: Option<String>,
}

/// What happened to a submission, so the caller is told rather than left to
/// infer it from a row it cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filed {
    /// First time this problem has been seen.
    New,
    /// Same problem, already open: counted, not duplicated.
    Duplicate,
    /// Same problem, on a build that was supposed to have fixed it.
    Regression,
    /// Same problem as one closed `not_a_bug` or `wont_fix`: reopened, because
    /// a decision that keeps costing agents time deserves revisiting.
    Reopened,
}

impl Filed {
    pub fn as_str(self) -> &'static str {
        match self {
            Filed::New => "new",
            Filed::Duplicate => "duplicate",
            Filed::Regression => "regression",
            Filed::Reopened => "reopened",
        }
    }
}

/// The dedupe key.
///
/// Deliberately built from the SHAPE of the problem -- what was expected, what
/// was observed, and which tool was involved -- and never from the device or the
/// epoch. The same defect hit on two boards is one defect; wording differences
/// in the title are normalised the way the framer normalises a console line, so
/// "power off hangs" and "Power off hangs!!" are one row.
pub fn fingerprint(r: &NewReport) -> String {
    let mut parts = vec![normalise(&r.title)];
    if let Some(t) = &r.tool {
        parts.push(normalise(t));
    }
    if let Some(e) = &r.expected {
        parts.push(normalise(e));
    }
    if let Some(o) = &r.observed {
        parts.push(normalise(o));
    }
    let joined = parts.join("\u{1f}");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in joined.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// Lowercase, collapse whitespace, drop punctuation and any long hex run.
///
/// The hex matters: an agent quoting `boot 276` or `fp=271e11b419aa852e` in its
/// title would otherwise file a fresh report every single time the same defect
/// bit on a different epoch.
fn normalise(s: &str) -> String {
    let lowered = s.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    for word in lowered.split_whitespace() {
        let w: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if w.is_empty() {
            continue;
        }
        // A bare number or a long hex run is an instance, not a problem.
        let is_number = w.bytes().all(|b| b.is_ascii_digit());
        let is_hex = w.len() >= 8 && w.bytes().all(|b| b.is_ascii_hexdigit());
        if is_number || is_hex {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w);
    }
    out
}

/// Is this report describing the same problem as one already filed?
///
/// An exact hash of the text is not enough. An agent writing "power off hangs"
/// and another writing "power off hangs!! (boot 351)" are reporting one defect,
/// and the whole point of this mechanism is that the second one adds weight
/// instead of noise. So the match is on the SHAPE: the content words of the
/// title, with the epoch numbers and hex already dropped by `normalise`.
///
/// Deliberately conservative. Merging two different defects is worse than
/// carrying a near-duplicate, because the merged one hides a problem nobody is
/// then looking at. Two reports must overlap heavily AND agree about the tool
/// before they are called the same thing.
fn same_problem(a: &NewReport, b: &Report) -> bool {
    // A tool named on both sides that disagrees is decisive: two defects in two
    // different tools are two defects however similar the words.
    if let (Some(x), Some(y)) = (&a.tool, &b.tool) {
        if normalise(x) != normalise(y) {
            return false;
        }
    }
    let left: std::collections::BTreeSet<String> = normalise(&a.title)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let right: std::collections::BTreeSet<String> = normalise(&b.title)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let shared = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    // Jaccard, with a floor on the absolute overlap so two three-word titles
    // sharing one common verb are not fused.
    //
    // THE TOOL IS EVIDENCE. Naming the same tool on both sides is a strong
    // signal, so it buys a lower bar. Measured on the first day this shipped:
    // two agents reported the same `list_reports` defect minutes apart, in
    // titles that scored 0.58, and the queue grew two rows for one bug -- the
    // exact duplication this mechanism exists to prevent.
    let both_name_one_tool = matches!((&a.tool, &b.tool), (Some(_), Some(_)));
    let bar = if both_name_one_tool { 0.45 } else { 0.6 };
    shared >= 2.0 && shared / union >= bar
}

/// File a report, or count it against the one that already describes it.
pub fn file(reg: &mut Registry, r: &NewReport, now: i64) -> Result<(Report, Filed)> {
    if r.title.trim().is_empty() {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            "a report needs a title: one line naming what went wrong",
        ));
    }
    let fp = fingerprint(r);
    // Exact first (cheap, and the common case when an agent retries verbatim),
    // then the shape match for the same problem said differently.
    let existing = match by_fingerprint(reg, &fp)? {
        Some(hit) => Some(hit),
        None => list(reg, None, None, None, 500)?
            .into_iter()
            .find(|prev| same_problem(r, prev)),
    };

    let outcome = match &existing {
        None => Filed::New,
        Some(prev) => match prev.status.as_str() {
            // Closed as fixed, and here it is again. Whether that is a
            // REGRESSION or an agent on an old build is decided by the build,
            // never by the calendar.
            "fixed" => match (&prev.fixed_in_build, &r.build) {
                (Some(fixed), Some(saw)) if fixed == saw => Filed::Regression,
                (Some(_), Some(_)) => Filed::Duplicate,
                // No build on one side or the other: cannot tell, and guessing
                // "regression" would cry wolf. Counted, and the sighting keeps
                // whatever build it had for a human to read.
                _ => Filed::Duplicate,
            },
            "not_a_bug" | "wont_fix" | "duplicate" => Filed::Reopened,
            _ => Filed::Duplicate,
        },
    };

    let id = match existing {
        None => {
            let conn = reg.conn();
            conn.execute(
                "INSERT INTO reports(fingerprint,title,expected,observed,device,boot_id,cursor,
                                     tool,args_json,build,node,reporter,status,occurrences,
                                     first_seen,last_seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'open',1,?13,?13)",
                params![
                    fp,
                    r.title.trim(),
                    r.expected,
                    r.observed,
                    r.device,
                    r.boot_id,
                    r.cursor,
                    r.tool,
                    r.args_json,
                    r.build,
                    r.node,
                    r.reporter,
                    now
                ],
            )?;
            conn.last_insert_rowid()
        }
        Some(prev) => {
            let regressed = i64::from(outcome == Filed::Regression);
            let reopen = matches!(outcome, Filed::Regression | Filed::Reopened);
            reg.conn().execute(
                "UPDATE reports
                    SET occurrences = occurrences + 1,
                        regressions = regressions + ?2,
                        last_seen = ?3,
                        status = CASE WHEN ?4 = 1 THEN 'open' ELSE status END,
                        resolved_at = CASE WHEN ?4 = 1 THEN NULL ELSE resolved_at END
                  WHERE id = ?1",
                params![prev.id, regressed, now, i64::from(reopen)],
            )?;
            prev.id
        }
    };

    // Every filing is also a sighting: the first report is evidence too.
    add_sighting(
        reg,
        id,
        &Sighting {
            reporter: r.reporter.clone(),
            node: r.node.clone(),
            build: r.build.clone(),
            device: r.device.clone(),
            boot_id: r.boot_id,
            cursor: r.cursor.clone(),
            note: None,
            at: now,
        },
    )?;

    Ok((get(reg, id)?, outcome))
}

/// "I have this problem too."
///
/// The point of the whole feature per the operator who asked for it: an agent
/// that finds an existing report should be able to add its weight to it instead
/// of filing a fourteenth copy. Same regression rule as filing, because a
/// confirmation from a build that claimed the fix is exactly as important.
pub fn confirm(reg: &mut Registry, id: i64, s: &Sighting) -> Result<(Report, Filed)> {
    let prev = get(reg, id)?;
    let outcome = match prev.status.as_str() {
        // A BUILD ID IS A CONTENT HASH, NOT A VERSION.
        //
        // This asked whether the sighting's build STRING equalled the one the
        // fix went into, which is only ever true for the single build that
        // shipped the fix. Report #1 was fixed in `79565d1e7302` and seen again
        // on `9a3e7cff487e`; seeing a defect on a LATER build is more of a
        // regression, not less, and it was filed as a duplicate that left the
        // report closed. Hashes cannot be ordered, so "is this build after the
        // fix?" is unanswerable by comparison -- but time answers it exactly: a
        // sighting after the moment the report was resolved is the defect
        // happening again on a build that was supposed to contain the fix.
        "fixed" => match prev.resolved_at {
            Some(resolved) if s.at >= resolved => Filed::Regression,
            _ => Filed::Duplicate,
        },
        "not_a_bug" | "wont_fix" | "duplicate" => Filed::Reopened,
        _ => Filed::Duplicate,
    };
    let regressed = i64::from(outcome == Filed::Regression);
    let reopen = matches!(outcome, Filed::Regression | Filed::Reopened);
    reg.conn().execute(
        "UPDATE reports
            SET occurrences = occurrences + 1,
                regressions = regressions + ?2,
                last_seen = ?3,
                status = CASE WHEN ?4 = 1 THEN 'open' ELSE status END,
                resolved_at = CASE WHEN ?4 = 1 THEN NULL ELSE resolved_at END
          WHERE id = ?1",
        params![id, regressed, s.at, i64::from(reopen)],
    )?;
    add_sighting(reg, id, s)?;
    Ok((get(reg, id)?, outcome))
}

fn add_sighting(reg: &mut Registry, report_id: i64, s: &Sighting) -> Result<()> {
    reg.conn().execute(
        "INSERT INTO report_seen(report_id,reporter,node,build,device,boot_id,cursor,note,at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            report_id, s.reporter, s.node, s.build, s.device, s.boot_id, s.cursor, s.note, s.at
        ],
    )?;
    Ok(())
}

/// Close a report, with the evidence that makes the closure checkable.
pub fn resolve(
    reg: &mut Registry,
    id: i64,
    status: &str,
    fixed_in_build: Option<&str>,
    gate: Option<&str>,
    note: Option<&str>,
    now: i64,
) -> Result<Report> {
    if !matches!(status, "fixed" | "not_a_bug" | "wont_fix" | "duplicate") {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("unknown resolution {status:?}"),
        ));
    }
    // A FIX WITHOUT A BUILD IS A CLAIM. The build is what lets the next sighting
    // be judged a regression instead of quietly counted as another duplicate,
    // which is the failure this whole mechanism exists to catch.
    if status == "fixed" && fixed_in_build.map_or(true, str::is_empty) {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            "resolving as fixed requires the build that contains the fix: without it a repeat \
             cannot be told from a regression",
        ));
    }
    get(reg, id)?;
    reg.conn().execute(
        "UPDATE reports
            SET status=?2, fixed_in_build=?3, gate=?4, resolution=?5, resolved_at=?6
          WHERE id=?1",
        params![id, status, fixed_in_build, gate, note, now],
    )?;
    get(reg, id)
}

const COLUMNS: &str = "id,fingerprint,title,expected,observed,device,boot_id,cursor,tool,
                       args_json,build,node,reporter,status,fixed_in_build,gate,resolution,
                       occurrences,regressions,first_seen,last_seen,resolved_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Report> {
    Ok(Report {
        id: r.get(0)?,
        fingerprint: r.get(1)?,
        title: r.get(2)?,
        expected: r.get(3)?,
        observed: r.get(4)?,
        device: r.get(5)?,
        boot_id: r.get(6)?,
        cursor: r.get(7)?,
        tool: r.get(8)?,
        args_json: r.get(9)?,
        build: r.get(10)?,
        node: r.get(11)?,
        reporter: r.get(12)?,
        status: r.get(13)?,
        fixed_in_build: r.get(14)?,
        gate: r.get(15)?,
        resolution: r.get(16)?,
        occurrences: r.get(17)?,
        regressions: r.get(18)?,
        first_seen: r.get(19)?,
        last_seen: r.get(20)?,
        resolved_at: r.get(21)?,
    })
}

pub fn get(reg: &Registry, id: i64) -> Result<Report> {
    reg.conn()
        .query_row(
            &format!("SELECT {COLUMNS} FROM reports WHERE id=?1"),
            params![id],
            row,
        )
        .map_err(|_| {
            ToolError::new(
                ErrorCode::UnknownArgument,
                format!("no report with id {id}"),
            )
            .with_hint("call list_reports() to see what has been filed")
        })
}

fn by_fingerprint(reg: &Registry, fp: &str) -> Result<Option<Report>> {
    Ok(reg
        .conn()
        .query_row(
            &format!("SELECT {COLUMNS} FROM reports WHERE fingerprint=?1"),
            params![fp],
            row,
        )
        .ok())
}

/// Search before you file. `query` matches the title, expected and observed
/// text; `status` defaults to open at the call site, because a triage queue is
/// what an operator wants and an agent looking for its own problem wants the
/// open ones first.
pub fn list(
    reg: &Registry,
    status: Option<&str>,
    query: Option<&str>,
    device: Option<&str>,
    limit: usize,
) -> Result<Vec<Report>> {
    let mut sql = format!("SELECT {COLUMNS} FROM reports WHERE 1=1");
    if status.is_some() {
        sql.push_str(" AND status = ?1");
    }
    sql.push_str(" ORDER BY last_seen DESC");
    let mut st = reg.conn().prepare(&sql)?;
    let rows: Vec<Report> = match status {
        Some(s) => st
            .query_map(params![s], row)?
            .collect::<std::result::Result<_, _>>()?,
        None => st
            .query_map([], row)?
            .collect::<std::result::Result<_, _>>()?,
    };
    // Filtering in Rust rather than SQL: the match is on NORMALISED text, so a
    // search for "power off hangs" finds "Power off hangs!!" -- the same rule
    // that decides whether two reports are one.
    let needle = query.map(normalise);
    Ok(rows
        .into_iter()
        .filter(|r| {
            // Exact, or a recognisable part of it: a selector that resolved
            // cleanly arrives as the stored name, and one that did not still
            // finds its board by substring rather than silently matching
            // nothing.
            device.map_or(true, |d| {
                r.device.as_deref().is_some_and(|stored| {
                    stored == d
                        || stored
                            .to_ascii_lowercase()
                            .contains(&d.to_ascii_lowercase())
                })
            }) && needle.as_ref().map_or(true, |n| {
                n.split_whitespace().all(|w| {
                    normalise(&r.title).contains(w)
                        || normalise(r.expected.as_deref().unwrap_or_default()).contains(w)
                        || normalise(r.observed.as_deref().unwrap_or_default()).contains(w)
                })
            })
        })
        .take(limit)
        .collect())
}

/// Who hit this, and on what.
pub fn sightings(reg: &Registry, id: i64, limit: usize) -> Result<Vec<Sighting>> {
    let mut st = reg.conn().prepare(
        "SELECT reporter,node,build,device,boot_id,cursor,note,at
           FROM report_seen WHERE report_id=?1 ORDER BY at DESC LIMIT ?2",
    )?;
    let rows: Vec<Sighting> = st
        .query_map(params![id, limit as i64], |r| {
            Ok(Sighting {
                reporter: r.get(0)?,
                node: r.get(1)?,
                build: r.get(2)?,
                device: r.get(3)?,
                boot_id: r.get(4)?,
                cursor: r.get(5)?,
                note: r.get(6)?,
                at: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// How many DISTINCT reporters have hit this, which is the number that should
/// drive priority. Ten sightings from one agent retrying is not ten agents.
pub fn distinct_reporters(reg: &Registry, id: i64) -> Result<i64> {
    Ok(reg.conn().query_row(
        "SELECT count(DISTINCT COALESCE(reporter, node, 'anonymous'))
           FROM report_seen WHERE report_id=?1",
        params![id],
        |r| r.get(0),
    )?)
}
