//! Search subsystem (§8.1) — three tiers behind one tool.
//!
//! `search_raw` alone is an unindexed line scan: insufficient at 800 MB, and
//! blind to matches that span line boundaries. So:
//!
//! * **Tier 1 — FTS5** for `terms` and `phrase`. Record text is its lines joined
//!   with `\n`, so a multiline phrase search is the *indexed fast path* rather
//!   than the fallback — the framer has already grouped every crash into one
//!   record, which is exactly the unit people search for.
//! * **Tier 2 — regex** over the candidate set FTS can pre-narrow, falling back
//!   to a full scan only for literal-free patterns. Rust's `regex` crate is
//!   linear-time, so a hostile pattern from an agent cannot pin a lab host.
//! * **Tier 3 — windowed scan** for matches that cross record boundaries.
//!   Explicitly the slow path; the response marks it `scan=true` with bytes
//!   scanned, so an agent is never misled about what a query cost.

use crate::error::{ErrorCode, Result, ToolError};
use crate::store::{DeviceStore, LineRow};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// All words present, any order.
    #[default]
    Terms,
    /// Exact contiguous phrase.
    Phrase,
    /// Full regex; `(?s)` permitted.
    Regex,
    /// Internal: the query string is already an FTS5 expression. Not part of the
    /// tool surface; used by tier 2 to hand its pre-narrowing terms to tier 1.
    #[serde(skip)]
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    Line,
    Record,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchScope {
    pub kind: ScopeKind,
    /// Window size, only meaningful for `ScopeKind::Window`.
    pub window: usize,
}

impl Default for SearchScope {
    fn default() -> Self {
        Self {
            kind: ScopeKind::Line,
            window: 20,
        }
    }
}

