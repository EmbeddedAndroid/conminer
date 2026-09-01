//! Incremental consumption (§8.2): cursor + server-side predicate long-poll.
//!
//! Agents never poll dumb loops and never receive pushes they did not ask for.
//! `follow` blocks server-side until a predicate fires or the timeout expires,
//! then returns *everything since the cursor* in mined form. "Boot the board and
//! tell me when it is up or crashed" is one call, and the agent burns zero
//! tokens during the ninety boring seconds of boot.
//!
//! A timeout returns `matched: none` **with the data so far**. A timeout is
//! data, not an error: knowing that nothing happened for two minutes is often
//! the answer.
//!
//! Implemented by polling the device store rather than by in-process signalling,
//! which is what lets mcpd and minerd be separate containers and still give the
//! agent a gap-free view.

use crate::error::{ErrorCode, Result, ToolError};
use crate::store::{Cursor, DeviceStore, RecordKind, Severity};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How much of an increment one call will account for. Generous, because the
/// counts are the product; bounded, because a month-long soak must not be read
/// into memory by a single `follow`.
const SCAN_LIMIT: usize = 50_000;

/// What a `follow` call is waiting for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// A specific line appears.
    Pattern(String),
    /// Any novel template — the crash you have not seen before.
    TemplateNew,
    /// Boot reaches a named stage.
    Stage(String),
    /// The device's prompt is detected: the "boot finished" signal.
    Prompt,
    /// The console goes silent for this long (settle detection).
    QuietMs(i64),
    /// A new boot epoch opens (a reset, a power cycle, a mark).
    Reset,
    /// A named watch fires (§F6).
    ///
    /// Watches capture perfectly overnight but used to deliver only on poll, so
    /// an agent babysitting a soak burned a wakeup every interval asking "yet?".
    /// The primitive to park cheaply already existed -- follow's server-side
    /// wait -- it just could not wait ON a watch.
    Watch(String),
    /// First of several.
    Any(Vec<Predicate>),
}

impl Predicate {
    /// Every watch name this predicate depends on (§F6).
    ///
    /// A parked `follow` has to ADVANCE those watches itself: the scanner only
    /// runs when a watch is polled, so waiting on a queue that nothing fills is
    /// a wait that never ends -- measured on the IQ10, where a follow parked on
    /// a live watch timed out at 60 s with `fired_total: 0` while the console
    /// was talking the whole time. The whole point of this predicate is that an
    /// agent stops calling poll_watch.
    pub fn watch_names(&self) -> Vec<String> {
        match self {
            Predicate::Watch(w) => vec![w.clone()],
            Predicate::Any(list) => list.iter().flat_map(|p| p.watch_names()).collect(),
            _ => Vec::new(),
        }
    }

    /// Parse the JSON form used on the tool surface.
    pub fn parse(v: &Value) -> Result<Self> {
        if let Some(list) = v.get("any").and_then(Value::as_array) {
            let inner: Result<Vec<Predicate>> = list.iter().map(Predicate::parse).collect();
            return Ok(Predicate::Any(inner?));
        }
        if let Some(p) = v.get("pattern").and_then(Value::as_str) {
            regex::Regex::new(p)
                .map_err(|e| ToolError::invalid_arg(format!("invalid until.pattern: {e}")))?;
            return Ok(Predicate::Pattern(p.to_string()));
        }
        if let Some(t) = v.get("template").and_then(Value::as_str) {
            if t != "new" {
                return Err(ToolError::invalid_arg("until.template must be \"new\""));
            }
            return Ok(Predicate::TemplateNew);
        }
        if let Some(s) = v.get("stage").and_then(Value::as_str) {
            return Ok(Predicate::Stage(s.to_string()));
        }
        if v.get("prompt").and_then(Value::as_bool) == Some(true) {
            return Ok(Predicate::Prompt);
        }
        if let Some(q) = v.get("quiet").and_then(Value::as_i64) {
            return Ok(Predicate::QuietMs(q));
        }
        if v.get("reset").and_then(Value::as_bool) == Some(true) {
            return Ok(Predicate::Reset);
        }
        if let Some(w) = v.get("watch").and_then(Value::as_str) {
            return Ok(Predicate::Watch(w.to_string()));
        }
        Err(ToolError::invalid_arg(
            "until must be one of {pattern}, {template:\"new\"}, {stage}, {prompt:true}, \
             {quiet:<ms>}, {reset:true}, {watch:<name>} or {any:[…]}",
        ))
    }