impl SearchScope {
    pub fn line() -> Self {
        Self {
            kind: ScopeKind::Line,
            window: 0,
        }
    }
    pub fn record() -> Self {
        Self {
            kind: ScopeKind::Record,
            window: 0,
        }
    }
    pub fn window(n: usize) -> Self {
        Self {
            kind: ScopeKind::Window,
            window: n,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub query: String,
    pub mode: SearchMode,
    pub scope: SearchScope,
    pub session_id: Option<i64>,
    pub boot_id: Option<i64>,
    pub max_results: usize,
    /// Resume point: only hits at or after this stream offset are returned.
    pub after_offset: Option<u64>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: SearchMode::Terms,
            scope: SearchScope::default(),
            session_id: None,
            boot_id: None,
            max_results: 100,
            after_offset: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// `line` or `record`.
    pub kind: String,
    /// Row id of the line or record.
    pub id: i64,
    /// Anchor for `get_context`: always a line id.
    pub line_id: i64,
    pub stream_offset: u64,
    pub session_id: i64,
    pub text: String,
    /// Byte offsets into `text` of each match.
    pub highlights: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Which tier answered: `fts`, `fts+regex`, `scan`, `window`.
    pub tier: String,
    /// True when the query fell back to reading rows rather than an index.
    pub scan: bool,
    pub rows_scanned: usize,
    pub bytes_scanned: u64,
    /// The cap was reached; pass `next_cursor` for more.
    pub capped: bool,
    pub next_offset: Option<u64>,
}

/// Run a search against a device store.
pub fn search(store: &DeviceStore, q: &SearchQuery) -> Result<SearchResult> {
    if q.query.trim().is_empty() {
        return Err(ToolError::invalid_arg("empty search query"));
    }
    match (q.scope.kind, q.mode) {
        (ScopeKind::Window, _) => window_scan(store, q),
        (_, SearchMode::Regex) => regex_search(store, q),
        _ => fts_search(store, q),
    }
}

// ------------------------------------------------------------------ tier 1 ---

/// Quote a token for FTS5. FTS5 treats bare punctuation as syntax, so every
/// user term is passed as a quoted string with internal quotes doubled.
fn fts_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn fts_expression(q: &SearchQuery) -> String {
    match q.mode {
        // FTS5 already ANDs bare terms; quoting each keeps punctuation literal.
        SearchMode::Terms => q
            .query
            .split_whitespace()
            .map(fts_quote)
            .collect::<Vec<_>>()
            .join(" "),
        // A quoted multi-token string *is* a phrase in FTS5.
        SearchMode::Phrase => fts_quote(q.query.trim()),
        SearchMode::Raw => q.query.clone(),
        SearchMode::Regex => unreachable!("regex never reaches tier 1"),
    }
}

fn fts_search(store: &DeviceStore, q: &SearchQuery) -> Result<SearchResult> {
    if !store.fts_enabled() {
        // `index=off` for this device: degrade to the scanning tier rather than
        // returning nothing, and say so in `tier`.
        let mut r = degraded_scan(store, q)?;
        r.tier = "scan (index=off)".into();
        return r_ok(r);
    }
    let expr = fts_expression(q);
    let record_scope = q.scope.kind == ScopeKind::Record;

    let sql = if record_scope {
        "SELECT r.id, r.first_line_id, l.stream_offset, r.session_id
           FROM record_fts f
           JOIN records r ON r.id = f.rowid
           JOIN raw_lines l ON l.id = r.first_line_id
          WHERE record_fts MATCH ?1
            AND (?2 IS NULL OR r.session_id = ?2)
            AND (?3 IS NULL OR r.boot_id = ?3)
            AND (?4 IS NULL OR l.stream_offset >= ?4)
          ORDER BY r.id LIMIT ?5"
    } else {
        "SELECT l.id, l.id, l.stream_offset, l.session_id
           FROM raw_fts f
           JOIN raw_lines l ON l.id = f.rowid
          WHERE raw_fts MATCH ?1
            AND (?2 IS NULL OR l.session_id = ?2)
            AND (?3 IS NULL OR l.boot_id = ?3)
            AND (?4 IS NULL OR l.stream_offset >= ?4)
          ORDER BY l.id LIMIT ?5"
    };

    let mut st = store.conn().prepare(sql).map_err(|e| {
        ToolError::new(
            ErrorCode::InvalidArgument,
            format!("search query rejected by the index: {e}"),
        )
    })?;
    let rows = st
        .query_map(
            params![
                expr,
                q.session_id,
                q.boot_id,
                q.after_offset.map(|o| o as i64),
                (q.max_results + 1) as i64
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .map_err(|e| {
            ToolError::new(
                ErrorCode::InvalidArgument,
                format!("search query rejected by the index: {e}"),
            )
        })?;

    let mut raw: Vec<(i64, i64, u64, i64)> = Vec::new();
    for row in rows {
        raw.push(row?);
    }
    let capped = raw.len() > q.max_results;
    raw.truncate(q.max_results);

    let mut hits = Vec::with_capacity(raw.len());
    for (id, line_id, offset, session_id) in raw {
        let text = if record_scope {
            store.record_text(id)?
        } else {
            store.line(line_id)?.lossy()
        };
        let highlights = highlight(&text, q);
        // A PHRASE MEANS THE PHRASE, PUNCTUATION AND ALL.
        //
        // FTS tokenises: `sirocco>` and `SIROCCO` are the same token to the
        // index, so a search for a shell prompt came back full of banner lines.
        // Reported from the bench. The index is still the right way to FIND
        // candidates -- it is what makes this fast -- but in phrase mode the
        // literal has to hold as well, and `highlight` has already computed
        // exactly that: no occurrence, no hit.
        if q.mode == SearchMode::Phrase && highlights.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            kind: if record_scope { "record" } else { "line" }.into(),
            id,
            line_id,
            stream_offset: offset,
            session_id,
            text,
            highlights,
        });
    }
    let next_offset = capped
        .then(|| hits.last().map(|h| h.stream_offset + 1))
        .flatten();

    r_ok(SearchResult {
        hits,
        tier: "fts".into(),
        scan: false,
        rows_scanned: 0,
        bytes_scanned: 0,
        capped,
        next_offset,
    })
}

fn r_ok(r: SearchResult) -> Result<SearchResult> {
    Ok(r)
}

// ------------------------------------------------------------------ tier 2 ---

/// Literal fragments a regex requires, usable to pre-narrow with FTS.
///
/// **Soundness is the whole game here.** Narrowing is an optimisation; if it can
/// drop a line the full scan would have matched, the search is simply wrong. Two
/// rules keep it safe:
///
/// * A literal is only usable if the pattern guarantees it starts at a **token
///   boundary** — the element before it is the pattern start, whitespace,
///   punctuation, `\s`, or `\b`. FTS5 indexes whole tokens, so `CPU` does not
///   match the token `CPU1`; a prefix query `"CPU"*` does, but only if `CPU` is
///   really at the token's start. `\d+CPU\d` gives no usable literal, and the
///   query falls through to a scan.
/// * Any alternation abandons narrowing entirely, since each branch would need
///   its own literal set.
///
/// Returned literals are meant to be used as FTS **prefix** terms.
pub fn extract_literals(pattern: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_anchored = false;
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    let mut alternation = false;
    // True when the next literal character would begin at a token boundary.
    let mut at_boundary = true;

    macro_rules! flush {
        ($boundary:expr) => {{
            if cur_anchored && cur.chars().count() >= 3 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            cur_anchored = false;
            at_boundary = $boundary;
        }};
    }

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let esc = chars.next().unwrap_or(' ');
                // `\s`, `\b`, `\W` and friends all imply a boundary follows;
                // `\d`, `\w` do not.
                let boundary = matches!(esc, 's' | 'b' | 'W' | 'S' | 'n' | 't' | 'r')
                    || !(esc.is_alphanumeric() || esc == '_');
                flush!(boundary);
            }
            '[' => {
                in_class = true;
                flush!(false);
            }
            ']' => {
                in_class = false;
                at_boundary = false;
            }
            '|' => {
                alternation = true;
                flush!(true);
            }
            '(' | ')' => flush!(at_boundary),
            '^' => flush!(true),
            '$' | '.' => flush!(false),
            '?' | '*' => {
                // The preceding character is optional: it cannot be required.
                cur.pop();
                flush!(false);
            }
            '+' | '{' | '}' => flush!(false),
            _ if in_class => {}
            _ if c.is_alphanumeric() || c == '_' || c == '-' => {
                if cur.is_empty() {
                    cur_anchored = at_boundary;
                }
                cur.push(c);
            }
            // Any other literal character (space, colon, slash…) is a token
            // separator, so whatever follows starts a token.
            _ => flush!(true),
        }
    }
    // A final flush with no successor: the boundary value is not read again.
    if cur_anchored && cur.chars().count() >= 3 {
        out.push(cur);
    }
    let _ = at_boundary;

    if alternation {
        return Vec::new();
    }
    out.retain(|s| s.chars().count() >= 3);
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out.truncate(3);
    out
}

fn regex_search(store: &DeviceStore, q: &SearchQuery) -> Result<SearchResult> {
    let re = build_regex(&q.query)?;
    let literals = extract_literals(&q.query);
    let record_scope = q.scope.kind == ScopeKind::Record;

    // Pre-narrow with FTS when the pattern carries usable literals.
    if store.fts_enabled() && !literals.is_empty() {
        // Prefix terms: the literal is known to start a token, but the token may
        // continue past it (`CPU` in `CPU1`).
        let expr = literals
            .iter()
            .map(|s| format!("{}*", fts_quote(s)))
            .collect::<Vec<_>>()
            .join(" ");
        let narrowed = SearchQuery {
            query: expr,
            mode: SearchMode::Raw,
            // Pull a generous candidate set: the regex will reject most of it.
            max_results: (q.max_results * 20).max(200),
            ..q.clone()
        };
        let mut candidates = fts_search(store, &narrowed)?;
        candidates.hits.retain(|h| re.is_match(&h.text));
        let capped = candidates.hits.len() > q.max_results;
        candidates.hits.truncate(q.max_results);
        for h in &mut candidates.hits {
            h.highlights = re
                .find_iter(&h.text)
                .map(|m| (m.start(), m.end()))
                .collect();
            h.kind = if record_scope { "record" } else { "line" }.into();
        }
        candidates.tier = "fts+regex".into();
        candidates.capped = capped;
        return Ok(candidates);
    }

    let mut r = degraded_scan(store, q)?;
    r.tier = "scan".into();
    Ok(r)
}

fn build_regex(pattern: &str) -> Result<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        // Bound compilation: a pathological pattern should be rejected, not
        // turned into a multi-megabyte DFA on a lab host.
        .size_limit(16 * 1024 * 1024)
        .dfa_size_limit(8 * 1024 * 1024)
        .build()
        .map_err(|e| ToolError::invalid_arg(format!("invalid regex: {e}")))
}