    fn label(&self) -> String {
        match self {
            Predicate::Pattern(p) => format!("pattern:{p}"),
            Predicate::TemplateNew => "template:new".into(),
            Predicate::Stage(s) => format!("stage:{s}"),
            Predicate::Prompt => "prompt".into(),
            Predicate::QuietMs(ms) => format!("quiet:{ms}"),
            Predicate::Reset => "reset".into(),
            Predicate::Watch(w) => format!("watch:{w}"),
            Predicate::Any(_) => "any".into(),
        }
    }
}

/// Everything that happened between two cursors.
#[derive(Debug, Clone, Serialize)]
pub struct Increment {
    /// Which predicate fired, or `null` on timeout.
    pub matched: Option<String>,
    pub evidence: Option<Value>,
    pub lines: usize,
    pub bytes: u64,
    /// Raw tail, capped by `max_lines`.
    pub tail: Vec<Value>,
    /// Templates seen for the first time in this increment.
    pub new_templates: Vec<Value>,
    /// Templates that fired again, with their delta — this is what keeps a
    /// forty-iteration boot loop from returning forty times the output.
    pub template_deltas: Vec<Value>,
    /// Repeat templates left out of `template_deltas` by its cap. Reported so a
    /// truncated list can never be mistaken for a complete one.
    pub deltas_omitted: usize,
    /// Repeat templates whose TEXT was dropped to fit the budget (§L3).
    pub delta_text_dropped: usize,
    /// Raw tail lines dropped to fit the budget, oldest first (§L3). `lines`
    /// and `search` still have every one of them.
    pub tail_omitted: usize,
    /// Novel templates whose TEXT was dropped to fit the budget (§L3). Their id,
    /// severity and count are still here; `template_detail` has the wording.
    pub novel_text_dropped: usize,
    /// Novel templates dropped outright. Only a very large increment reaches
    /// this, and it is counted rather than silently cut.
    pub novel_omitted: usize,
    pub stages: Vec<Value>,
    pub boots_opened: Vec<i64>,
    pub crash_records: Vec<i64>,
    pub cursor: String,
    pub idle_ms: Option<i64>,
    /// The increment was larger than one call can account for. Counts below are
    /// then a floor, and the flag says so rather than letting them read as
    /// exact.
    pub scan_truncated: bool,
}

/// The prompt patterns `Predicate::Prompt` tests against.
///
/// ONE REPRESENTATION, so `follow` and `console_state` cannot disagree. This
/// used to be a private `Vec<(Regex, bool)>` -- the same patterns, flattened,
/// with its own matcher. That second matcher is what report #4 hit: it tested
/// stored lines with a bare `is_match`, while `Prompts::classify` (which
/// `console_state` and `boot_report` both use) knows that async kernel output
/// is not what a console is waiting at, that a printk can land ON the prompt's
/// own line, and that an unterminated prompt lives in the partial buffer. Two
/// matchers meant two verdicts from one evidence base.
#[derive(Debug, Clone)]
pub struct PromptSet {
    pub prompts: crate::runner::Prompts,
}