/// Full scan over lines or records. The honest slow path.
fn degraded_scan(store: &DeviceStore, q: &SearchQuery) -> Result<SearchResult> {
    let matcher = Matcher::new(q)?;
    let record_scope = q.scope.kind == ScopeKind::Record;

    let mut hits = Vec::new();
    let mut rows_scanned = 0usize;
    let mut bytes_scanned = 0u64;
    let mut capped = false;
    let mut next_offset = None;

    if record_scope {
        let mut st = store.conn().prepare(
            "SELECT r.id, r.first_line_id, l.stream_offset, r.session_id
               FROM records r JOIN raw_lines l ON l.id = r.first_line_id
              WHERE (?1 IS NULL OR r.session_id = ?1)
                AND (?2 IS NULL OR r.boot_id = ?2)
                AND (?3 IS NULL OR l.stream_offset >= ?3)
              ORDER BY r.id",
        )?;
        let rows = st.query_map(
            params![q.session_id, q.boot_id, q.after_offset.map(|o| o as i64)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)?,
                ))
            },
        )?;
        for row in rows {
            let (id, line_id, offset, session_id) = row?;
            rows_scanned += 1;
            let text = store.record_text(id)?;
            bytes_scanned += text.len() as u64;
            let h = matcher.find(&text);
            if !h.is_empty() {
                if hits.len() >= q.max_results {
                    capped = true;
                    next_offset = Some(offset);
                    break;
                }
                hits.push(SearchHit {
                    kind: "record".into(),
                    id,
                    line_id,
                    stream_offset: offset,
                    session_id,
                    text,
                    highlights: h,
                });
            }
        }
    } else {
        for l in scan_lines(store, q)? {
            rows_scanned += 1;
            bytes_scanned += l.bytes.len() as u64;
            let text = l.lossy();
            let h = matcher.find(&text);
            if !h.is_empty() {
                if hits.len() >= q.max_results {
                    capped = true;
                    next_offset = Some(l.stream_offset);
                    break;
                }
                hits.push(SearchHit {
                    kind: "line".into(),
                    id: l.id,
                    line_id: l.id,
                    stream_offset: l.stream_offset,
                    session_id: l.session_id,
                    text,
                    highlights: h,
                });
            }
        }
    }

    Ok(SearchResult {
        hits,
        tier: "scan".into(),
        scan: true,
        rows_scanned,
        bytes_scanned,
        capped,
        next_offset,
    })
}

fn scan_lines(store: &DeviceStore, q: &SearchQuery) -> Result<Vec<LineRow>> {
    let mut st = store.conn().prepare(
        "SELECT id,session_id,boot_id,stream_offset,ts_mono,ts_wall,stage_id,bytes,terminator,
                truncated,continuation
           FROM raw_lines
          WHERE (?1 IS NULL OR session_id = ?1)
            AND (?2 IS NULL OR boot_id = ?2)
            AND (?3 IS NULL OR stream_offset >= ?3)
          ORDER BY id",
    )?;
    let rows = st.query_map(
        params![q.session_id, q.boot_id, q.after_offset.map(|o| o as i64)],
        crate::store::device::map_line_pub,
    )?;
    let mut v = Vec::new();
    for r in rows {
        v.push(r?);
    }
    Ok(v)
}

// ------------------------------------------------------------------ tier 3 ---

/// Sliding-window scan for matches that cross record boundaries.
fn window_scan(store: &DeviceStore, q: &SearchQuery) -> Result<SearchResult> {
    let n = q.scope.window.max(2);
    let matcher = Matcher::new(q)?;
    let lines = scan_lines(store, q)?;

    let mut hits = Vec::new();
    let mut bytes_scanned = 0u64;
    let mut capped = false;
    let mut next_offset = None;
    let mut reported: std::collections::BTreeSet<i64> = Default::default();

    for start in 0..lines.len() {
        let end = (start + n).min(lines.len());
        let text = lines[start..end]
            .iter()
            .map(LineRow::lossy)
            .collect::<Vec<_>>()
            .join("\n");
        bytes_scanned += text.len() as u64;
        let h = matcher.find(&text);
        if h.is_empty() {
            continue;
        }
        // Windows overlap, so the same span is seen up to `n` times. Anchor each
        // hit to the line its match *starts* on and report that line once — the
        // agent gets one hit per match, not one per window.
        let anchor_idx = start + line_index_of(&lines[start..end], h[0].0);
        let anchor = &lines[anchor_idx];
        if !reported.insert(anchor.id) {
            continue;
        }
        if hits.len() >= q.max_results {
            capped = true;
            next_offset = Some(anchor.stream_offset);
            break;
        }
        hits.push(SearchHit {
            kind: "window".into(),
            id: anchor.id,
            line_id: anchor.id,
            stream_offset: anchor.stream_offset,
            session_id: anchor.session_id,
            text,
            highlights: h,
        });
    }

    Ok(SearchResult {
        hits,
        tier: format!("window({n})"),
        scan: true,
        rows_scanned: lines.len(),
        bytes_scanned,
        capped,
        next_offset,
    })
}

/// Which line of a `\n`-joined window a byte offset falls in.
fn line_index_of(lines: &[LineRow], offset: usize) -> usize {
    let mut acc = 0usize;
    for (i, l) in lines.iter().enumerate() {
        acc += l.lossy().len() + 1; // the joining newline
        if offset < acc {
            return i;
        }
    }
    lines.len().saturating_sub(1)
}

// ---------------------------------------------------------------- matching ---

enum Matcher {
    Terms(Vec<String>),
    Phrase(String),
    Regex(regex::Regex),
}

impl Matcher {
    fn new(q: &SearchQuery) -> Result<Self> {
        Ok(match q.mode {
            SearchMode::Terms => Matcher::Terms(
                q.query
                    .split_whitespace()
                    .map(|s| s.to_lowercase())
                    .collect(),
            ),
            SearchMode::Phrase | SearchMode::Raw => Matcher::Phrase(q.query.trim().to_lowercase()),
            SearchMode::Regex => Matcher::Regex(build_regex(&q.query)?),
        })
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Matcher::Regex(re) => re.find_iter(text).map(|m| (m.start(), m.end())).collect(),
            Matcher::Phrase(p) => {
                let hay = text.to_lowercase();
                let mut out = Vec::new();
                let mut from = 0;
                while let Some(i) = hay[from..].find(p.as_str()) {
                    out.push((from + i, from + i + p.len()));
                    from += i + p.len().max(1);
                }
                out
            }
            Matcher::Terms(terms) => {
                let hay = text.to_lowercase();
                // All terms must be present, or it is not a hit at all.
                if !terms.iter().all(|t| hay.contains(t.as_str())) {
                    return Vec::new();
                }
                let mut out = Vec::new();
                for t in terms {
                    let mut from = 0;
                    while let Some(i) = hay[from..].find(t.as_str()) {
                        out.push((from + i, from + i + t.len()));
                        from += i + t.len().max(1);
                    }
                }
                out.sort();
                out
            }
        }
    }
}

fn highlight(text: &str, q: &SearchQuery) -> Vec<(usize, usize)> {
    Matcher::new(q).map(|m| m.find(text)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_extraction_is_conservative() {
        assert_eq!(extract_literals("Kernel panic"), vec!["Kernel", "panic"]);
        assert_eq!(
            extract_literals(r"Unhandled fault: at 0x[0-9a-f]+"),
            vec!["Unhandled", "fault"]
        );
        // An optional trailing char must not be treated as required.
        assert_eq!(extract_literals("colou?r"), vec!["colo"]);
        // Alternation makes every branch optional: narrow nothing.
        assert!(extract_literals("panic|oops").is_empty());
        // Nothing usable.
        assert!(extract_literals(r"^\d+$").is_empty());
        assert!(extract_literals(r"[a-z]{3,}").is_empty());
    }

    #[test]
    fn fts_quoting_survives_punctuation_and_quotes() {
        assert_eq!(fts_quote("errno"), "\"errno\"");
        assert_eq!(fts_quote("a\"b"), "\"a\"\"b\"");
        let q = SearchQuery {
            query: "mmc0: error".into(),
            mode: SearchMode::Terms,
            ..Default::default()
        };
        assert_eq!(fts_expression(&q), "\"mmc0:\" \"error\"");
        let q = SearchQuery {
            query: "Kernel panic - not syncing".into(),
            mode: SearchMode::Phrase,
            ..Default::default()
        };
        assert_eq!(fts_expression(&q), "\"Kernel panic - not syncing\"");
    }

    #[test]
    fn terms_require_every_word_but_not_the_order() {
        let m = Matcher::new(&SearchQuery {
            query: "panic syncing".into(),
            mode: SearchMode::Terms,
            ..Default::default()
        })
        .unwrap();
        assert!(!m.find("Kernel panic - not syncing: x").is_empty());
        assert!(!m.find("syncing then panic").is_empty());
        assert!(m.find("Kernel panic only").is_empty());
    }

    #[test]
    fn phrase_requires_contiguity() {
        let m = Matcher::new(&SearchQuery {
            query: "not syncing".into(),
            mode: SearchMode::Phrase,
            ..Default::default()
        })
        .unwrap();
        assert!(!m.find("Kernel panic - not syncing: x").is_empty());
        assert!(m.find("not really syncing").is_empty());
    }

    #[test]
    fn a_hostile_regex_is_rejected_or_stays_linear() {
        // Rust's regex crate has no backtracking, so a nested quantifier is
        // linear rather than catastrophic — but the size limits still bound it.
        let q = SearchQuery {
            query: "(a+)+b".into(),
            mode: SearchMode::Regex,
            ..Default::default()
        };
        let m = Matcher::new(&q).unwrap();
        let hay = "a".repeat(4000);
        let start = std::time::Instant::now();
        assert!(m.find(&hay).is_empty());
        assert!(
            start.elapsed().as_millis() < 500,
            "matching must stay linear"
        );
    }
}