impl PromptSet {
    /// No patterns: a `{prompt:true}` predicate cannot be answered.
    pub fn empty() -> Self {
        Self {
            prompts: crate::runner::Prompts(Vec::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prompts.0.is_empty()
    }

    /// Was THIS LINE a commandable prompt?
    ///
    /// The per-line question a watch replay asks, which is genuinely different
    /// from "what is the console at" -- but it must not be a second MATCHER.
    /// This used to test the patterns directly with a bare `is_match`, missing
    /// everything `Prompts::classify` knows: that async kernel output is not
    /// what a console waits at, and that a printk can land on the prompt's own
    /// line. It delegates now, so there is one classifier in the codebase.
    pub fn line_is_prompt(&self, text: &str) -> bool {
        self.prompts.commandable(text).is_some()
    }
}

/// Evaluate the increment between `from` and the store's head.
///
/// Pure and synchronous: the caller decides how long to keep asking. That keeps
/// the predicate logic testable without a clock or a live device.
pub fn increment(
    store: &DeviceStore,
    from: &Cursor,
    until: &Predicate,
    max_lines: usize,
    prompts: &PromptSet,
    now_ms: i64,
) -> Result<Increment> {
    let start = store.resolve_cursor(from)?;
    // The tail cap bounds what is *returned*, not what is *counted*. Tying the
    // two together would make a boot loop under-report its own deltas, which is
    // exactly the number the agent is relying on instead of the raw output.
    let lines = store.lines_after(start, SCAN_LIMIT)?;
    let scan_truncated = lines.len() >= SCAN_LIMIT;
    let head = store.head_cursor();

    let bytes = lines
        .iter()
        .map(|l| l.bytes.len() as u64 + l.terminator.raw().len() as u64)
        .sum();
    let idle_ms = lines
        .last()
        .map(|l| l.ts_wall)
        .or_else(|| {
            store
                .recent_lines(1)
                .ok()
                .and_then(|v| v.first().map(|l| l.ts_wall))
        })
        .map(|t| (now_ms - t).max(0));

    // Records and templates for the same span.
    let first_id = lines.first().map(|l| l.id).unwrap_or(i64::MAX);
    let records = store.records_after_line(first_id, SCAN_LIMIT)?;
    let mut new_templates = Vec::new();
    // Deltas are capped independently of `max_lines`.
    //
    // A busy boot re-hits hundreds of known templates, and every one of them was
    // being returned in full. Measured against a real session: follow responses
    // of 65-87 KB EACH, dominated by this list, regardless of what max_lines the
    // caller asked for -- so the one tool meant for cheap incremental waiting was
    // the most expensive call in the surface. NEW templates are the interesting
    // half and stay uncapped; repeats of things already known get a ceiling and
    // an honest count of what was left out.
    // §L3. The deltas dominate the payload, and most of them carry text nobody
    // reads. Measured on a real boot: 21.5 KB of follow response for 45 KB of
    // console, over the 20 KB budget the whole mining story rests on.
    //
    // A REPEAT of a template that is neither novel nor severe is two integers
    // worth of news: "this fired again, N times". Its text is one
    // `template_detail` away for the rare case somebody wants it, and across ten
    // rounds nobody ever did. Novel templates and anything at warn-or-worse keep
    // their text, because those are the rows an agent actually reads.
    //
    // The budget is NOT raised to accommodate the payload. Relaxing the number
    // because the harness went red is the failure this project keeps catching in
    // itself; the payload gets smaller instead.
    const MAX_DELTAS: usize = 200;
    let mut template_deltas: Vec<Value> = Vec::new();
    let mut repeats: Vec<(bool, Value)> = Vec::new();
    let mut counts: std::collections::BTreeMap<i64, i64> = Default::default();
    let mut crash_records = Vec::new();
    for r in &records {
        if r.kind == RecordKind::Crash {
            crash_records.push(r.id);
        }
        if let Some(tid) = r.template_id {
            *counts.entry(tid).or_default() += 1;
        }
    }
    for (tid, n) in &counts {
        let t = store.template(*tid)?;
        // Novel to this increment when the template's first-ever record lies
        // inside the span. A template that merely fired again returns a *count*,
        // which is what keeps a forty-iteration boot loop from returning forty
        // times the output.
        let novel = store
            .records_for_template(*tid, None, None, 1, 0)?
            .first()
            .map(|r| r.first_line_id >= first_id)
            .unwrap_or(false);
        if novel {
            new_templates.push(json!({
                "template_id": t.id,
                "text": t.text,
                "severity": t.severity,
                "count": n,
            }));
        } else {
            // Severe repeats keep their text; ordinary ones become the count
            // that is all they were saying.
            let severe = t.severity <= Severity::Warn;
            repeats.push((
                severe,
                if severe {
                    json!({
                        "template_id": t.id,
                        "text": t.text,
                        "severity": t.severity,
                        "count": n,
                    })
                } else {
                    json!({"template_id": t.id, "count": n})
                },
            ));
        }
    }
    // Severe first, so a cap can only ever drop the rows nobody reads. Sorting
    // AFTER collection rather than truncating in map order is the difference
    // between "the 200 lowest template ids" and "the 200 that matter".
    repeats.sort_by_key(|(severe, _)| !*severe);
    let deltas_omitted = repeats.len().saturating_sub(MAX_DELTAS);
    template_deltas.extend(repeats.into_iter().take(MAX_DELTAS).map(|(_, v)| v));

    let stages = store.stages_after_line(first_id)?;
    let boots_opened = store.boots_after_offset(start)?;

    let (matched, evidence) = evaluate(
        until,
        &lines,
        &new_templates,
        &stages,
        &boots_opened,
        prompts,
        idle_ms,
        store,
    );

    let tail: Vec<Value> = lines
        .iter()
        .rev()
        .take(max_lines)
        .rev()
        .map(|l| {
            json!({
                "line_id": l.id,
                "offset": l.stream_offset,
                "ts_wall": l.ts_wall,
                "boot_id": l.boot_id,
                "text": l.lossy(),
            })
        })
        .collect();

    let mut inc = Increment {
        matched,
        evidence,
        lines: lines.len(),
        bytes,
        tail,
        new_templates,
        template_deltas,
        deltas_omitted,
        delta_text_dropped: 0,
        tail_omitted: 0,
        novel_text_dropped: 0,
        novel_omitted: 0,
        stages,
        boots_opened,
        crash_records,
        cursor: head.encode(),
        idle_ms,
        scan_truncated,
    };

    // §L3. THE BUDGET IS AN INVARIANT, NOT AN ASPIRATION.
    //
    // A count cap is a proxy for size, and a proxy drifts: measured on the ADP's
    // 129 KB epoch, 200 two-integer deltas plus this boot's novel templates
    // still came to 20,279 B against a 20,000 B budget. So the response is
    // measured and trimmed until it fits, and everything it dropped is counted
    // -- a truncated answer that says nothing about being truncated is the shape
    // of every silent cut this project has had to fix.
    //
    // THE ORDER IS THE DESIGN. Trimming in the wrong order is how the first
    // version of this ladder discarded every mined row to preserve a hundred
    // RAW lines, which is exactly backwards for a tool whose whole claim is that
    // the summary beats the log. Cheapest to lose goes first:
    //
    //   1. ordinary repeats  -- a count, and the text is one call away
    //   2. tail past 30      -- raw lines, and `lines`/`search` still have them
    //   3. severe repeats    -- degraded to id+count, not dropped
    //   4. novel text past 20-- degraded to id+severity+count, not dropped
    //   5. novel rows        -- the last thing to go, and only under a flood
    const RESPONSE_BUDGET: usize = 20_000;
    // The tool surface wraps this in a device + freshness envelope before it
    // reaches the wire, and the budget is measured out there. Measured at
    // 2.3-2.4 KB on this rig, so the allowance is not a guess.
    const ENVELOPE_ALLOWANCE: usize = 3_000;
    const TAIL_FLOOR: usize = 30;
    const NOVEL_TEXT_KEPT: usize = 20;
    let fits = RESPONSE_BUDGET.saturating_sub(ENVELOPE_ALLOWANCE);
    let over = |inc: &Increment| serde_json::to_string(inc).map(|s| s.len()).unwrap_or(0) > fits;

    // 1. Ordinary repeats: the sort above put severe first, so the end of the
    //    vector is always the least useful row left.
    while over(&inc)
        && inc
            .template_deltas
            .last()
            .is_some_and(|d| d.get("text").is_none())
    {
        let drop = (inc.template_deltas.len() / 10).max(1);
        let keep = inc.template_deltas.len().saturating_sub(drop);
        inc.template_deltas.truncate(keep);
        inc.deltas_omitted += drop;
    }

    // 2. Raw tail, down to a floor that still shows what the console was doing.
    while over(&inc) && inc.tail.len() > TAIL_FLOOR {
        let drop = ((inc.tail.len() - TAIL_FLOOR) / 2).max(1);
        // The OLDEST lines go: the end of the tail is the freshest evidence.
        inc.tail.drain(0..drop);
        inc.tail_omitted += drop;
    }

    // 3. What is left of the deltas -- severe ones, degraded to id + count
    //    rather than dropped, so the ids still say WHICH errors repeated.
    if over(&inc) {
        for d in inc.template_deltas.iter_mut() {
            if let Some(o) = d.as_object_mut() {
                if o.remove("text").is_some() {
                    inc.delta_text_dropped += 1;
                }
            }
        }
    }

    // 4. The TEXT of novel templates past the first handful. A novel template is
    //    the most valuable row here, so it is degraded rather than dropped: id,
    //    severity and count still say "this is new and it fired eleven times",
    //    and `template_detail` has the wording for the one an agent reads.
    if over(&inc) {
        for e in inc.new_templates.iter_mut().skip(NOVEL_TEXT_KEPT) {
            if let Some(o) = e.as_object_mut() {
                if o.remove("text").is_some() {
                    inc.novel_text_dropped += 1;
                }
            }
        }
    }

    // 5. Novel rows themselves. Only reachable when one increment mints hundreds
    //    of them, and still counted rather than silently cut.
    while over(&inc) && inc.new_templates.len() > 1 {
        let drop = (inc.new_templates.len() / 10).max(1);
        let keep = inc.new_templates.len() - drop;
        inc.new_templates.truncate(keep);
        inc.novel_omitted += drop;
    }

    Ok(inc)
}

#[allow(clippy::too_many_arguments)]
fn evaluate(
    until: &Predicate,
    lines: &[crate::store::LineRow],
    new_templates: &[Value],
    stages: &[Value],
    boots: &[i64],
    prompts: &PromptSet,
    idle_ms: Option<i64>,
    store: &DeviceStore,
) -> (Option<String>, Option<Value>) {
    match until {
        Predicate::Watch(name) => {
            // §F6. A watch fires when it has something undelivered. The scanner
            // already queues firings; this just lets a parked `follow` be woken
            // by one instead of an agent polling on a timer.
            match store.peek_watch_hits(name, 1) {
                Ok(hits) if !hits.is_empty() => {
                    let h = &hits[0];
                    (
                        Some(until.label()),
                        Some(json!({
                            "watch": name,
                            "at": h.at,
                            "offset": h.stream_offset,
                            "matched": h.matched,
                            "evidence": h.evidence,
                        })),
                    )
                }
                _ => (None, None),
            }
        }
        Predicate::Any(list) => {
            for p in list {
                let (m, e) = evaluate(
                    p,
                    lines,
                    new_templates,
                    stages,
                    boots,
                    prompts,
                    idle_ms,
                    store,
                );
                if m.is_some() {
                    // The response says *which* predicate fired, so the agent
                    // does not have to re-derive why it woke up.
                    return (m, e);
                }
            }
            (None, None)
        }
        Predicate::Pattern(p) => {
            let re = match regex::Regex::new(p) {
                Ok(r) => r,
                Err(_) => return (None, None),
            };
            for l in lines {
                let text = l.lossy();
                if re.is_match(&text) {
                    return (
                        Some(until.label()),
                        Some(json!({"line_id": l.id, "text": text})),
                    );
                }
            }
            (None, None)
        }
        Predicate::TemplateNew => new_templates
            .first()
            .map(|t| (Some(until.label()), Some(t.clone())))
            .unwrap_or((None, None)),
        Predicate::Stage(want) => stages
            .iter()
            .find(|s| s["name"] == want.as_str())
            .map(|s| (Some(until.label()), Some(s.clone())))
            .unwrap_or((None, None)),
        Predicate::Prompt => {
            // A PROMPT FROM BEFORE THIS EPOCH IS NOT THIS EPOCH REACHING A PROMPT.
            //
            // `follow` with no cursor starts a grace window BEFORE the epoch
            // boundary, so the first seconds of a boot -- which are attributed to
            // the epoch that is closing, because the boundary is placed when the
            // tool ran and not when the board acted -- are not missed. That grace
            // also contains the PREVIOUS epoch's prompt, and matching it answered
            // "the board reached its prompt" the instant the follow started.
            //
            // Measured on the Uno-Q: a reset opened epoch 276, and
            // `until:{prompt:true}` returned immediately with `sirocco> ` from
            // line 66730 while epoch 276 had got as far as `APP admit`/`CONSOLE`.
            // An agent then ran commands against a board that was still booting.
            //
            // The boundary grace is right for stages and patterns -- early boot
            // output genuinely lands there -- and wrong for a prompt, which by
            // definition cannot precede the boot that is supposed to reach it.
            // The current BOOT, not the current EPOCH. `session` and `mark`
            // epochs open without the board restarting -- a capture reconnect
            // opens one, so every deploy adds another -- and comparing against
            // the newest epoch id therefore discarded the very lines the boot
            // produced. Measured on the Uno-Q: epoch 473 was the power-on that
            // reached `sirocco> `, epochs 474-477 were empty `session` markers,
            // and `until:{prompt:true}` skipped every line in 473 and timed out
            // against a board that had been sitting at its shell for twenty
            // minutes. Forcing a newline with `run_command` "fixed" it only
            // because the new line landed in the current epoch.
            //
            // ONE ORACLE. This predicate used to carry its own flattened copy of
            // the prompt patterns, its own matcher, and its own scan of the
            // stored lines, and then call into `console` for the live half. That
            // second implementation is what let `console_state` say `at_prompt,
            // commandable` while this returned nothing at all. Both halves are
            // now the same function `console_state` and `boot_report` ask, so
            // they cannot answer differently.
            let current = crate::console::boot_floor(store).ok().flatten();
            if prompts.is_empty() {
                return (None, None);
            }
            let found = current.and_then(|floor| {
                crate::console::prompt_for_boot(store, &prompts.prompts, floor, true)
                    .ok()
                    .flatten()
            });
            match found {
                Some((p, src)) => (
                    Some(until.label()),
                    Some(json!({
                        "text": p.raw,
                        "boot_id": current,
                        // Whether the board SAID this (a stored line) or is
                        // sitting on it with no newline yet. An agent plans
                        // differently for each.
                        "unterminated": src.is_unterminated(),
                        "note": "the console is at this prompt; the same answer \
                                 console_state and boot_report give",
                    })),
                ),
                None => (None, None),
            }
        }
        Predicate::QuietMs(want) => match idle_ms {
            Some(ms) if ms >= *want => (
                Some(until.label()),
                Some(json!({"idle_ms": ms, "required_ms": want})),
            ),
            _ => (None, None),
        },
        Predicate::Reset => boots
            .first()
            .map(|b| (Some(until.label()), Some(json!({"boot_id": b}))))
            .unwrap_or((None, None)),
    }
}

/// Every firing of `until` in the stored stream from `from_offset` onwards.
///
/// This is the durable-watch counterpart to [`increment`]. `increment` answers
/// "has it happened yet?" and stops at the first hit, because a live `follow`
/// returns as soon as its predicate is satisfied. A watch is read *after* the
/// fact, so the useful answer is every firing with its own offset and timestamp:
/// an agent that reconnects after twenty minutes wants the three resets it
/// missed, not the first one.
///
/// Returns the hits and the offset scanned to. The offset is the caller's resume
/// point and is deliberately conservative: it advances only over lines actually
/// examined, so a capped scan re-reads rather than skips.
pub fn scan_hits(
    store: &DeviceStore,
    from_offset: u64,
    until: &Predicate,
    max_hits: usize,
    prompts: &PromptSet,
    now_ms: i64,
) -> Result<(Vec<crate::store::WatchHit>, u64)> {
    let lines = store.lines_after(from_offset, SCAN_LIMIT)?;
    let scanned_to = lines
        .last()
        .map(|l| l.end_offset())
        .unwrap_or(from_offset)
        .max(from_offset);

    let mut hits = Vec::new();
    collect_hits(
        store,
        until,
        &lines,
        from_offset,
        prompts,
        now_ms,
        &mut hits,
    )?;
    // Stream order, not predicate order: an `any:[…]` watch must read as a
    // timeline, otherwise "what happened while I was gone" comes back grouped by
    // predicate and the agent has to re-sort it to see the sequence.
    hits.sort_by_key(|h| (h.stream_offset, h.at));
    hits.truncate(max_hits);
    Ok((hits, scanned_to))
}

fn collect_hits(
    store: &DeviceStore,
    until: &Predicate,
    lines: &[crate::store::LineRow],
    from_offset: u64,
    prompts: &PromptSet,
    now_ms: i64,
    out: &mut Vec<crate::store::WatchHit>,
) -> Result<()> {
    use crate::store::WatchHit;
    let label = until.label();
    match until {
        // A watch that watches another watch would need the scanner to run in a
        // defined order and would double-record the same event. `follow` can
        // wait on a watch (§F6); a watch cannot BE another watch.
        Predicate::Watch(_) => {}
        Predicate::Any(list) => {
            for p in list {
                collect_hits(store, p, lines, from_offset, prompts, now_ms, out)?;
            }
        }
        Predicate::Pattern(p) => {
            let re = regex::Regex::new(p)
                .map_err(|e| ToolError::invalid_arg(format!("invalid watch pattern: {e}")))?;
            for l in lines {
                let text = l.lossy();
                if re.is_match(&text) {
                    out.push(WatchHit {
                        at: l.ts_wall,
                        stream_offset: l.stream_offset,
                        matched: label.clone(),
                        evidence: json!({"line_id": l.id, "text": text}),
                    });
                }
            }
        }
        Predicate::TemplateNew => {
            let first_id = lines.first().map(|l| l.id).unwrap_or(i64::MAX);
            let records = store.records_after_line(first_id, SCAN_LIMIT)?;
            let mut seen = std::collections::BTreeSet::new();
            for r in &records {
                let Some(tid) = r.template_id else { continue };
                if !seen.insert(tid) {
                    continue;
                }
                let novel = store
                    .records_for_template(tid, None, None, 1, 0)?
                    .first()
                    .map(|f| f.first_line_id >= first_id)
                    .unwrap_or(false);
                if !novel {
                    continue;
                }
                let t = store.template(tid)?;
                let line = store.line(r.first_line_id)?;
                out.push(WatchHit {
                    at: line.ts_wall,
                    stream_offset: line.stream_offset,
                    matched: label.clone(),
                    evidence: json!({
                        "template_id": t.id, "text": t.text, "severity": t.severity,
                        "record_id": r.id,
                    }),
                });
            }
        }
        Predicate::Stage(want) => {
            let first_id = lines.first().map(|l| l.id).unwrap_or(i64::MAX);
            for s in store.stages_after_line(first_id)? {
                if s["name"] != want.as_str() {
                    continue;
                }
                let offset = s["banner_line_id"]
                    .as_i64()
                    .and_then(|id| store.line(id).ok())
                    .map(|l| l.stream_offset)
                    .unwrap_or(from_offset);
                out.push(WatchHit {
                    at: s["entered_ts"].as_i64().unwrap_or(now_ms),
                    stream_offset: offset,
                    matched: label.clone(),
                    evidence: s,
                });
            }
        }
        Predicate::Prompt => {
            for l in lines {
                let text = l.lossy();
                if prompts.line_is_prompt(&text) {
                    out.push(WatchHit {
                        at: l.ts_wall,
                        stream_offset: l.stream_offset,
                        matched: label.clone(),
                        evidence: json!({"line_id": l.id, "text": text}),
                    });
                }
            }
        }
        Predicate::QuietMs(want) => {
            // Every gap in the *stored* stream, plus the trailing gap to now.
            // Replaying gaps is what makes a watch equivalent to having been
            // connected: silence is an event, and it is recoverable after the
            // fact because the timestamps are durable.
            for pair in lines.windows(2) {
                let gap = pair[1].ts_wall - pair[0].ts_wall;
                if gap >= *want {
                    out.push(WatchHit {
                        at: pair[0].ts_wall + gap,
                        stream_offset: pair[1].stream_offset,
                        matched: label.clone(),
                        evidence: json!({
                            "idle_ms": gap, "required_ms": want,
                            "before_line_id": pair[0].id, "after_line_id": pair[1].id,
                        }),
                    });
                }
            }
            if let Some(last) = lines.last() {
                let gap = now_ms - last.ts_wall;
                if gap >= *want {
                    out.push(WatchHit {
                        at: now_ms,
                        stream_offset: last.end_offset(),
                        matched: label.clone(),
                        evidence: json!({
                            "idle_ms": gap, "required_ms": want,
                            "before_line_id": last.id, "still_silent": true,
                        }),
                    });
                }
            }
        }
        Predicate::Reset => {
            for b in store.boots_after_offset(from_offset)? {
                let row = store.boot(b)?;
                out.push(WatchHit {
                    at: row.opened_at,
                    stream_offset: row.opened_offset,
                    matched: label.clone(),
                    evidence: json!({"boot_id": row.id, "seq": row.seq,
                                     "opened_by": row.opened_by}),
                });
            }
        }
    }
    Ok(())
}

/// Validate a caller-supplied timeout against `api.follow_timeout_max_s`.
pub fn clamp_timeout(requested: Option<i64>, default_s: u64, max_s: u64) -> Result<i64> {
    let t = requested.unwrap_or(default_s as i64);
    if t <= 0 {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            "timeout must be positive",
        ));
    }
    Ok(t.min(max_s as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_parse_from_their_json_form() {
        assert_eq!(
            Predicate::parse(&json!({"pattern": "Kernel panic"})).unwrap(),
            Predicate::Pattern("Kernel panic".into())
        );
        assert_eq!(
            Predicate::parse(&json!({"template": "new"})).unwrap(),
            Predicate::TemplateNew
        );
        assert_eq!(
            Predicate::parse(&json!({"stage": "userspace"})).unwrap(),
            Predicate::Stage("userspace".into())
        );
        assert_eq!(
            Predicate::parse(&json!({"prompt": true})).unwrap(),
            Predicate::Prompt
        );
        assert_eq!(
            Predicate::parse(&json!({"quiet": 30000})).unwrap(),
            Predicate::QuietMs(30000)
        );
        let any = Predicate::parse(&json!({
            "any": [{"prompt": true}, {"template": "new"}, {"quiet": 30000}]
        }))
        .unwrap();
        assert!(matches!(any, Predicate::Any(ref v) if v.len() == 3));
    }

    #[test]
    fn a_malformed_predicate_is_a_structured_error() {
        for bad in [
            json!({}),
            json!({"template": "old"}),
            json!({"pattern": "([unclosed"}),
        ] {
            assert_eq!(
                Predicate::parse(&bad).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
    }

    #[test]
    fn timeouts_are_clamped_to_the_configured_ceiling() {
        assert_eq!(clamp_timeout(None, 30, 600).unwrap(), 30);
        assert_eq!(clamp_timeout(Some(120), 30, 600).unwrap(), 120);
        assert_eq!(clamp_timeout(Some(99_999), 30, 600).unwrap(), 600);
        assert!(clamp_timeout(Some(0), 30, 600).is_err());
    }

    #[test]
    fn a_credential_gate_is_not_a_prompt() {
        let set = PromptSet {
            prompts: crate::runner::Prompts(vec![
                crate::runner::Prompt {
                    re: regex::Regex::new("=> $").unwrap(),
                    raw: "=> $".into(),
                    kind: crate::framer::profile::PromptKind::Bootloader,
                },
                crate::runner::Prompt {
                    re: regex::Regex::new("login: $").unwrap(),
                    raw: "login: $".into(),
                    kind: crate::framer::profile::PromptKind::CredentialGate,
                },
            ]),
        };
        assert!(set.line_is_prompt("=> "));
        assert!(
            !set.line_is_prompt("board login: "),
            "the board is up but not commandable; prompt:true must not fire"
        );
    }
}
