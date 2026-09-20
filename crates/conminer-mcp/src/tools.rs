//! The MCP tool surface (§8).
//!
//! Every tool obeys the same three rules, because they are what make the whole
//! system usable by an agent rather than merely queryable:
//!
//! * **Response budget discipline** — hard caps, totals and cursors, never an
//!   unbounded dump. A looping kernel crash comes back as
//!   `{template, count: 41283, first_ts, last_ts}` plus three examples.
//! * **Structured errors** (§14.6) — `code`, `message`, `hint`, and a `detail`
//!   payload an agent can act on (candidate device lists, tails, offending keys).
//! * **A freshness envelope** (§8.4) on every read, so an answer describes its
//!   own currency instead of inviting the agent to assume it is current.

use crate::state::Context;
use conminer_core::console::ConsoleState;
use conminer_core::error::{ErrorCode, Result, ToolError};
use conminer_core::follow::Predicate;
use conminer_core::search::{SearchMode, SearchQuery, SearchScope};
use conminer_core::store::{
    DeviceRow, RecordKind, Severity, TemplateOrder, TemplateQuery, Verdict,
};
use serde_json::{json, Map, Value};

/// One tool: its MCP advertisement and its implementation.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub call: fn(&Context, &Map<String, Value>) -> Result<Value>,
    /// Mutating tools require the §15.1 lease; reads never do.
    pub mutating: bool,
}

// ------------------------------------------------------------- arg helpers ---

fn s<'a>(a: &'a Map<String, Value>, k: &str) -> Result<&'a str> {
    a.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_arg(format!("missing string argument `{k}`")))
}

fn opt_s<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

fn i(a: &Map<String, Value>, k: &str) -> Result<i64> {
    a.get(k)
        .and_then(Value::as_i64)
        .ok_or_else(|| ToolError::invalid_arg(format!("missing integer argument `{k}`")))
}

fn opt_i(a: &Map<String, Value>, k: &str) -> Option<i64> {
    a.get(k).and_then(Value::as_i64)
}

fn flag(a: &Map<String, Value>, k: &str) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Clamp a caller-supplied limit to the configured cap (§16 `api.*`).
///
/// Silently returning fewer results than asked for would be a lie; the response
/// always reports `capped` so the agent knows to page.
fn capped(a: &Map<String, Value>, k: &str, default: usize, max: usize) -> usize {
    a.get(k)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
        .clamp(1, max)
}

fn device(ctx: &Context, a: &Map<String, Value>) -> Result<DeviceRow> {
    ctx.device_or_only(opt_s(a, "device"))
}

/// Per-call output options, set by the handler from the universal arguments
/// `freshness` and `ansi`.
///
/// Thread-local rather than a field on `Context`, because `Context` is shared by
/// every concurrent request while a tool body runs start-to-finish on the thread
/// that invoked it: one caller asking for a slim response must never slim
/// another caller's.
#[derive(Debug, Clone)]
pub struct CallOpts {
    /// §P1. Who is asking, when the call arrived from another node:
    /// `<node>/<agent>`. Empty for a local call.
    ///
    /// This is what makes a lease mean the same thing on both sides of a fleet.
    /// It is per-CALL rather than per-process because the alternative -- setting
    /// a process-wide holder when a proxied request arrives -- would have one
    /// node's agent silently inherit another's identity for every call after it.
    pub origin: String,
    /// §P2. The nodes this call has already traversed, oldest first.
    ///
    /// Relaying means a node forwards on somebody else's behalf, so a fleet
    /// whose routes disagree even briefly can hand a call back to a node that
    /// already saw it. Carrying the path makes that detectable at the moment of
    /// forwarding rather than as a hang; a hop count alone cannot tell a long
    /// chain from a two-node ping-pong.
    pub path: Vec<String>,
    /// Attach the freshness envelope. Off is for callers who poll in a loop and
    /// already know the console state.
    pub envelope: bool,
    /// Strip ANSI/VT escapes from console text in responses.
    pub strip_ansi: bool,
}

impl Default for CallOpts {
    fn default() -> Self {
        // Stripping by default: escape sequences are for a terminal, and every
        // consumer of these fields is a model or a diff. They cost tokens, they
        // make identical lines compare unequal, and no tool here renders them.
        // `ansi: "keep"` is there for the caller replaying into a real terminal.
        Self {
            envelope: true,
            strip_ansi: true,
            origin: String::new(),
            path: Vec::new(),
        }
    }
}

thread_local! {
    // A RefCell rather than a Cell: the origin is a String, so these options are
    // no longer Copy. The cost is one borrow per call and the gain is that WHO
    // is asking travels with the same mechanism as HOW they want the answer
    // shaped -- one place to reason about, one place to clear.
    // `const`: every field is now const-constructible, so the thread-local
    // needs no lazy initialisation check on each access.
    static CALL_OPTS: std::cell::RefCell<CallOpts> = const {
        std::cell::RefCell::new(CallOpts {
            path: Vec::new(),
            envelope: true,
            strip_ansi: true,
            origin: String::new(),
        })
    };
}

/// §P1. Remember who is asking, for the duration of one call.
///
/// Set from the request's origin header before dispatch and cleared after, so a
/// proxied call leases as its caller and the next local call on this thread does
/// not inherit that identity.
pub fn set_origin(origin: &str) {
    CALL_OPTS.with(|c| c.borrow_mut().origin = origin.to_string());
}

/// The nodes this call has already passed through (§P2).
///
/// Rides in the same thread-local as the origin, for the same reason: it is set
/// once when a proxied request arrives and must not leak into the next request
/// on this thread. Empty for a call that started here.
pub fn set_call_path(path: &str) {
    CALL_OPTS.with(|c| {
        c.borrow_mut().path = path
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    });
}

pub fn call_path() -> Vec<String> {
    CALL_OPTS.with(|c| c.borrow().path.clone())
}

/// Set the response-shaping options for this call.
///
/// The ORIGIN is deliberately preserved: it is set once when a proxied request
/// arrives and must survive the per-call options that the dispatcher installs
/// immediately afterwards. Clobbering it here is how a federated lease would
/// quietly go back to being anonymous.
pub fn set_call_opts(o: CallOpts) {
    CALL_OPTS.with(|c| {
        let mut cur = c.borrow_mut();
        let path = cur.path.clone();
        let origin = std::mem::take(&mut cur.origin);
        *cur = o;
        cur.origin = origin;
        cur.path = path;
    });
}

pub fn call_opts() -> CallOpts {
    CALL_OPTS.with(|c| c.borrow().clone())
}

/// Wrap a payload with the freshness envelope.
fn fresh(ctx: &Context, dev: &DeviceRow, mut payload: Value) -> Result<Value> {
    if !call_opts().envelope {
        // The caller asked for the answer without the standing context. Still
        // name the device: a response that cannot be attributed is worse than a
        // large one.
        if let Some(o) = payload.as_object_mut() {
            o.insert("device".into(), json!(dev.display_name()));
        }
        return Ok(payload);
    }
    // Checked before the mutable borrow below.
    let has_console = payload.get("console").is_some();
    if let Some(o) = payload.as_object_mut() {
        o.insert("device".into(), json!(dev.display_name()));
        let mut env = ctx.freshness(dev)?;
        // The perceived state travels with every response, so an agent is *told*
        // what the console is doing rather than deducing it (§8.5).
        // ...but not twice. A tool whose whole answer IS the console state (or
        // that already reports it) would otherwise ship the same block at top
        // level and again under `freshness`: console_state was measured emitting
        // `distinct_fingerprints` four times in a 1276-byte response.
        if !has_console {
            if let (Some(e), Ok(state)) = (env.as_object_mut(), console_state(ctx, dev)) {
                e.insert("console".into(), state);
            }
        }
        o.insert("freshness".into(), env);
    }
    Ok(payload)
}

fn severity_from(a: &Map<String, Value>, k: &str) -> Option<Severity> {
    let v = a.get(k)?.as_str()?;
    serde_json::from_value(Value::String(v.to_string())).ok()
}

fn order_from(a: &Map<String, Value>) -> TemplateOrder {
    match opt_s(a, "order") {
        Some("first_seen") => TemplateOrder::FirstSeen,
        Some("last_seen") => TemplateOrder::LastSeen,
        Some("severity") => TemplateOrder::Severity,
        _ => TemplateOrder::Count,
    }
}

const DEVICE_ARG: &str = "Device selector: nickname, canonical id, `tag:k=v` query, or a unique \
substring. Omit when only one device exists.";

/// Run an async operation from a synchronous tool handler.
///
/// Tool calls already execute on a blocking thread, so a small current-thread
/// runtime here is the cheapest correct bridge — and it keeps every tool a plain
/// function, which is what makes the registry a simple table.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

/// The prompt set for a device: profile-supplied defaults plus everything the
/// device has been taught or observed to settle at (§8.5).
/// Project a template row for the wire.
///
/// The table of contents is the thing an agent reads *instead of* the log, so
/// its cost per row is the whole argument for the tool existing. Measured on a
/// 109-template device the full row was ~510 bytes against ~55 bytes for the
/// average console line it stands for: a 10x dedup gain handed straight back as
/// per-row metadata. `tokens` alone was 18% of the response and is a second copy
/// of `text`; the three absolute-millisecond timestamps were another 24%.
///
/// Compact keeps exactly what triage branches on. Everything dropped is one
/// `template_detail` call away, and that call is made for a handful of rows, not
/// for all of them.
fn project_template(t: &conminer_core::store::TemplateRow, full: bool) -> Value {
    if full {
        return serde_json::to_value(t).unwrap_or(Value::Null);
    }
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(t.id));
    o.insert("text".into(), json!(t.text));
    o.insert(
        "count".into(),
        json!(t.scoped_count.unwrap_or(t.total_count)),
    );
    // Omit fields whose value is "unknown": on a fresh boot most rows carry
    // severity=unknown AND stage=unknown, so this was ~40 bytes per row of the
    // agent being told nothing. Absent means unknown, and the schema says so.
    if t.severity != conminer_core::store::Severity::Unknown {
        o.insert("severity".into(), json!(t.severity));
    }
    if let Some(s) = &t.stage {
        if s != "unknown" {
            o.insert("stage".into(), json!(s));
        }
    }
    // Only the last-seen instant survives, and only when the scope computed one:
    // "when did this last happen" is actionable, "when was it first seen in
    // session 3" is provenance and belongs in the detail call.
    if let Some(ts) = t.scoped_last_ts {
        o.insert("last_ts".into(), json!(ts));
    }
    if let Some(v) = t.verdict {
        o.insert("verdict".into(), json!(v));
        // The note is the entire reason the verdict was recorded, and it only
        // exists on the minority of rows that carry one.
        if let Some(n) = &t.verdict_note {
            o.insert("note".into(), json!(n));
        }
        if let Some(k) = &t.verdict_ticket {
            o.insert("ticket".into(), json!(k));
        }
    }
    Value::Object(o)
}

/// Every present device with its USB by-path, so a controller can be matched to
/// the board it actually sits on. Two identical controllers on one bench cannot
/// be told apart by name.
fn present_with_topology(ctx: &Context) -> Vec<(String, Option<String>)> {
    // PRESENT, as the name says. This was every row the registry had ever held,
    // so a console could resolve a controller that had been unplugged for days
    // -- and, because a peer's canonical id embeds the owner's device path, one
    // sitting on another host entirely. Seven call sites read this list; they
    // all now get the same answer as dashd and the announce path.
    ctx.registry()
        .all_devices()
        .map(|ds| conminer_core::store::registry::present_on_this_host(&ds))
        .unwrap_or_default()
}

/// The commandable-prompt set `follow` and the watches match against.
///
/// A CONSOLE THAT IS NOT THERE HAS NO PROMPT TO BE AT.
///
/// EDL re-enumerates the board's UART away, so the prompt still sitting in the
/// capture loop's partial buffer is a memory of the console it had. `console_state`
/// has always refused on that basis; `follow` never knew about capture state at
/// all, so `until:{prompt:true}` kept firing `matched: prompt` for a board in
/// download mode -- and the agent's next move is a command typed into a UART
/// that no longer exists. Found by the agreement invariant rather than on the
/// bench, which is the whole point of it.
fn prompt_set(ctx: &Context, dev: &DeviceRow) -> Result<conminer_core::follow::PromptSet> {
    // Live capture health (§W4): `follow{prompt}` must not fire for a board an
    // actuation just sent into EDL, even on the row this call was resolved with.
    let health = ctx.capture_health(dev);
    if health == conminer_core::live::CaptureState::AwayInEdl.as_str() {
        return Ok(conminer_core::follow::PromptSet::empty());
    }
    Ok(conminer_core::follow::PromptSet {
        prompts: prompts_for(ctx, dev)?,
    })
}

pub(crate) fn prompts_for(
    ctx: &Context,
    dev: &DeviceRow,
) -> Result<conminer_core::runner::Prompts> {
    use conminer_core::framer::profile::PromptKind;
    use conminer_core::runner::{Prompt, Prompts};
    let mut out = Vec::new();
    for p in ctx.profiles().all() {
        for pat in &p.prompts {
            out.push(Prompt {
                re: pat.re.clone(),
                raw: pat.raw.clone(),
                kind: pat.kind,
            });
        }
    }
    for l in ctx.with_store(dev, |st| st.prompts(None))? {
        if let (Ok(re), Some(kind)) = (regex::Regex::new(&l.pattern), PromptKind::parse(&l.kind)) {
            // A learned prompt outranks a profile default: it is what this
            // device actually settles at.
            out.insert(
                0,
                Prompt {
                    re,
                    raw: l.pattern,
                    kind,
                },
            );
        }
    }
    Ok(Prompts(out))
}

/// The device's ser2net endpoint, or a structured error explaining why not.
fn endpoint_for(ctx: &Context, dev: &DeviceRow) -> Result<String> {
    let port = dev.ser2net_port.ok_or_else(|| {
        ToolError::new(
            ErrorCode::DeviceGone,
            format!("{} has no ser2net endpoint", dev.display_name()),
        )
        .with_hint("discoveryd assigns one; is the device present and not excluded?")
    })?;
    Ok(format!("{}:{port}", ctx.config().ser2net_host()))
}

/// Read the device's live console state (§8.5).
fn console_state(ctx: &Context, dev: &DeviceRow) -> Result<serde_json::Value> {
    use conminer_core::console::{derive, Observation};
    use conminer_core::live::CaptureState;
    // CAPTURE HEALTH, FROM THE COLUMN THAT HOLDS IT.
    //
    // This read `state`, which is PRESENCE since capture health moved to its own
    // column -- so `discovered` (what discovery writes on every sweep) fell
    // through to NotListening and every console on the bench reported
    // `commandable: false` with "no live capture attestation", while MCP TX
    // worked perfectly. Reported from the web console within minutes of that
    // deploy: a regression of mine, and exactly the coupling the split was
    // supposed to end.
    // Live, not the snapshot row: an actuation earlier in THIS call may have
    // published a new capture state, and the console block in the envelope must
    // reflect it (§W4, report #7).
    let health_owned = ctx.capture_health(dev);
    let health = health_owned.as_str();
    let capture = match health {
        "listening" => CaptureState::Listening,
        "streaming" => CaptureState::Streaming,
        "garbage" => CaptureState::Garbage,
        // EDL IS NOT "I DO NOT KNOW". Folding it into NotListening threw away
        // the one fact that matters: the UART re-enumerated away on purpose, and
        // no bytes can arrive to contradict the prompt still sitting in the
        // store. So a board freshly in EDL kept reporting `at_prompt,
        // commandable: true` -- in the response of the very call that put it
        // there. The distinction between off, idle and in-EDL is the whole point
        // of naming these states.
        "away_in_edl" => CaptureState::AwayInEdl,
        // `open_failed` remains a genuine "no attestation to be had": ser2net
        // could not open the tty and cannot say why.
        _ => CaptureState::NotListening,
    };
    let obs = Observation {
        capture,
        now_ms: ctx.now(),
        hung_after_ms: ctx
            .config()
            .hung_after_s_for(&[dev.display_name(), dev.label().unwrap_or_default()])
            as i64
            * 1000,
        loop_min_epochs: ctx.config().state.loop_min_epochs,
        active_txn: None,
    };
    // AN ACTUATION IN FLIGHT OUTRANKS THE BUFFER, exactly as a transaction
    // does: conminer is the one pressing the buttons, so it knows what the
    // console is doing better than the prompt it printed before the press.
    if let Some(f) = ctx.actuation_in_flight(dev.id) {
        let mut v = conminer_core::console::to_json(&ConsoleState::Actuating {
            tool: f.tool.to_string(),
            action: f.action.clone(),
            phase: f.phase.clone(),
        });
        if let Some(o) = v.as_object_mut() {
            o.insert("actuation".into(), f.to_json(ctx.now()));
        }
        return Ok(v);
    }
    let prompts = prompts_for(ctx, dev)?;
    let state = ctx.with_store(dev, |st| derive(st, &prompts, &obs))?;
    let (state, last_known, annotated) =
        decay_when_the_board_stops_answering(ctx, dev, state, &obs)?;
    if let Some(v) = annotated {
        return Ok(v);
    }
    let mut v = conminer_core::console::to_json(&state);
    if let (Some(o), Some(last)) = (v.as_object_mut(), last_known) {
        o.insert("last_known".into(), last);
    }
    Ok(v)
}

/// §M1. A CLAIM THAT THE BOARD IS UP MUST DECAY WHEN THE BOARD STOPS ANSWERING.
///
/// Every state below says, one way or another, "the board is there": a login
/// gate is waiting, a shell is idle, epochs are repeating. Each was true when it
/// was observed, and each keeps its shape long after the power goes away,
/// because the evidence behind it -- a prompt in the buffer, fingerprints in the
/// chain -- does not expire on its own. Measured on this rig: a board verified
/// off reported `login_wait`, "the board is up and waiting for a login", and a
/// dark ADP reported `boot_looping` from fingerprints minted an hour earlier.
///
/// So a present-tense claim survives only while the present tense supports it.
/// TWO things must hold before it decays: silence past the hung threshold, and
/// evidence the board is not answering -- the controller saying `off`, or, for a
/// controller with no sense line, an actuation epoch that produced no bytes at
/// all. Neither alone is enough: a quiet shell is still a shell, and a board
/// that is talking is up whatever a power line claims.
///
/// Nothing is thrown away. The decayed claim rides along as `last_known`, so
/// "it was at a login gate before it went dark" is still one field away -- it
/// just stops being asserted as what the console is doing NOW.
fn decay_when_the_board_stops_answering(
    ctx: &Context,
    dev: &DeviceRow,
    state: ConsoleState,
    obs: &conminer_core::console::Observation,
) -> Result<(ConsoleState, Option<Value>, Option<Value>)> {
    let claims_the_board_is_up = matches!(
        state,
        ConsoleState::BootLooping { .. }
            | ConsoleState::Unstable { .. }
            | ConsoleState::LoginWait { .. }
            | ConsoleState::AtPrompt { .. }
            | ConsoleState::AtPromptWithTraffic { .. }
            | ConsoleState::AtUnknownPrompt { .. }
            | ConsoleState::Streaming
            | ConsoleState::Booting { .. }
    );
    if !claims_the_board_is_up {
        return Ok((state, None, None));
    }
    let (silent, quiet_epoch, off_verified) = ctx.with_store(dev, |st| {
        let silent = conminer_core::console::silence_ms(st, obs.now_ms)?;
        // An epoch opened by an ACTUATION that then produced nothing is the
        // no-sense-line controller's version of "the board did not come back".
        let quiet_epoch = st
            .latest_boot()?
            .is_some_and(|b| b.bytes == 0 && b.opened_by != "session");
        let off_verified = verified_off_after_last_byte(st)?;
        Ok((silent, quiet_epoch, off_verified))
    })?;
    // A VERIFIED OFF CONTRADICTS THE CLAIM OUTRIGHT, no silence threshold and
    // no sense line needed: conminer itself concluded the board went down, and
    // nothing has arrived since. Report #15 (bravo, Uno Q): an `off` that
    // escalated out of EDL let the board boot to `sirocco> ` mid-workflow, then
    // powered it off; the epoch held a full boot log so it was not "quiet", the
    // Bughopper has no sense line, and console_state kept asserting
    // at_prompt/commandable for that pre-off prompt while three run_commands
    // got zero bytes.
    if let Some(off) = off_verified {
        let mut last = conminer_core::console::to_json(&state);
        if let Some(o) = last.as_object_mut() {
            o.insert("silent_ms".into(), json!(silent.unwrap_or(0)));
            o.insert(
                "decayed_because".into(),
                json!(
                    "a power off was verified after this console's last byte, and nothing \
                       has arrived since; what the console showed before that is not what it \
                       is doing now"
                ),
            );
            o.insert("power_off_verified_at".into(), json!(off));
        }
        return Ok((ConsoleState::NoSignal, Some(last), None));
    }
    if !silent.is_some_and(|ms| ms >= obs.hung_after_ms) {
        return Ok((state, None, None));
    }
    let sensed_off = probe_power_state(ctx, dev).as_deref() == Some("off");
    if !(sensed_off || quiet_epoch) {
        // NO EVIDENCE IS NOT EVIDENCE OF LIFE EITHER. On a controller with no
        // sense line, "off" and "idling at a login gate" are both silence, and
        // conminer cannot tell them apart -- so the claim stands, which is the
        // honest answer. What it must not do is present it as timeless: an
        // agent reading "the board is up and waiting for a login" deserves to
        // see that nobody has heard from it in ninety seconds, and decide.
        let mut v = conminer_core::console::to_json(&state);
        if let (Some(o), Some(ms)) = (v.as_object_mut(), silent) {
            o.insert("silent_ms".into(), json!(ms));
            if ms >= obs.hung_after_ms {
                o.insert(
                    "staleness".into(),
                    json!(concat!(
                        "last observed this long ago; this controller cannot measure power, ",
                        "so a board that is off and a board waiting at a prompt look the same ",
                        "from here",
                    )),
                );
            }
        }
        //
        // The STATE ITSELF is unchanged: a shell quiet for ten minutes is still
        // a shell, and `run_command` drives it fine. Four rounds were spent
        // making that answer correct; this adds the age, it does not replace
        // the answer.
        return Ok((state, None, Some(v)));
    }
    let mut last = conminer_core::console::to_json(&state);
    if let Some(o) = last.as_object_mut() {
        o.insert("silent_ms".into(), json!(silent.unwrap_or(0)));
        o.insert(
            "decayed_because".into(),
            json!(if sensed_off {
                "the controller reports the board off and nothing has arrived since"
            } else {
                "an actuation opened an epoch that produced no bytes, and nothing has arrived \
                 since"
            }),
        );
    }
    Ok((ConsoleState::NoSignal, Some(last), None))
}

/// The wall time of the most recent power action on this console, if it was an
/// `off` conminer VERIFIED and it was recorded at or after the console's last
/// received byte. Both the actuation's own event and a background escalation's
/// final event carry `{action, effect}`, so both count; a later `on`, `reset`
/// or `cycle` supersedes it, and any byte after it means the board is talking
/// whatever the event says.
fn verified_off_after_last_byte(
    st: &conminer_core::store::DeviceStore,
) -> conminer_core::error::Result<Option<i64>> {
    let Some(ev) = st.events(None, Some("power"), 1)?.into_iter().next() else {
        return Ok(None);
    };
    let (_id, at, offset, _kind, data) = ev;
    let is_off = data["action"] == "off";
    let verified = data["effect"]["verified"] == true;
    // Recorded at or after every byte the console has produced.
    let nothing_since = offset >= st.stream_offset();
    Ok((is_off && verified && nothing_since).then_some(at))
}

/// Which kind a matched prompt pattern belongs to, from the same source
/// `console_state` reads (§F7).
fn prompt_kind_of(ctx: &Context, dev: &DeviceRow, pattern: Option<&str>) -> Option<String> {
    let pattern = pattern?;
    // Learned/configured rows first: a device that has been taught outranks the
    // profile guess that found it.
    if let Ok(rows) = ctx.with_store(dev, |st| st.prompts(None)) {
        if let Some(r) = rows.iter().find(|r| r.pattern == pattern) {
            return Some(r.kind.clone());
        }
    }
    ctx.profiles()
        .all()
        .iter()
        .flat_map(|p| p.prompts.iter())
        .find(|p| p.raw == pattern)
        .map(|p| p.kind.as_str().to_string())
}

/// What a tool costs in wall-clock, and what must hold before calling it (§F8).
///
/// Only the tools whose cost is surprising: everything else is a database read
/// in single-digit milliseconds and saying so for sixty tools would be noise.
fn cost_and_precondition(name: &str) -> (Option<&'static str>, Option<&'static str>) {
    match name {
        "run_command" => (
            Some("~7 s minimum: TX is paced and echo-verified per character. Batch with `;`."),
            Some("a commandable prompt (console_state.commandable); a lease on the device"),
        ),
        "power" => (
            Some("hook + up to ~15 s of effect verification; `off` also watches USB for EDL"),
            Some("a lease on the device, or on every console of the target"),
        ),
        "boot_mode" => (
            Some("hook time; straps LATCH -- the mode persists until `clear`"),
            Some("a lease; the mode must be one of this board's configured boot_modes"),
        ),
        "flash" => (
            Some("as long as the lab's flash hook takes; minutes are normal"),
            Some("a lease; an image reference the hook understands"),
        ),
        "pull_file" | "push_file" => (
            Some("console-paced: ~11 KB/s at 115200. Small files only."),
            Some("a logged-in shell prompt; a lease"),
        ),
        "follow" => (
            Some("parks server-side until the predicate fires or timeout_s elapses"),
            None,
        ),
        "rebuild_templates" => (
            Some("re-mints every template id on the device: seconds to minutes"),
            Some("nothing, but every template id cited elsewhere changes"),
        ),
        "diagnose" => (
            Some("opens its own connection and probes: a second or two"),
            None,
        ),
        _ => (None, None),
    }
}

// ---------------------------------------------------------- actuation (F1) --

/// Where an actuation lands: one console, or every console of a target.
///
/// Boards are multi-console but hooks are per-board and epochs are per-device.
/// Actuating one console of a six-console board therefore put the power epoch on
/// a console that never speaks while the boot evidence accrued somewhere else --
/// measured on the NordAU, and the kind of thing every agent hits once. Naming
/// the TARGET makes the epochs land everywhere the evidence might.
struct ActuationScope {
    /// Whose hook runs, and whose store anchors the response.
    primary: DeviceRow,
    /// Every console the action should open an epoch on (never empty).
    consoles: Vec<DeviceRow>,
    /// Every console whose TRAFFIC proves the board responded.
    ///
    /// Wider than `consoles` on purpose, and only for the device form: opening
    /// an epoch on a sibling would strand boot evidence, but REFUSING to look at
    /// that sibling is how a board that booted perfectly gets called dead. On a
    /// four-interface EVK only one port talks; aim at either of the others --
    /// which the dashboard's own panel does, and which any agent may do -- and
    /// verification watched a console that was never going to speak, waited out
    /// the full boot window, then power-cycled a healthy board to "recover" it.
    /// Measured at 74 s per press, with `verified: false` on a board at a prompt.
    watched: Vec<DeviceRow>,
    target: Option<String>,
    /// Members deliberately left out: controllers capture nothing.
    exempt: Vec<String>,
    /// Advisory attached to device-form calls on a board that has siblings.
    note: Option<String>,
    /// Consoles whose lease is missing, and those already held.
    ///
    /// §F5. Only ever populated for a DRY RUN, where a missing lease is a thing
    /// to REPORT rather than a reason to refuse: planning is read-only, and
    /// making it take leases means a "what would this do" call can bump another
    /// agent off a board it is using. A real actuation still refuses.
    lease_missing: Vec<String>,
    lease_held: Vec<String>,
}

impl ActuationScope {
    /// The `lease_check` a dry run reports.
    fn lease_check(&self) -> Value {
        if self.lease_missing.is_empty() {
            json!("ok")
        } else {
            json!({
                "missing": self.lease_missing,
                "held": self.lease_held,
                "why": "this is a dry run, so the missing leases are reported rather than \
                        refused; the same call without dry_run would fail until they are \
                        acquired",
            })
        }
    }
}

/// Every console driven by the same controller as `d`, including `d` itself.
///
/// The controller INSTANCE is the board: two consoles that resolve to the same
/// controller tty are two ports on one piece of hardware, so if either speaks,
/// the board is alive. Falls back to `d` alone when no controller resolves,
/// which is the honest answer for a console nothing drives.
/// How far back a follow with no cursor should look.
///
/// The board does not wait for the caller. Between `power reset` returning and
/// `follow` being called there is a gap -- an RPC hop, an agent's next thought,
/// a relayed call across the fleet -- and everything the board printed in it was
/// invisible, because the default start was the live head. Measured on the
/// bench: a flash banner captured at line 2884 was missed by a follow that
/// started at 2900 and then timed out, and only a historical search found it.
///
/// So the default is THE START OF THIS BOOT, which is what "watch for the
/// banner after a reset" means -- with two bounds that keep it honest:
///
/// * a GRACE before the epoch boundary, because bytes arriving around a reset
///   can be attributed to the epoch that is closing (the boundary is placed
///   when the tool ran, not when the board acted);
/// * a CEILING on the lookback, so following a board that has been up for hours
///   does not match something it said this morning.
const FOLLOW_BOUNDARY_GRACE_BYTES: u64 = 4 * 1024;
const FOLLOW_MAX_LOOKBACK_BYTES: u64 = 256 * 1024;

fn default_follow_start(
    st: &conminer_core::store::DeviceStore,
) -> (conminer_core::store::Cursor, &'static str) {
    let head = st.head_cursor();
    let head_off = head.offset;
    let floor = head_off.saturating_sub(FOLLOW_MAX_LOOKBACK_BYTES);
    match st.latest_boot() {
        Ok(Some(b)) => {
            let want = b.opened_offset.saturating_sub(FOLLOW_BOUNDARY_GRACE_BYTES);
            if want >= floor {
                (st.cursor_at(want), "this boot, plus the boundary grace")
            } else {
                // The epoch is older than the ceiling: this board has been up a
                // long time, so "since the boot started" would be a search of
                // history rather than a follow.
                (
                    st.cursor_at(floor),
                    "the recent tail (this boot is older than the lookback)",
                )
            }
        }
        _ => (head, "the live head (no boot epoch on this device)"),
    }
}

/// WHY THIS CONSOLE LOOKS THE WAY IT DOES, in one sentence, in order.
///
/// Lifted out of `diagnose` so the ORDER can be tested, because the order is the
/// design: several of these are true at once on a real bench, and the one that
/// gets said decides what the reader does next. The case that forced it out:
/// a board in EDL is NOT silent -- its UART re-enumerates away and ser2net
/// serves a device-open failure, which is bytes -- so the wedged-console arm
/// matched and a deliberate flash entry was reported as a capture fault.
#[allow(clippy::too_many_arguments)]
pub(crate) fn console_verdict(
    ignored: bool,
    state: &str,
    // The PERCEIVED console state (at_prompt, streaming, ...), distinct from the
    // device-row `state` above (discovered/listening/gone). Report #19 reopened
    // because diagnose passed the device state here, so the "idle at a prompt"
    // branch -- which keys on the CONSOLE state -- could never match in the real
    // call path even though the isolated unit test passed.
    console_state: &str,
    endpoint: Option<&String>,
    probe: Option<&Value>,
    edl: bool,
    power_state: Option<&str>,
    no_endpoint_reason: &'static str,
) -> &'static str {
    match (endpoint, probe) {
        _ if ignored || state == "gone" => no_endpoint_reason,
        (None, _) => no_endpoint_reason,
        // EDL EXPLAINS A REFUSED CONNECTION TOO, and must be said before the
        // generic "is ser2net listening?".
        //
        // The open-failure case below was fixed first, but a vanished UART does
        // not always leave ser2net serving failure text: when the tty is gone at
        // config time the port is not served at all, so the probe cannot even
        // connect. Same cause, one rung higher, and it sent the reader off to
        // check ser2net while the board was exactly where they had just put it.
        (_, Some(p)) if p["connected"] == false && edl => {
            "the board is in EDL, which re-enumerates its USB and takes the UART with it: there \
             is no tty to serve, so nothing is listening on that port. Expected, not a broken \
             ser2net -- the port returns on its own when the board leaves EDL"
        }
        (_, Some(p)) if p["connected"] == false => {
            "cannot connect to the endpoint: is ser2net listening on that port?"
        }
        // A board in EDL is silent BY DESIGN. Say so, instead of making the
        // caller guess between "off", "idle" and "in the one state where
        // flashing is possible".
        (_, Some(p)) if p["bytes_received"] == 0 && edl => {
            "the board is in EDL (a live 05c6 QDL gadget is answering on USB), where the console \
             is silent by design -- this is not a fault and not 'off'"
        }
        // EDL EXPLAINS AN OPEN FAILURE TOO, and must be said before the wedge.
        //
        // Entering EDL re-enumerates the board's USB: the UART interface goes
        // away, ser2net cannot open a tty that no longer exists, and it serves
        // its failure text to whoever connects. That is expected, needs no
        // restart, and ends by itself when the board leaves EDL. Reported from a
        // live flashing session, where this read as "the console is wedged".
        (_, Some(p)) if p["open_failed"] == true && edl => {
            "the board is in EDL, which re-enumerates its USB: the UART interface is gone and \
             ser2net cannot open it, so it serves a device-open failure. Expected, not a wedged \
             console -- the port returns on its own when the board leaves EDL, and no restart \
             is needed"
        }
        // Check this BEFORE the byte count: the banner is bytes, so a wedged
        // console otherwise reads as "delivering data".
        (_, Some(p)) if p["open_failed"] == true => {
            // TWO CAUSES, ONE SYMPTOM, AND SER2NET CANNOT TELL THEM APART.
            // The tty may be held by something else (a real wedge), or it may
            // not exist at all -- the board is off, or mid-reset, and took its
            // UART with it. Asserting the first sends somebody restarting
            // ser2net over a board that is simply not powered.
            "ser2net could NOT OPEN the serial device and is serving its failure text to clients \
             instead of the board -- so nothing here came from the board. Either the device node \
             is absent (board off, in EDL, or unplugged) or something else holds the tty. Check \
             power first; ser2net never retries a failed open, so a genuine wedge needs a restart"
        }
        // A powered-off board is SILENT ON PURPOSE. Saying so ends the hunt
        // instead of starting one.
        (_, Some(p)) if p["bytes_received"] == 0 && power_state == Some("off") => {
            "the board is POWERED OFF, so this console is silent as expected -- power it on \
             before reading anything into the quiet"
        }
        (_, Some(p)) if p["bytes_received"] == 0 && power_state == Some("on") => {
            "the board is POWERED ON but the console delivered nothing: either it is idle at a \
             prompt, or ser2net is not forwarding this port"
        }
        // A RECOGNISED, COMMANDABLE PROMPT IN THE BUFFER IS EVIDENCE THE BOARD
        // IS UP, and an idle shell is silent BY DEFINITION -- so a probe that
        // gets nothing while the console sits at a commandable prompt is idle,
        // not "maybe off". Reported (#19) as a contradiction: the verdict said
        // "the board may be powered off" in the very response whose console
        // block said at_prompt, kind=rtos_shell, commandable=true, and a leased
        // run_command then drove it. On a controller with no power sense, off
        // and idle-at-prompt are indistinguishable from bytes alone; the prompt
        // already computed is the tie-breaker the verdict was throwing away.
        (_, Some(p))
            if p["bytes_received"] == 0
                && matches!(console_state, "at_prompt" | "at_prompt_with_traffic") =>
        {
            "connected, and the probe window was silent -- which is exactly what an idle shell \
             does: the console is at a recognised, commandable prompt, so the board is up and \
             waiting for a command, not off. run_command will drive it"
        }
        (_, Some(p)) if p["bytes_received"] == 0 => {
            "connected but received NOTHING: the board may be powered off, or ser2net is not \
             forwarding to a second client"
        }
        _ => "endpoint is delivering data",
    }
}

fn board_siblings(ctx: &Context, d: &DeviceRow) -> Vec<DeviceRow> {
    let present = present_with_topology(ctx);
    let cfg = ctx.config();
    let mine = cfg.controller_port_for(
        &d.canonical,
        d.by_path.as_deref(),
        present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
    );
    let Some(mine) = mine else {
        return vec![d.clone()];
    };
    let all = ctx.registry().all_devices().unwrap_or_default();
    let mut out: Vec<DeviceRow> = all
        .into_iter()
        .filter(|c| {
            c.node.is_none()
                && !c.ignored
                && c.ser2net_port.is_some()
                && cfg
                    .controller_port_for(
                        &c.canonical,
                        c.by_path.as_deref(),
                        present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                    )
                    .as_deref()
                    == Some(mine.as_str())
        })
        .collect();
    if !out.iter().any(|c| c.id == d.id) {
        out.push(d.clone());
    }
    out
}

/// Resolve `device` or `target` into the consoles an action must cover.
fn actuation_scope(ctx: &Context, a: &Map<String, Value>) -> Result<ActuationScope> {
    let target = opt_s(a, "target");
    let device = opt_s(a, "device");
    if target.is_some() && device.is_some() {
        return Err(ToolError::invalid_arg(
            "`device` and `target` are mutually exclusive: one names a console, the other names \
             every console of a board",
        ));
    }
    // A dry run is a PLANNING call: it resolves, checks and prints, and touches
    // nothing. Refusing it for a missing lease made the read-only path require
    // taking leases, which can bump another agent's workflow for an action that
    // was never going to happen.
    let planning = flag(a, "dry_run");
    let Some(t) = target else {
        let d = ctx.device_or_only(device)?;
        let mut lease_missing = Vec::new();
        let mut lease_held = Vec::new();
        match ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now()) {
            Ok(()) => lease_held.push(d.display_name().to_string()),
            Err(e) if !planning => return Err(e),
            Err(_) => lease_missing.push(d.display_name().to_string()),
        }
        // Not an error: the device form is correct and still supported. But an
        // agent aiming at one console of a multi-console board is usually about
        // to look for boot evidence on the wrong one.
        let note = d.target.clone().map(|t| {
            format!(
                "this console belongs to target {t:?}; epochs on its sibling consoles were NOT \
                 opened -- actuate by target if you want the boot evidence anchored on all of them"
            )
        });
        let watched = board_siblings(ctx, &d);
        return Ok(ActuationScope {
            consoles: vec![d.clone()],
            watched,
            primary: d,
            target: None,
            exempt: Vec::new(),
            note,
            lease_missing,
            lease_held,
        });
    };

    let (consoles, exempt) = conminer_core::target::console_members(&ctx.registry(), t)?;

    // EVERY missing lease at once. Failing on the first means an operator
    // holding four of five learns about the fifth only after fixing the fourth
    // -- and rounds 3 and 4 both reproduced an error that named none of them.
    let (holder, now) = (ctx.holder(), ctx.now());
    let mut missing = Vec::new();
    let mut held = Vec::new();
    for c in &consoles {
        match ctx.registry().require_lease(c.id, &holder, now) {
            Ok(()) => held.push(c.display_name().to_string()),
            Err(_) => missing.push(c.display_name().to_string()),
        }
    }
    if !missing.is_empty() && !planning {
        return Err(ToolError::new(
            ErrorCode::LeaseRequired,
            format!(
                "actuating target {t:?} needs a lease on each of its {} consoles; {} missing",
                consoles.len(),
                missing.len()
            ),
        )
        // Point at the one call that fixes it. This named `missing[0]`, so an
        // agent following the hint acquired one console, got the same error
        // about the next, and walked the list by hand.
        .with_hint(format!("acquire({{\"target\": {t:?}}})"))
        .with_detail(json!({
            "missing": missing,
            "held": held,
            // Named so nobody goes looking for a lease they do not need.
            "exempt_not_consoles": exempt,
        })));
    }

    // Any console of a board resolves the same hook, but prefer one that has a
    // hook configured so a target with mixed members still actuates.
    let primary = consoles
        .iter()
        .find(|c| {
            ctx.config()
                .power_hook_for_at(
                    c.display_name(),
                    &c.canonical,
                    c.by_path.as_deref(),
                    std::iter::empty(),
                )
                .is_some()
        })
        .cloned()
        .unwrap_or_else(|| consoles[0].clone());

    Ok(ActuationScope {
        primary,
        // The target form already covers the whole board, so the two sets agree.
        watched: consoles.clone(),
        consoles,
        target: Some(t.to_string()),
        exempt,
        note: None,
        lease_missing: missing,
        lease_held: held,
    })
}

/// The USB ports attributed to this board, from config or from the `usb_ports`
/// tag the selftest writes after an EDL entry it drove itself.
pub(crate) fn declared_usb_ports(ctx: &Context, d: &DeviceRow) -> Vec<String> {
    let mut ports = ctx.config().usb_ports_for(&[
        d.canonical.as_str(),
        d.display_name(),
        d.label().unwrap_or_default(),
    ]);
    if let Some(tagged) = d.tags.get("usb_ports") {
        ports.extend(
            tagged
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
        );
    }
    ports.sort();
    ports.dedup();
    ports
}

/// Is any of this board's own USB ports on the bus right now?
///
/// A SYSFS PATH TEST, not a device open. `usb::scan()` opens every device on the
/// host and reads a descriptor with a 300 ms timeout each -- twelve devices on
/// the bravo bench, seconds per call -- which is the right tool for "is this
/// gadget alive" and far too heavy for "did this port go away". Presence is a
/// directory entry.
fn board_ports_present(ports: &[String]) -> bool {
    ports_present_under(std::path::Path::new("/sys/bus/usb/devices"), ports)
}

fn ports_present_under(root: &std::path::Path, ports: &[String]) -> bool {
    ports.iter().any(|p| root.join(p).exists())
}

/// Can an "off" be confirmed at all, and by what?
///
/// Separated so the DECISION can be tested without a board: it is the thing that
/// cost an operator eighty seconds per power-off. Three cases, and only one of
/// them is worth waiting for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OffEvidence {
    /// The board holds USB ports that were on the bus before the press: watch
    /// for them to leave, which is proof and costs a directory lookup.
    WatchUsb,
    /// The console was talking, so it going quiet means something.
    WatchConsole,
    /// Neither: the console was already silent and there is no USB presence to
    /// lose. Waiting cannot produce evidence, so say so now.
    Impossible,
}

pub(crate) fn off_evidence(was_talking: bool, has_ports: bool, port_before: bool) -> OffEvidence {
    if has_ports && port_before {
        OffEvidence::WatchUsb
    } else if was_talking {
        OffEvidence::WatchConsole
    } else {
        OffEvidence::Impossible
    }
}

/// Ask the console to say something, so its silence afterwards will mean
/// something.
///
/// A board with no power sense and no USB presence cannot have an `off`
/// confirmed: the console-silence check needs the console to have been TALKING,
/// and a board idle at a login prompt says nothing on its own. Measured on the
/// Uno-Q: `power off` was truthful but unconfirmable every time.
///
/// So, ON REQUEST ONLY, send a newline first. A live board echoes or reprints
/// its prompt; a board already off says nothing and the answer is unchanged.
/// This TRANSMITS to the board, which is why it is never the default: a newline
/// at the wrong prompt is somebody's keystroke, and conminer does not put
/// characters on a line unasked.
fn poke_console(ctx: &Context, d: &DeviceRow) -> Result<bool> {
    use conminer_core::runner::{BrokeredTransport, Transport};
    use std::time::Duration;
    if !ctx.config().dashboard.allow_tx {
        return Err(ToolError::new(
            ErrorCode::SendDisabled,
            "verify:\"poke\" transmits a newline to the board, and TX is disabled on this server",
        ));
    }
    let endpoint = endpoint_for(ctx, d)?;
    let (broker_sock, broker_dev) = broker_read_path(ctx, d);
    block_on(async move {
        let mut io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
        let mut scratch = [0u8; 512];
        // Answer telnet negotiation before writing, as `send` does.
        let _ = io.read(&mut scratch, Duration::from_millis(300)).await;
        io.write_all(b"\r").await?;
        // A prompt comes back fast or not at all; this is a liveness question,
        // not a command.
        // `read` answers with how many bytes arrived, or None on a quiet
        // socket: both are answers to "is anything alive down there".
        let answered = io
            .read(&mut scratch, Duration::from_millis(1200))
            .await
            .ok()
            .flatten()
            .is_some_and(|n| n > 0);
        Ok(answered)
    })
}

/// Where every console in scope stands RIGHT NOW, by device id.
///
/// Taken before a hook runs, so the epoch it eventually opens can begin where
/// the stream was when the button was pressed rather than where it ended up
/// after the hook returned and the effect was verified -- seconds later, with
/// the board's answer already on the wire.
fn stream_marks(ctx: &Context, scope: &ActuationScope) -> std::collections::HashMap<i64, u64> {
    let mut marks = std::collections::HashMap::new();
    for c in &scope.consoles {
        if let Ok(off) = ctx.with_store(c, |st| Ok(st.head_cursor().offset)) {
            marks.insert(c.id, off);
        }
    }
    marks
}

/// Open one epoch per console for a single action, tied together (§F1).
fn open_actuation_epochs(
    ctx: &Context,
    scope: &ActuationScope,
    opened_by: &str,
    label: Option<&str>,
    event: &str,
    payload: &Value,
    // Where each console's stream stood BEFORE the action ran, by device id.
    // Empty for callers with nothing to actuate (a bare `mark`), which start at
    // the head as they always did.
    marks: &std::collections::HashMap<i64, u64>,
) -> Result<Vec<Value>> {
    let now = ctx.now();
    // One id for one action. Derived from the moment and the board, so it needs
    // no uuid dependency and reads sensibly in a database.
    let group = format!("g{now}-{}", scope.primary.id);
    let multi = scope.consoles.len() > 1;
    let mut opened = Vec::new();
    for c in &scope.consoles {
        let row = ctx.with_store(c, |st| {
            let session = st.latest_session()?.map(|s| s.id);
            // THE EPOCH BEGINS WHERE THE STREAM STOOD WHEN WE ACTED, not where
            // it stands now: the board answers while the hook is still running.
            let boot =
                st.open_boot_at(opened_by, label, now, session, marks.get(&c.id).copied())?;
            if multi {
                st.set_boot_group(boot.id, &group)?;
            }
            // The event goes on EVERY epoch, not only the one whose hook ran: an
            // agent asking the console that stayed silent still learns that
            // somebody powered this board, and when.
            st.append_event(session, Some(boot.id), now, event, payload)?;
            Ok(json!({
                "device": c.display_name(),
                "boot_id": boot.id,
                "boot_seq": boot.seq,
                // The tie itself, in the response that created it. Without this
                // a caller had to re-read boot_report on some other console to
                // learn which epochs belonged to the action it just took.
                "group_id": multi.then(|| group.clone()),
                // THE CURSOR IS THE BOUNDARY, not the head. Handing back "now"
                // is what let a caller follow from a point already past the
                // banner the action produced.
                "cursor": st.cursor_at(boot.opened_offset).encode(),
            }))
        })?;
        opened.push(row);
    }
    Ok(opened)
}

// ------------------------------------------------------------------ tools ----

pub fn registry() -> &'static [Tool] {
    &[
        Tool {
            name: "list_devices",
            description: "Discovered devices with endpoints, line settings, tags, active \
                          profile/stage, and what each console last showed itself to be.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description":
                        "Optional selector or `tag:k=v AND tag:k=v` query to narrow the list."},
                    "detail": {"type": "boolean", "default": false, "description":
                        "Include identity, tags, target, pinned_profile, template count and the \
                         last captured line. Off by default: picking a device needs a name, an \
                         endpoint and whether it is alive."},
                    "power": {"type": "boolean", "default": false, "description":
                        "Ask each board's controller whether it is powered on, adding `power` \
                         (\"on\"/\"off\"/\"unknown\") and `power_source` (the controller that \
                         answered). Costs ~1-2s PER BOARD, which is why it is opt-in; the query \
                         runs once per controller, not once per console. \"unknown\" means the \
                         controller could not answer and is never a synonym for \"off\"."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let devices = match opt_s(a, "filter") {
                    Some(f) => ctx.device_group(f)?,
                    None => ctx.registry().all_devices()?,
                };
                // Terse by default: at ~1.1KB/device the full shape is mostly
                // fields an agent reads once, and this is often the first call
                // of a session.
                let detail = flag(a, "detail");
                let power_map = if flag(a, "power") {
                    power_by_controller(ctx, &devices)
                } else {
                    Default::default()
                };
                // Resolved once for the whole listing: the controller lookup
                // needs to know what is plugged in HERE, and asking per row
                // would re-scan for every device on the bench.
                let present = present_with_topology(ctx);
                // EVERY row the registry holds, not the filtered view being
                // rendered: the controller's row is exactly what a filter drops,
                // and the shared controls builder reads its name.
                let all_rows = ctx.registry().all_devices().unwrap_or_default();
                let mut out = Vec::new();
                for d in &devices {
                    let endpoint = d
                        .ser2net_port
                        .map(|p| format!("tcp://{}:{p}", ctx.config().ser2net.bind));
                    if !detail {
                        // The port names the device; a label rides ALONGSIDE it,
                        // never in place of it. Only emitted when one is set, so
                        // an unlabelled bench pays nothing for the field.
                        let mut row = json!({
                            "device": d.display_name(),
                            "endpoint": endpoint,
                            "line": d.line.summary(),
                            "state": d.state,
                            // PRESENCE ABOVE, CAPTURE HEALTH HERE. They were one field with
                            // two writers; a reader asking "is this console recording" was
                            // answered by whichever process wrote last.
                            "capture_state": d.capture_state,
                            "ignored": d.ignored,
                        });
                        // §P1. WHOSE HARDWARE IS THIS. A fleet-wide listing that
                        // does not say where a board lives invites somebody to
                        // power-cycle the right name on the wrong host, so the
                        // owning node and its address ride on every row -- name
                        // for typing, address for knowing which rack.
                        if let (Some(node), Some(o)) = (d.node.as_deref(), row.as_object_mut()) {
                            o.insert("node".into(), json!(node));
                            o.insert("node_host".into(), json!(d.node_host));
                            o.insert("owned_by_peer".into(), json!(true));
                        }
                        if let (Some(l), Some(o)) = (d.label(), row.as_object_mut()) {
                            o.insert("label".into(), json!(l));
                        }
                        add_power(&mut row, &power_map, &d.canonical);
                        out.push(row);
                        continue;
                    }
                    // §P1. A PEER'S BOARD HAS NO STORE HERE. Its sessions,
                    // templates and last line live on the owner, one federated
                    // call away; opening a local store to answer would create an
                    // empty one, take its writer lock and then answer every
                    // later question about that board with silence.
                    let (sessions, templates, last) = if d.kind.is_remote() {
                        (0, 0, None)
                    } else {
                        ctx.with_store(d, |st| {
                            Ok((
                                st.list_sessions(1)?.len(),
                                st.template_count()?,
                                st.recent_lines(1)?.into_iter().next().map(|l| l.lossy()),
                            ))
                        })?
                    };
                    let mut row = json!({
                        "device": d.display_name(),
                        "canonical": d.canonical,
                        // §P1. The owning node, and where it is. `null` means
                        // this host: the common case pays one null field, and a
                        // remote board can never be mistaken for a local one.
                        "node": d.node,
                        "node_host": d.node_host,
                        "kind": d.kind.as_str(),
                        // Kept as `nickname` here for the tools that set it, and
                        // mirrored as `label` because that is what it IS: a name
                        // an operator hung on a port, not the port's identity.
                        "nickname": d.nickname,
                        "label": d.nickname,
                        // Surfaced so a human knows moving the cable moves the name.
                        "identity": d.identity,
                        "tags": d.tags,
                        "target": d.target,
                        "endpoint": endpoint,
                        "line": d.line.summary(),
                        "pinned_profile": d.pinned_profile,
                        "state": d.state,
                        // PRESENCE ABOVE, CAPTURE HEALTH HERE. They were one field with
                        // two writers; a reader asking "is this console recording" was
                        // answered by whichever process wrote last.
                        "capture_state": d.capture_state,
                        "ignored": d.ignored,
                        "observed": d.observed,
                        "has_sessions": sessions > 0,
                        "templates": templates,
                        "last_line": last,
                        // HOW FAR THE OWNER IS FROM HERE. 0 = this node owns it.
                        //
                        // §P2 gave the registry a `hops` column and made import
                        // prefer a shorter path, then never published the number
                        // -- so the receiver fell back to a constant, every row
                        // arrived claiming the same distance, and "a shorter
                        // path wins" compared 2 < 2 and never fired ONCE. The
                        // effect was not a stale route but silent data loss:
                        // three nodes each relay each other, so every row was
                        // written twice a tick, and the second-hand copy --
                        // carrying whatever the RELAY happened to know, which
                        // for controls was nothing -- overwrote the owner's own
                        // answer. Measured across the fleet: every peer board on
                        // two of three nodes had null controls, no controller,
                        // no power buttons, and no power state, while the owner
                        // published all four correctly every five seconds.
                        "hops": if d.node.is_some() { d.hops } else { 0 },
                        // §P2. HOW THIS BOARD IS DRIVEN, from the only node that
                        // can tell: its owner. A peer cannot work this out for
                        // itself -- the controller profiles match a by-id name
                        // against hardware plugged in here -- so a relayed board
                        // showed no controller and no power buttons on every
                        // node but this one. For a row we hold on somebody
                        // else's behalf, pass on what they told us.
                        // §P2. How this board is driven, from the only node
                        // that can tell: its owner. A peer cannot work this out
                        // for itself, since the controller profiles match a
                        // by-id name against hardware plugged in HERE, so a
                        // relayed board showed no controller and no power
                        // buttons on every node but this one. For a row we hold
                        // on somebody else's behalf, pass on what they told us.
                        //
                        // Only when the sender actually said something. An
                        // absent key is "I did not tell you", not "there are no
                        // controls", and writing NULL for it erases the owner's
                        // own answer. With three nodes relaying each other that
                        // erasure propagates: one node's blank overwrites
                        // another's good value, which the third relays back.
                        //
                        // The object itself is built by the shared builder the
                        // announce path also uses, so the two cannot drift.
                        "controls": match &d.remote_controls {
                            Some(c) => c.clone(),
                            None => conminer_core::peers::inventory::controls_for(
                                ctx.config(),
                                d,
                                &all_rows,
                                &present,
                            ),
                        },
                    });
                    add_power(&mut row, &power_map, &d.canonical);
                    out.push(row);
                }
                Ok(json!({"devices": out, "count": devices.len(), "server_now": ctx.now()}))
            },
        },
        Tool {
            name: "list_sessions",
            description: "Session history for a device, including post-hoc file ingests.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 20, 500);
                let rows = ctx.with_store(&d, |st| st.list_sessions(limit))?;
                fresh(ctx, &d, json!({"sessions": rows, "capped": rows.len() >= limit}))
            },
        },
        Tool {
            name: "list_templates",
            description: "The table of contents: deduplicated templates with counts, severities \
                          and first/last seen. This is what you read instead of the log.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer", "description": "Epoch id; see list_boots."},
                    "stage": {"type": "string", "description": "e.g. kernel, uboot, bl31."},
                    "min_count": {"type": "integer"},
                    "min_severity": {"type": "string",
                        "enum": ["emerg","alert","crit","err","warn","notice","info","debug"],
                        "description": "Only templates at least this severe."},
                    "new_only": {"type": "boolean", "description":
                        "Only templates first seen in the scoped session — the 'what's new?' query."},
                    "vs_baseline": {"type": ["boolean","string"], "description":
                        "Only templates that did NOT fire in the blessed baseline epoch: 'what is \
                         new versus the last boot that worked'. true uses the baseline named \
                         'default'; pass a string to name another."},
                    "verdict": {"type": "array", "items": {"type": "string",
                        "enum": ["benign","known_bad","investigating","interesting"]},
                        "description": "Keep only templates carrying one of these verdicts."},
                    "include_benign": {"type": "boolean", "default": false, "description":
                        "Templates you annotated as benign are hidden by default; the response \
                         always reports how many were hidden."},
                    "view": {"type": "string", "enum": ["compact","full"], "default": "compact",
                        "description":
                        "compact returns id, text, count, severity, stage and any verdict — what \
                         triage actually branches on, at roughly a third of the tokens. full adds \
                         tokens, profile and the first-seen provenance."},
                    "order": {"type": "string", "enum": ["count","first_seen","last_seen","severity"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50},
                    "offset": {"type": "integer", "minimum": 0, "default": 0}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 50, ctx.config().api.max_results.max(50));
                let full = opt_s(a, "view") == Some("full");
                let only_verdicts = match a.get("verdict").and_then(Value::as_array) {
                    Some(list) => list
                        .iter()
                        .filter_map(Value::as_str)
                        .map(Verdict::parse)
                        .collect::<Result<Vec<_>>>()?,
                    None => Vec::new(),
                };
                // An explicit `verdict` filter wins: asking to see benign
                // templates and being handed none would be absurd.
                let hide_verdicts = if flag(a, "include_benign") || !only_verdicts.is_empty() {
                    Vec::new()
                } else {
                    vec![Verdict::Benign]
                };
                let baseline_name = match a.get("vs_baseline") {
                    None | Some(Value::Bool(false)) => None,
                    Some(Value::Bool(true)) => Some("default".to_string()),
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(ToolError::invalid_arg(
                        "vs_baseline must be true or a baseline name",
                    )),
                };

                let mut q = TemplateQuery {
                    session_id: opt_i(a, "session"),
                    boot_id: opt_i(a, "boot"),
                    stage: opt_s(a, "stage").map(str::to_string),
                    min_count: opt_i(a, "min_count"),
                    min_severity: severity_from(a, "min_severity"),
                    new_only: flag(a, "new_only"),
                    not_in_boot: None,
                    only_verdicts,
                    hide_verdicts,
                    order: order_from(a),
                    limit: limit + 1,
                    offset: opt_i(a, "offset").unwrap_or(0) as usize,
                };

                let (mut rows, total, matching, hidden, baseline) =
                    ctx.with_store(&d, |st| {
                        let baseline = match &baseline_name {
                            Some(name) => {
                                let b = st.baseline(name)?.ok_or_else(|| {
                                    ToolError::new(
                                        ErrorCode::UnknownBaseline,
                                        format!("no baseline named {name:?} on this device"),
                                    )
                                })?;
                                q.not_in_boot = Some(b.boot_id);
                                Some(b)
                            }
                            None => None,
                        };
                        let rows = st.list_templates(&q)?;
                        let matching = st.count_templates(&q)?;
                        // Same predicate, verdict filter removed: the difference
                        // is exactly what the filter took away.
                        let unfiltered = TemplateQuery {
                            hide_verdicts: Vec::new(),
                            ..q.clone()
                        };
                        let hidden = st.count_templates(&unfiltered)? - matching;
                        Ok((rows, st.template_count()?, matching, hidden, baseline))
                    })?;

                let more = rows.len() > limit;
                rows.truncate(limit);
                let templates: Vec<Value> =
                    rows.iter().map(|t| project_template(t, full)).collect();
                // A truncated table of contents must say what to do about it.
                // Measured on a board that had booted Linux: 3,207 matching
                // templates all-time against 968 in the current epoch, so an
                // agent asking for "everything" was paying for history it did
                // not want AND still not seeing all of it. Naming the levers
                // costs a few dozen bytes and saves a blind `limit=1000`.
                // Say the EFFECTIVE limit, not the requested one.
                //
                // `limit` is clamped to api.max_results, silently: asking for
                // 1000 returned ~101. A caller that then paged by its own
                // requested limit stepped offset by 1000 and skipped ~900 rows
                // per call -- most of the table was never returned by anything.
                // next_offset has always been computed from the effective limit,
                // so the fix is to make callers use it and to stop hiding the
                // clamp.
                let requested = a.get("limit").and_then(Value::as_u64).map(|v| v as usize);
                let clamped = requested.is_some_and(|r| r > limit);
                let narrowing = if more {
                    let mut m = format!(
                        "showing {} of {matching} matching. Narrow rather than raising limit: \
                         `boot` scopes to one epoch (usually far smaller), `new_only` hides what \
                         earlier boots already showed, `min_severity` and `stage` cut by kind. \
                         To page, pass offset=next_offset from this response -- do NOT step by \
                         your own limit.",
                        templates.len()
                    );
                    if clamped {
                        m.push_str(&format!(
                            " NOTE: limit was reduced to {limit} (server maximum); paging by the \
                             limit you asked for would skip rows."
                        ));
                    }
                    Some(m)
                } else {
                    None
                };
                fresh(ctx, &d, json!({
                    "templates": templates,
                    "returned": templates.len(),
                    // The limit actually applied, so a caller can tell its
                    // request was reduced without having to count rows.
                    "effective_limit": limit,
                    "narrowing": narrowing,
                    "matching": matching,
                    "total_templates": total,
                    // Never silently shorter: an agent must be able to tell a
                    // quiet console from a well-triaged one.
                    "hidden_by_verdict": hidden.max(0),
                    "vs_baseline": baseline,
                    "view": if full { "full" } else { "compact" },
                    "capped": more,
                    "next_offset": more.then(|| q.offset + limit),
                }))
            },
        },
        Tool {
            name: "template_detail",
            description: "One template in full: counts, first/last seen, timeline buckets, and N \
                          verbatim example records.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["template_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "template_id": {"type": "integer"},
                    "session": {"type": "integer"},
                    "examples": {"type": "integer", "minimum": 0, "maximum": 20, "default": 3},
                    "buckets": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let id = i(a, "template_id")?;
                let session = opt_i(a, "session");
                let examples = capped(a, "examples", 3, 20);
                let buckets = capped(a, "buckets", 20, 200);
                let payload = ctx.with_store(&d, |st| {
                    let t = st.template(id)?;
                    let timeline = st.template_timeline(id, session, buckets)?;
                    let recs = st.records_for_template(id, session, None, examples, 0)?;
                    let mut examples_out = Vec::new();
                    for r in &recs {
                        examples_out.push(json!({
                            "record_id": r.id,
                            "session_id": r.session_id,
                            "boot_id": r.boot_id,
                            "kind": r.kind,
                            "severity": r.severity,
                            "stage_id": r.stage_id,
                            "profile": r.profile,
                            "truncated": r.truncated,
                            "fields": r.fields,
                            "text": st.record_text(r.id)?,
                            "first_line_id": r.first_line_id,
                        }));
                    }
                    Ok(json!({
                        "template": t,
                        "timeline": timeline.iter()
                            .map(|(ts, n)| json!({"ts": ts, "count": n}))
                            .collect::<Vec<_>>(),
                        "examples": examples_out,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "get_records",
            description: "Verbatim raw records for a template. The drill-down from the table of \
                          contents into actual bytes.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["template_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "template_id": {"type": "integer"},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer"},
                    "n": {"type": "integer", "minimum": 1, "maximum": 200, "default": 5},
                    "offset": {"type": "integer", "minimum": 0, "default": 0}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let id = i(a, "template_id")?;
                let n = capped(a, "n", 5, ctx.config().api.max_raw_lines.max(5));
                let offset = opt_i(a, "offset").unwrap_or(0) as usize;
                let payload = ctx.with_store(&d, |st| {
                    let recs = st.records_for_template(
                        id, opt_i(a, "session"), opt_i(a, "boot"), n + 1, offset,
                    )?;
                    let more = recs.len() > n;
                    let mut out = Vec::new();
                    for r in recs.iter().take(n) {
                        out.push(json!({
                            "record_id": r.id,
                            "session_id": r.session_id,
                            "boot_id": r.boot_id,
                            "kind": r.kind,
                            "severity": r.severity,
                            "truncated": r.truncated,
                            "fields": r.fields,
                            "first_line_id": r.first_line_id,
                            "last_line_id": r.last_line_id,
                            "text": st.record_text(r.id)?,
                        }));
                    }
                    Ok(json!({
                        "records": out,
                        "capped": more,
                        "next_offset": more.then_some(offset + n),
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "get_context",
            description: "±N verbatim raw lines around any line id — the anchor every search hit \
                          and record carries.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["line_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "line_id": {"type": "integer"},
                    "before": {"type": "integer", "minimum": 0, "maximum": 500, "default": 10},
                    "after": {"type": "integer", "minimum": 0, "maximum": 500, "default": 10}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let line_id = i(a, "line_id")?;
                let max = ctx.config().api.max_raw_lines;
                let before = capped(a, "before", 10, max);
                let after = capped(a, "after", 10, max);
                let payload = ctx.with_store(&d, |st| {
                    // §F9. Asked BEFORE the lookup, because a pruned line is
                    // simply missing and `context` would answer UNKNOWN_LINE --
                    // which reads as "you made that up" rather than "those bytes
                    // were reclaimed, and everything derived from them is still
                    // queryable". Different answer, different next move.
                    let horizon = st.pruned_before_offset();
                    if horizon > 0 && st.line(line_id).is_err() {
                        return Err(ToolError::new(
                            ErrorCode::Pruned,
                            format!(
                                "line {line_id} is below this device's retention horizon \
                                 (raw pruned before offset {horizon})"
                            ),
                        )
                        .with_hint(
                            "templates, epochs, stages and metrics for that range are still \
                             here -- ask list_templates or boot_report instead",
                        ));
                    }
                    let lines = st.context(line_id, before, after)?;
                    Ok(json!({
                        "lines": lines.iter().map(|l| json!({
                            "line_id": l.id,
                            "anchor": l.id == line_id,
                            "offset": l.stream_offset,
                            "ts_wall": l.ts_wall,
                            "boot_id": l.boot_id,
                            "text": l.lossy(),
                            "truncated": l.truncated,
                            "continuation": l.continuation,
                        })).collect::<Vec<_>>(),
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "search",
            description: "Indexed search over raw lines and framed records. `terms` = all words \
                          present; `phrase` = exact contiguous; `regex` = full regex. Scope \
                          `record` searches a whole crash dump as one unit; `window` finds spans \
                          crossing record boundaries.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string"},
                    "mode": {"type": "string", "enum": ["terms","phrase","regex"], "default": "terms"},
                    "scope": {"type": "string", "enum": ["line","record","window"], "default": "line"},
                    "window": {"type": "integer", "minimum": 2, "maximum": 200, "default": 20,
                        "description": "Lines per sliding window; only used with scope=window."},
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "devices": {"description":
                        "§K3. Search SEVERAL stores in one call: \"all\", a list of selectors, or \
                         a `tag:k=v` query. Mutually exclusive with `device`. Every hit gains a \
                         `device` field and the response gains `by_device`. Excluded ports, \
                         mined `file:` stores and derived `#` sub-devices are left out unless \
                         `include_derived` is set."},
                    "include_derived": {"type": "boolean", "default": false, "description":
                        "Include `file:` pseudo-devices and `<console>#dmesg` sub-devices in a \
                         `devices` search."},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50},
                    "cursor": {"type": "string", "description":
                        "Opaque resume point returned as `next_cursor` by a capped response."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let across = resolve_device_set(ctx, a)?;
                let d = match &across {
                    // The response anchors on the first store either way, but a
                    // cross-device search must not fail merely because no single
                    // `device` was named.
                    Some(set) => set.local[0].clone(),
                    None => device(ctx, a)?,
                };
                let mode = match opt_s(a, "mode") {
                    Some("phrase") => SearchMode::Phrase,
                    Some("regex") => SearchMode::Regex,
                    Some("terms") | None => SearchMode::Terms,
                    Some(other) => {
                        return Err(ToolError::invalid_arg(format!("unknown mode {other:?}")))
                    }
                };
                let win = capped(a, "window", ctx.config().search.window_default_lines,
                                 ctx.config().search.window_max_lines);
                let scope = match opt_s(a, "scope") {
                    Some("record") => SearchScope::record(),
                    Some("window") => SearchScope::window(win),
                    Some("line") | None => SearchScope::line(),
                    Some(other) => {
                        return Err(ToolError::invalid_arg(format!("unknown scope {other:?}")))
                    }
                };
                let max = capped(a, "max_results", 50, ctx.config().api.max_results.max(50));
                let after = match opt_s(a, "cursor") {
                    Some(c) => Some(ctx.with_store(&d, |st| {
                        st.resolve_cursor(&conminer_core::store::Cursor::decode(c)?)
                    })?),
                    None => None,
                };
                let q = SearchQuery {
                    query: s(a, "query")?.to_string(),
                    mode,
                    scope,
                    session_id: opt_i(a, "session"),
                    boot_id: opt_i(a, "boot"),
                    max_results: max,
                    after_offset: after,
                };
                if let Some(set) = &across {
                    let payload =
                        search_across(ctx, &set.local, &set.remote, &q, max, opt_s(a, "cursor"))?;
                    return fresh(ctx, &d, payload);
                }
                let (r, next) = ctx.with_store(&d, |st| {
                    let r = conminer_core::search::search(st, &q)?;
                    let next = r.next_offset.map(|o| st.cursor_at(o).encode());
                    Ok((r, next))
                })?;
                fresh(ctx, &d, json!({
                    "hits": r.hits,
                    "tier": r.tier,
                    "scan": r.scan,
                    "rows_scanned": r.rows_scanned,
                    "bytes_scanned": r.bytes_scanned,
                    "capped": r.capped,
                    "next_cursor": next,
                }))
            },
        },
        Tool {
            name: "search_raw",
            description: "Regex over raw lines. Thin alias for search(mode=regex, scope=line), \
                          kept for parity with uart-mcp's query_serial_logs.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string"},
                    "device": {"type": "string", "description": DEVICE_ARG},
                    // §K3: forwarded to `search`, so the cross-device form works
                    // here identically rather than being a second dialect.
                    "devices": {"description":
                        "\"all\", a list of selectors, or a `tag:k=v` query. Mutually exclusive \
                         with `device`; every hit gains a `device` field."},
                    "include_derived": {"type": "boolean", "default": false},
                    "session": {"type": "integer"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let mut fwd = a.clone();
                let pattern = s(a, "pattern")?.to_string();
                fwd.insert("query".into(), json!(pattern));
                fwd.insert("mode".into(), json!("regex"));
                fwd.insert("scope".into(), json!("line"));
                fwd.remove("pattern");
                (registry().iter().find(|t| t.name == "search").expect("search exists").call)(ctx, &fwd)
            },
        },
        Tool {
            name: "get_recent",
            description: "Tail of a device's stream, verbatim. Parity with uart-mcp's \
                          get_recent_logs.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "lines": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50},
                    "format": {"type": "string", "enum": ["text", "records"], "default": "text",
                        "description": "text: one string, newline separated (default, ~5x smaller). \
                                        records: per-line objects with line_id/offset/ts_wall."},
                    "include_blank": {"type": "boolean", "default": false,
                        "description": "Keep blank lines. Off by default: a blank line costs a whole \
                                        object to say nothing."},
                    "suppress_noise": {"type": "boolean", "default": false,
                        "description": "Collapse lines whose template an operator has muted \
                                        (annotate_template verdict:\"benign\") into a count, so a \
                                        message repeating four times a second cannot fill the \
                                        response. The count is REPORTED, never silently dropped."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let n = capped(a, "lines", 50, ctx.config().api.max_raw_lines);
                // Default to plain text. Measured on the IQ10: 20 lines came back
                // as 8420 bytes of JSON wrapping roughly 1600 bytes of console
                // text, because every line shipped line_id/offset/ts_wall around
                // ~80 bytes of content. Reading a console is the hottest path an
                // agent has, and it almost never needs the per-line ids.
                let records = opt_s(a, "format") == Some("records");
                let keep_blank = flag(a, "include_blank");
                let suppress = flag(a, "suppress_noise");
                let payload = ctx.with_store(&d, |st| {
                    let all = st.recent_lines(n)?;
                    let capped_out = all.len() >= n;
                    // §F11. A muted template is one an operator has already
                    // judged benign. Dropping its lines here is what stops a
                    // crash-looping board from spending the whole response on
                    // one repeated message -- and the count is returned, so a
                    // suppressed flood can never be mistaken for a quiet board.
                    let muted: Vec<i64> = if suppress {
                        st.muted_templates()?
                    } else {
                        Vec::new()
                    };
                    let mut suppressed = 0usize;
                    let noisy: std::collections::HashSet<i64> = muted.iter().copied().collect();
                    let kept: Vec<_> = all
                        .iter()
                        .filter(|l| keep_blank || !l.lossy().trim().is_empty())
                        .filter(|l| {
                            if noisy.is_empty() {
                                return true;
                            }
                            match st.template_of_line(l.id) {
                                Ok(Some(t)) if noisy.contains(&t) => {
                                    suppressed += 1;
                                    false
                                }
                                _ => true,
                            }
                        })
                        .collect();
                    if records {
                        Ok(json!({
                            "lines": kept.iter().map(|l| json!({
                                "line_id": l.id, "offset": l.stream_offset,
                                "ts_wall": l.ts_wall, "text": l.lossy(),
                            })).collect::<Vec<_>>(),
                            "capped": capped_out,
                            "suppressed_noise_lines": suppressed,
                        }))
                    } else {
                        let text = kept
                            .iter()
                            .map(|l| l.lossy())
                            .collect::<Vec<_>>()
                            .join("\n");
                        // `count`, not `lines`: `lines` is the per-line ARRAY in records
                        // mode, and one name for two shapes is how an agent ends up
                        // indexing an integer.
                        Ok(json!({
                            "text": text,
                            "count": kept.len(),
                            "capped": capped_out,
                            "suppressed_noise_lines": suppressed,
                        }))
                    }
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "boot_stages",
            description: "Stage timeline for a session or epoch, with the banner line that opened \
                          each stage. Answers 'did it die in BL31 or after handoff?'.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = ctx.with_store(&d, |st| {
                    let stages = st.stages(opt_i(a, "session"), opt_i(a, "boot"))?;
                    Ok(json!({"stages": stages, "count": stages.len()}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "list_boots",
            description: "Boot-epoch history with outcomes and semantic fingerprints. Equal \
                          fingerprints mean the board behaved identically — 'same crash again' is \
                          a hash comparison, not a log read.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 20},
                    "view": {"type": "string", "enum": ["compact","full"], "default": "compact",
                        "description":
                        "compact returns id, seq, fingerprint, outcome and bytes — enough to spot a \
                         loop; full adds session, label, offsets and image binding."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 20, 1000);
                let full = opt_s(a, "view") == Some("full");
                let payload = ctx.with_store(&d, |st| {
                    let boots = st.list_boots(limit)?;
                    // How far back the current fingerprint has been stable — the
                    // "looping since epoch N" signal.
                    let stable_since = boots.first().and_then(|b| b.fingerprint.clone()).map(|fp| {
                        boots.iter()
                            .take_while(|b| b.fingerprint.as_deref() == Some(fp.as_str()))
                            .last()
                            .map(|b| b.seq)
                            .unwrap_or(0)
                    });
                    // A boot-looping board is exactly where an agent reads the
                    // most epochs, so this is the list that most needs to be
                    // cheap: the whole question is "are these the same?", which
                    // five fields answer.
                    let rows: Vec<Value> = boots.iter().map(|b| if full {
                        serde_json::to_value(b).unwrap_or(Value::Null)
                    } else {
                        json!({
                            "id": b.id, "seq": b.seq, "bytes": b.bytes,
                            "fingerprint": b.fingerprint, "outcome": b.outcome,
                            "opened_by": b.opened_by, "opened_at": b.opened_at,
                        })
                    }).collect();
                    Ok(json!({
                        "boots": rows,
                        "capped": boots.len() >= limit,
                        "fingerprint_stable_since": stable_since,
                        "view": if full { "full" } else { "compact" },
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "boot_report",
            description: "The one-call answer to 'what happened?': classifies an epoch as booted, \
                          booting (a stage entered and output still arriving), crashed, hung, \
                          looping, garbage, no_output, unknown_capture, or in_progress (the epoch \
                          is open and talking but has not reached a stage yet), with the stage \
                          timeline and the templates novel to that boot. Two different \
                          fingerprints are reported and they answer different questions: \
                          `fingerprint` is the SHAPE of the epoch (its template sequence, sealed \
                          when the epoch closes) and `build_fingerprints`/`versions` are what the \
                          BOARD said it was running.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boot": {"type": "integer", "description": "Epoch id; defaults to the latest."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = crate::report::boot_report(ctx, &d, opt_i(a, "boot"))?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "diff_sessions",
            description: "Templates new in B, gone from B, and count-shifted between two sessions \
                          of one device. The regression question, answered directly.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["a", "b"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "a": {"type": "integer"},
                    "b": {"type": "integer"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 100, 1000);
                let payload = crate::report::diff_sessions(ctx, &d, i(a, "a")?, i(a, "b")?, limit)?;
                fresh(ctx, &d, cap_lists(payload, limit))
            },
        },
        Tool {
            name: "stats",
            description: "Line/record/template counts, compression ratio, fragmentation health \
                          metric, and index size.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let r = ctx.config().retention.clone();
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    // §F9. Retention alongside the counts: how much raw is
                    // actually here, how far back it goes, and where the horizon
                    // sits -- so "the db is enormous" is a number, not a
                    // discovery made by a full disk.
                    //
                    // AND WHETHER ANYTHING WILL ACTUALLY DO IT. Reporting only
                    // `pruned_before_offset` left the important question
                    // unanswered: a bench can sit at offset 0 forever either
                    // because nothing is old enough yet, or because no policy is
                    // configured and nothing ever will be. Those look identical
                    // and lead to opposite actions, and the ADP reached 222k
                    // lines / 120 MB while that ambiguity stood.
                    let age_rule = r.raw_keep_days > 0;
                    let size_rule = r.raw_keep_bytes > 0;
                    let cutoff_ts =
                        age_rule.then(|| now - (r.raw_keep_days as i64) * 86_400_000);
                    let oldest = st.oldest_raw_ts()?;
                    let raw_bytes = st.raw_bytes()?;
                    Ok(json!({
                        "stats": st.stats(opt_i(a, "session"))?,
                        "retention": {
                            "raw_bytes": raw_bytes,
                            "oldest_raw_ts": oldest,
                            "pruned_before_offset": st.pruned_before_offset(),
                            "cutoff_ts": cutoff_ts,
                            // What a prune right now would be entitled to drop.
                            "eligible_now": match (cutoff_ts, oldest) {
                                (Some(c), Some(o)) => o < c,
                                _ => size_rule && raw_bytes > r.raw_keep_bytes,
                            },
                            // When the oldest byte here becomes prunable.
                            "next_eligible_ts": match (age_rule, oldest) {
                                (true, Some(o)) => {
                                    Some(o + (r.raw_keep_days as i64) * 86_400_000)
                                }
                                _ => None,
                            },
                            "policy": {
                                "raw_keep_days": r.raw_keep_days,
                                "raw_keep_bytes": r.raw_keep_bytes,
                                "keep_epochs": r.keep_epochs,
                                "protect_baselines": r.protect_baselines,
                            },
                            // NOTHING RUNS THIS ON A TIMER. Said plainly rather
                            // than implied by an absence, because "a policy is
                            // configured" and "the bytes will actually go" are
                            // different claims.
                            "armed": age_rule || size_rule,
                            "enforced_by": if age_rule || size_rule {
                                "a policy is configured, but pruning happens only when prune() is \
                                 called -- there is no background sweep"
                            } else {
                                "no retention policy is configured: raw bytes grow without bound \
                                 until prune() is called by hand (set retention.raw_keep_days or \
                                 retention.raw_keep_bytes to define one)"
                            },
                        },
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "ingest_file",
            description: "Mine a log file into a session queryable by every other tool. 10 KB to \
                          800 MB, gzip auto-detected, conminer export archives recognised.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "description":
                        "Path as visible inside the minerd container."},
                    "device": {"type": "string", "description":
                        "Device to attach the session to. Defaults to a synthetic device keyed on \
                         the file path, so re-ingesting the same artifact is diffable."},
                    "profile": {"type": "string", "description":
                        "Pin a framer profile instead of auto-detecting."},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let path = shared_path(s(a, "path")?);
                let dev = match opt_s(a, "device") {
                    Some(sel) => ctx.device(sel)?,
                    None => {
                        let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                        let canonical = format!("file:{}", abs.display());
                        let now = ctx.now();
                        let mut reg = ctx.registry();
                        match reg.device_by_canonical(&canonical)? {
                            Some(d) => d,
                            None => reg.upsert_device(
                                &canonical, None,
                                conminer_core::store::IdentityKind::ById, None, now,
                            )?,
                        }
                    }
                };
                // The pipeline takes the device's writer lock for its lifetime,
                // so this cannot race minerd or another ingest (§3).
                ctx.forget_store(dev.id);
                let store = conminer_core::store::DeviceStore::open(
                    &ctx.data_dir().join(&dev.db_file),
                    &dev.canonical,
                    ctx.config().fts_for(dev.display_name()),
                )?;
                // The one place the big page cache earns its keep: a bulk ingest
                // touches several large indexes at once, and the small
                // steady-state cache thrashes on every insert. It is asked for
                // HERE, on this connection, for the length of this ingest --
                // rather than being every store's permanent floor.
                store.use_bulk_cache()?;
                let mut pipe = conminer_core::pipeline::Pipeline::new(
                    store,
                    ctx.profiles().clone(),
                    ctx.config().clone(),
                    dev.display_name(),
                    opt_s(a, "profile").or(dev.pinned_profile.as_deref()),
                    ctx.clock().clone(),
                )?;
                let mut opts = conminer_core::ingest::IngestOptions::from_config(ctx.config());
                opts.label = opt_s(a, "label").map(str::to_string);
                let report = conminer_core::ingest::ingest_file(&mut pipe, &path, &opts)?;
                drop(pipe);
                fresh(ctx, &dev, json!({"ingest": report}))
            },
        },
        Tool {
            name: "report_issue",
            description: "File a bug report about conminer itself, with the evidence attached \
                          automatically. SEARCH FIRST with list_reports and confirm_report an \
                          existing one rather than filing a near-duplicate. A report is a claim \
                          for a human to triage; it never changes what any tool answers.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string", "description": "One line naming what went wrong."},
                    "expected": {"type": "string", "description":
                        "What you expected. This pair is what turns a report into a test, so be \
                         concrete: the value, the state, the verdict you looked for."},
                    "observed": {"type": "string", "description": "What you got instead."},
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "tool": {"type": "string", "description":
                        "The tool whose answer was wrong, if it was one."},
                    "args": {"type": "object", "description":
                        "The arguments you called it with, so the call can be repeated."},
                    "reporter": {"type": "string", "description":
                        "Who is filing: an agent name or session id. Distinct reporters are what \
                         drive priority -- ten sightings from one retry loop is not ten agents."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::reports as rep;
                let now = ctx.now();
                // EVIDENCE THE AGENT DOES NOT HAVE TO ASSEMBLE. Where it is,
                // which epoch, which bytes, and which code saw it -- all known
                // here, all previously reconstructed by hand from prose.
                let (device, boot_id, cursor) = match opt_s(a, "device") {
                    Some(sel) => match ctx.device_or_only(Some(sel)) {
                        Ok(d) => {
                            let anchor = ctx
                                .with_store(&d, |st| {
                                    Ok((st.latest_boot()?.map(|b| b.id), st.head_cursor().encode()))
                                })
                                .unwrap_or((None, String::new()));
                            (Some(d.display_name().to_string()), anchor.0, Some(anchor.1))
                        }
                        // An unresolvable selector must not lose the report: the
                        // text is still worth having, and the selector itself may
                        // BE the bug.
                        Err(_) => (Some(sel.to_string()), None, None),
                    },
                    None => (None, None, None),
                };
                let r = rep::NewReport {
                    title: opt_s(a, "title").unwrap_or_default().to_string(),
                    expected: opt_s(a, "expected").map(str::to_string),
                    observed: opt_s(a, "observed").map(str::to_string),
                    device,
                    boot_id,
                    cursor,
                    tool: opt_s(a, "tool").map(str::to_string),
                    args_json: a.get("args").map(|v| v.to_string()),
                    build: Some(conminer_core::build_id().to_string()),
                    node: Some(ctx.node_name()),
                    reporter: opt_s(a, "reporter").map(str::to_string),
                };
                let mut reg = ctx.registry();
                let (report, filed) = rep::file(&mut reg, &r, now)?;
                let reporters = rep::distinct_reporters(&reg, report.id)?;
                Ok(json!({
                    "report": report,
                    // Told, not inferred: an agent that filed a duplicate should
                    // know it added weight rather than noise, and one that filed
                    // a REGRESSION should know the fix it was promised broke.
                    "filed": filed.as_str(),
                    "distinct_reporters": reporters,
                    "note": match filed {
                        rep::Filed::New => "filed",
                        rep::Filed::Duplicate =>
                            "this matches an existing report; counted against it rather than \
                             filed twice",
                        rep::Filed::Regression =>
                            "THIS WAS FIXED IN THE BUILD YOU ARE RUNNING. Reopened as a \
                             regression, which is worth saying out loud to whoever fixed it",
                        rep::Filed::Reopened =>
                            "this had been closed without a fix; reopened, because a decision \
                             that keeps costing agents time deserves revisiting",
                    }
                }))
            },
        },
        Tool {
            name: "list_reports",
            description: "What has already been reported. CALL THIS BEFORE report_issue: if your \
                          problem is here, confirm_report it instead of filing another copy.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string",
                        "enum": ["open","fixed","not_a_bug","wont_fix","duplicate","all"],
                        "description": "Defaults to open: the triage queue."},
                    "query": {"type": "string", "description":
                        "Words to match in the title, expected or observed text. Matched on \
                         normalised text, so punctuation and epoch numbers do not matter."},
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20},
                    "detail": {"type": "boolean", "default": false, "description":
                        "Include who hit each one and on which build."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::reports as rep;
                let status = match opt_s(a, "status") {
                    Some("all") => None,
                    Some(s) => Some(s.to_string()),
                    None => Some("open".to_string()),
                };
                let limit = a
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                // RESOLVE THE SELECTOR FIRST. A report stores the device it was
                // filed against as the resolved name; an agent asks with
                // whatever it types -- a nickname, a substring, a tag query. The
                // filter compared those two strings directly, so
                // `list_reports {device: "uno-q"}` answered `count: 0` for a
                // board with three reports on it, which reads exactly like the
                // reports having been lost. Two agents filed that as a data-loss
                // bug within minutes of each other, and they were right to.
                let device = opt_s(a, "device").map(|sel| {
                    ctx.device_or_only(Some(sel))
                        .map(|d| d.display_name().to_string())
                        .unwrap_or_else(|_| sel.to_string())
                });
                let reg = ctx.registry();
                let rows = rep::list(
                    &reg,
                    status.as_deref(),
                    opt_s(a, "query"),
                    device.as_deref(),
                    limit,
                )?;
                let detail = a.get("detail").and_then(Value::as_bool).unwrap_or(false);
                let mut out = Vec::new();
                for r in rows {
                    let mut v = serde_json::to_value(&r).unwrap_or_else(|_| json!({}));
                    if let Some(o) = v.as_object_mut() {
                        o.insert(
                            "distinct_reporters".into(),
                            json!(rep::distinct_reporters(&reg, r.id)?),
                        );
                        if detail {
                            o.insert("seen_by".into(), json!(rep::sightings(&reg, r.id, 20)?));
                        }
                    }
                    out.push(v);
                }
                Ok(json!({"reports": out, "count": out.len()}))
            },
        },
        Tool {
            name: "confirm_report",
            description: "\"I have this problem too.\" Adds your sighting to an existing report \
                          instead of filing a duplicate, with your own build and evidence. If \
                          the report was already fixed in the build you are running, this \
                          reopens it as a regression.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "integer"},
                    "note": {"type": "string", "description":
                        "Anything that differs from the original: your symptom, your board."},
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "reporter": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::reports as rep;
                let now = ctx.now();
                let id = a
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ToolError::invalid_arg("id must be a report id"))?;
                let (device, boot_id, cursor) = match opt_s(a, "device") {
                    Some(sel) => match ctx.device_or_only(Some(sel)) {
                        Ok(d) => {
                            let anchor = ctx
                                .with_store(&d, |st| {
                                    Ok((st.latest_boot()?.map(|b| b.id), st.head_cursor().encode()))
                                })
                                .unwrap_or((None, String::new()));
                            (Some(d.display_name().to_string()), anchor.0, Some(anchor.1))
                        }
                        Err(_) => (Some(sel.to_string()), None, None),
                    },
                    None => (None, None, None),
                };
                let s = rep::Sighting {
                    reporter: opt_s(a, "reporter").map(str::to_string),
                    node: Some(ctx.node_name()),
                    build: Some(conminer_core::build_id().to_string()),
                    device,
                    boot_id,
                    cursor,
                    note: opt_s(a, "note").map(str::to_string),
                    at: now,
                };
                let mut reg = ctx.registry();
                let (report, filed) = rep::confirm(&mut reg, id, &s)?;
                let reporters = rep::distinct_reporters(&reg, report.id)?;
                Ok(json!({
                    "report": report,
                    "filed": filed.as_str(),
                    "distinct_reporters": reporters
                }))
            },
        },
        Tool {
            name: "resolve_report",
            description: "Close a report with the evidence that makes the closure checkable. \
                          Resolving as fixed REQUIRES the build containing the fix: without it a \
                          later sighting cannot be told from a regression.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["id", "status"],
                "properties": {
                    "id": {"type": "integer"},
                    "status": {"type": "string",
                        "enum": ["fixed","not_a_bug","wont_fix","duplicate"]},
                    "build": {"type": "string", "description":
                        "The build fingerprint containing the fix. Required for `fixed`."},
                    "gate": {"type": "string", "description":
                        "The test that holds the fix, so the closure is checkable by name."},
                    "note": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::reports as rep;
                let now = ctx.now();
                let id = a
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ToolError::invalid_arg("id must be a report id"))?;
                let status = opt_s(a, "status")
                    .ok_or_else(|| ToolError::invalid_arg("status is required"))?;
                let mut reg = ctx.registry();
                let report = rep::resolve(
                    &mut reg,
                    id,
                    status,
                    opt_s(a, "build"),
                    opt_s(a, "gate"),
                    opt_s(a, "note"),
                    now,
                )?;
                Ok(json!({"report": report}))
            },
        },
        Tool {
            name: "peers",
            description: "The fleet: which conminer nodes this one can see, how fresh that \
                          belief is, and where each one lives. A device whose id starts \
                          `peer:<node>/` is owned by the named node -- every call for it is \
                          proxied there, and its console re-exports on a local port here.",
            mutating: false,
            schema: || json!({"type": "object", "properties": {}, "additionalProperties": false}),
            call: |ctx, _a| {
                use conminer_core::peers::registry as pr;
                let now = ctx.now();
                let ttl = ctx.config().peers.ttl_s;
                let reg = ctx.registry();
                let rows = pr::all(&reg)?;
                let devices = reg.remote_devices().unwrap_or_default();
                drop(reg);
                let list: Vec<Value> = rows
                    .iter()
                    .map(|p| {
                        let owned = devices.iter().filter(|d| d.node.as_deref() == Some(&p.name));
                        json!({
                            "node": p.name,
                            "host": p.host,
                            "instance_id": p.instance_id,
                            "mcp_url": p.mcp_url,
                            "dash_url": p.dash_url,
                            "source": p.source.as_str(),
                            // WHICH BUILD that node is running, and whether it
                            // matches this one. A fleet proxies tool calls
                            // between nodes, so a behaviour difference between
                            // builds arrives looking like a misbehaving board.
                            // Empty until the peer has advertised itself.
                            "build": p.version,
                            "build_matches": match p.version.as_deref() {
                                None | Some("") => Value::Null,
                                Some(v) => json!(v == conminer_core::build_id()),
                            },
                            // Live is a BELIEF with an age, so both ship: an
                            // operator deciding whether to trust a stale rack
                            // needs the number, not just the colour.
                            "live": p.is_live(now, ttl),
                            "age_s": p.age_ms(now) / 1000,
                            "ttl_s": ttl,
                            // Advertising and answering are different facts: a
                            // node can beacon happily with a crashed mcpd.
                            "answering": p.ok,
                            "last_error": p.last_error,
                            "advert_count": p.advert_count,
                            "devices": owned.count(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "node": ctx.node_name(),
                    // §P1. THIS NODE'S OWN ID, because a statically configured
                    // peer has no other way to learn it. Without it the far side
                    // must invent a placeholder, and the same node then exists
                    // twice in its table -- once under the invented id and once
                    // under the real one heard on the beacon. Measured on the
                    // first two-host bring-up, where "alpha" appeared as two
                    // peers whose syncs then fought over the same rows.
                    "instance_id": conminer_core::peers::Identity::load_or_create(
                        &ctx.config().paths.data_dir,
                        "",
                        now,
                    )
                    .map(|i| i.instance_id)
                    .unwrap_or_default(),
                    // THIS node's build, so the answer is self-contained: a
                    // caller comparing fleet versions should not have to ask
                    // each node separately and hope it asked the same question.
                    "build": conminer_core::build_id(),
                    "peers": list,
                    // §P3. The reverse channel, as it actually stands: who is
                    // asking US for work, and what is parked waiting for them.
                    // Without this, a call that relays and a call that dials
                    // look identical right up until one of them stops working.
                    "relay": ctx.relay().stats(ctx.now()),
                    "count": rows.len(),
                    // The one line an operator actually needs. Every node in a
                    // fleet runs the same build or the fleet is not one fleet:
                    // calls proxied to a node that behaves differently look like
                    // hardware misbehaving, not like a deployment problem.
                    // ONE NAME, ONE NODE. Device rows key on the node NAME and
                    // the router turns a name into an address, so two hosts
                    // answering to one name is a wrong-board actuation waiting
                    // to happen. Reported here because the failure is otherwise
                    // invisible: every table just looks like it has duplicates.
                    "name_collisions": pr::name_collisions_of(&rows)
                        .into_iter()
                        .map(|(name, ids)| json!({"name": name, "instances": ids}))
                        .collect::<Vec<_>>(),
                    "fleet_build": {
                        "in_sync": rows
                            .iter()
                            .filter(|p| !p.version.as_deref().unwrap_or_default().is_empty())
                            .all(|p| p.version.as_deref() == Some(conminer_core::build_id())),
                        "unknown": rows
                            .iter()
                            .filter(|p| p.version.as_deref().unwrap_or_default().is_empty())
                            .map(|p| p.name.clone())
                            .collect::<Vec<_>>(),
                    },
                }))
            },
        },
        Tool {
            name: "peer_announce",
            description: "A peer telling this node what it is and what hardware it owns. \
                          §P3. Machine-to-machine, and the only way a node behind one-way \
                          connectivity ever learns the fleet: inventory is otherwise a PULL, \
                          so a node that can reach nobody sees nobody, however many peers can \
                          reach IT. The caller is the side that could open the connection.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["node", "instance_id"],
                "properties": {
                    "node": {"type": "string", "description": "the announcing node's name"},
                    "instance_id": {"type": "string"},
                    "build": {"type": "string"},
                    "host": {"type": "string"},
                    "mcp_url": {"type": "string"},
                    "dash_url": {"type": "string"},
                    "ser2net_host": {"type": "string"},
                    "devices": {"type": "array", "items": {"type": "object"},
                        "description": "rows in `list_devices {detail:true}` shape"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::peers::registry as pr;
                let node = s(a, "node")?.to_string();
                let instance_id = s(a, "instance_id")?.to_string();
                // A NODE MUST NOT ANNOUNCE ITSELF TO ITSELF. Same rule as the
                // pull side: a config that names its own host would otherwise
                // give this node a proxied copy of hardware it holds the tty for.
                let me = conminer_core::peers::Identity::load_or_create(&ctx.config().paths.data_dir, "", ctx.now())
                    .map(|i| i.instance_id)
                    .unwrap_or_default();
                if instance_id == me {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        "that announcement is from this node itself".to_string(),
                    ));
                }
                let now = ctx.now();
                let advert = pr::Advert {
                    instance_id: instance_id.clone(),
                    name: node.clone(),
                    version: opt_s(a, "build").unwrap_or_default().to_string(),
                    mcp_url: opt_s(a, "mcp_url").unwrap_or_default().to_string(),
                    dash_url: opt_s(a, "dash_url").unwrap_or_default().to_string(),
                    ser2net_host: opt_s(a, "ser2net_host").unwrap_or_default().to_string(),
                    ser2net_ports: vec![],
                };
                let host = opt_s(a, "host").map(str::to_string);
                let mut reg = ctx.registry();
                // Clear any stand-in row the operator's `[peers] nodes` entry
                // left behind for this same node. On a one-way link its probe
                // never succeeds, so nothing else will ever clear it.
                pr::adopt_announcement(&mut reg, &advert.mcp_url, &advert.name)?;
                pr::upsert_advert(&mut reg, &advert, pr::PeerSource::Push, host.as_deref(), now)?;
                let devices: Vec<Value> = a
                    .get("devices")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let row = pr::by_name(&reg, &node)?.ok_or_else(|| {
                    ToolError::new(ErrorCode::Internal, "the peer row vanished".to_string())
                })?;
                let base = ctx.config().ser2net.base_port;
                let me_name = ctx.node_name().to_string();
                let (added, updated) = conminer_core::peers::inventory::import_devices(
                    &mut reg, &row, &devices, base, now, &me_name,
                )?;
                Ok(json!({
                    "node": node,
                    "accepted": devices.len(),
                    "added": added,
                    "updated": updated,
                    // SAY WHETHER THIS IS A ONE-WAY RELATIONSHIP. The announcer
                    // can reach us; whether we can reach it back is a different
                    // question, and the answer decides whether its boards can be
                    // actuated from here or only watched.
                    "return_path": row.ok,
                }))
            },
        },
        Tool {
            name: "peer_poll",
            description: "A peer asking whether this node has any work for it to run. \
                          §P3. Parks for up to `wait_ms` and returns one call, or null. \
                          This is the reverse channel: it lets a node that can only be \
                          DIALLED still have its hardware driven by a node it can only \
                          dial, by running that node's calls on its behalf.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["node"],
                "properties": {
                    "node": {"type": "string", "description": "the polling node's name"},
                    "wait_ms": {"type": "integer", "minimum": 0, "maximum": 25000,
                        "description": "how long to park before answering with nothing"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let node = s(a, "node")?.to_string();
                // A NODE MUST NOT COLLECT ITS OWN WORK. Nothing here would ever
                // queue a call for this node -- those are answered locally -- so
                // a self-poll can only be a misconfiguration, and parking on it
                // would tie up a thread forever saying nothing.
                if node == ctx.node_name() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        "that poll is from this node itself".to_string(),
                    ));
                }
                let wait = std::time::Duration::from_millis(
                    a.get("wait_ms").and_then(Value::as_u64).unwrap_or(0),
                );
                // Recorded before we park, not after: a worker that waits the
                // full 25 s for nothing is listening the whole time, and a
                // dashboard refreshing in that window must not paint the node
                // as unreachable.
                conminer_core::peers::registry::note_poll(&mut ctx.registry(), &node, ctx.now())?;
                let call = ctx.relay().take(&node, wait, ctx.now());
                Ok(json!({
                    "node": ctx.node_name(),
                    "call": call.map(|c| c.to_json()),
                }))
            },
        },
        Tool {
            name: "peer_result",
            description: "A peer returning the answer to a call it collected with peer_poll. \
                          §P3. The raw JSON-RPC result, unwrapped by the waiting caller with \
                          the same code that unwraps a directly forwarded reply -- so a \
                          relayed tool error arrives as that tool's error.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["id", "node", "result"],
                "properties": {
                    "id": {"type": "integer", "description": "the call id from peer_poll"},
                    "node": {"type": "string", "description": "the answering node's name"},
                    "result": {"type": "object", "description": "the JSON-RPC `result` object"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let id = a
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgument, "id is required".to_string()))?;
                let node = s(a, "node")?.to_string();
                let result = a.get("result").cloned().unwrap_or(Value::Null);
                let accepted = ctx.relay().complete(id, &node, result);
                // NOT an error when it is refused. The usual reason is that the
                // caller gave up while the owner was working, and telling the
                // owner it failed would send it hunting a fault that is not
                // there. Say plainly that nobody was waiting.
                Ok(json!({"accepted": accepted, "id": id}))
            },
        },
        Tool {
            name: "list_profiles",
            description: "Framer profiles available to this server, with their stage order and \
                          how many patterns each declares.",
            mutating: false,
            schema: || json!({"type": "object", "properties": {}, "additionalProperties": false}),
            call: |ctx, _a| {
                Ok(json!({
                    "profiles": ctx.profiles().all().iter().map(|p| json!({
                        "name": p.name, "stage": p.stage, "stage_rank": p.stage_rank,
                        "overlay": p.overlay, "banners": p.banners.len(),
                        "triggers": p.triggers.len(), "prompts": p.prompts.len(),
                        "record_timeout_s": p.record_timeout_s,
                    })).collect::<Vec<_>>()
                }))
            },
        },
        Tool {
            name: "get_prompts",
            description: "Prompts expected per boot stage, with provenance (profile, configured, \
                          learned) and confidence. Answers 'what prompt should I expect when this \
                          image reaches U-Boot?' from the device's own history.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "stage": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = crate::report::get_prompts(ctx, &d, opt_s(a, "stage"))?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "name_device",
            description: "Bind a nickname to a device. The nickname follows the adapter, not the \
                          ttyUSBn number, so it survives replug and reboot.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["device", "nickname"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "nickname": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device(s(a, "device")?)?;
                ctx.registry().set_nickname(d.id, s(a, "nickname")?)?;
                Ok(json!({"device": s(a, "nickname")?, "canonical": d.canonical}))
            },
        },
        Tool {
            name: "forget_device",
            description: "Delete a device row that is no longer real. Refuses anything still                           present: a `gone` row for a board whose cable fell out must survive,                           because its port, nickname and capture history are how it comes back.                           For rows that were never a board at all -- a flash gadget that                           enumerated once during EDL, a positional id left by a re-plug. Pass                           `drop_data` to delete its mined store too.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["device"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "drop_data": {"type": "boolean", "default": false, "description":
                        "also delete the device's store file. Default false: forgetting a row \
                         and destroying its capture history are different decisions."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device(s(a, "device")?)?;
                // PRESENT MEANS NO. Forgetting a live device would delete the row
                // discovery is about to recreate, losing its port assignment and
                // nickname for nothing -- and if it were mid-capture, minerd holds
                // its writer lock and the row would come back anyway. The state is
                // the check: only something discovery no longer sees can be junk.
                if d.state != "gone" && d.state != "ignored" {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "{} is {}, not gone: only a device that is no longer present \
                             can be forgotten",
                            d.display_name(),
                            d.state
                        ),
                    )
                    .with_hint(
                        "unplug it first, or exclude it in [discovery] if it should never \
                         have been opened",
                    ));
                }
                let drop_data = a
                    .get("drop_data")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let store = ctx.data_dir().join(&d.db_file);
                ctx.registry().forget_device(d.id)?;
                let mut dropped = false;
                if drop_data {
                    // Best effort, and reported: a store that could not be deleted
                    // must not read as one that was.
                    dropped = std::fs::remove_file(&store).is_ok();
                    for suffix in ["-wal", "-shm"] {
                        let _ = std::fs::remove_file(format!("{}{suffix}", store.display()));
                    }
                }
                Ok(json!({
                    "forgotten": d.canonical,
                    "was": d.state,
                    "data_dropped": dropped,
                    "note": if drop_data && !dropped {
                        json!("the row is gone; its store file could not be deleted")
                    } else {
                        Value::Null
                    },
                }))
            },
        },
        Tool {
            name: "tag_device",
            description: "Attach or remove key/value tags. Tags are selectors: \
                          `tag:role=ap-console AND tag:rack=r2` addresses a device, and is how a \
                          30-console lab stays navigable. A tag with an EMPTY value is a bare \
                          label -- `needs-rma` -- which is why removal takes `remove` rather than \
                          treating an empty value as a delete.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["device"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "tags": {"type": "object", "additionalProperties": {"type": "string"},
                        "description": "added or updated; an empty value is a bare label"},
                    "remove": {"type": "array", "items": {"type": "string"},
                        "description": "keys to drop"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device(s(a, "device")?)?;
                let tags: std::collections::BTreeMap<String, String> = a
                    .get("tags")
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let remove: Vec<String> = a
                    .get("remove")
                    .and_then(Value::as_array)
                    .map(|xs| {
                        xs.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if tags.is_empty() && remove.is_empty() {
                    return Err(ToolError::invalid_arg(
                        "give `tags` to add or update, or `remove` to drop keys",
                    ));
                }
                if !tags.is_empty() {
                    ctx.registry().set_tags(d.id, &tags)?;
                }
                if !remove.is_empty() {
                    ctx.registry().remove_tags(d.id, &remove)?;
                }
                let now = ctx.registry().device(d.id)?;
                Ok(json!({"device": d.display_name(), "tags": now.tags}))
            },
        },
        Tool {
            name: "identify",
            description: "Map cable to board safely: observed identity, last-traffic snippet, USB \
                          topology and endpoint. Read-only.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "dtr_pulse": {"type": "boolean", "default": false, "description":
                        "Pulse DTR to make the board react. DTR is wired to RESET on many boards, \
                         so this requires the device lease and is refused without one."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                if flag(a, "dtr_pulse") {
                    // Read-only by default; the disruptive mode is lease-gated.
                    ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                    return Err(ToolError::new(
                        ErrorCode::Unsupported,
                        "dtr_pulse needs a live capture session",
                    )
                    .with_hint("start minerd for this device, then retry"));
                }
                let tail = ctx.with_store(&d, |st| {
                    Ok(st.recent_lines(5)?.iter().map(|l| l.lossy()).collect::<Vec<_>>())
                })?;
                fresh(ctx, &d, json!({
                    "canonical": d.canonical,
                    "nickname": d.nickname,
                    "identity": d.identity,
                    "by_path": d.by_path,
                    // Informational only; no tool addresses a device by tty name.
                    "tty": d.tty,
                    "endpoint": d.ser2net_port.map(|p| format!("tcp://{}:{p}", ctx.config().ser2net.bind)),
                    "line": d.line.summary(),
                    "observed": d.observed,
                    "tail": tail,
                }))
            },
        },
        Tool {
            name: "rebuild_templates",
            description: "Regenerate every template from raw. This is how a similarity threshold \
                          is retuned retroactively; raw bytes are never touched.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "similarity": {"type": "number", "minimum": 0, "maximum": 1}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let mut cfg = conminer_core::drain::DrainConfig::from(&ctx.config().mine);
                if let Some(v) = a.get("similarity").and_then(Value::as_f64) {
                    if !(0.0..=1.0).contains(&v) {
                        return Err(ToolError::invalid_arg("similarity must be in 0.0..=1.0"));
                    }
                    cfg.similarity = v;
                }
                let profiles = ctx.profiles().clone();
                let payload = ctx.with_store(&d, |st| {
                    let before = st.template_count()?;
                    let after = st.rebuild_templates(cfg, &profiles)?;
                    Ok(json!({"before": before, "after": after, "similarity": cfg.similarity}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "selftest",
            description: "Run the acceptance gauntlet against a real board and report every check \
                          with its evidence. Composed entirely from the other tools, so anything \
                          it cannot express is a finding about the surface rather than a gap in \
                          the harness. ALWAYS leaves the board off and its straps clear, on every \
                          exit path including failure — `keep_on` is the only way to leave it \
                          powered, and it says so in the response.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description":
                        "A board (every console gets grouped epochs). Preferred over `device`."},
                    "device": {"type": "string", "description":
                        "A single console, for boards that have only one."},
                    "suites": {"type": "array", "items": {"type": "string",
                        "enum": ["capture","actuation","edl","mining","provenance","honesty"]},
                        "description": "Default: all of them."},
                    "steal": {"type": "boolean", "default": false, "description":
                        "Take leases held by others. Explicit, because interrupting somebody \
                         mid-flash is the one thing a self-test must not do by accident."},
                    "keep_on": {"type": "boolean", "default": false, "description":
                        "Leave the board powered at the end. Off by default, always."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let suites = a
                    .get("suites")
                    .and_then(Value::as_array)
                    .map(|v| {
                        v.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                crate::selftest::run(
                    ctx,
                    crate::selftest::Opts {
                        target: opt_s(a, "target").map(str::to_string),
                        device: opt_s(a, "device").map(str::to_string),
                        suites,
                        steal: flag(a, "steal"),
                        keep_on: flag(a, "keep_on"),
                    },
                )
            },
        },
        Tool {
            name: "backfill_versions",
            description: "Re-read stored raw and fill in the firmware versions for epochs captured \
                          before the banner extractors existed — WITHOUT re-minting templates. \
                          `rebuild_templates` also refreshes these, but renumbers every template \
                          on the way; this is the cheap half when all you want is \"what was \
                          running on that boot\". Epochs whose raw has been pruned are listed as \
                          skipped_pruned rather than silently returning nothing.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                ctx.registry()
                    .require_lease(d.id, &ctx.holder(), ctx.now())?;
                // Flattened across every profile, for the reason F2 was fixed:
                // a component is identified by ITS OWN banner, not by whichever
                // profile happened to frame the line it arrived on.
                let banners: Vec<conminer_core::framer::profile::VersionBanner> = ctx
                    .profiles()
                    .all()
                    .iter()
                    .flat_map(|p| p.version_banners.iter().cloned())
                    .collect();
                let payload = ctx.with_store(&d, |st| st.backfill_versions(&banners))?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "export_session",
            description: "Write a session to a single portable archive (raw plus metadata) for a \
                          bug report or cross-lab sharing. ingest_file reads it back byte-exact.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["session", "path"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"},
                    "path": {"type": "string", "description":
                        "Destination. A BARE FILENAME lands in the shared export directory \
                         (/exports, bind-mounted to ./exports on the host) and is retrievable \
                         without `docker cp`; an absolute path is honoured as given, which is \
                         container-local unless you mapped it."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let session = i(a, "session")?;
                let path = shared_path(s(a, "path")?);
                let cap = (ctx.config().export.max_gb * 1024.0 * 1024.0 * 1024.0) as i64;
                let payload = ctx.with_store(&d, |st| {
                    let meta = st.session(session)?;
                    if meta.bytes > cap {
                        return Err(ToolError::new(
                            ErrorCode::IngestTooLarge,
                            format!("session is {} bytes, over the {cap}-byte export guard", meta.bytes),
                        )
                        .with_hint("raise export.max_gb, or export a narrower session"));
                    }
                    let mut f = std::fs::File::create(&path)?;
                    let n = st.export_session(session, &mut f)?;
                    // Say where this landed OUTSIDE the container. An export the
                    // caller cannot find is an export they have to `docker cp`,
                    // which was the whole finding.
                    let host = host_hint(&path);
                    Ok(json!({
                        "path": path.display().to_string(),
                        "host_path": host,
                        "raw_bytes": n,
                        "archive_bytes": std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "mark",
            description: "Open a new boot epoch before you flip power. Call this *first*: it is \
                          what makes 'did it boot?' unanswerable from the previous boot's output.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let now = ctx.now();
                let label = opt_s(a, "label");
                let payload = ctx.with_store(&d, |st| {
                    let session = st.latest_session()?.map(|s| s.id);
                    let boot = st.open_boot("mark", label, now, session)?;
                    Ok(json!({
                        "boot_id": boot.id,
                        "boot_seq": boot.seq,
                        "cursor": st.head_cursor().encode(),
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "acquire",
            description: "Reserve a console, or with `target` every console of a board. Reads are \
                          always unrestricted; every mutating tool requires the lease, so two \
                          agents cannot interleave whole workflows.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "target": {"type": "string", "description":
                        "A board: takes the lease on EVERY one of its consoles, which is exactly \
                         what power/boot_mode with the same `target` require. Mutually exclusive \
                         with `device`."},
                    "holder": {"type": "string", "description": "Defaults to this connection's id."},
                    "ttl_s": {"type": "integer", "minimum": 1},
                    "steal": {"type": "boolean", "default": false, "description":
                        "Take a lease held by someone else. Explicit by design."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                // The lease must take the same selector the actuation takes.
                //
                // `power` and `boot_mode` accept a `target` and then require a
                // lease on every console of it, but acquire only ever spoke
                // console. So the documented flow, `acquire <target>` then
                // `power <target>`, could not be written: acquire read the
                // target as a device substring and failed AMBIGUOUS_DEVICE, and
                // the caller had to scrape the console list out of a
                // LEASE_REQUIRED detail and acquire each one by hand.
                let target = opt_s(a, "target");
                let device = opt_s(a, "device");
                if target.is_some() && device.is_some() {
                    return Err(ToolError::invalid_arg(
                        "`device` and `target` are mutually exclusive: one names a console, the \
                         other names every console of a board",
                    ));
                }
                if let Some(h) = opt_s(a, "holder") {
                    ctx.set_holder(h);
                }
                let cfg = ctx.config().lease.clone();
                let ttl = opt_i(a, "ttl_s").unwrap_or(cfg.ttl_s as i64);
                let steal = flag(a, "steal");
                let Some(t) = target else {
                    let d = ctx.device(s(a, "device")?)?;
                    let lease = ctx.registry().acquire_lease(
                        d.id,
                        &ctx.holder(),
                        ctx.now(),
                        ttl,
                        cfg.max_s as i64,
                        steal,
                    )?;
                    return Ok(json!({"lease": lease, "device": d.display_name()}));
                };
                let (consoles, exempt) = {
                    let reg = ctx.registry();
                    conminer_core::target::console_members(&reg, t)?
                };
                let (holder, now) = (ctx.holder(), ctx.now());
                // Every blocker at once. Failing on the first means an operator
                // holding four of five consoles learns about the fifth only
                // after clearing the fourth, which is the same lesson
                // `actuation_scope` already learned about reporting missing
                // leases.
                let mut blocked = Vec::new();
                if !steal {
                    for c in &consoles {
                        let held = ctx.registry().lease(c.id)?;
                        if let Some(l) = held {
                            if l.expires_at > now && l.holder != holder {
                                blocked.push(json!({
                                    "console": c.display_name(),
                                    "holder": l.holder,
                                    "expires_at": l.expires_at,
                                }));
                            }
                        }
                    }
                }
                if !blocked.is_empty() {
                    return Err(ToolError::new(
                        ErrorCode::LeaseHeld,
                        format!(
                            "target {t:?} has {} of its {} console(s) leased by someone else",
                            blocked.len(),
                            consoles.len()
                        ),
                    )
                    .with_hint(format!(
                        "acquire({{\"target\": {t:?}, \"steal\": true}}) to take them"
                    ))
                    .with_detail(json!({"blocked": blocked})));
                }
                // All of them or none. A half-taken target is the state this
                // tool exists to prevent: the actuation still refuses, and the
                // consoles it did take are now blocking whoever could have
                // finished the job.
                let mut leases = Vec::new();
                let mut taken: Vec<i64> = Vec::new();
                for c in &consoles {
                    let got = ctx.registry().acquire_lease(
                        c.id,
                        &holder,
                        now,
                        ttl,
                        cfg.max_s as i64,
                        steal,
                    );
                    match got {
                        Ok(l) => {
                            taken.push(c.id);
                            leases.push(json!({"console": c.display_name(), "lease": l}));
                        }
                        Err(e) => {
                            for id in &taken {
                                let _ = ctx.registry().release_lease(*id, &holder);
                            }
                            return Err(e);
                        }
                    }
                }
                Ok(json!({
                    "target": t,
                    "leases": leases,
                    "consoles": consoles.iter().map(|c| c.display_name()).collect::<Vec<_>>(),
                    "exempt": exempt,
                }))
            },
        },
        Tool {
            name: "release",
            description: "Release a console lease, or with `target` every console of a board, so \
                          another agent can acquire it. Leases also expire on their own, so a \
                          crashed agent cannot hold one forever.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "target": {"type": "string", "description":
                        "A board: releases every one of its consoles. The counterpart of \
                         acquire({target}), and mutually exclusive with `device`."},
                    "force": {"type": "boolean", "default": false, "description":
                        "Drop the lease whoever holds it. For a lease stranded by a crashed or \
                         disconnected holder, which is otherwise unreclaimable through this tool."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                // The counterpart of acquire({target}): a lease taken by board
                // has to be returnable by board, or the asymmetry just moves to
                // the end of the workflow.
                if let Some(t) = opt_s(a, "target") {
                    if opt_s(a, "device").is_some() {
                        return Err(ToolError::invalid_arg(
                            "`device` and `target` are mutually exclusive: one names a console, \
                             the other names every console of a board",
                        ));
                    }
                    let consoles = {
                        let reg = ctx.registry();
                        conminer_core::target::console_members(&reg, t)?.0
                    };
                    let holder = ctx.holder();
                    let force = flag(a, "force");
                    // Keep going past the ones that were not held. A partial
                    // release that stops at the first console this holder never
                    // had would strand the rest, which is the failure the
                    // all-or-nothing acquire is meant to make impossible.
                    let mut released = Vec::new();
                    let mut not_held = Vec::new();
                    for c in &consoles {
                        let done = if force {
                            ctx.registry().force_release_lease(c.id)
                        } else {
                            ctx.registry().release_lease(c.id, &holder).map(|()| true)
                        };
                        match done {
                            Ok(true) => {
                                let _ = ctx.registry().release_exclusive(c.id);
                                released.push(c.display_name().to_string());
                            }
                            Ok(false) | Err(_) => not_held.push(c.display_name().to_string()),
                        }
                    }
                    return Ok(json!({
                        "target": t,
                        "released": released,
                        "not_held": not_held,
                        "forced": force,
                    }));
                }
                let d = ctx.device(s(a, "device")?)?;
                if flag(a, "force") {
                    let dropped = ctx.registry().force_release_lease(d.id)?;
                    // A CLAIM MUST NOT OUTLIVE THE LEASE IT WAS TAKEN UNDER.
                    //
                    // `claim_exclusive` hands one console to one protocol, and
                    // the holder that took it can die mid-transfer. Its claim
                    // then blocks every later caller, `claim_exclusive` points
                    // at a `release_exclusive(device)` tool that has never
                    // existed, and the console is unreachable until somebody
                    // restarts mcpd (report #27). Dropping the lease is the
                    // moment that claim stopped meaning anything.
                    ctx.registry().release_exclusive(d.id)?;
                    return Ok(json!({"released": dropped, "device": d.display_name(),
                                     "forced": true}));
                }
                ctx.registry().release_lease(d.id, &ctx.holder())?;
                // Same rule for an ordinary release: the claim belonged to the
                // work the lease was covering.
                ctx.registry().release_exclusive(d.id)?;
                Ok(json!({"released": d.display_name()}))
            },
        },
        Tool {
            name: "follow",
            description: "Wait server-side until something happens, then return everything since \
                          your cursor in mined form. WITH NO CURSOR IT STARTS AT THIS BOOT, not at \
                          the live head: the board does not wait for you, and everything it \
                          printed between your `power` call and this one would otherwise be \
                          invisible. `until` is a predicate: a pattern, a novel \
                          template, a stage, the prompt, a quiet period, a reset, or the first of \
                          several. Burns no tokens while waiting; a timeout returns the data so \
                          far rather than an error.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "cursor": {"type": "string", "description":
                        "Where to resume. Omit to start from the current head."},
                    "until": {"type": "object", "description":
                        "One of {pattern:<regex>}, {template:\"new\"}, {stage:<name>}, \
                         {prompt:true}, {quiet:<ms>}, {reset:true}, {watch:<name>} (park until a \
                         durable watch fires), or {any:[…]}."},
                    "timeout_s": {"type": "integer", "minimum": 1},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::follow::{clamp_timeout, increment, Predicate};
                let d = device(ctx, a)?;
                let until = match a.get("until") {
                    Some(v) => Predicate::parse(v)?,
                    // No predicate: return whatever has arrived and come back.
                    None => Predicate::QuietMs(0),
                };
                let timeout = clamp_timeout(
                    opt_i(a, "timeout_s"),
                    ctx.config().follow.default_timeout_s,
                    ctx.config().api.follow_timeout_max_s,
                )?;
                let max_lines = capped(a, "max_lines", 100, ctx.config().api.max_raw_lines.max(100));
                let (start, start_from) = match opt_s(a, "cursor") {
                    Some(c) => (conminer_core::store::Cursor::decode(c)?, "cursor"),
                    None => ctx.with_store(&d, |st| Ok(default_follow_start(st)))?,
                };
                let prompt_set = prompt_set(ctx, &d)?;

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout as u64);
                // §F6. ADVANCE the watches this predicate waits on, every pass.
                //
                // The scanner only runs when a watch is polled, so a follow
                // parked on a watch used to wait on a queue nothing was filling:
                // measured on the IQ10, 60 s of timeout with `fired_total: 0`
                // while the console talked throughout. Scanning here is what
                // makes "stop calling poll_watch" true.
                let watches = until.watch_names();
                loop {
                    if !watches.is_empty() {
                        let now = ctx.now();
                        let ps = prompt_set.clone();
                        ctx.with_store(&d, |st| {
                            for name in &watches {
                                let Ok(w) = st.watch(name) else { continue };
                                let Ok(pred) = Predicate::parse(&w.predicate) else {
                                    continue;
                                };
                                let (hits, scanned_to) = conminer_core::follow::scan_hits(
                                    st, w.scanned_to, &pred, 1000, &ps, now,
                                )?;
                                st.record_watch_hits(w.id, &hits, scanned_to, now)?;
                            }
                            Ok(())
                        })?;
                    }
                    let inc = ctx.with_store(&d, |st| {
                        increment(st, &start, &until, max_lines, &prompt_set, ctx.now())
                    })?;
                    if inc.matched.is_some() || std::time::Instant::now() >= deadline {
                        // A timeout is data, not an error (§8.2). `start_from`
                        // says WHERE the watch began, because "not found" means
                        // something different depending on the answer.
                        return fresh(ctx, &d, json!({
                            "follow": inc,
                            "timed_out": inc.matched.is_none(),
                            "start_from": start_from,
                            "started_at": start.encode(),
                        }));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            },
        },
        Tool {
            name: "run_command",
            description: "Send a command as a transaction: assert the prompt, send \
                          character-at-a-time with echo verification, capture until the prompt \
                          returns, and climb a recovery ladder if it does not. Always returns a \
                          truthful terminal state with evidence — never infers success from \
                          silence.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "command": {"type": "string"},
                    "timeout_s": {"type": "integer", "minimum": 1},
                    "echo": {"type": "boolean", "default": true, "description":
                        "Per-character echo verification. Set false for no-echo consoles, which \
                         downgrades to pacing-only."},
                    "force": {"type": "boolean", "default": false, "description":
                        "Send even at an unrecognised prompt. Explicit by design."},
                    "verbose": {"type": "boolean", "default": false, "description":
                        "Include the forensic fields (rungs_attempted, preamble, detail). Off by \
                         default: a successful command does not need them. Failures always carry \
                         their diagnostic regardless."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{BrokeredTransport, CommandOptions, Runner};
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                ctx.require_no_actuation(&d)?;
                // Refuse BEFORE transmitting into a board in a flash/recovery
                // mode (report #23): no newline, no command bytes.
                ctx.require_console_deliverable(&d)?;
                if let Some((holder, proto)) = ctx.registry().exclusive_claim(d.id)? {
                    return Err(ToolError::new(
                        ErrorCode::ExclusiveClaimed,
                        format!("the port is claimed by {holder:?} for {proto:?}"),
                    ));
                }
                let cmd = s(a, "command")?.to_string();
                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let prompts = prompts_for(ctx, &d)?;
                let mut opts = CommandOptions::new(
                    ctx.config().runner_for(d.display_name()),
                    ctx.config().line_for(d.display_name()),
                );
                opts.echo = a.get("echo").and_then(Value::as_bool).unwrap_or(true);
                opts.force = flag(a, "force");
                if let Some(t) = opt_i(a, "timeout_s") {
                    opts.timeout_s = t.max(1) as u64;
                }

                let txn = block_on(async move {
                    let io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                    Runner::new(io, prompts, opts).at(&endpoint).run(&cmd).await
                })?;

                // The transaction is a first-class timeline event, so a command
                // that triggered a crash can be linked to the crash record.
                let now = ctx.now();
                let confirmed = txn.prompt_matched.clone();
                let kind = prompt_kind_of(ctx, &d, confirmed.as_deref());
                ctx.with_store(&d, |st| {
                    st.append_event(None, None, now, "run_command", &json!({
                        "command": txn.command,
                        "status": txn.status,
                        "duration_ms": txn.duration_ms,
                    }))?;
                    // §F7. ONE OWNER FOR PROMPT KNOWLEDGE. A successful command
                    // is proof this device sits at this prompt and takes input --
                    // the strongest evidence available, and it used to be thrown
                    // away, which is why console_state could keep reporting
                    // `commandable: false` at a prompt run_command had just
                    // driven. Recording it here means the next console_state
                    // reads what the runner learned.
                    if let (Some(p), Some(k)) = (&confirmed, &kind) {
                        let boot = st.latest_boot()?.map(|b| b.id);
                        st.observe_prompt(p, k, None, now, boot)?;
                    }
                    Ok(())
                })?;

                if let Some(e) = txn.as_error() {
                    return Err(e);
                }
                // Terse by default. On the happy path an agent wants the status
                // and the output; `rungs_attempted`, `preamble` and `detail` are
                // forensic fields that matter when something went wrong, and a
                // successful command paid for all of them on every call.
                if flag(a, "verbose") {
                    return fresh(ctx, &d, json!({"transaction": txn}));
                }
                fresh(ctx, &d, json!({"transaction": {
                    "status": txn.status,
                    "command": txn.command,
                    // A command's output is console text like any other. Measured
                    // on the IQ10: a printf of two colour codes came back with
                    // seven escape sequences plus the shell's bracketed-paste
                    // toggles, in the field an agent diffs against expected
                    // output. `ansi: "keep"` returns the bytes verbatim.
                    "output": txn.output,
                    "output_capped": txn.output_capped,
                    "duration_ms": txn.duration_ms,
                }}))
            },
        },
        Tool {
            name: "diagnose",
            description: "Why is this console not answering? Opens its OWN connection to the \
                          device's ser2net endpoint and reports what it actually sees: bytes \
                          received, first bytes, whether a prompt is present, the lease holder, \
                          and the capture state. Call this FIRST when a console looks wrong.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "wait_ms": {"type": "integer", "minimum": 100, "maximum": 10000, "default": 1500,
                        "description": "How long to listen on the probe connection."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{strip_telnet, telnet_refusals};
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                let d = ctx.device_or_only(opt_s(a, "device"))?;
                let wait = opt_i(a, "wait_ms").unwrap_or(1500).clamp(100, 10_000) as u64;
                let endpoint = endpoint_for(ctx, &d).ok();
                // AN EXPIRED LEASE IS NOT A LEASE.
                //
                // `require_lease` has always treated one as free -- acquisition
                // reclaims it -- but this diagnostic published the row whatever
                // its expiry, so a reservation that lapsed an hour ago still read
                // as "held by dashboard" and an agent stepped around a board
                // nobody was using. Reported from a live session. The history is
                // still worth having, so it moves to a field that cannot be
                // mistaken for a live holder.
                let now = ctx.now();
                let (lease, stale_lease) = match ctx.registry().lease(d.id)? {
                    Some(l) if l.expires_at > now => (Some(l), Value::Null),
                    Some(l) => (
                        None,
                        json!({
                            "holder": l.holder,
                            "expired_at": l.expires_at,
                            "expired_ms_ago": now - l.expires_at,
                            "note": "expired, so the device is free: acquire() will take it",
                        }),
                    ),
                    None => (None, Value::Null),
                };

                // The probe opens its OWN connection on purpose. The defining
                // failure this exists for: minerd was capturing 130863 bytes
                // while a fresh client received ZERO on the same port, because
                // ser2net withholds data until telnet negotiation is answered.
                // Nothing that reads the store could have shown that.
                let probe = endpoint.clone().map(|ep| {
                    block_on(async move {
                        let mut connected = false;
                        let mut got: Vec<u8> = Vec::new();
                        let deadline = std::time::Duration::from_millis(wait);
                        let attempt = tokio::time::timeout(deadline, async {
                            // The probe reads through the broker too, so
                            // `diagnose` reports what the SHARED reader sees
                            // rather than what a private second connection sees.
                            // Those disagreed on this rig for hours.
                            let mut sock = tokio::net::TcpStream::connect(&ep).await?;
                            connected = true;
                            let mut buf = [0u8; 4096];
                            let until = tokio::time::Instant::now() + deadline;
                            while tokio::time::Instant::now() < until && got.len() < 4096 {
                                let left = until - tokio::time::Instant::now();
                                match tokio::time::timeout(left, sock.read(&mut buf)).await {
                                    Ok(Ok(0)) | Err(_) => break,
                                    Ok(Ok(n)) => {
                                        // Answer negotiation, or ser2net never speaks.
                                        let r = telnet_refusals(&buf[..n]);
                                        if !r.is_empty() {
                                            let _ = sock.write_all(&r).await;
                                        }
                                        got.extend_from_slice(&strip_telnet(&buf[..n]));
                                    }
                                    Ok(Err(e)) => return Err(e),
                                }
                            }
                            Ok::<_, std::io::Error>(())
                        })
                        .await;
                        let err = match attempt {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.to_string()),
                            Err(_) => Some("timed out".to_string()),
                        };
                        let text = String::from_utf8_lossy(&got).to_string();
                        // A console can answer with ser2net's device-open failure
                        // banner instead of the board. Those bytes are NOT board
                        // output, and counting them as such is how a dead console
                        // passes for a live one.
                        let banner = conminer_core::runner::is_open_failure_banner(&got);
                        // SER2NET TALKING ABOUT ITSELF IS NOT THE BOARD TALKING.
                        //
                        // The comment above has always said these bytes are not
                        // board output; the count below reported them as board
                        // output anyway. So one response said "the console is
                        // wedged" and, three fields up, "46 bytes received" --
                        // and an agent reading it concluded conminer was calling
                        // ser2net's error text delivered console data. It was.
                        // The failure text moves to a field of its own and the
                        // board's byte count tells the truth: nothing arrived.
                        json!({
                            "endpoint": ep,
                            "connected": connected,
                            "bytes_received": if banner { 0 } else { got.len() },
                            "open_failed": banner,
                            "first_bytes": if banner {
                                String::new()
                            } else {
                                text.chars().take(200).collect::<String>()
                            },
                            "server_message": if banner {
                                json!(text.trim().chars().take(200).collect::<String>())
                            } else {
                                Value::Null
                            },
                            "server_message_bytes": if banner { json!(got.len()) } else { Value::Null },
                            "error": err,
                        })
                    })
                });

                // ASK the controller instead of speculating. `diagnose` used to
                // say a silent console "may be powered off" -- the answer was
                // always one query away, and a human had to go read a controller
                // line by hand to settle it.
                let power_state = probe_power_state(ctx, &d);
                // Out-of-band truth: the console cannot report EDL, because in
                // EDL it says nothing at all.
                let usb = conminer_core::usb::scan();
                // §L6. A ZOMBIE BELONGS TO A PORT, NOT TO THE BENCH.
                //
                // Matching ran bus-wide, so the ADP's abandoned ADB gadget --
                // 18d1:d002, bus 3, from a boot an hour earlier -- was counted
                // against the Nord and refused to certify its cleanup. The
                // Nord had touched nothing on that bus.
                //
                // With `usb_ports` configured for a board, `usb_zombies` means
                // "on this board's own hub ports" and nothing else. Without it,
                // attribution is genuinely unknown, and the count still ships
                // -- under a name that says so -- rather than being blamed on
                // whichever board happened to ask.
                let all_ghosts = conminer_core::usb::zombies(&usb);
                // Config declares it; the registry LEARNS it. A rig's cabling
                // is not a shipped default (conminer.toml is a reference for
                // the code's defaults, §16), so the tag is where a real bench
                // records which USB ports are this board's -- set by hand with
                // `tag`, or written by the selftest from its own EDL entry,
                // where the causal link is not a guess: we put this board into
                // download mode and this is the gadget that appeared.
                let ports = declared_usb_ports(ctx, &d);
                let scoped = !ports.is_empty();
                // AND SO DOES A DOWNLOAD GADGET. Same rule, same ports: EDL was
                // matched bus-wide long after zombies were scoped, so on a host
                // with two boards either one sitting in download mode answered
                // for both. `edl_scope` says which claim this is, because "a
                // board here is in EDL" must not be read as "this board is".
                let edl = conminer_core::usb::in_edl_on_ports(&usb, &ports);
                // A RESPONSE MUST NOT CARRY `edl: true` AND A COMMANDABLE
                // CONSOLE AT THE SAME TIME.
                //
                // The probe just established that this board is in download
                // mode -- its UART re-enumerates away, so there is no console to
                // be at a prompt on. But the freshness envelope derives its
                // console block from the device row's `capture_state`, which
                // still says `listening` until the capture loop next fails to
                // read, so ONE response said `edl: true` and
                // `console.state=at_prompt, commandable=true` together.
                // Reported twice on the Uno-Q; the second sighting is what
                // showed it leaking through `diagnose` and not just
                // `boot_mode`.
                //
                // `diagnose` is the authoritative prober -- it is the tool whose
                // whole job is going and looking -- so when it learns this, it
                // records it. Every later reader then agrees, and the capture
                // loop re-validates on its own deadline (the `stale_capture`
                // note above is the same rule pointing the other way).
                if edl {
                    // §W4: publish only; the envelope reads capture health live.
                    let mut reg = ctx.registry();
                    let _ = conminer_core::live::publish_capture_state(
                        &mut reg,
                        d.id,
                        conminer_core::live::CaptureState::AwayInEdl,
                    );
                }

                // COMPUTE THE CONSOLE BLOCK AFTER the EDL discovery is published,
                // not before (report #7, reopened). `console_state` derives from
                // the live capture health, so computing it up front -- while the
                // capture loop still said `listening` -- let diagnose ship
                // `edl: true` beside `console.state=at_prompt, commandable=true`
                // in the SAME response. Publishing `away_in_edl` first makes this
                // read the board's actual state: no commandable console in EDL.
                let state = console_state(ctx, &d).unwrap_or(Value::Null);
                let (ghosts, unattributed) = if scoped {
                    let mine = all_ghosts
                        .iter()
                        .filter(|z| conminer_core::usb::on_ports(z, &ports))
                        .count();
                    (mine, all_ghosts.len() - mine)
                } else {
                    (all_ghosts.len(), 0)
                };
                // "no endpoint" is a symptom, not an answer: say WHY there is no
                // console. From the operator's side "this console does not exist"
                // and "this console is broken" look identical, and today they
                // looked identical for hours.
                let no_endpoint_reason = if d.ignored {
                    "this device is excluded by config (discovery.exclude or a controller profile \
                     claims it), so it has no ser2net console by design"
                } else if d.state == "gone" {
                    "this device is in state=gone: its /dev/serial/by-id node is absent, so it was \
                     excluded from the ser2net config and has no console"
                } else {
                    "no ser2net port is assigned to this device"
                };
                // State wins over a stale port assignment: a gone device can
                // still carry an orphaned port, and reporting "cannot connect"
                // would send the reader hunting ser2net for a device that is not
                // plugged in.
                // WHEN THE PROBE AND THE STORED STATE DISAGREE, SAY SO.
                //
                // The probe just connected and read; `capture_state` is what
                // minerd last published, and minerd only publishes when
                // something happens to it. So a console that recovered while
                // minerd sat idle reads as broken in one field and healthy in
                // another, in the same response -- reported from the bench as a
                // state inconsistency, and it is one. The probe is the fresher
                // witness, and it is this tool's whole reason for existing.
                let stale_capture = match capture_state_is_stale(
                    d.capture_state.as_deref().unwrap_or(d.state.as_str()),
                    probe.as_ref(),
                    edl,
                ) {
                    Some(stored) => json!(format!(
                        "capture_state says {stored:?}, but this probe connected and read bytes \
                         just now: minerd has not re-attached yet. It re-dials a failed console \
                         periodically, so this should correct itself; the probe is the fresher \
                         witness either way"
                    )),
                    None => Value::Null,
                };

                let verdict = console_verdict(
                    d.ignored,
                    &d.state,
                    state.get("state").and_then(Value::as_str).unwrap_or(""),
                    endpoint.as_ref(),
                    probe.as_ref(),
                    edl,
                    power_state.as_deref(),
                    no_endpoint_reason,
                );

                // The divergence conminer could not previously express, and the
                // one that cost the most: minerd captured NOTHING while the port
                // was streaming, because its read loop died on a store error and
                // reattached forever. console_state reported 0 bytes, so the
                // board looked dead. A live probe plus the store's own recency is
                // enough to say this outright.
                let probe_got = probe
                    .as_ref()
                    .and_then(|p| p["bytes_received"].as_u64())
                    .unwrap_or(0);
                let captured = state
                    .get("bytes_this_boot")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        ctx.freshness(&d)
                            .ok()
                            .and_then(|f| f.get("bytes_this_boot").and_then(Value::as_u64))
                    })
                    .unwrap_or(0);
                let verdict: &str = if probe_got > 0 && captured == 0 {
                    "the endpoint IS delivering data but capture is NOT recording it: the device \
                     will look dead (0 bytes) while the board is talking. Check minerd's log for a \
                     read loop that keeps ending and reattaching."
                } else {
                    verdict
                };

                fresh(ctx, &d, json!({
                    "verdict": verdict,
                    // Measured, not guessed. `null` means this controller cannot
                    // answer (the Bughopper's CBUS pins are outputs with no sense
                    // line back from the board) -- which must never be rendered
                    // as "off", because a silent-but-running board would then be
                    // misread as a dead one.
                    "power": power_state,
                    "edl": edl,
                    "edl_scope": if scoped {
                        json!({"ports": ports, "attribution": "this board's own hub ports"})
                    } else {
                        json!({
                            "ports": [],
                            "attribution": "unknown: no usb_ports configured for this device, so \
                                            a QDL gadget anywhere on this host counts",
                        })
                    },
                    // Stale entries mislead the NEXT session, so name them here
                    // even though nothing is wrong with this console.
                    "usb_zombies": ghosts,
                    // What the count above actually covers, so a reader is never
                    // left guessing whether it is about this board (§L6).
                    "usb_zombies_scope": if scoped {
                        json!({"ports": ports, "attribution": "this board's own hub ports"})
                    } else {
                        json!({
                            "ports": [],
                            "attribution": "unknown: no usb_ports configured for this device, so \
                                            the count is bench-wide and may belong to another board",
                        })
                    },
                    "usb_zombies_elsewhere": unattributed,
                    "probe": probe,
                    "console": state,
                    "lease": lease,
                    "stale_lease": stale_lease,
                    "stale_capture_state": stale_capture,
                    "device_state": d.state,
                    "capture_state": d.capture_state,
                }))
            },
        },
        Tool {
            name: "help",
            description: "Index of every tool, and the full schema for one of them. The default \
                          advertised surface is the core console/power set; everything else stays \
                          callable and is discoverable here. Call with no argument for names plus \
                          one line each (cheap); with `tool` for that tool's full schema.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description":
                        "Return this tool's full input schema, ready to call."}
                },
                "additionalProperties": false
            }),
            call: |_ctx, a| {
                if let Some(name) = opt_s(a, "tool") {
                    let t = find(name).ok_or_else(|| {
                        ToolError::new(ErrorCode::InvalidArgument, format!("no tool {name:?}"))
                            .with_hint("call help() with no argument for the index")
                    })?;
                    let (cost, precondition) = cost_and_precondition(t.name);
                    return Ok(json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": (t.schema)(),
                        "mutating": t.mutating,
                        // §F8. MACHINE-READABLE, so an agent can plan without
                        // parsing prose: what this costs in wall-clock, and what
                        // must be true before calling it.
                        "cost": cost,
                        "precondition": precondition,
                    }));
                }
                // Names plus a first line only. The whole index costs less than
                // a couple of full schemas.
                Ok(json!({
                    "tools": registry().iter().map(|t| json!({
                        "name": t.name,
                        "summary": t.description.split_terminator(['.', '\n'])
                            .next().unwrap_or(t.description).trim(),
                        "core": CORE_TOOLS.contains(&t.name),
                    })).collect::<Vec<_>>(),
                    "note": "call help({tool}) for a full schema; any tool here is callable",
                }))
            },
        },
        Tool {
            name: "send",
            description: "Low-level passthrough: write raw bytes to the console with no prompt \
                          assertion, echo verification or capture. Disabled unless \
                          runner.allow_raw_send is set, because it forfeits every reliability \
                          guarantee run_command provides.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["data"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "data": {"type": "string"},
                    "hex": {"type": "boolean", "default": false, "description":
                        "Interpret `data` as hex bytes."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                if !ctx.config().runner.allow_raw_send {
                    return Err(ToolError::new(
                        ErrorCode::SendDisabled,
                        "the raw send passthrough is disabled",
                    ));
                }
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                ctx.require_no_actuation(&d)?;
                // Same as run_command: no raw bytes into a board being flashed.
                ctx.require_console_deliverable(&d)?;
                let raw = s(a, "data")?;
                let bytes = if flag(a, "hex") {
                    let cleaned: String = raw.chars().filter(|c| c.is_ascii_hexdigit()).collect();
                    if cleaned.len() % 2 != 0 {
                        return Err(ToolError::invalid_arg("hex data must have an even length"));
                    }
                    (0..cleaned.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).unwrap_or(0))
                        .collect::<Vec<u8>>()
                } else {
                    raw.as_bytes().to_vec()
                };
                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let n = bytes.len();
                block_on(async move {
                    use conminer_core::runner::{BrokeredTransport, Transport};
                    use std::time::Duration;
                    let mut io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                    // Answer telnet negotiation BEFORE writing. ser2net's
                    // accepter is telnet and it withholds the port until the
                    // client replies; a write into an un-negotiated connection
                    // produced "Device open failure: Value or file not found"
                    // with raw IAC bytes leaking back. `read` answers the offer
                    // (see TcpTransport::read), so one short read is enough --
                    // this is the same defect fixed for the runner, on a second
                    // code path that was missed.
                    let mut scratch = [0u8; 512];
                    let _ = io.read(&mut scratch, Duration::from_millis(300)).await;
                    io.write_all(&bytes).await
                })?;
                fresh(ctx, &d, json!({"sent_bytes": n}))
            },
        },
        Tool {
            name: "power",
            description: "Drive the device's external power hook and open a boot epoch around it, \
                          so the next boot_report cannot be answered from the previous boot. \
                          conminer does not switch power itself; it invokes the lab's tooling and \
                          records the event. ONE ACTUATION PER BOARD AT A TIME: a second power or \
                          boot_mode call on the same board while one is running is refused with \
                          ACTUATION_IN_FLIGHT rather than raced against it. Budget for the whole \
                          workflow, not the press: an `off` on a board found alive in EDL escalates \
                          to reset-then-off and takes two hook timeouts plus ~33 s of settling; the \
                          response's `duration_ms` says what it actually took.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "target": {"type": "string", "description":
                        "Actuate a whole board instead of one console: opens a linked epoch on \
                         EVERY console of the target, so the boot evidence is anchored wherever \
                         it lands. Mutually exclusive with `device`."},
                    "action": {"type": "string", "enum": ["on", "off", "cycle", "reset"]},
                    "label": {"type": "string"},
                    "verify": {"type": "string", "enum": ["auto", "poke"], "default": "auto",
                        "description":
                        "How hard to try to CONFIRM the effect. \"auto\" uses what is already \
                         there: the controller's power sense, the board's own USB port leaving \
                         the bus, or the console going quiet. \"poke\" additionally sends a \
                         newline BEFORE an `off` so a board idle at a prompt answers -- which is \
                         the only way to confirm an off on a controller with no power sense and a \
                         board that enumerates nothing. It transmits to the board, so it is opt-in."},
                    "dry_run": {"type": "boolean", "description":
                        "Validate everything and show the exact hook argv WITHOUT running it or \
                         opening any epoch."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::hooks::{self, PowerAction};
                let scope = actuation_scope(ctx, a)?;
                let d = scope.primary.clone();
                let action = PowerAction::parse(s(a, "action")?)?;
                let plan = PowerPlan::resolve(ctx, &d)?;

                // §F5. EVERY validation, NOTHING actuated.
                //
                // The nickname-to-hook bug cost two rounds of broken ADP power
                // control; one dry run would have shown `--device adp-ventuno`
                // in the argv and ended it in a single call, at zero risk. So
                // this stops exactly here: after selector resolution, lease
                // check, target expansion and argv construction -- before exec.
                if flag(a, "dry_run") {
                    return fresh(ctx, &d, json!({
                        "dry_run": true,
                        "hook": {
                            "command": hooks::render(&plan.hook.template, &plan.args(action.as_str())),
                            "timeout_s": plan.timeout.as_secs(),
                        },
                        "would_open_epochs": scope.consoles.iter()
                            .map(|c| json!({"device": c.display_name(), "label": opt_s(a, "label")}))
                            .collect::<Vec<_>>(),
                        "target": scope.target,
                        "lease_check": scope.lease_check(),
                        "note": scope.note,
                    }));
                }

                // The hook runs *before* the epoch opens: a hook that fails must
                // not leave an epoch describing a boot nobody triggered. But the
                // epoch must still START here, before the board can answer --
                // hence the mark.
                // ASK THE BOARD TO SPEAK BEFORE TAKING ITS POWER AWAY.
                //
                // Only for `off`, only when asked: a console that was already
                // silent cannot be confirmed to have gone silent, and on a
                // controller with no power sense that leaves an `off` truthful
                // but unconfirmable forever.
                // ONE ACTUATION PER BOARD AT A TIME, hook to verified effect.
                //
                // Claimed here, after every validation and the dry run, and held
                // by the guard until this call returns by any path. The lease
                // cannot do this job: the second call comes from the SAME agent,
                // whose client gave up on the first one and moved on while the
                // escalation below was still pressing buttons.
                let in_flight = ctx.begin_actuation("power", action.as_str(), &scope.consoles)?;
                let payload = run_power(
                    ctx,
                    &scope,
                    &d,
                    action,
                    &plan,
                    opt_s(a, "label"),
                    opt_s(a, "verify") == Some("poke"),
                    in_flight,
                    std::time::Instant::now(),
                )?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "boot_mode",
            description: "Bring the board up in a chosen mode — EDL/Sahara, fastboot, UEFI — by \
                          invoking the lab's boot-mode hook, and open an epoch labelled with it. \
                          Separate from `power` because it is a different question: power means \
                          the same thing everywhere, while the valid modes are a property of the \
                          silicon.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["mode"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "target": {"type": "string", "description":
                        "Apply to a whole board: opens a linked epoch on every console of the \
                         target. Mutually exclusive with `device`."},
                    "dry_run": {"type": "boolean", "description":
                        "Validate the mode and show the hook argv WITHOUT running it."},
                    "mode": {"type": "string", "description":
                        "One of this device's configured `boot_modes`, or \"clear\" to release \
                         every latched strap. On strap-latching controllers (Bantam) setting a \
                         mode ONLY arms it -- the board keeps running until you `power reset`, \
                         and the strap STAYS SET, so every later boot lands in that mode until \
                         it is cleared. Sequencing controllers (Bughopper) do the whole thing \
                         in one action and release automatically."},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::hooks;
                let scope = actuation_scope(ctx, a)?;
                let d = scope.primary.clone();
                let mode = s(a, "mode")?;
                let present = present_with_topology(ctx);

                // Validated against the configured set rather than passed
                // through: this string becomes a hook argument, and a typo that
                // reached the board would be a boot into something unintended.
                // "clear" is always accepted: a latched strap is a trap, and the
                // only escape used to be the container CLI (`bantam-power set
                // MD_EDL 0`). A caller who can SET a strap through this tool must
                // be able to release it through the same tool.
                let allowed = ctx.config().boot_modes_for(d.display_name(), &d.canonical);
                let clearing = mode.eq_ignore_ascii_case("clear")
                    || mode.eq_ignore_ascii_case("none")
                    || mode.eq_ignore_ascii_case("normal");
                if !clearing && !allowed.iter().any(|m| m == mode) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        format!("{mode:?} is not a configured boot mode for {}",
                                d.display_name()),
                    )
                    .with_hint(
                        "every listed mode ENTERS something; pass \"clear\" to release every \
                         latched strap and return the board to a normal boot",
                    )
                    .with_detail(json!({
                        "boot_modes": allowed,
                        "also_accepted": ["clear", "none", "normal"],
                    })));
                }
                let hook = ctx
                    .config()
                    .boot_mode_hook_for_at(
                        d.display_name(),
                        &d.canonical,
                        d.by_path.as_deref(),
                        present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                    )
                    .ok_or_else(|| {
                        ToolError::new(
                            ErrorCode::HookNotConfigured,
                            format!("no boot_mode hook for {}", d.display_name()),
                        )
                    })?;
                // Normalised so a hook sees one spelling for "release everything".
                let mode: &str = if clearing { "clear" } else { mode };

                // The CONTROLLER decides how long its own hooks may take: a Bughopper
                // claims a USB interface and holds PM_RESIN_N for 6s (~35s wall),
                // which the 30s global default killed mid-action -- so `power off`
                // and `cycle` on that board always returned HOOK_TIMEOUT, sometimes
                // having actuated and sometimes not.
                let timeout = std::time::Duration::from_secs(
                    hook.power_timeout_s
                        .unwrap_or(ctx.config().hooks.power_timeout_s),
                );
                // HOOKS GET THE CANONICAL PATH, never the display name.
                // A nickname is how a HUMAN or an agent selects a device; it is
                // not a hardware identifier. Substituting it into `{device}` fed
                // "adp-ventuno" to a hook that resolves an FTDI by its by-id
                // path, which then matched four devices and failed DEVICE_GONE --
                // so naming a board permanently broke its power control.
                let name = d.canonical.clone();
                let controller = hook.controller.clone().unwrap_or_default();
                let args = [
                    ("mode", mode),
                    ("device", name.as_str()),
                    ("controller", controller.as_str()),
                ];

                // §F5: a strap is exactly the thing to check before setting it.
                // Latched straps persist across boots, so a wrong mode is a trap
                // that outlives the call that set it.
                if flag(a, "dry_run") {
                    return fresh(ctx, &d, json!({
                        "dry_run": true,
                        "mode": mode,
                        "hook": {
                            "command": hooks::render(&hook.template, &args),
                            "timeout_s": timeout.as_secs(),
                        },
                        "would_open_epochs": scope.consoles.iter()
                            .map(|c| json!({"device": c.display_name()}))
                            .collect::<Vec<_>>(),
                        "target": scope.target,
                        "lease_check": scope.lease_check(),
                        "note": scope.note,
                    }));
                }

                // Hook first, epoch after: a hook that failed must not leave an
                // epoch describing a boot nobody triggered. The epoch's START is
                // marked here all the same -- a Bughopper sequences the reset
                // itself, so the board is already talking when the hook returns.
                // Same exclusion as `power`: a strap change is an actuation of
                // the same board, and racing one against a power workflow is
                // the same hardware race.
                let _in_flight = ctx.begin_actuation("boot_mode", mode, &scope.consoles)?;
                let marks = stream_marks(ctx, &scope);
                let result = block_on(hooks::run(&hook.template, &args, timeout))?;

                let label = opt_s(a, "label").map(str::to_string)
                    .unwrap_or_else(|| format!("boot_mode {mode}"));
                let event = json!({"mode": mode, "hook": result});
                let opened = open_actuation_epochs(
                    ctx, &scope, "power", Some(&label), "boot_mode", &event, &marks,
                )?;
                let first = opened.first().cloned().unwrap_or(Value::Null);
                // IS THE BOARD IN THE MODE, OR MERELY ARMED FOR IT?
                //
                // The two controller kinds differ and the caller cannot guess.
                // A Bantam LATCHES a strap -- the board enters on its next boot,
                // so a reset is still owed and the strap must be cleared later.
                // A Bughopper sequences the reset itself while holding the
                // strap, so this call IS the entry and a reset afterwards boots
                // the board straight back OUT of it.
                //
                // Measured on the ADP: `boot_mode EDL` followed by the reset
                // that a Bantam needs left no QDL gadget at all, and read as
                // "EDL is broken on this board" when the second step had simply
                // undone the first.
                let immediate = ctx
                    .config()
                    .controller_for(&d.canonical)
                    .is_some_and(|c| c.mode_enters_immediately);
                let clearing = matches!(
                    mode.to_ascii_lowercase().as_str(),
                    "clear" | "none" | "normal"
                );
                let mut payload = json!({
                    "boot_id": first.get("boot_id").cloned().unwrap_or(Value::Null),
                    "seq": first.get("boot_seq").cloned().unwrap_or(Value::Null),
                    "mode": mode,
                    "hook": result,
                    "entered": immediate && !clearing,
                    "next": if clearing {
                        Value::Null
                    } else if immediate {
                        json!("this controller sequenced the entry itself: the board is in the \
                               mode NOW. Do not reset -- that boots it back out.")
                    } else {
                        json!("this controller latched a strap: reset (or power cycle) the board \
                               to enter the mode, and clear the strap when you are done.")
                    },
                });
                if let Some(o) = payload.as_object_mut() {
                    if scope.target.is_some() {
                        o.insert("target".into(), json!(scope.target));
                        o.insert("opened".into(), json!(opened));
                        o.insert("exempt_not_consoles".into(), json!(scope.exempt));
                    }
                    if let Some(n) = &scope.note {
                        o.insert("note".into(), json!(n));
                    }
                }
                // THE UART IS GONE BY DESIGN, SO SAY SO IN THIS VERY RESPONSE.
                //
                // The freshness envelope is derived from the store, and the
                // store still holds the prompt the board was sitting at plus a
                // `listening` capture claim -- the capture loop cannot know the
                // device vanished until it next fails to read it. So a call that
                // just sequenced the board into EDL answered with
                // `console.state=at_prompt, commandable=true`, while a
                // `diagnose` issued immediately afterwards correctly said
                // `edl=true, device_state=gone, not_listening`. Two answers
                // about one board, and the wrong one invites an agent to send a
                // command into a console that no longer exists.
                //
                // Not a guess: `immediate` means this controller sequenced the
                // entry itself, and EDL is precisely the state in which the
                // console re-enumerates away. The capture loop re-validates it
                // on its own deadline.
                // §W4: publish the new capture health; the envelope reads it
                // LIVE (Context::capture_health), so there is no row to patch.
                if immediate && !clearing && mode.eq_ignore_ascii_case("edl") {
                    let mut reg = ctx.registry();
                    let _ = conminer_core::live::publish_capture_state(
                        &mut reg,
                        d.id,
                        conminer_core::live::CaptureState::AwayInEdl,
                    );
                }
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "actuation_status",
            description: "What is actuating on this board right now, and how the last actuation \
                          ended. Read-only, no lease. A `power off` on a board found alive in EDL \
                          escalates (reset-then-off) and answers before that finishes; the board \
                          stays claimed until it is done and any power/boot_mode call meanwhile is \
                          refused with ACTUATION_IN_FLIGHT. Poll this until `in_flight` is null; \
                          `last` then carries the final effect and where the time went.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "target": {"type": "string", "description":
                        "A whole board. Mutually exclusive with `device`."},
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let target = opt_s(a, "target");
                let device = opt_s(a, "device");
                if target.is_some() && device.is_some() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgument,
                        "`device` and `target` are mutually exclusive",
                    ));
                }
                let consoles: Vec<DeviceRow> = match target {
                    Some(t) => conminer_core::target::console_members(&ctx.registry(), t)?.0,
                    None => vec![ctx.device_or_only(device)?],
                };
                let now = ctx.now();
                let mut in_flight = Value::Null;
                let mut last: Option<Value> = None;
                let rows: Vec<Value> = consoles
                    .iter()
                    .map(|c| {
                        let f = ctx.actuation_in_flight(c.id).map(|f| f.to_json(now));
                        let l = ctx.last_actuation_outcome(c.id);
                        if in_flight.is_null() {
                            if let Some(f) = &f {
                                in_flight = f.clone();
                            }
                        }
                        // The most recently finished one across the board.
                        if let Some(l) = &l {
                            let newer = match last.as_ref().and_then(|p| p["finished_ms"].as_i64())
                            {
                                None => true,
                                Some(p) => l["finished_ms"].as_i64().unwrap_or(0) > p,
                            };
                            if newer {
                                last = Some(l.clone());
                            }
                        }
                        json!({
                            "device": c.display_name(),
                            "in_flight": f,
                            "last": l,
                        })
                    })
                    .collect();
                let d = consoles[0].clone();
                fresh(ctx, &d, json!({
                    "target": target,
                    "board_free": in_flight.is_null(),
                    "in_flight": in_flight,
                    "last": last,
                    "consoles": rows,
                    "note": "in_flight and last are held by this mcpd process; a restart forgets \
                             them, and the epochs (list_boots) remain the durable record",
                }))
            },
        },
        Tool {
            name: "flash",
            description: "Drive the device's external flash hook, open a provisioning span, and \
                          bind the image identity to every epoch that follows — which is what \
                          makes 'first boot after flash' answerable.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["image"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "image": {"type": "string", "description": "Image reference passed to the hook."},
                    "git_sha": {"type": "string"},
                    "image_hash": {"type": "string"},
                    "name": {"type": "string"},
                    // §F5. The call body has ALWAYS honoured this: it plans, it
                    // reports a missing lease instead of refusing, and it
                    // returns the hook argv without running it or opening a
                    // provisioning span. Only the schema omitted it, and
                    // `additionalProperties: false` turns an undeclared argument
                    // into INVALID_ARGUMENT -- so the shared guidance promised a
                    // preview of the most destructive hook on the rig while the
                    // strict argument check rejected every attempt to use one,
                    // and the implementation behind it was unreachable.
                    "dry_run": {"type": "boolean", "description":
                        "Validate everything and show the exact hook argv WITHOUT running it, \
                         opening a provisioning span, or binding an image."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::hooks;
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                // §G6. A dry run PLANS; it never takes a lease. The missing
                // lease is reported below instead of refused, so asking "what
                // would this flash run" cannot bump another agent off a board.
                let planning = flag(a, "dry_run");
                let lease_check = match ctx
                    .registry()
                    .require_lease(d.id, &ctx.holder(), ctx.now())
                {
                    Ok(()) => json!("ok"),
                    Err(e) if !planning => return Err(e),
                    Err(_) => json!({
                        "missing": [d.display_name()],
                        "held": [],
                        "why": "this is a dry run, so the missing lease is reported rather than \
                                refused; the same call without dry_run would fail until it is \
                                acquired",
                    }),
                };
                let image = s(a, "image")?.to_string();
                let template = ctx
                    .config()
                    .devices
                    .get(d.display_name())
                    .and_then(|o| o.hooks.flash.clone())
                    .ok_or_else(|| {
                        ToolError::new(
                            ErrorCode::HookNotConfigured,
                            format!("no flash hook for {}", d.display_name()),
                        )
                    })?;
                let timeout = std::time::Duration::from_secs(ctx.config().hooks.flash_timeout_s);
                // HOOKS GET THE CANONICAL PATH, never the display name.
                // A nickname is how a HUMAN or an agent selects a device; it is
                // not a hardware identifier. Substituting it into `{device}` fed
                // "adp-ventuno" to a hook that resolves an FTDI by its by-id
                // path, which then matched four devices and failed DEVICE_GONE --
                // so naming a board permanently broke its power control.
                let name = d.canonical.clone();
                // §F5. The highest-consequence hook on the rig gets the same
                // "show me what you would run" affordance as the others.
                if flag(a, "dry_run") {
                    return fresh(ctx, &d, json!({
                        "dry_run": true,
                        "image": image,
                        "hook": {
                            "command": hooks::render(
                                &template,
                                &[("image", image.as_str()), ("device", name.as_str())],
                            ),
                            "timeout_s": timeout.as_secs(),
                        },
                        "lease_check": lease_check,
                    }));
                }
                let result = block_on(hooks::run(
                    &template,
                    &[("image", &image), ("device", &name)],
                    timeout,
                ))?;

                let now = ctx.now();
                let meta = json!({
                    "name": opt_s(a, "name").unwrap_or(&image),
                    "git_sha": opt_s(a, "git_sha"),
                    "image_hash": opt_s(a, "image_hash"),
                });
                let payload = ctx.with_store(&d, |st| {
                    let image_id = st.bind_image(
                        opt_s(a, "name").or(Some(&image)),
                        opt_s(a, "git_sha"),
                        opt_s(a, "image_hash"),
                        "flash_hook",
                        &meta,
                        now,
                    )?;
                    let session = st.latest_session()?.map(|s| s.id);
                    let boot = st.open_boot("flash", Some(&image), now, session)?;
                    st.set_boot_image(boot.id, image_id)?;
                    st.append_event(session, Some(boot.id), now, "flash", &json!({
                        "image": image, "image_id": image_id, "hook": result,
                    }))?;
                    Ok(json!({
                        "image_id": image_id,
                        "boot_id": boot.id,
                        "cursor": st.head_cursor().encode(),
                        "hook": result,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "set_image",
            description: "Bind a build identity to the current and subsequent epochs without \
                          flashing, so diff_builds can aggregate across every boot of a build.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "git_sha": {"type": "string"},
                    "image_hash": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                if opt_s(a, "name").is_none()
                    && opt_s(a, "git_sha").is_none()
                    && opt_s(a, "image_hash").is_none()
                {
                    return Err(ToolError::invalid_arg(
                        "give at least one of name, git_sha or image_hash",
                    ));
                }
                let now = ctx.now();
                let meta = json!({
                    "name": opt_s(a, "name"),
                    "git_sha": opt_s(a, "git_sha"),
                    "image_hash": opt_s(a, "image_hash"),
                });
                let payload = ctx.with_store(&d, |st| {
                    let id = st.bind_image(
                        opt_s(a, "name"), opt_s(a, "git_sha"), opt_s(a, "image_hash"),
                        "explicit", &meta, now,
                    )?;
                    if let Some(b) = st.latest_boot()? {
                        st.set_boot_image(b.id, id)?;
                    }
                    Ok(json!({"image_id": id}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "diff_builds",
            description: "Aggregate templates across every epoch of two builds and report what is \
                          new, gone or count-shifted. This is the regression question — 'build A \
                          versus build B' — rather than the positional one.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["a", "b"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "a": {"type": "string", "description": "Image name, git sha or hash."},
                    "b": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 100, 1000);
                let payload = crate::report::diff_builds(ctx, &d, s(a, "a")?, s(a, "b")?, limit)?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "set_line",
            description: "Change UART line settings. RFC2217 lets any consumer renegotiate the \
                          line and that affects everyone attached, so the documented contract is \
                          to change it here: the registry is the source of truth and the change is \
                          recorded as a line-config event in the device's timeline.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "baud": {"type": "integer", "minimum": 1, "description":
                        "A standard rate. Anything else is refused: a nonsense baud is accepted \
                         silently by the tty layer and turns a working console into garbage, \
                         which reads as a dead board."},
                    "data_bits": {"type": "integer", "minimum": 5, "maximum": 8},
                    "parity": {"type": "string", "enum": ["none","even","odd","mark","space"]},
                    "stop_bits": {"type": "integer", "minimum": 1, "maximum": 2},
                    "flow": {"type": "string", "enum": ["none","rtscts","xonxoff"]},
                    "persist": {"type": "boolean", "default": false, "description":
                        "Update the registry. Otherwise the change reverts on the next session."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let mut line = if d.line == conminer_core::config::LineConfig::default() {
                    ctx.config().line_for(d.display_name())
                } else {
                    d.line.clone()
                };
                if let Some(v) = opt_i(a, "baud") {
                    // Validate against the real rates. `minimum: 1` let baud 7
                    // through, and the port dutifully reported "7 8N1" -- a
                    // console set to an impossible rate produces garbage, which
                    // looks exactly like a board that has stopped talking.
                    const RATES: &[i64] = &[
                        1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800,
                        500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 3000000,
                    ];
                    if !RATES.contains(&v) {
                        return Err(ToolError::invalid_arg(format!(
                            "{v} is not a standard baud rate"
                        ))
                        .with_hint("a non-standard rate is silently accepted by the tty and \
                                    produces garbage that reads as a dead board")
                        .with_detail(json!({"accepted": RATES})));
                    }
                    line.baud = v as u32;
                }
                if let Some(v) = opt_i(a, "data_bits") { line.data_bits = v as u8; }
                if let Some(v) = opt_i(a, "stop_bits") { line.stop_bits = v as u8; }
                if let Some(v) = opt_s(a, "parity") {
                    line.parity = serde_json::from_value(json!(v))
                        .map_err(|e| ToolError::invalid_arg(format!("bad parity: {e}")))?;
                }
                if let Some(v) = opt_s(a, "flow") {
                    line.flow = serde_json::from_value(json!(v))
                        .map_err(|e| ToolError::invalid_arg(format!("bad flow: {e}")))?;
                }
                let persist = flag(a, "persist");
                if persist {
                    ctx.registry().set_line(d.id, &line)?;
                }
                // Recorded on the timeline so garbage before or after a change is
                // attributable to it (§3.2).
                let now = ctx.now();
                let summary = line.summary();
                ctx.with_store(&d, |st| {
                    st.append_event(None, None, now, "line_config", &json!({
                        "line": summary, "persist": persist,
                    }))?;
                    Ok(())
                })?;
                fresh(ctx, &d, json!({"line": summary, "persisted": persist}))
            },
        },
        Tool {
            name: "start_session",
            description: "Open an explicit session boundary for a test run, so its templates and \
                          counts can be scoped and diffed against another run.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let id = st.begin_session(
                        conminer_core::store::SessionSource::Live, now,
                        opt_s(a, "label"), None, None,
                    )?;
                    Ok(json!({"session_id": id, "cursor": st.head_cursor().encode()}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "end_session",
            description: "Close a session boundary, so its line, record and template counts are \
                          final and it can be diffed against another run.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["session"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let id = i(a, "session")?;
                let now = ctx.now();
                ctx.with_store(&d, |st| st.end_session(id, now))?;
                fresh(ctx, &d, json!({"session_id": id, "ended": true}))
            },
        },
        Tool {
            name: "claim_exclusive",
            description: "Hand the port to a binary protocol (Sahara, fastboot, zmodem, a GDB \
                          stub). Capture keeps running — nothing is ever lost — but line framing \
                          suspends and the span is marked binary instead of being mis-mined as \
                          garbage.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "protocol": {"type": "string"},
                    "release": {"type": "boolean", "default": false}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let now = ctx.now();
                if flag(a, "release") {
                    ctx.registry().release_exclusive(d.id)?;
                    ctx.with_store(&d, |st| {
                        st.append_event(None, None, now, "exclusive_release", &json!({}))?;
                        Ok(())
                    })?;
                    return fresh(ctx, &d, json!({"claimed": false}));
                }
                let holder = ctx.holder();
                ctx.registry().claim_exclusive(d.id, &holder, opt_s(a, "protocol"), now)?;
                ctx.with_store(&d, |st| {
                    st.append_event(None, None, now, "exclusive_claim", &json!({
                        "protocol": opt_s(a, "protocol"),
                    }))?;
                    Ok(())
                })?;
                fresh(ctx, &d, json!({"claimed": true, "protocol": opt_s(a, "protocol")}))
            },
        },
        Tool {
            name: "evaluate_policy",
            description: "Turn the miner into a regression gate: judge a session against an \
                          allowlist of known templates and fingerprints, a severity threshold, and \
                          an optional fail-only-on-novel-crash mode. Returns a machine verdict and \
                          a human summary.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer"},
                    "allow_templates": {"type": "array", "items": {"type": "integer"},
                        "description": "Template ids that are known and accepted (flaky issues)."},
                    "allow_fingerprints": {"type": "array", "items": {"type": "string"}},
                    "fail_at_or_above": {"type": "string",
                        "enum": ["emerg","alert","crit","err","warn","notice","info","debug"],
                        "default": "err"},
                    "novel_only": {"type": "boolean", "default": false, "description":
                        "Fail only on templates never seen before, not on known-bad ones."},
                    "use_verdicts": {"type": "boolean", "default": true, "description":
                        "Waive templates already annotated benign via annotate_template, so the \
                         allowlist lives in the store instead of in every call."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = crate::report::evaluate_policy(ctx, &d, a)?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "console_state",
            description: "What the console is perceived to be doing right now: no_signal, garbage, \
                          booting, at_prompt, login_wait, at_unknown_prompt, streaming, hung, \
                          boot_looping (with its loop class), unstable, or unknown when there is no \
                          capture attestation to justify a claim.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {"device": {"type": "string", "description": DEVICE_ARG}},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let state = console_state(ctx, &d)?;
                fresh(ctx, &d, json!({"console": state}))
            },
        },
        Tool {
            name: "list_targets",
            description: "Logical targets: several consoles belonging to one device under test \
                          (AP plus EC, BMC plus host, secure plus normal world).",
            mutating: false,
            schema: || json!({"type": "object", "properties": {}, "additionalProperties": false}),
            call: |ctx, _a| {
                let targets = conminer_core::target::list(&ctx.registry())?;
                Ok(json!({"targets": targets, "count": targets.len()}))
            },
        },
        Tool {
            name: "name_target",
            description: "Name a board's console group. A target is derived from the USB hub its \
                          consoles share, so it arrives called \"2.1\" -- exact and unmemorable. \
                          Naming it makes `target: \"uno-q\"` work everywhere `2.1` did, for \
                          `power`, `target_mark` and `target_context` alike. Omit `name` to fall \
                          back to the topology.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": {"type": "string", "description":
                        "the target as it is called now -- its topology name (\"2.1\") or a name \
                         given earlier"},
                    "name": {"type": "string", "description":
                        "the new name. Omit to clear it and let the USB topology name the group \
                         again."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let target = s(a, "target")?;
                let name = opt_s(a, "name");
                let mut reg = ctx.registry();
                let members = conminer_core::target::rename(&mut reg, target, name)?;
                // What it is called NOW, so a caller can use the answer
                // directly. Clearing hands the group back to topology -- and on
                // a console with no USB topology to hand it back to (an ingested
                // file, a by-path row) there is no target left at all, which is
                // a thing to say rather than to paper over with the old name.
                let now = conminer_core::target::list(&reg)?
                    .into_iter()
                    .find(|t| t.members.iter().any(|m| members.contains(m)))
                    .map(|t| t.name);
                let note = if name.is_some() || now.is_some() {
                    Value::Null
                } else {
                    json!("these consoles have no USB topology to fall back on, so they now \
                           belong to no target; name one to group them again")
                };
                Ok(json!({"target": now, "members": members, "was": target, "note": note}))
            },
        },
        Tool {
            name: "target_context",
            description: "Interleave every console of a target around a moment in host time — \
                          which is what answers 'what did the EC see when the AP panicked'. \
                          Ordering uses host receipt time, because two boards do not share a clock.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["target", "around_ts"],
                "properties": {
                    "target": {"type": "string"},
                    "around_ts": {"type": "integer", "description":
                        "Host wall timestamp (ms), typically from the record you are investigating."},
                    "window_ms": {"type": "integer", "minimum": 1, "default": 5000},
                    "per_device": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let window = opt_i(a, "window_ms").unwrap_or(5_000).max(1);
                let per = capped(a, "per_device", 100, ctx.config().api.max_raw_lines.max(100));
                let lines = conminer_core::target::interleave(
                    &ctx.registry(), ctx.data_dir(), s(a, "target")?,
                    i(a, "around_ts")?, window, per,
                )?;
                Ok(json!({
                    "target": s(a, "target")?,
                    "lines": lines,
                    "count": lines.len(),
                    "capped": lines.len() >= per,
                }))
            },
        },
        Tool {
            name: "target_mark",
            description: "Open a boot epoch on every console of a target at once. A power event \
                          affects the whole DUT, so leaving one console on its old epoch would \
                          make the consoles incomparable exactly when correlating them matters.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": {"type": "string"},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let target = s(a, "target")?;
                let holder = ctx.holder();
                let now = ctx.now();
                // Collect first: holding the registry guard across the loop body
                // would deadlock, because checking the lease re-locks it.
                // Consoles only: the target's controller is `ignored`, captures
                // nothing, and cannot meaningfully be leased.
                let (members, _skipped) =
                    conminer_core::target::console_members(&ctx.registry(), target)?;
                for d in &members {
                    ctx.registry().require_lease(d.id, &holder, now)?;
                }
                conminer_core::target::open_epoch_on_all(
                    &ctx.registry(), ctx.data_dir(), target, "mark", opt_s(a, "label"), now,
                )
            },
        },
        Tool {
            name: "push_file",
            description: "Move a file onto a console-only board. Chunked so it cannot overrun the \
                          target's tty input buffer, and verified by the *target's* own checksum — \
                          a transfer only the host believes in has not been verified.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["local_path", "remote_path"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "local_path": {"type": "string"},
                    "remote_path": {"type": "string"},
                    "strategy": {"type": "string", "enum": ["base64", "zmodem", "loady"],
                        "default": "base64"},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 1048576}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{BrokeredTransport, CommandOptions, Runner};
                use conminer_core::transfer;
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let strategy = transfer::Strategy::parse(opt_s(a, "strategy").unwrap_or("base64"))?;
                if strategy != transfer::Strategy::Base64 {
                    return Err(ToolError::new(
                        ErrorCode::Unsupported,
                        format!("the {} strategy needs lrzsz on the target and an exclusive claim",
                                strategy.as_str()),
                    )
                    .with_hint("use strategy=base64, which works on any Unix userspace"));
                }
                let data = std::fs::read(s(a, "local_path")?)?;
                let max = opt_i(a, "max_bytes").unwrap_or(1_048_576).max(1) as u64;
                let plan = transfer::plan_push(&data, s(a, "remote_path")?, max)?;

                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let prompts = prompts_for(ctx, &d)?;
                let mut opts = CommandOptions::new(
                    ctx.config().runner_for(d.display_name()),
                    ctx.config().line_for(d.display_name()),
                );
                opts.echo = false; // long base64 lines: pace, do not verify per char
                let commands = plan.commands.clone();
                let verify = plan.verify_command.clone();

                let checksum = block_on(async move {
                    for c in &commands {
                        let io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                        let t = Runner::new(io, prompts.clone(), opts.clone()).at(&endpoint).run(c).await?;
                        if let Some(e) = t.as_error() {
                            return Err(e);
                        }
                    }
                    let io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                    let t = Runner::new(io, prompts, opts).at(&endpoint).run(&verify).await?;
                    Ok(t.output)
                })?;
                transfer::verify_push(&plan, &checksum)?;

                fresh(ctx, &d, json!({
                    "pushed": plan.remote_path,
                    "bytes": plan.bytes,
                    "sha256": plan.sha256,
                    "chunks": plan.chunks,
                    "strategy": plan.strategy,
                    "verified_by_target": true,
                }))
            },
        },
        Tool {
            name: "pull_file",
            description: "Read a file off a console-only board, verifying it against the \
                          checksum the target itself reports before returning any bytes.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["remote_path", "local_path"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "remote_path": {"type": "string"},
                    "local_path": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{BrokeredTransport, CommandOptions, Runner};
                use conminer_core::transfer;
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let cmds = transfer::plan_pull(s(a, "remote_path")?)?;
                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let prompts = prompts_for(ctx, &d)?;
                let mut opts = CommandOptions::new(
                    ctx.config().runner_for(d.display_name()),
                    ctx.config().line_for(d.display_name()),
                );
                opts.max_output_bytes = 4 * 1024 * 1024;

                let outputs = block_on(async move {
                    let mut out = Vec::new();
                    for c in &cmds {
                        let io = BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                        let t = Runner::new(io, prompts.clone(), opts.clone()).at(&endpoint).run(c).await?;
                        if let Some(e) = t.as_error() {
                            return Err(e);
                        }
                        out.push(t.output);
                    }
                    Ok(out)
                })?;
                let data = transfer::finish_pull(&outputs[0], &outputs[1])?;
                let local = s(a, "local_path")?;
                std::fs::write(local, &data)?;
                fresh(ctx, &d, json!({
                    "pulled": s(a, "remote_path")?,
                    "local_path": local,
                    "bytes": data.len(),
                    "verified_by_target": true,
                }))
            },
        },
        Tool {
            name: "ingest_pstore",
            description: "Ingest a kernel pstore/ramoops dump. The console can miss a crash the \
                          kernel preserved; this mines it with the ordinary linux profile and \
                          tags the session so it is distinguishable from live capture.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "path": {"type": "string", "description":
                        "A file from /sys/fs/pstore, as visible inside the container."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                let path = shared_path(s(a, "path")?);
                let raw = std::fs::read(&path)?;
                let body = conminer_core::codec::pstore_body(&raw);
                let tmp = ctx.data_dir().join(".pstore-ingest.tmp");
                std::fs::write(&tmp, &body)?;

                ctx.forget_store(d.id);
                let store = conminer_core::store::DeviceStore::open(
                    &ctx.data_dir().join(&d.db_file),
                    &d.canonical,
                    ctx.config().fts_for(d.display_name()),
                )?;
                let mut pipe = conminer_core::pipeline::Pipeline::new(
                    store, ctx.profiles().clone(), ctx.config().clone(),
                    d.display_name(), Some("linux"), ctx.clock().clone(),
                )?;
                let mut opts = conminer_core::ingest::IngestOptions::from_config(ctx.config());
                opts.source = conminer_core::store::SessionSource::Pstore;
                opts.label = Some(format!("pstore {}", path.display()));
                let report = conminer_core::ingest::ingest_file(&mut pipe, &tmp, &opts)?;
                drop(pipe);
                let _ = std::fs::remove_file(&tmp);
                fresh(ctx, &d, json!({"ingest": report, "source": "pstore"}))
            },
        },
        Tool {
            name: "symbolize",
            description: "Annotate a crash record's raw addresses with symbol names, using a \
                          System.map or nm output supplied for the running image. Best-effort and \
                          stored as a derived annotation — the raw record is never modified.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["record_id", "symbols"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "record_id": {"type": "integer"},
                    "symbols": {"type": "string", "description":
                        "Path to a System.map / `nm -n` file for the image that produced the crash."},
                    "offset": {"type": "integer", "default": 0, "description":
                        "Relocation base to subtract before lookup (KASLR)."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device_or_only(opt_s(a, "device"))?;
                let record_id = i(a, "record_id")?;
                let table = conminer_core::symbolize::SymbolTable::load(
                    std::path::Path::new(s(a, "symbols")?),
                )?;
                let base = opt_i(a, "offset").unwrap_or(0) as u64;
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let text = st.record_text(record_id)?;
                    let resolved = table.annotate(&text, base);
                    // Derived annotation, stored beside the record. The raw
                    // bytes stay exactly as captured (§6).
                    st.annotate(record_id, "symbolize", &json!({"frames": resolved}), now)?;
                    Ok(json!({
                        "record_id": record_id,
                        "frames": resolved,
                        "symbols_loaded": table.len(),
                        "best_effort": true,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "classify_prompt",
            description: "Teach the server what an unfamiliar idle line is: a shell, a bootloader, \
                          an RTOS shell, a monitor, a credential gate, or noise to ignore. Each \
                          unfamiliar console is then a one-time teaching event.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["device", "pattern", "kind"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "pattern": {"type": "string", "description": "Regex matching the idle line."},
                    "kind": {"type": "string",
                        "enum": ["shell","bootloader","rtos_shell","monitor","credential_gate","ignore"]},
                    "stage": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = ctx.device(s(a, "device")?)?;
                let pattern = s(a, "pattern")?;
                let kind = s(a, "kind")?;
                conminer_core::framer::profile::PromptKind::parse(kind)
                    .ok_or_else(|| ToolError::invalid_arg(format!("unknown prompt kind {kind:?}")))?;
                regex::Regex::new(pattern)
                    .map_err(|e| ToolError::invalid_arg(format!("invalid prompt regex: {e}")))?;
                // A prompt that also matches ordinary status output would make
                // every `follow(prompt:true)` fire early (LAVA's lesson, §8.3).
                crate::report::validate_prompt_distinctiveness(pattern)?;
                let now = ctx.now();
                ctx.with_store(&d, |st| {
                    st.learn_prompt(pattern, kind, "learned", opt_s(a, "stage"), now)
                })?;
                Ok(json!({"device": d.display_name(), "pattern": pattern, "kind": kind}))
            },
        },
        // ------------------------------------------------------ triage memory --
        Tool {
            name: "annotate_template",
            description: "Record a standing verdict on a template so it is not re-triaged every \
                          session: benign (hidden from the table of contents and waived by the \
                          regression gate), known_bad, investigating, or interesting. Pass \
                          verdict=null to clear it.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["template_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "template_id": {"type": "integer"},
                    "verdict": {"type": ["string","null"],
                        "enum": ["benign","known_bad","investigating","interesting", null],
                        "description": "null clears the verdict."},
                    "note": {"type": "string", "description":
                        "Why. This is what a later session reads instead of re-deriving it."},
                    "ticket": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let id = i(a, "template_id")?;
                let verdict = match a.get("verdict") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(Verdict::parse(v.as_str().ok_or_else(|| {
                        ToolError::invalid_arg("verdict must be a string or null")
                    })?)?),
                };
                let author = ctx.holder();
                let now = ctx.now();
                let row = ctx.with_store(&d, |st| {
                    st.set_verdict(id, verdict, opt_s(a, "note"), opt_s(a, "ticket"),
                                   Some(&author), now)?;
                    st.verdict(id)
                })?;
                Ok(json!({
                    "device": d.display_name(),
                    "template_id": id,
                    "verdict": row,
                    "cleared": verdict.is_none(),
                }))
            },
        },
        Tool {
            name: "list_verdicts",
            description: "Every standing verdict on this device: what a previous session already \
                          decided about each template, so triage resumes instead of restarting.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "verdict": {"type": "string",
                        "enum": ["benign","known_bad","investigating","interesting"]}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let only = match opt_s(a, "verdict") {
                    Some(v) => Some(Verdict::parse(v)?),
                    None => None,
                };
                let rows = ctx.with_store(&d, |st| {
                    let vs = st.verdicts(only)?;
                    // The template text is what makes a verdict list readable;
                    // an id-only list would force one call per row to use it.
                    let mut out = Vec::new();
                    for v in vs {
                        let text = st.template(v.template_id).map(|t| t.text).unwrap_or_default();
                        out.push(json!({"verdict": v, "text": text}));
                    }
                    Ok(out)
                })?;
                Ok(json!({"device": d.display_name(), "verdicts": rows, "count": rows.len()}))
            },
        },
        // ------------------------------------------------- values in the slots --
        Tool {
            name: "template_values",
            description: "The numbers inside a template's <*> slots, across every occurrence: min, \
                          max, mean, first/last and the distinct values. This is how you read a \
                          measurement (time-to-prompt, MemTotal, an errno) as a series instead of \
                          re-reading the lines.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["template_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "template_id": {"type": "integer"},
                    "slot": {"type": "integer", "minimum": 0, "description":
                        "Token index of one wildcard. Omit for every slot."},
                    "session": {"type": "integer"},
                    "boot": {"type": "integer"},
                    "max_records": {"type": "integer", "minimum": 1, "maximum": 100000,
                        "default": 2000},
                    "samples": {"type": "integer", "minimum": 0, "maximum": 500, "default": 20,
                        "description": "Individual values returned per slot, newest last."}
                },
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50,
                        "description":
                        "Cap each list in the answer; measured at 5KB unbounded. Each capped list \
                         reports how many entries it omitted."},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let profiles = ctx.profiles().clone();
                let payload = ctx.with_store(&d, |st| {
                    conminer_core::values::template_values(
                        st,
                        &profiles,
                        i(a, "template_id")?,
                        opt_i(a, "slot").map(|v| v as usize),
                        opt_i(a, "session"),
                        opt_i(a, "boot"),
                        capped(a, "max_records", 2000, 100_000),
                        capped(a, "samples", 20, 500),
                    )
                })?;
                fresh(ctx, &d, cap_lists(payload, capped(a, "limit", 50, 1000)))
            },
        },
        // ----------------------------------------------------------- baselines --
        Tool {
            name: "set_baseline",
            description: "Bless an epoch as the reference point for 'what is new'. Later calls to \
                          list_templates(vs_baseline=true) and boot_report then diff against the \
                          last boot that actually worked, not merely against this session.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boot": {"type": "integer", "description": "Epoch id; defaults to the latest."},
                    "name": {"type": "string", "default": "default", "description":
                        "Several baselines can coexist, e.g. one per image."},
                    "note": {"type": "string"},
                    "clear": {"type": "boolean", "description": "Remove this baseline instead."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = opt_s(a, "name").unwrap_or("default");
                let now = ctx.now();
                if flag(a, "clear") {
                    let gone = ctx.with_store(&d, |st| st.clear_baseline(name))?;
                    return Ok(json!({"device": d.display_name(), "name": name, "cleared": gone}));
                }
                let payload = ctx.with_store(&d, |st| {
                    let boot = match opt_i(a, "boot") {
                        Some(b) => st.boot(b)?,
                        None => st.latest_boot()?.ok_or_else(|| {
                            ToolError::new(ErrorCode::UnknownBoot, "no epochs recorded yet")
                        })?,
                    };
                    st.set_baseline(name, boot.id, opt_s(a, "note"), now)?;
                    Ok(json!({
                        "name": name,
                        "boot_id": boot.id,
                        "boot_seq": boot.seq,
                        "fingerprint": boot.fingerprint,
                        "templates": st.templates_in_boot(boot.id)?.len(),
                    }))
                })?;
                Ok(json!({"device": d.display_name(), "baseline": payload}))
            },
        },
        Tool {
            name: "list_baselines",
            description: "Blessed reference epochs on this device, with what each one contained.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {"device": {"type": "string", "description": DEVICE_ARG}},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let rows = ctx.with_store(&d, |st| {
                    let mut out = Vec::new();
                    for b in st.baselines()? {
                        let boot = st.boot(b.boot_id).ok();
                        out.push(json!({
                            "baseline": b,
                            "boot_seq": boot.as_ref().map(|x| x.seq),
                            "fingerprint": boot.as_ref().and_then(|x| x.fingerprint.clone()),
                            "outcome": boot.as_ref().and_then(|x| x.outcome.clone()),
                            "templates": st.templates_in_boot(b.boot_id)?.len(),
                        }));
                    }
                    Ok(out)
                })?;
                Ok(json!({"device": d.display_name(), "baselines": rows, "count": rows.len()}))
            },
        },
        Tool {
            name: "diff_boots",
            description: "Two epochs side by side: templates new in B, gone from B, count-shifted, \
                          and — unlike a template diff — how the stage timings moved. 'It still \
                          boots but handoff is 400 ms slower' is only visible here.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["a", "b"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "a": {"type": "integer", "description": "Baseline epoch id."},
                    "b": {"type": "integer", "description": "Epoch to judge."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 50, ctx.config().api.max_results.max(50));
                // Cap EVERY list, not only the one the report's own limit
                // covers: measured at 9KB with the limit already in place,
                // because a diff answers with several parallel lists at once.
                crate::report::diff_boots(ctx, &d, i(a, "a")?, i(a, "b")?, limit)
                    .map(|p| cap_lists(p, limit))
            },
        },
        // ------------------------------------------------------ durable watches --
        Tool {
            name: "create_watch",
            description: "A follow() predicate that keeps firing while you are disconnected. \
                          Every firing is recorded against the stored stream with its own \
                          timestamp, so an overnight soak is readable after the fact instead of \
                          requiring an agent to sit on a long poll.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name", "until"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string", "description":
                        "Stable handle for poll_watch. Re-creating a name replaces it."},
                    "until": {"type": "object", "description":
                        "The same predicate follow() takes: {pattern}, {template:\"new\"}, \
                         {stage}, {prompt:true}, {quiet:<ms>}, {reset:true} or {any:[…]}."},
                    "from": {"type": "string", "enum": ["now","start"], "default": "now",
                        "description": "Watch from here on, or replay the whole stored stream."},
                    "notify": {"type": "object", "description":
                        "§K4. PUSH firings instead of waiting to be polled -- for an overnight \
                         soak with no agent attached. {url, secret?, min_interval_s?}. Firings \
                         inside the window coalesce into ONE post (a flapping board fires every \
                         few seconds; one post each is a DoS on the receiver). With `secret`, \
                         each post carries X-Conminer-Signature: sha256=<hmac of the body>. \
                         Delivery is best-effort: poll_watch stays the source of truth and a \
                         firing is never consumed by a post that was not acknowledged. The URL \
                         must match `[notify] allow` in conminer.toml. NOTE: the URL is resolved \
                         from INSIDE the conminer container, so 127.0.0.1 is conminer itself -- a \
                         receiver on the lab host needs that host's LAN address or a compose \
                         service name.",
                        "properties": {
                            "url": {"type": "string"},
                            "secret": {"type": "string"},
                            "min_interval_s": {"type": "integer", "minimum": 1, "maximum": 3600,
                                "default": 60}
                        },
                        "required": ["url"],
                        "additionalProperties": false}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let until = a.get("until").ok_or_else(|| {
                    ToolError::invalid_arg("until is required")
                })?;
                // Parsed now so a malformed predicate fails at creation, not at
                // 3am when the agent comes back for the results.
                Predicate::parse(until)?;
                let replay = opt_s(a, "from") == Some("start");
                // §K4. The allowlist is checked HERE, before anything is
                // stored: a watch that will refuse to post at 3am is worse than
                // one that refuses to be created now, while somebody is reading
                // the error.
                let notify = a.get("notify").cloned();
                if let Some(n) = &notify {
                    let url = n["url"].as_str().ok_or_else(|| {
                        ToolError::invalid_arg("notify.url is required when notify is given")
                    })?;
                    crate::push::check_allowed(&ctx.config().notify, url)?;
                }
                // §M2. Allowed is not the same as reachable. A loopback URL
                // passes the allowlist and then silently never delivers, which
                // looks exactly like broken plumbing three hours later.
                let loopback_note = notify
                    .as_ref()
                    .and_then(|n| n["url"].as_str())
                    .filter(|u| crate::push::is_container_loopback(u))
                    .map(|_| crate::push::LOOPBACK_NOTE);
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let from = if replay { 0 } else { st.stream_offset() };
                    // §F6. A watch cannot BE another watch: the scanner would
                    // need a defined order between them and one event would be
                    // recorded twice. `follow {until:{watch}}` is how you wait
                    // on a watch; this is how you make one.
                    if matches!(
                        conminer_core::follow::Predicate::parse(until)?,
                        conminer_core::follow::Predicate::Watch(_)
                    ) {
                        return Err(ToolError::invalid_arg(
                            "a watch cannot watch another watch; use follow {until:{watch:<name>}} \
                             to park until one fires",
                        ));
                    }
                    let id = st.create_watch(name, until, from, now)?;
                    if let Some(n) = &notify {
                        st.set_watch_notify(
                            name,
                            n["url"].as_str(),
                            n["secret"].as_str(),
                            n["min_interval_s"].as_i64().unwrap_or(60).clamp(1, 3600),
                        )?;
                    }
                    let mut w = json!({"watch_id": id, "name": name, "from_offset": from,
                                       "until": until, "replaying": replay});
                    if let (Some(o), Some(note)) = (w.as_object_mut(), loopback_note) {
                        o.insert("note".into(), json!(note));
                    }
                    Ok(w)
                })?;
                // AFTER the store write has committed, not inside it.
                //
                // Invalidating from inside the transaction opened a race with
                // the delivery sweep: the sweep could rebuild its armed-device
                // cache from the pre-commit state, conclude this device has no
                // pushing watch, and then trust that for the whole TTL. Measured
                // as "the watch fired, and 130 seconds later there had been no
                // delivery ATTEMPT at all" -- which reads as dead plumbing
                // rather than as a stale cache.
                if notify.is_some() {
                    crate::push::invalidate_armed_cache();
                }
                Ok(json!({"device": d.display_name(), "watch": payload}))
            },
        },
        Tool {
            name: "poll_watch",
            description: "Everything a watch caught since you last asked, oldest first, each with \
                          its own timestamp and offset. A consuming read: a firing is delivered \
                          once.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "max_hits": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50},
                    "peek": {"type": "boolean", "default": false, "description":
                        "Read without consuming, so the same hits come back next call."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let max_hits = capped(a, "max_hits", 50, 1000);
                let peek = flag(a, "peek");
                let prompts = prompt_set(ctx, &d)?;
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let w = st.watch(name)?;
                    // Scan first, then read: the durable part is that a firing is
                    // written down before it is reported, so a crash between the
                    // two re-delivers rather than loses it.
                    let (hits, scanned_to) = conminer_core::follow::scan_hits(
                        st, w.scanned_to, &Predicate::parse(&w.predicate)?,
                        max_hits.max(1000), &prompts, now,
                    )?;
                    st.record_watch_hits(w.id, &hits, scanned_to, now)?;
                    let (out, remaining) = st.take_watch_hits(w.id, max_hits, !peek)?;
                    Ok(json!({
                        "watch": {"name": w.name, "until": w.predicate, "created_at": w.created_at},
                        "hits": out,
                        "returned": out.len(),
                        "remaining": remaining,
                        "scanned_to": scanned_to,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "list_watches",
            description: "Watches on this device and how many firings are waiting to be read.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {"device": {"type": "string", "description": DEVICE_ARG}},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let rows = ctx.with_store(&d, |st| {
                    let mut out = Vec::new();
                    for w in st.watches()? {
                        let (_, pending) = st.take_watch_hits(w.id, 0, false)?;
                        // §F6. `pending` is "anything to read?"; `fired_total`
                        // is "did anything happen at all?" -- the cheap
                        // morning-after question after an overnight soak.
                        let fired_total = st.watch_fired_total(w.id)?;
                        out.push(json!({"watch": w, "pending": pending,
                                        "fired_total": fired_total}));
                    }
                    Ok(out)
                })?;
                Ok(json!({"device": d.display_name(), "watches": rows, "count": rows.len()}))
            },
        },
        Tool {
            name: "delete_watch",
            description: "Remove a watch and any firings it has not delivered.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let gone = ctx.with_store(&d, |st| st.delete_watch(name))?;
                Ok(json!({"device": d.display_name(), "name": name, "deleted": gone}))
            },
        },
        // --------------------------------------------------- the timeline spine --
        Tool {
            name: "attach_evidence",
            description: "Attach evidence from another tool — a JTAG halt, a rail measurement, a \
                          flash log, a CI result — to this device's timeline, landing on whichever \
                          boot epoch covers its timestamp. Silicon answers usually live in the \
                          join between the console and something else.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["source", "data"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "source": {"type": "string", "description":
                        "Where it came from, e.g. jtag, power, flash, ci, note."},
                    "data": {"description": "Any JSON payload. Stored verbatim."},
                    "at": {"type": "integer", "description":
                        "Unix ms. Defaults to now. The epoch is chosen from this."},
                    "boot": {"type": "integer", "description":
                        "Pin the epoch explicitly instead of deriving it from `at`."},
                    "summary": {"type": "string", "description":
                        "One line for the timeline view."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let source = s(a, "source")?;
                let at = opt_i(a, "at").unwrap_or_else(|| ctx.now());
                let payload = a.get("data").cloned().unwrap_or(Value::Null);
                let summary = opt_s(a, "summary").map(str::to_string);
                let out = ctx.with_store(&d, |st| {
                    // Derive the epoch from the timestamp so an external tool
                    // does not have to know conminer's epoch numbering.
                    let boot = match opt_i(a, "boot") {
                        Some(b) => Some(st.boot(b)?.id),
                        None => st.boot_at(at)?.map(|b| b.id),
                    };
                    let id = st.append_event(
                        None, boot, at,
                        &format!("evidence:{source}"),
                        &json!({"summary": summary, "data": payload}),
                    )?;
                    Ok(json!({"evidence_id": id, "boot_id": boot, "at": at, "source": source}))
                })?;
                Ok(json!({"device": d.display_name(), "attached": out}))
            },
        },
        Tool {
            name: "timeline",
            description: "One ordered view of an epoch: stage transitions, crashes, power and \
                          flash events, and every piece of attached evidence, interleaved by time. \
                          This is the join that makes 'the rail sagged 40 ms before it hung' \
                          visible.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boot": {"type": "integer", "description": "Defaults to the latest epoch."},
                    "evidence_only": {"type": "boolean", "default": false},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 200}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let limit = capped(a, "limit", 200, 2000);
                let evidence_only = flag(a, "evidence_only");
                let payload = ctx.with_store(&d, |st| {
                    let b = match opt_i(a, "boot") {
                        Some(id) => st.boot(id)?,
                        None => st.latest_boot()?.ok_or_else(|| {
                            ToolError::new(ErrorCode::UnknownBoot, "no epochs recorded yet")
                        })?,
                    };
                    let mut items: Vec<Value> = Vec::new();
                    let rel = |at: i64| at - b.opened_at;

                    items.push(json!({
                        "at": b.opened_at, "offset_ms": 0, "what": "epoch_open",
                        "detail": {"boot_id": b.id, "seq": b.seq, "opened_by": b.opened_by},
                    }));
                    if !evidence_only {
                        for st_row in st.stages(None, Some(b.id))? {
                            items.push(json!({
                                "at": st_row.entered_ts, "offset_ms": rel(st_row.entered_ts),
                                "what": "stage", "detail": {"name": st_row.name,
                                                            "profile": st_row.profile},
                            }));
                        }
                        for r in st.records_in_boot(b.id, Some(RecordKind::Crash), 50)? {
                            let line = st.line(r.first_line_id)?;
                            items.push(json!({
                                "at": line.ts_wall, "offset_ms": rel(line.ts_wall),
                                "what": "crash",
                                "detail": {"record_id": r.id, "severity": r.severity,
                                           "text": line.lossy()},
                            }));
                        }
                    }
                    let prefix = evidence_only.then_some("evidence:");
                    for (id, at, _off, kind, data) in
                        st.timeline_events(Some(b.id), prefix, limit)?
                    {
                        items.push(json!({
                            "at": at, "offset_ms": rel(at),
                            "what": kind, "detail": data, "event_id": id,
                        }));
                    }
                    items.sort_by_key(|v| v["at"].as_i64().unwrap_or(0));
                    let capped_out = items.len() > limit;
                    items.truncate(limit);
                    Ok(json!({
                        "boot": {"id": b.id, "seq": b.seq, "opened_at": b.opened_at,
                                 "outcome": b.outcome},
                        "items": items,
                        "returned": items.len(),
                        "capped": capped_out,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        // ------------------------------------------------------------- bisect ----
        Tool {
            name: "bisect_start",
            description: "Begin a bisect over an ordered list of builds, oldest first. conminer \
                          keeps the bookkeeping and tells you which candidate to try next; your \
                          own flash tooling does the flashing.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name", "candidates"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "candidates": {"type": "array", "items": {"type": "string"}, "minItems": 2,
                        "description": "Build refs, oldest first. Order is the search axis."},
                    "predicate": {"type": "object", "description":
                        "What counts as bad, for auto-classification when a boot id is reported: \
                         {template_id} present, {fingerprint} seen, or {outcome} matched."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let candidates: Vec<String> = a
                    .get("candidates").and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                if candidates.len() < 2 {
                    return Err(ToolError::invalid_arg(
                        "a bisect needs at least two candidates to search between",
                    ));
                }
                let predicate = a.get("predicate").cloned().unwrap_or(Value::Null);
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    st.create_bisect(name, &candidates, &predicate, now)?;
                    let b = st.bisect(name)?;
                    Ok(json!({"bisect": b, "next": b.next_step()}))
                })?;
                Ok(json!({"device": d.display_name(), "started": payload}))
            },
        },
        Tool {
            name: "bisect_report",
            description: "Record a candidate's verdict and get the next one to test. Pass `boot` \
                          instead of `verdict` to have it classified from the epoch using the \
                          bisect's predicate.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name", "candidate"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "candidate": {"type": "string", "description":
                        "The candidate ref, or its index."},
                    "verdict": {"type": "string", "enum": ["good", "bad", "skip"]},
                    "boot": {"type": "integer", "description":
                        "Classify from this epoch instead of stating a verdict."},
                    "note": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let b = st.bisect(name)?;
                    let idx = b.index_of(s(a, "candidate")?)?;
                    let boot = opt_i(a, "boot");
                    let (verdict, derived) = match (opt_s(a, "verdict"), boot) {
                        (Some(v), _) => (v.to_string(), None),
                        (None, Some(boot_id)) => {
                            let (v, why) = crate::report::classify_for_bisect(
                                st, boot_id, &b.predicate,
                            )?;
                            (v, Some(why))
                        }
                        (None, None) => {
                            return Err(ToolError::invalid_arg(
                                "pass either a verdict or a boot to classify",
                            ))
                        }
                    };
                    st.record_bisect_result(b.id, idx, &verdict, boot, opt_s(a, "note"), now)?;

                    let b = st.bisect(name)?;
                    let step = b.next_step();
                    // A finished search is written down, so a later call does not
                    // re-derive the answer from scratch.
                    if let conminer_core::bisect::Step::Found { candidate, .. } = &step {
                        st.finish_bisect(b.id, "done", Some(candidate), now)?;
                    } else if matches!(step, conminer_core::bisect::Step::Inconclusive { .. }) {
                        st.finish_bisect(b.id, "inconclusive", None, now)?;
                    }
                    Ok(json!({
                        "recorded": {"candidate": b.candidates[idx], "verdict": verdict,
                                     "classified_from_boot": derived},
                        "next": step,
                        // Surfaced, never smoothed over: a non-monotonic result
                        // means the failure is intermittent and the halving is
                        // about to pin an innocent build.
                        "contradiction": b.contradiction(),
                        "tested": b.results.len(),
                        "candidates": b.candidates.len(),
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "bisect_status",
            description: "Where a bisect has got to: verdicts so far, the next candidate, and the \
                          culprit once it is pinned.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?;
                let payload = ctx.with_store(&d, |st| {
                    let b = st.bisect(name)?;
                    Ok(json!({
                        "bisect": b, "next": b.next_step(),
                        "contradiction": b.contradiction(),
                    }))
                })?;
                Ok(json!({"device": d.display_name(), "status": payload}))
            },
        },
        Tool {
            name: "list_bisects",
            description: "Bisects on this device, newest first, so a search interrupted \
                          yesterday can be picked up rather than restarted.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {"device": {"type": "string", "description": DEVICE_ARG}},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let names = ctx.with_store(&d, |st| st.bisects())?;
                Ok(json!({"device": d.display_name(), "bisects": names, "count": names.len()}))
            },
        },
        // ------------------------------------------------------------- decode ----
        Tool {
            name: "decode",
            description: "What the numbers mean: errno names, AArch64 ESR exception classes and \
                          fault status, GIC INTID/SPI arithmetic (the off-by-32 between a device \
                          tree and a register dump), PSCI codes, and addresses resolved against \
                          this board's memory map. Ambiguous values return every reading with the \
                          assumption it rests on.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description":
                        "Supplies the memory map for address decoding."},
                    "text": {"type": "string", "description":
                        "A console line. Every number in it is decoded."},
                    "value": {"type": "string", "description":
                        "A single value, decimal or 0x-hex."},
                    "line_id": {"type": "integer", "description":
                        "Decode a stored line, by the anchor search() returned."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                // The device is optional: errno and ESR need no board context,
                // and refusing to decode them without one would be unhelpful.
                let dev = match opt_s(a, "device") {
                    Some(sel) => Some(ctx.device(sel)?),
                    None => ctx.device_or_only(None).ok(),
                };
                let regions = dev
                    .as_ref()
                    .map(|d| ctx.config().memory_map_for(&[d.display_name(), d.label().unwrap_or_default()]))
                    .unwrap_or_default();

                let text = match (opt_s(a, "text"), opt_s(a, "value"), opt_i(a, "line_id")) {
                    (Some(t), _, _) => t.to_string(),
                    (None, Some(v), _) => v.to_string(),
                    (None, None, Some(id)) => {
                        let d = dev.clone().ok_or_else(|| {
                            ToolError::new(ErrorCode::UnknownDevice, "line_id needs a device")
                        })?;
                        ctx.with_store(&d, |st| Ok(st.line(id)?.lossy()))?
                    }
                    _ => return Err(ToolError::invalid_arg("pass text, value or line_id")),
                };
                let decoded = conminer_core::decode::decode_line(&text, &regions);
                Ok(json!({
                    "input": text,
                    "decoded": decoded,
                    "regions_known": regions.len(),
                }))
            },
        },
        // ---------------------------------------------------------- absence -----
        Tool {
            name: "learn_expectations",
            description: "Learn the shape of a normal boot on this device from reference epochs: \
                          which templates appear, how reliably, and roughly when. What counts as \
                          normal is your call, not a heuristic.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boots": {"type": "array", "items": {"type": "integer"},
                        "description": "Reference epochs. Omit to use every epoch whose outcome \
                                        is `booted`."},
                    "min_boots": {"type": "integer", "minimum": 1, "default": 3,
                        "description": "Refuse to learn from fewer than this: a skeleton fitted \
                                        to one boot is not a skeleton."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let min_boots = capped(a, "min_boots", 3, 1000);
                let explicit: Option<Vec<i64>> = a
                    .get("boots").and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_i64).collect());
                let now = ctx.now();
                let payload = ctx.with_store(&d, |st| {
                    let boots = match explicit {
                        Some(b) => b,
                        None => st.list_boots(10_000)?
                            .into_iter()
                            .filter(|b| b.outcome.as_deref() == Some("booted"))
                            .map(|b| b.id)
                            .collect(),
                    };
                    if boots.len() < min_boots {
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgument,
                            format!("only {} reference epoch(s) available, need {min_boots}",
                                    boots.len()),
                        )
                        .with_hint(
                            "pass `boots` explicitly, lower `min_boots`, or capture more good \
                             boots first — a skeleton fitted to one boot describes that boot, \
                             not the device",
                        ));
                    }
                    let learned = conminer_core::absence::learn(st, &boots, now)?;
                    Ok(json!({
                        "reference_boots": boots.len(),
                        "expectations": learned,
                        "boots": boots,
                    }))
                })?;
                Ok(json!({"device": d.display_name(), "learned": payload}))
            },
        },
        Tool {
            name: "missing_in_boot",
            description: "What this epoch did NOT print that a normal boot does — the shape a \
                          bring-up failure usually takes, and the one thing a novel-template list \
                          cannot tell you. Also reports what printed but arrived late.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boot": {"type": "integer", "description": "Defaults to the latest epoch."},
                    "min_reliability": {"type": "number", "minimum": 0, "maximum": 1,
                        "default": 0.9, "description":
                        "How often a template must appear in reference boots to count as \
                         'normally present'. Below it, absence is reported separately."},
                    "late_factor": {"type": "number", "minimum": 1, "default": 3.0,
                        "description": "Flag a line as late past this multiple of its usual offset."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50,
                        "description":
                        "Cap each list in the answer. Measured at 84KB unbounded on a real boot, \
                         which is a fifth of a session's budget for one question. Each capped list \
                         reports how many entries it omitted."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let min_rel = a.get("min_reliability").and_then(Value::as_f64).unwrap_or(0.9);
                let late = a.get("late_factor").and_then(Value::as_f64).unwrap_or(3.0);
                let limit = capped(a, "limit", 50, ctx.config().api.max_results.max(50));
                let payload = ctx.with_store(&d, |st| {
                    let boot = match opt_i(a, "boot") {
                        Some(b) => st.boot(b)?.id,
                        None => st.latest_boot()?
                            .ok_or_else(|| ToolError::new(
                                ErrorCode::UnknownBoot, "no epochs recorded yet"))?
                            .id,
                    };
                    conminer_core::absence::missing_in(st, boot, min_rel, late)
                })?;
                // Cap every list in the answer, and SAY what was dropped: a
                // silently shortened list reads as "nothing else was missing",
                // which is the opposite of what this tool is for.
                let payload = cap_lists(payload, limit);
                fresh(ctx, &d, payload)
            },
        },
        // -------------------------------------------------------- provenance ----
        Tool {
            name: "transfer_file",
            description: "Pull a file off a console-only board at protocol speed (zmodem), instead \
                          of base64 through the shell. Claims the port exclusively for the \
                          duration so the miner does not template the binary span, and ALWAYS \
                          releases it — including on failure.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["remote_path"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "direction": {"type": "string", "enum": ["pull"], "default": "pull",
                        "description": "Push is out of scope on this rig (no writes to board \
                                        storage)."},
                    "remote_path": {"type": "string"},
                    "local_path": {"type": "string", "description":
                        "A BARE FILENAME lands in the shared export directory (/exports, \
                         bind-mounted to ./exports on the host); the response carries the \
                         host-visible path."},
                    "protocol": {"type": "string", "enum": ["zmodem"], "default": "zmodem"},
                    "max_bytes": {"type": "integer", "default": 8388608}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{BrokeredTransport, CommandOptions, Runner};
                let d = device(ctx, a)?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let remote = s(a, "remote_path")?.to_string();
                let max_bytes = opt_i(a, "max_bytes").unwrap_or(8 * 1024 * 1024).max(1) as u64;
                let local = shared_path(
                    opt_s(a, "local_path").unwrap_or_else(|| {
                        remote.rsplit('/').next().unwrap_or("pulled.bin")
                    }),
                );

                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let prompts = prompts_for(ctx, &d)?;
                let mk_opts = || {
                    CommandOptions::new(
                        ctx.config().runner_for(d.display_name()),
                        ctx.config().line_for(d.display_name()),
                    )
                };

                // PRECONDITIONS FIRST, while the port is still shared: a missing
                // `sz` discovered after claiming would mean releasing a claim
                // nobody needed, and round 1's lesson was that a stuck claim
                // costs a console until someone notices.
                let probe = {
                    let (ep, bs, bd) = (endpoint.clone(), broker_sock.clone(), broker_dev.clone());
                    let (p, o) = (prompts.clone(), mk_opts());
                    let cmd = format!("command -v sz && stat -c %s {remote}");
                    block_on(async move {
                        let io = BrokeredTransport::connect(&ep, &bs, &bd).await?;
                        Runner::new(io, p, o).at(&ep).run(&cmd).await
                    })?
                };
                if let Some(e) = probe.as_error() {
                    return Err(e);
                }
                let mut lines = probe.output.lines().filter(|l| !l.trim().is_empty());
                let sz_path = lines.next().unwrap_or_default().trim().to_string();
                if !sz_path.starts_with('/') {
                    return Err(ToolError::new(
                        ErrorCode::HookNotConfigured,
                        "this board has no `sz`: zmodem needs lrzsz on the target",
                    )
                    .with_hint("install lrzsz on the board, or use pull_file for a small file"));
                }
                let remote_size: u64 = lines
                    .next()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if remote_size == 0 {
                    return Err(ToolError::new(
                        ErrorCode::NoSuchPath,
                        format!("{remote} is empty or unreadable on the board"),
                    ));
                }
                if remote_size > max_bytes {
                    return Err(ToolError::new(
                        ErrorCode::IngestTooLarge,
                        format!("{remote} is {remote_size} bytes, over the {max_bytes}-byte cap"),
                    )
                    .with_hint("raise max_bytes, or pull a narrower file"));
                }

                // The claim marks the span binary so the miner stops templating
                // what is about to be framing bytes, and keeps every other
                // consumer off the port.
                let now = ctx.now();
                ctx.registry().claim_exclusive(d.id, &ctx.holder(), Some("zmodem"), now)?;
                let started = std::time::Instant::now();
                let result = conminer_core::transfer::zmodem_pull(
                    &endpoint,
                    &remote,
                    &local,
                    std::time::Duration::from_secs(
                        (remote_size / 8_000).clamp(30, 600),
                    ),
                );
                // ALWAYS. A claim outliving its transfer is how a console gets
                // lost until a human notices it is unreachable.
                let released = ctx.registry().release_exclusive(d.id);
                let bytes = result?;
                released?;

                // Verified against the BOARD's own numbers, not ours: a transfer
                // only the host believes in has not been verified.
                let sum = {
                    let (ep, bs, bd) = (endpoint.clone(), broker_sock.clone(), broker_dev.clone());
                    let (p, o) = (prompts.clone(), mk_opts());
                    let cmd = format!("cksum {remote}");
                    block_on(async move {
                        let io = BrokeredTransport::connect(&ep, &bs, &bd).await?;
                        Runner::new(io, p, o).at(&ep).run(&cmd).await
                    })
                    .ok()
                };
                let local_bytes = std::fs::read(&local).unwrap_or_default();
                let local_cksum = conminer_core::transfer::cksum(&local_bytes);
                let target_cksum = sum
                    .as_ref()
                    .and_then(|t| t.output.split_whitespace().next().map(str::to_string));
                let verified = match &target_cksum {
                    Some(t) if *t == local_cksum.to_string() => "cksum+size",
                    Some(_) => {
                        return Err(ToolError::new(
                            // The board's own checksum disagreed with ours: the
                            // bytes are not the file. Never a soft warning --
                            // "a transfer only the host believes in has not been
                            // verified".
                            ErrorCode::Internal,
                            format!(
                                "checksum differs: board says {target_cksum:?}, local file is \
                                 {local_cksum}"
                            ),
                        ))
                    }
                    None => "size_only",
                };

                let secs = started.elapsed().as_secs_f64().max(0.001);
                fresh(ctx, &d, json!({
                    "bytes": bytes,
                    "remote_size": remote_size,
                    "seconds": (secs * 10.0).round() / 10.0,
                    "throughput_bps": (bytes as f64 / secs).round() as i64,
                    "verified": verified,
                    "local_path": local.display().to_string(),
                    "host_path": host_hint(&local),
                }))
            },
        },
        Tool {
            name: "snapshot_dmesg",
            description: "Pull the kernel ring buffer into its own mined session, so the lines the \
                          console never showed (suppressed by loglevel, or rate-limited) become \
                          queryable like everything else. Captured into a SEPARATE session, so \
                          2,000 replayed lines never pollute the console's templates or the \
                          current epoch's byte counts.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "source": {"type": "string", "enum": ["dmesg"], "default": "dmesg"},
                    "args": {"type": "string", "description":
                        "Extra dmesg flags. Allow-listed: -T, -x, -k, -l <levels>."},
                    "timeout_s": {"type": "integer", "minimum": 5, "maximum": 600,
                        "default": 300, "description":
                        "How long to let the board stream. The default is sized from the CONSOLE, \
                         not from taste: a 115200 line delivers ~11 KB/s, and a routine full \
                         dmesg is 65 KB, so 120s truncated an ordinary capture on real hardware. \
                         300s covers a ~3 MB dump at that rate."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                use conminer_core::runner::{BrokeredTransport, CommandOptions, Runner};
                let d = device(ctx, a)?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;

                // Allow-listed, because this string becomes part of a command
                // line on the board. A flag that reached the shell unchecked
                // would be an injection point on a rig where the console is
                // root.
                let extra = opt_s(a, "args").unwrap_or_default().trim().to_string();
                let allowed = ["-T", "-x", "-k", "-l", "--ctime", "--decode", "--kernel"];
                for tok in extra.split_whitespace() {
                    let ok = allowed.contains(&tok)
                        // `-l err,warn` takes a value: letters and commas only.
                        || tok.chars().all(|c| c.is_ascii_alphanumeric() || c == ',');
                    if !ok {
                        return Err(ToolError::invalid_arg(format!(
                            "dmesg argument {tok:?} is not allow-listed"
                        ))
                        .with_detail(json!({"allowed": allowed})));
                    }
                }

                let endpoint = endpoint_for(ctx, &d)?;
                let (broker_sock, broker_dev) = broker_read_path(ctx, &d);
                let prompts = prompts_for(ctx, &d)?;
                let mut opts = CommandOptions::new(
                    ctx.config().runner_for(d.display_name()),
                    ctx.config().line_for(d.display_name()),
                );
                // 300s, because the console's own pace decides this: ~11 KB/s at
                // 115200, and a routine 65 KB dmesg therefore needs ~6s of pure
                // streaming plus whatever the board is doing between lines. The
                // old 120s default truncated a perfectly ordinary full dmesg on
                // the IQ10 -- reported honestly as `truncated: true, rc: null`,
                // but a default that fails on the normal case is still wrong.
                let timeout_s = opt_i(a, "timeout_s").unwrap_or(300).clamp(5, 600) as u64;
                opts.timeout_s = timeout_s;
                // THE CAP THAT ACTUALLY BINDS. §H1.
                //
                // `CommandOptions::new` defaults `max_output_bytes` to 64 KB --
                // right for a shell command, absurd for a ring buffer -- and
                // this call never raised it. Measured twice on the IQ10: a full
                // dmesg truncated at exactly 65,574 bytes (65,536 + the marker
                // text), the SAME byte count both times, which is the signature
                // of a buffer and not of a clock. The call returned in 23s
                // against a 300s budget.
                //
                // It is raised to the configured snapshot cap so the refusal
                // that already exists below -- IngestTooLarge at
                // api.max_snapshot_bytes, with a hint -- is what a caller
                // actually meets, instead of a silent 64 KB cut wearing a
                // timeout's clothes.
                let snapshot_cap = ctx.config().api.max_snapshot_bytes;
                opts.max_output_bytes = snapshot_cap as usize;
                // The BOARD is talking here, not us: echo verification applies
                // to the command line, never to the kilobytes it prints back.
                opts.echo = true;

                // Delimiters, so a truncated capture is detectable rather than
                // silently short: without the END marker and the exit code, a
                // board that reset mid-dump would look like a small dmesg.
                let nonce = format!("{:x}", ctx.now());
                let cmd = format!(
                    "echo SNAP-BEGIN-{nonce}; dmesg {extra}; echo SNAP-END-{nonce}-rc=$?"
                );

                let txn = block_on(async move {
                    let io =
                        BrokeredTransport::connect(&endpoint, &broker_sock, &broker_dev).await?;
                    Runner::new(io, prompts, opts).at(&endpoint).run(&cmd).await
                })?;
                if let Some(e) = txn.as_error() {
                    return Err(e);
                }

                let begin = format!("SNAP-BEGIN-{nonce}");
                let end = format!("SNAP-END-{nonce}-rc=");
                let out = &txn.output;
                let body: String = match (out.find(&begin), out.find(&end)) {
                    (Some(b), Some(e)) => out[b + begin.len()..e].trim_start_matches('\n').into(),
                    // No end marker: the board stopped talking mid-dump. Keep
                    // what arrived and SAY it is partial.
                    (Some(b), None) => out[b + begin.len()..].into(),
                    _ => out.clone(),
                };
                let truncated = !out.contains(&end);
                // WHICH limit, measured -- never guessed. §H1.
                //
                // The first version of this hint asserted a timeout for every
                // truncation, and was wrong on the only case anybody hit: a
                // capture cut by the output cap after 23s of a 300s budget was
                // told "the END marker never arrived within 300s". A hint that
                // invents a cause is worse than none, because it sends the
                // reader to the wrong knob -- and it sent ME there, which is how
                // the 64 KB cap survived a round of "fixing" it.
                //
                // The runner reports whether IT capped; the elapsed time says
                // whether the clock ran out. Anything else is the board falling
                // silent mid-dump, which is a third thing entirely.
                let elapsed_s = txn.duration_ms / 1000;
                let hit_time = elapsed_s + 2 >= timeout_s;
                let truncated_by = if txn.output_capped {
                    "output_cap"
                } else if hit_time {
                    "timeout"
                } else {
                    "console_stopped"
                };
                let rc: Option<i64> = out
                    .find(&end)
                    .and_then(|i| out[i + end.len()..].split_whitespace().next())
                    .and_then(|v| v.trim().parse().ok());

                let cap = ctx.config().api.max_snapshot_bytes;
                if body.len() as u64 > cap {
                    return Err(ToolError::new(
                        ErrorCode::IngestTooLarge,
                        format!("dmesg returned {} bytes, over the {cap}-byte cap", body.len()),
                    )
                    .with_hint("narrow it: args \"-l err,warn\" or raise api.max_snapshot_bytes"));
                }

                let bound_boot = ctx.with_store(&d, |st| Ok(st.latest_boot()?.map(|b| b.id)))?;

                // ITS OWN STORE, not the console's.
                //
                // Measured on the IQ10: the first version opened an ingest
                // pipeline on the console's own store, which takes the device
                // writer lock -- and minerd holds that lock for every live
                // console, with `flock(LOCK_EX)`, which BLOCKS. The call hung
                // until the client gave up.
                //
                // A sibling store is also the strongest form of what this
                // feature is for: a ring-buffer replay is not console output, so
                // it must not touch the console's templates, its epoch byte
                // counts, or its cursor. `bound_boot` ties the two together, and
                // list_templates on each side is the diff the spec asked for.
                let snap_name = format!("{}#dmesg", d.canonical);
                let snap_dev = {
                    let mut reg = ctx.registry();
                    let row = reg.upsert_device(
                        &snap_name,
                        None,
                        conminer_core::store::IdentityKind::ById,
                        None,
                        ctx.now(),
                    )?;
                    // Never bridged, never actuated: it is a place to put mined
                    // text, not a port.
                    reg.set_ignored(row.id, true)?;
                    row
                };
                ctx.forget_store(snap_dev.id);
                // Its OWN session, fed through the SAME pipeline as ingest_file:
                // the ring buffer is a replay of the past, and mixing it into the
                // live stream would inflate this epoch's byte counts and mint
                // template occurrences that never crossed the wire twice.
                let label = format!("snapshot:dmesg boot={bound_boot:?}");
                let store = conminer_core::store::DeviceStore::open(
                    &ctx.data_dir().join(&snap_dev.db_file),
                    &snap_dev.canonical,
                    ctx.config().fts_for(d.display_name()),
                )?;
                let mut pipe = conminer_core::pipeline::Pipeline::new(
                    store,
                    ctx.profiles().clone(),
                    ctx.config().clone(),
                    d.display_name(),
                    d.pinned_profile.as_deref(),
                    ctx.clock().clone(),
                )?;
                let mut iopts = conminer_core::ingest::IngestOptions::from_config(ctx.config());
                iopts.label = Some(label);
                iopts.source = conminer_core::store::SessionSource::File;
                let report = conminer_core::ingest::ingest_reader(
                    &mut pipe,
                    std::io::Cursor::new(body.as_bytes().to_vec()),
                    conminer_core::ingest::Codec::Plain,
                    &iopts,
                    Some("snapshot:dmesg".to_string()),
                )?;
                drop(pipe);

                fresh(ctx, &d, json!({
                    // Where the snapshot landed. Query it like any device:
                    // list_templates on this vs the console's epoch is "what the
                    // ring buffer had that the console never showed".
                    "snapshot_device": snap_dev.display_name(),
                    "session_id": report.session_id,
                    "lines": report.lines,
                    "records": report.records,
                    "new_templates": report.new_templates,
                    "bound_boot": bound_boot,
                    "truncated": truncated,
                    "rc": rc,
                    "bytes": body.len(),
                    // A truncated capture is a state the caller can DO something
                    // about, and saying which knob turns it beats leaving them to
                    // infer that a null rc means "ran out of time".
                    "truncated_by": truncated.then_some(truncated_by),
                    "truncated_hint": truncated.then(|| match truncated_by {
                        "output_cap" => format!(
                            "the capture hit the {snapshot_cap}-byte output cap after \
                             {} bytes and {elapsed_s}s, so this is partial: narrow it with \
                             args \"-l err,warn\" (or \"-k\"), or raise api.max_snapshot_bytes",
                            body.len()
                        ),
                        // Both numbers, and what each one IS. Saying "never
                        // arrived within 5s" beside "21s elapsed" reads as a
                        // contradiction even though both are true: the budget
                        // bounds the wait for the marker, while the call also
                        // pays the runner's settle and drain.
                        "timeout" => format!(
                            "the END marker never arrived: the budget was {timeout_s}s and the \
                             call took {elapsed_s}s in all, with {} bytes captured. Narrow it \
                             with args \"-l err,warn\", or raise timeout_s (max 600)",
                            body.len()
                        ),
                        _ => format!(
                            "the console stopped mid-dump after {} bytes in {elapsed_s}s, well \
                             inside the {timeout_s}s budget and the {snapshot_cap}-byte cap -- \
                             the board went quiet or reset rather than any limit being reached",
                            body.len()
                        ),
                    }),
                }))
            },
        },
        Tool {
            name: "prune",
            description: "Reclaim disk by dropping VERBATIM BYTES older than a cutoff. Templates, \
                          epochs, stages, fingerprints, verdicts, metrics and version extractions \
                          are kept — they are the compressed knowledge, and they are the point. \
                          Queries into a pruned range then fail PRUNED rather than returning less \
                          than they should.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "before_ts": {"type": "integer", "description":
                        "Prune raw older than this wall-clock ms."},
                    "keep_bytes": {"type": "integer", "description":
                        "Prune oldest raw until at most this many bytes remain."},
                    "dry_run": {"type": "boolean", "description":
                        "Report what would go without removing anything."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                ctx.registry().require_lease(d.id, &ctx.holder(), ctx.now())?;
                let before_ts = opt_i(a, "before_ts");
                let keep_bytes = opt_i(a, "keep_bytes").map(|v| v.max(0) as u64);
                let cfg_keep_days = ctx.config().retention.raw_keep_days;
                let cfg_keep_bytes = ctx.config().retention.raw_keep_bytes;
                let protect = ctx.config().retention.protect_baselines;
                let now = ctx.now();
                let dry = flag(a, "dry_run");

                let payload = ctx.with_store(&d, |st| {
                    let before_bytes = st.raw_bytes()?;
                    // Age rule: the argument wins, else the configured policy.
                    let cutoff_ts = before_ts.or_else(|| {
                        (cfg_keep_days > 0)
                            .then(|| now - (cfg_keep_days as i64) * 86_400_000)
                    });
                    let mut target = match cutoff_ts {
                        Some(ts) => st.offset_at_ts(ts)?,
                        None => None,
                    };
                    // A baseline epoch is evidence somebody deliberately kept.
                    // Ageing it out silently would break the comparison it
                    // exists for, so it is a floor on what may go.
                    let protected = if protect { st.protected_offset()? } else { None };
                    if let (Some(t), Some(p)) = (target, protected) {
                        if t > p {
                            target = Some(p);
                        }
                    }

                    let mut removed = 0usize;
                    if !dry {
                        if let Some(off) = target {
                            removed += st.prune_before(off)?;
                        }
                        let cap = keep_bytes.or((cfg_keep_bytes > 0).then_some(cfg_keep_bytes));
                        if let Some(cap) = cap {
                            removed += st.enforce_size_cap(cap)?;
                        }
                    }
                    let after_bytes = st.raw_bytes()?;
                    Ok(json!({
                        "dry_run": dry,
                        "lines_removed": removed,
                        "raw_bytes_before": before_bytes,
                        "raw_bytes_after": after_bytes,
                        "reclaimed_bytes": before_bytes.saturating_sub(after_bytes),
                        "cutoff_ts": cutoff_ts,
                        "protected_offset": protected,
                        "pruned_before_offset": st.pruned_before_offset(),
                        // Said out loud, because this is the promise: the answer
                        // to "what happened on that boot" survives the bytes.
                        "kept": "templates, epochs, stages, fingerprints, verdicts, metrics, \
                                 version extractions",
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "pin_metric",
            description: "Name a number that lives in a template slot, so it can be asked for as a \
                          series later. The durable version of a template_values query: the values \
                          are resolved from occurrences that already exist, so a metric pinned \
                          today answers about boots from last week.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name", "template_id"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "template_id": {"type": "integer"},
                    "slot": {"type": "integer", "description":
                        "TOKEN INDEX of the wildcard holding the number -- the same numbering \
                         `template_values` reports, NOT the Nth wildcard. Omit it when the \
                         template has exactly one wildcard and it will be chosen for you; a \
                         slot that is not a wildcard is refused with the list of the ones that \
                         are, because a metric pinned to a non-slot yields an empty series \
                         forever and nothing says why."},
                    "agg": {"type": "string", "enum": ["last", "first", "min", "max"],
                        "default": "last", "description":
                        "Which occurrence counts when an epoch has several."},
                    "unit": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?.to_string();
                let template_id = i(a, "template_id")?;
                let want_slot = opt_i(a, "slot");
                let agg = opt_s(a, "agg").unwrap_or("last").to_string();
                if !["last", "first", "min", "max"].contains(&agg.as_str()) {
                    return Err(ToolError::invalid_arg(format!(
                        "agg {agg:?} must be one of last|first|min|max"
                    )));
                }
                let unit = opt_s(a, "unit").map(str::to_string);
                let now = ctx.now();
                let profiles = ctx.profiles().clone();
                let payload = ctx.with_store(&d, |st| {
                    // Fail here rather than at read time: a metric pinned to a
                    // template that does not exist is a question that can never
                    // be answered, and the caller is right here to fix it.
                    let t = st.template(template_id)?;
                    // THE SLOT MUST BE A SLOT. `slot: 0` on a template whose
                    // value sits at token 3 was accepted without a word and
                    // produced a permanently empty series -- a metric that
                    // silently answers nothing is worse than one that refuses to
                    // exist, because the emptiness reads as "the board never
                    // printed it".
                    let wildcards: Vec<i64> = t
                        .tokens
                        .iter()
                        .enumerate()
                        .filter(|(_, tok)| tok.as_str() == conminer_core::drain::WILDCARD)
                        .map(|(i, _)| i as i64)
                        .collect();
                    let slot = match (want_slot, wildcards.as_slice()) {
                        (Some(s), _) if wildcards.contains(&s) => s,
                        // Exactly one slot and no preference: there is nothing
                        // to guess between.
                        (None, [only]) => *only,
                        (want, _) => {
                            // Show what actually sat in each slot, so the right
                            // one is obvious rather than merely legal.
                            let samples = conminer_core::values::template_values(
                                st, &profiles, template_id, None, None, None, 200, 2,
                            )
                            .unwrap_or(Value::Null);
                            let msg = match want {
                                Some(s) => format!(
                                    "template {template_id} has no wildcard at token {s}"
                                ),
                                None => format!(
                                    "template {template_id} has {} wildcards, so `slot` is \
                                     required",
                                    wildcards.len()
                                ),
                            };
                            return Err(ToolError::new(ErrorCode::InvalidArgument, msg)
                                .with_hint(format!(
                                    "its wildcard slots are {wildcards:?} (token indices, not \
                                     the Nth wildcard); the samples show what each one holds"
                                ))
                                .with_detail(json!({
                                    "template": t.text,
                                    "tokens": t.tokens,
                                    "wildcard_slots": wildcards,
                                    "samples": samples.get("slots").cloned().unwrap_or(Value::Null),
                                })));
                        }
                    };
                    st.pin_metric(&name, template_id, slot, &agg, unit.as_deref(), now)?;
                    Ok(json!({"name": name, "template_id": template_id, "slot": slot,
                              "agg": agg, "unit": unit}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "unpin_metric",
            description: "Forget a pinned metric. The occurrences it read stay where they are.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?.to_string();
                let payload = ctx.with_store(&d, |st| {
                    Ok(json!({"name": name, "removed": st.unpin_metric(&name)?}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "list_metrics",
            description: "Which numbers this device has pinned, and where they come from.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {"device": {"type": "string", "description": DEVICE_ARG}},
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = ctx.with_store(&d, |st| {
                    let rows: Vec<Value> = st.metrics()?.into_iter()
                        .map(|(name, template_id, slot, agg, unit, pinned_at)| json!({
                            "name": name, "template_id": template_id, "slot": slot,
                            "agg": agg, "unit": unit, "pinned_at": pinned_at,
                        }))
                        .collect();
                    Ok(json!({"metrics": rows}))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "metric_series",
            description: "One pinned number per epoch, oldest first, with min/max/mean/last — \
                          \"is this getting slower?\" as a series instead of three searches.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "name": {"type": "string"},
                    "last": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let name = s(a, "name")?.to_string();
                let last = opt_i(a, "last").unwrap_or(50).clamp(1, 500) as usize;
                let profiles = ctx.profiles().clone();
                let payload = ctx.with_store(&d, |st| {
                    let Some((template_id, slot, agg, unit)) = st.metric(&name)? else {
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgument,
                            format!("no metric named {name:?} on this device"),
                        )
                        .with_hint("pin one with pin_metric, or list_metrics to see what exists"));
                    };
                    let mut points = Vec::new();
                    let mut values: Vec<f64> = Vec::new();
                    for b in st.list_boots(last)?.into_iter().rev() {
                        let epoch_vals = conminer_core::values::slot_numbers(
                            st, &profiles, template_id, slot as usize, b.id,
                        )?;
                        if epoch_vals.is_empty() {
                            continue;
                        }
                        // One value per epoch: which occurrence counts is the
                        // caller's choice, made when the metric was pinned.
                        let v = match agg.as_str() {
                            "first" => epoch_vals[0],
                            "min" => epoch_vals.iter().cloned().fold(f64::INFINITY, f64::min),
                            "max" => epoch_vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                            _ => *epoch_vals.last().expect("non-empty"),
                        };
                        values.push(v);
                        points.push(json!({
                            "boot_id": b.id, "seq": b.seq, "label": b.label,
                            "value": v, "occurrences": epoch_vals.len(), "ts": b.opened_at,
                        }));
                    }
                    let n = values.len() as f64;
                    let stats = if values.is_empty() {
                        json!({"samples": 0})
                    } else {
                        json!({
                            "samples": values.len(),
                            "min": values.iter().cloned().fold(f64::INFINITY, f64::min),
                            "max": values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                            "mean": (values.iter().sum::<f64>() / n * 100.0).round() / 100.0,
                            "last": values.last(),
                        })
                    };
                    Ok(json!({
                        "name": name, "template_id": template_id, "slot": slot,
                        "agg": agg, "unit": unit, "points": points, "stats": stats,
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "noise",
            description: "What is FLOODING this console right now, by RATE -- not by lifetime \
                          count. Answers \"one message is repeating four times a second: which, \
                          and how much of the output is it?\" before you read anything raw. Mute \
                          it with annotate_template {verdict:\"benign\"}; get_recent then \
                          collapses it with suppress_noise.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "window_s": {"type": "integer", "minimum": 5, "maximum": 86400,
                        "default": 120, "description":
                        "How far back to measure. Short windows answer \"right now\"."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 5}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let window_s = opt_i(a, "window_s").unwrap_or(120).clamp(5, 86_400);
                let limit = opt_i(a, "limit").unwrap_or(5).clamp(1, 50) as usize;
                let now = ctx.now();
                let since = now - window_s * 1000;
                let payload = ctx.with_store(&d, |st| {
                    let total = st.lines_since_ts(since)?.max(0);
                    let muted = st.muted_templates()?;
                    let rows: Vec<Value> = st.noisiest_templates(since, now, limit)?
                        .into_iter()
                        .map(|mut r| {
                            let n = r["count_in_window"].as_i64().unwrap_or(0);
                            if let Some(o) = r.as_object_mut() {
                                // Share of the WHOLE output: "137 times" means
                                // something different on a board that printed 140
                                // lines than on one that printed 14,000.
                                // Capped at 100: a share above it would be an
                                // arithmetic artefact of rollup granularity, and
                                // an agent reading "118% of the output" learns
                                // nothing except that the number is wrong.
                                o.insert("share_of_output".into(), json!(if total > 0 {
                                    (((n as f64 / total as f64) * 1000.0).round() / 10.0)
                                        .min(100.0)
                                } else { 0.0 }));
                                let id = o["template_id"].as_i64().unwrap_or(-1);
                                o.insert("muted".into(), json!(muted.contains(&id)));
                            }
                            r
                        })
                        .collect();
                    // Judged on the BURST rate: a message arriving four times a
                    // second is flooding whether the caller asked about the last
                    // minute or the last day, and diluting it across a wide
                    // window is how a real flood gets called "ordinary output".
                    let flooding = rows
                        .first()
                        .and_then(|r| r["burst_per_min"].as_f64())
                        .unwrap_or(0.0);
                    let share = rows
                        .first()
                        .and_then(|r| r["share_of_output"].as_f64())
                        .unwrap_or(0.0);
                    let lines_per_min =
                        ((total as f64) / (window_s as f64 / 60.0) * 10.0).round() / 10.0;
                    Ok(json!({
                        "window_s": window_s,
                        "lines_in_window": total,
                        "lines_per_min": lines_per_min,
                        "top": rows,
                        "muted_templates": muted,
                        // The number alone does not tell an agent what to DO.
                        // Three different situations, three different moves.
                        // Measured on the ADP mid-boot: 1,969 lines in two
                        // minutes with no single message above 20% share --
                        // calling that "ordinary output" undersells what an
                        // agent is about to walk into, and calling it a flood
                        // would cry wolf at every normal boot.
                        "advice": if flooding >= 60.0 && share >= 50.0 {
                            "one message is repeating at least once a second: mute it with \
                             annotate_template {verdict:\"benign\"} and pass suppress_noise:true \
                             to get_recent, or you will spend the window reading it"
                        } else if lines_per_min >= 300.0 {
                            "high output but no single message dominates: this is a boot or a \
                             broad storm, not one repeating line. Read templates, not lines -- \
                             list_templates {min_severity} -- or park with follow {until:{quiet}}"
                        } else if total > 0 {
                            "nothing is flooding; ordinary output"
                        } else {
                            "this console has said nothing in the window"
                        },
                    }))
                })?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "stage_timings",
            description: "How long each boot stage took, across recent epochs, with a trend. \
                          Answers \"is boot getting slower?\" without hand-assembling numbers from \
                          several boot_report calls.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "last": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20},
                    "stage": {"type": "string", "description":
                        "Only this stage, e.g. \"kernel\"."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let last = opt_i(a, "last").unwrap_or(20).clamp(1, 500) as usize;
                let payload = crate::report::stage_timings(ctx, &d, last, opt_s(a, "stage"))?;
                fresh(ctx, &d, payload)
            },
        },
        Tool {
            name: "provenance",
            description: "What is actually running versus what was last flashed. Catches the \
                          failure that invalidates everything downstream of it: debugging a stale \
                          image because the flash silently did not take.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string", "description": DEVICE_ARG},
                    "boot": {"type": "integer", "description": "Defaults to the latest epoch."}
                },
                "additionalProperties": false
            }),
            call: |ctx, a| {
                let d = device(ctx, a)?;
                let payload = ctx.with_store(&d, |st| {
                    let boot = match opt_i(a, "boot") {
                        Some(b) => st.boot(b)?,
                        None => st.latest_boot()?.ok_or_else(|| {
                            ToolError::new(ErrorCode::UnknownBoot, "no epochs recorded yet")
                        })?,
                    };
                    crate::report::provenance(st, &boot)
                })?;
                fresh(ctx, &d, payload)
            },
        },
    ]
}

pub fn find(name: &str) -> Option<&'static Tool> {
    registry().iter().find(|t| t.name == name)
}

/// The MCP `tools/list` payload.
/// Tools a console-driving session actually reaches for.
///
/// The full registry advertises 67 tools at ~45KB, of which SCHEMAS are 65%
/// (29.7KB) -- and that cost is paid at the start of EVERY session, before any
/// work happens. A day of hardware bring-up used roughly this dozen. The rest
/// stay callable; they are simply not pre-loaded into the agent's context.
pub const CORE_TOOLS: &[&str] = &[
    "list_devices",
    "console_state",
    "get_recent",
    "get_context",
    "search",
    "run_command",
    "send",
    "acquire",
    "release",
    "follow",
    "power",
    "actuation_status",
    "boot_mode",
    "list_boots",
    "boot_stages",
    "boot_report",
    "list_templates",
    "stats",
    "help",
    "diagnose",
];

/// What the console did after a power action, and whether conminer had to
/// escalate to make it true.
///
/// Deliberately conservative: it reports what it observed and at most ONE
/// escalation. Retrying forever on hardware that is not responding is how a
/// bench ends up in an unknown state.
/// Where to read a console from, and under what name.
///
/// mcpd reads through the broker (minerd is the single reader of each device)
/// and still writes straight to ser2net, so a broker outage can never swallow a
/// command headed for a board.
/// Ask the board's controller whether it is powered on.
///
/// Returns "on", "off", or None when this controller cannot answer (the
/// Bughopper drives power over CBUS with no readback line, so it genuinely
/// cannot). None means UNKNOWN and must never be rendered as "off": a guess
/// dressed as a fact is how a silent-but-running board gets misread.
/// What each device's controller says about its board's power.
///
/// ONE QUERY PER CONTROLLER INSTANCE, and an answer is shared ONLY between
/// consoles that resolve to the same instance. Both halves matter:
///
///  * per instance, not per console, because a controller query costs ~1-2s and
///    the NordAU alone has six consoles hanging off one Bantam;
///  * per INSTANCE, not per controller PROFILE, because this bench has two
///    Bantams and they are both called "bantam". Grouping on the profile name
///    is what made the dashboard publish the IQ10's "off" for the NordAU, which
///    was powered on at the time. A board must never be able to report another
///    board's power.
///
/// The value is `Some("on"|"off")` or `None` for unknown, and unknown is a real
/// answer -- the Bughopper is a commanded-only controller that genuinely cannot
/// measure. `None` must never be rendered as "off".
fn power_by_controller(ctx: &Context, devices: &[DeviceRow]) -> PowerMap {
    let present = present_with_topology(ctx);
    let mut out: PowerMap = Default::default();
    let mut groups: std::collections::BTreeMap<String, Vec<&DeviceRow>> = Default::default();
    for d in devices {
        // NOT EVERY ROW HAS A BOARD BEHIND IT, and the Bantam profile claims
        // `controls = "*"` on purpose so a newly plugged board gets working
        // buttons with no config. The two combine badly: measured on the rig,
        // `file:/tmp/board-boot.log` -- a MINED LOG -- resolved to the IQ10's
        // Bantam and reported `power: "off"`. A log file cannot be powered, and
        // an answer borrowed from an unrelated board is the same defect this
        // whole function exists to remove, wearing a different hat.
        if let Some(why) = not_a_powerable_board(ctx, &d.canonical) {
            out.insert(d.canonical.clone(), (None, None, Some(why.into())));
            continue;
        }
        // No resolved instance means probe this console on its own: slower, and
        // correct. Falling back to anything SHARED is the bug this exists to
        // prevent.
        let Some(key) = ctx.config().controller_port_for(
            &d.canonical,
            d.by_path.as_deref(),
            present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
        ) else {
            out.insert(
                d.canonical.clone(),
                (
                    None,
                    None,
                    Some(
                        "no controller resolves for this device, so nothing can be asked -- \
                         this is not a claim that the board is off"
                            .into(),
                    ),
                ),
            );
            continue;
        };
        groups.entry(key).or_default().push(d);
    }
    for (controller, members) in groups {
        let Some(first) = members.first() else {
            continue;
        };
        let state = probe_power_state(ctx, first);
        let why = state.is_none().then(|| {
            "this controller cannot measure power (it drives the button with no sense line back)"
                .to_string()
        });
        for m in &members {
            out.insert(
                m.canonical.clone(),
                (state.clone(), Some(controller.clone()), why.clone()),
            );
        }
    }
    out
}

/// Why this row could never have a power state, if it could not.
///
/// A device list holds more than boards: mined logs, internal sibling stores,
/// and the controllers themselves. Asking a controller hook about any of them
/// produces an answer about SOME OTHER board, which is worse than no answer.
fn not_a_powerable_board(ctx: &Context, canonical: &str) -> Option<&'static str> {
    if canonical.starts_with("file:") {
        return Some("this is a mined log, not a board: there is nothing to power");
    }
    if canonical.contains('#') {
        return Some("this is an internal sibling store, not a console on a board");
    }
    if ctx.config().controller_profile_of(canonical).is_some() {
        return Some(
            "this is the controller itself, not a board it powers -- ask one of its consoles",
        );
    }
    None
}

/// power reading, which controller answered, and why it is unknown when it is.
type PowerMap = std::collections::HashMap<String, (Option<String>, Option<String>, Option<String>)>;

/// Attach a power reading to a row, saying "unknown" out loud.
///
/// Emitted as the STRING "unknown" rather than omitted or null: a missing field
/// invites a reader to fill the gap with an assumption, and the assumption that
/// costs hardware is "no reading means off".
fn add_power(row: &mut Value, map: &PowerMap, canonical: &str) {
    let Some((state, source, why)) = map.get(canonical) else {
        return;
    };
    if let Some(o) = row.as_object_mut() {
        o.insert(
            "power".into(),
            json!(state.clone().unwrap_or_else(|| "unknown".into())),
        );
        o.insert("power_source".into(), json!(source));
        // WHY it is unknown, because the three reasons call for different
        // moves: a controller that cannot measure, a device with no controller,
        // and a row that is not a board at all.
        if let Some(w) = why {
            o.insert("power_unknown_because".into(), json!(w));
        }
    }
}

/// §K3. Resolve the `devices` selector into the stores a search must visit.
///
/// `None` means the caller did not ask for a cross-device search and the
/// ordinary single-device path applies.
///
/// The exclusion rule is the SAME one the G4 selector fix uses, deliberately
/// reusing it rather than restating it: derived sub-devices (`<console>#dmesg`)
/// and `file:` pseudo-devices are not boards, and a question like "has this
/// error appeared anywhere?" means anywhere on the BENCH. `include_derived`
/// opts them back in for the caller who really does mean every store.
fn resolve_device_set(ctx: &Context, a: &Map<String, Value>) -> Result<Option<DeviceSet>> {
    let Some(spec) = a.get("devices") else {
        return Ok(None);
    };
    if a.contains_key("device") {
        return Err(ToolError::invalid_arg(
            "`device` and `devices` are mutually exclusive: one names a console, the other names \
             a set of them",
        ));
    }
    // A session or an epoch is a number that means something on ONE device.
    for k in ["session", "boot"] {
        if a.contains_key(k) {
            return Err(ToolError::invalid_arg(format!(
                "`{k}` is device-scoped: a {k} id means nothing across devices, so it cannot be \
                 combined with `devices`. Search one device for that, or drop `{k}`."
            )));
        }
    }
    let include_derived = flag(a, "include_derived");
    let reg = ctx.registry();
    let mut out: Vec<DeviceRow> = match spec {
        Value::String(sel) if sel == "all" => reg.all_devices()?,
        Value::String(sel) => reg.resolve_all(sel)?,
        Value::Array(list) => {
            let mut v = Vec::new();
            for item in list {
                let sel = item.as_str().ok_or_else(|| {
                    ToolError::invalid_arg("`devices` entries must be selector strings")
                })?;
                v.push(reg.resolve(sel)?);
            }
            v
        }
        _ => {
            return Err(ToolError::invalid_arg(
                "`devices` must be \"all\", a list of selectors, or a tag query",
            ))
        }
    };
    out.retain(|d| {
        if d.ignored {
            return false;
        }
        include_derived || (!d.canonical.starts_with("file:") && !d.canonical.contains('#'))
    });
    if out.is_empty() {
        return Err(ToolError::new(
            ErrorCode::UnknownDevice,
            "no devices match that selector once excluded ports, mined files and derived \
             sub-devices are removed",
        )
        .with_hint("pass include_derived:true to search file: and #-suffixed stores too"));
    }
    // Deterministic order, so a paginated search resumes where it left off.
    out.sort_by(|a, b| a.canonical.cmp(&b.canonical));
    out.dedup_by(|a, b| a.id == b.id);

    // §P1. A peer's board has no store here.
    //
    // `all_devices` includes the rows this node holds on a peer's behalf, and
    // handing one to the local store path is the bug this guards: `with_store`
    // fails INTERNAL with "owned by node ... there is no local store to read",
    // and one peer-owned console took down the whole fan-out:
    // search_raw({devices:"all"}) died on the first `peer:<node>/...` row.
    //
    // Dropping them quietly is the WRONG repair. `devices: "all"` asks "has
    // this appeared anywhere on the bench", and an answer that silently means
    // "anywhere on this node" is a false negative in the one tool whose job is
    // to find the occurrence. So they come back named, the caller is told the
    // answer is partial, and an all-remote match is an error rather than an
    // empty result.
    let (remote, local): (Vec<DeviceRow>, Vec<DeviceRow>) =
        out.into_iter().partition(|d| d.kind.is_remote());
    if local.is_empty() {
        let node = remote
            .first()
            .and_then(|d| d.node.clone())
            .unwrap_or_default();
        return Err(ToolError::new(
            ErrorCode::UnknownDevice,
            format!(
                "every device matching that selector is owned by another node ({} of them); \
                 this node has no store for any of them",
                remote.len()
            ),
        )
        .with_hint(format!(
            "search one of them directly, which federates: search({{\"device\": \"{node}/<selector>\"}})"
        ))
        .with_detail(json!({
            "elsewhere": remote.iter().map(|d| json!({
                "device": d.display_name(),
                "node": d.node.clone().unwrap_or_default(),
            })).collect::<Vec<_>>(),
        })));
    }
    Ok(Some(DeviceSet { local, remote }))
}

/// The devices a cross-device call resolved to, split by who owns them.
///
/// `remote` is carried rather than discarded so the response can say the search
/// was partial. See `resolve_device_set`.
struct DeviceSet {
    local: Vec<DeviceRow>,
    remote: Vec<DeviceRow>,
}

/// Run one device's search and attribute every hit to it.
fn search_one(
    ctx: &Context,
    d: &DeviceRow,
    q: &SearchQuery,
    cursor: Option<&str>,
) -> Result<(Vec<Value>, Option<String>, bool)> {
    let mut q = q.clone();
    q.after_offset = match cursor {
        Some(c) => Some(ctx.with_store(d, |st| {
            st.resolve_cursor(&conminer_core::store::Cursor::decode(c)?)
        })?),
        None => None,
    };
    ctx.with_store(d, |st| {
        let r = conminer_core::search::search(st, &q)?;
        let next = r.next_offset.map(|o| st.cursor_at(o).encode());
        // Named `attribution` rather than `name`: this is a LABEL for a
        // response row, never a hook argument, and §N1's guard forbids the
        // display name reaching a hook for good reason. Keeping the two
        // visibly different is cheaper than teaching the guard an exception.
        let attribution = d.display_name().to_string();
        let hits = r
            .hits
            .into_iter()
            .map(|h| {
                let mut v = serde_json::to_value(h).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    // Attribution on EVERY hit: a cross-device result set whose
                    // rows do not say where they came from is a list of strings.
                    o.insert("device".into(), json!(attribution));
                }
                v
            })
            .collect::<Vec<_>>();
        Ok((hits, next, r.capped))
    })
}

/// §K3. Search several stores and interleave the answers.
fn search_across(
    ctx: &Context,
    devices: &[DeviceRow],
    elsewhere: &[DeviceRow],
    q: &SearchQuery,
    max: usize,
    cursor: Option<&str>,
) -> Result<Value> {
    // The top-level cursor is a map of per-device cursors, so resuming carries
    // each store on from its own position rather than restarting the slow ones.
    let resume: std::collections::BTreeMap<String, String> = match cursor {
        Some(c) => serde_json::from_str(c).map_err(|_| {
            ToolError::new(
                ErrorCode::InvalidCursor,
                "this cursor did not come from a cross-device search",
            )
            .with_hint("pass back exactly what `next_cursor` returned")
        })?,
        None => Default::default(),
    };

    let mut per_device: Vec<(String, Vec<Value>, Option<String>, bool)> = Vec::new();
    for d in devices {
        // Sequential and per-store on purpose: each query is 25-45 ms, and a
        // cross-store index would be a second source of truth to keep correct.
        let (hits, next, capped) =
            search_one(ctx, d, q, resume.get(d.display_name()).map(String::as_str))?;
        per_device.push((d.display_name().to_string(), hits, next, capped));
    }

    // ROUND-ROBIN, so one chatty board cannot starve the rest. A board that
    // prints the error every second would otherwise fill `max_results` before
    // the board that printed it once ever got a turn -- and that one occurrence
    // is usually the interesting one.
    let mut hits: Vec<Value> = Vec::new();
    let mut idx = 0usize;
    while hits.len() < max {
        let mut took = false;
        for (_, dev_hits, _, _) in per_device.iter() {
            if let Some(h) = dev_hits.get(idx) {
                hits.push(h.clone());
                took = true;
                if hits.len() >= max {
                    break;
                }
            }
        }
        if !took {
            break;
        }
        idx += 1;
    }

    let mut next_map: std::collections::BTreeMap<String, String> = Default::default();
    let by_device: Vec<Value> = per_device
        .iter()
        .map(|(name, dev_hits, next, capped)| {
            if let Some(c) = next {
                next_map.insert(name.clone(), c.clone());
            }
            json!({
                "device": name,
                "hits": dev_hits.len(),
                "capped": capped,
                "next_cursor": next,
            })
        })
        .collect();
    let more = hits.len() >= max || !next_map.is_empty();
    // What was not looked at is part of the answer. A caller asking "anywhere
    // on the bench" who is handed only this node's boards, with nothing saying
    // so, reads an empty result as "it never happened".
    let not_searched: Vec<Value> = elsewhere
        .iter()
        .map(|d| {
            json!({
                "device": d.display_name(),
                "node": d.node.clone().unwrap_or_default(),
            })
        })
        .collect();
    let mut payload = json!({
        "hits": hits,
        "by_device": by_device,
        "devices_searched": devices.len(),
        "capped": more,
        "next_cursor": (!next_map.is_empty())
            .then(|| serde_json::to_string(&next_map).unwrap_or_default()),
    });
    if !not_searched.is_empty() {
        let o = payload.as_object_mut().expect("just built an object");
        o.insert("not_searched".into(), json!(not_searched));
        o.insert(
            "partial_because".into(),
            json!(format!(
                "{} device(s) here are owned by other nodes and have no store on this one; \
                 search them directly with <node>/<selector>, which federates",
                not_searched.len()
            )),
        );
    }
    Ok(payload)
}

/// Is the stored capture state contradicted by what this probe just saw?
///
/// Returns the stored state when it should be called stale, so the caller can
/// name it. Pure, because the shape that matters is a combination of four
/// facts and every one of them has to be tried: an integration test against a
/// socket can only produce whichever shape the timing happens to give it, and
/// an idle connection legitimately reports either "no error" or "timed out".
///
/// Two rules.
///
/// A probe that saw NOTHING proves nothing. This hint exists for a console that
/// RECOVERED while minerd sat idle, and recovery is proven by bytes arriving.
/// The test was `connected && !open_failed && error.is_null()`, which a probe
/// that connects, reads zero bytes and returns no error also passes -- so a
/// board sitting in EDL was told minerd had failed to re-attach. Whether
/// an agent saw that came down to which way an empty read returned, since the
/// identical probe with `error="timed out"` stayed silent.
///
/// And never call a state stale that this same call just confirmed. A
/// board-scoped EDL detection corroborates `away_in_edl`: capture is parked
/// because the UART re-enumerated away, which is the design. Where bytes really
/// are flowing during EDL, `verdict` reports that divergence in terms of what is
/// actually wrong rather than blaming a re-attach that is not owed.
pub fn capture_state_is_stale<'a>(
    stored: &'a str,
    probe: Option<&Value>,
    edl: bool,
) -> Option<&'a str> {
    if !matches!(stored, "open_failed" | "away_in_edl") {
        return None;
    }
    if edl && stored == "away_in_edl" {
        return None;
    }
    let p = probe?;
    let read_something = p["bytes_received"].as_u64().unwrap_or(0) > 0;
    let clean = read_something
        && p["connected"] == true
        && p["open_failed"] != true
        && p["error"].is_null();
    clean.then_some(stored)
}

/// Everything `power` works out before it touches a board.
///
/// Its own type so that a second tool can press power through this exact path.
/// A second copy of the press, its verification and its escalation would drift
/// from this one the way the two inventory builders did, and the difference
/// would be found on hardware.
struct PowerPlan {
    hook: conminer_core::config::ResolvedHook,
    timeout: std::time::Duration,
    device: String,
    settle: String,
    controller: String,
}

impl PowerPlan {
    fn resolve(ctx: &Context, d: &DeviceRow) -> Result<Self> {
        let present = present_with_topology(ctx);
        let hook = ctx
            .config()
            .power_hook_for_at(
                d.display_name(),
                &d.canonical,
                d.by_path.as_deref(),
                present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
            )
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::HookNotConfigured,
                    format!("no power hook for {}", d.display_name()),
                )
                .with_hint(
                    "configure [devices.<id>.hooks].power, or add a [[controllers]] \
                     profile whose `controls` glob matches this console",
                )
            })?;
        // The CONTROLLER decides how long its own hooks may take: a Bughopper
        // claims a USB interface and holds PM_RESIN_N for 6s (~35s wall),
        // which the 30s global default killed mid-action -- so `power off`
        // and `cycle` on that board always returned HOOK_TIMEOUT, sometimes
        // having actuated and sometimes not.
        let timeout = std::time::Duration::from_secs(
            hook.power_timeout_s
                .unwrap_or(ctx.config().hooks.power_timeout_s),
        );
        // Hooks get the canonical path, never the display name.
        // A nickname is how a HUMAN or an agent selects a device; it is
        // not a hardware identifier. Substituting it into `{device}` fed
        // a nickname to a hook that resolves an FTDI by its by-id path,
        // which then matched four devices and failed DEVICE_GONE, so
        // naming a board permanently broke its power control.
        let name = d.canonical.clone();
        let settle = format!("{}", hook.off_settle_s);
        let controller = hook.controller.clone().unwrap_or_default();
        Ok(Self {
            hook,
            timeout,
            device: name,
            settle,
            controller,
        })
    }

    fn args<'a>(&'a self, action: &'a str) -> [(&'static str, &'a str); 4] {
        [
            ("action", action),
            ("device", self.device.as_str()),
            ("controller", self.controller.as_str()),
            ("off_settle", self.settle.as_str()),
        ]
    }
}

/// Press power, verify what the BOARD did, open the epochs, and hand any
/// escalation to its own thread. The caller has already claimed the board.
///
/// Takes the claim by value because an escalation outlives this call and has to
/// keep the board claimed until it is done.
#[allow(clippy::too_many_arguments)]
fn run_power(
    ctx: &Context,
    scope: &ActuationScope,
    d: &DeviceRow,
    action: conminer_core::hooks::PowerAction,
    plan: &PowerPlan,
    label: Option<&str>,
    poke: bool,
    in_flight: crate::state::ActuationGuard,
    started: std::time::Instant,
) -> Result<Value> {
    use conminer_core::hooks;
    let hook = &plan.hook;
    let timeout = plan.timeout;
    let args = plan.args(action.as_str());
    let poked = if action.as_str() == "off" && poke {
        Some(poke_console(ctx, d)?)
    } else {
        None
    };
    let marks = stream_marks(ctx, scope);
    let result = block_on(hooks::run(&hook.template, &args, timeout))?;

    // ---- verify the ACTION, not the exit code ----------------
    //
    // A hook returning 0 means "the command ran", never "the board
    // did what you asked". Stress testing found both halves of that
    // gap on real hardware:
    //   * a Bughopper `off` that reported ok while the console kept
    //     talking (~1 in 20): a 6s CBUS hold that did not latch as a
    //     PMIC long-press.
    //   * a `reset` that reported ok, opened a fresh epoch, and
    //     captured ZERO bytes: the board wedged after ~8 resets and
    //     no further reset could recover it. Every hook kept saying
    //     ok, so a loop reading exit codes would spin forever.
    //
    // So conminer checks what it can see and escalates once. It is
    // the lowest layer in the stack: if it lies, everything above it
    // inherits the lie.
    in_flight.phase("verify");
    let (verified, escalation) = verify_power_effect(
        ctx,
        d,
        &scope.watched,
        action.as_str(),
        hook,
        timeout,
        poked,
    );

    let event = json!({"action": action, "hook": result, "effect": verified});
    let opened = open_actuation_epochs(ctx, scope, "power", label, "power", &event, &marks)?;

    // The caller gets its answer when escalation is decided, not
    // When it is done. The epoch is open, the hook has run, and the
    // rest -- two more presses and half a minute of settling -- runs
    // here on its own thread, holding the board's claim the whole
    // way so nobody actuates into the middle of it. What it finds is
    // left for actuation_status; the caller's `effect` says so.
    let outcome_ids: Vec<i64> = scope.consoles.iter().map(|c| c.id).collect();
    let started_ms = ctx.now();
    let base = json!({
        "tool": "power",
        "action": action,
        "target": scope.target,
        "device": d.display_name(),
        "boot_id": opened.first().and_then(|o| o.get("boot_id").cloned()),
        "started_ms": started_ms,
    });
    match escalation {
        Some(cont) => {
            let bg = ctx.clone();
            let guard = in_flight;
            let consoles = scope.consoles.clone();
            let action_name = action.as_str().to_string();
            let escalation_of = opened
                .first()
                .and_then(|o| o.get("boot_id").and_then(Value::as_i64));
            std::thread::Builder::new()
                .name("power-escalation".into())
                .spawn(move || {
                    let ids = guard.ids().to_vec();
                    let effect = cont(&bg, &ids);
                    // DURABLE, on every console: the epoch's own
                    // event said "escalation running"; this is how
                    // it ended. console_state's decay reads it, and
                    // an mcpd restart does not lose it.
                    let now = bg.now();
                    let ev = json!({
                        "action": action_name,
                        "effect": effect,
                        "escalation_of": escalation_of,
                        "source": "escalation",
                    });
                    for c in &consoles {
                        let _ = bg.with_store(c, |st| {
                            let session = st.latest_session()?.map(|s| s.id);
                            let boot = st.latest_boot()?.map(|b| b.id);
                            st.append_event(session, boot, now, "power", &ev)?;
                            Ok(())
                        });
                    }
                    let mut done = base;
                    if let Some(o) = done.as_object_mut() {
                        o.insert("effect".into(), effect);
                        o.insert("finished_ms".into(), json!(now));
                    }
                    bg.record_actuation_outcome(&ids, done);
                    drop(guard);
                })
                .map_err(|e| {
                    ToolError::new(
                        ErrorCode::Internal,
                        format!("could not start the escalation thread: {e}"),
                    )
                })?;
        }
        None => {
            let mut done = base;
            if let Some(o) = done.as_object_mut() {
                o.insert("effect".into(), verified.clone());
                o.insert("finished_ms".into(), json!(ctx.now()));
            }
            ctx.record_actuation_outcome(&outcome_ids, done);
        }
    }

    // The primary's epoch stays at the top level so every existing
    // caller keeps working unchanged; `opened` is the whole picture.
    let first = opened.first().cloned().unwrap_or(Value::Null);
    let mut payload = json!({
        "boot_id": first.get("boot_id").cloned().unwrap_or(Value::Null),
        "boot_seq": first.get("boot_seq").cloned().unwrap_or(Value::Null),
        "cursor": first.get("cursor").cloned().unwrap_or(Value::Null),
        "hook": result,
        // What the BOARD did, as distinct from what the hook
        // returned. An agent that only reads `hook` learns nothing
        // about whether the board complied.
        "effect": verified,
        // How long the whole workflow held the board. A caller whose
        // client gave up before this arrived reads it from the next
        // call and learns what timeout the board actually needs.
        "duration_ms": started.elapsed().as_millis() as u64,
    });
    if let Some(o) = payload.as_object_mut() {
        if scope.target.is_some() {
            o.insert("target".into(), json!(scope.target));
            o.insert("opened".into(), json!(opened));
            o.insert("exempt_not_consoles".into(), json!(scope.exempt));
        }
        if let Some(n) = &scope.note {
            o.insert("note".into(), json!(n));
        }
    }
    Ok(payload)
}

fn probe_power_state(ctx: &Context, d: &DeviceRow) -> Option<String> {
    use conminer_core::hooks;
    let present = present_with_topology(ctx);
    let hook = ctx.config().power_state_hook_for_at(
        &d.canonical,
        d.by_path.as_deref(),
        present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
    )?;
    let controller = hook.controller.clone().unwrap_or_default();
    // HOOKS GET THE CANONICAL PATH, never the display name.
    // A nickname is how a HUMAN or an agent selects a device; it is
    // not a hardware identifier. Substituting it into `{device}` fed
    // "adp-ventuno" to a hook that resolves an FTDI by its by-id
    // path, which then matched four devices and failed DEVICE_GONE --
    // so naming a board permanently broke its power control.
    let name = d.canonical.clone();
    // A PROBE, not an actuation: shared with anyone already waiting on the same
    // controller for the same question. See `hooks::probe` -- nothing older than
    // the request is ever served, so this stays exact.
    let res = block_on(hooks::probe(
        &hook.template,
        &[
            ("device", &name),
            ("controller", &controller),
            ("action", ""),
        ],
        std::time::Duration::from_secs(15),
    ))
    .ok()?;
    // Parse the FIRST token of the first non-empty line, by equality.
    //
    // Substring matching is not good enough and this is not hypothetical: the
    // Bughopper answers "unknown (commanded-only controller; cbus=...)" and a
    // `contains("on")` matched the "on" inside "cONtroller", so a board that
    // cannot report its power read as ON -- including while it was off. A power
    // indicator that invents a state is worse than one that admits ignorance.
    let first = res
        .stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match first.as_str() {
        "on" => Some("on".into()),
        "off" => Some("off".into()),
        _ => None,
    }
}

/// Cap every array in an object payload, recording what was omitted.
///
/// The analysis tools answer with several parallel lists at once, and an
/// unbounded one is how a single question eats a session's budget: measured,
/// `missing_in_boot` returned 84KB. Truncating silently would be worse than the
/// size, because these lists are absence evidence -- a short list reads as "and
/// nothing else", so every cut is counted next to the list it came from.
fn cap_lists(payload: Value, limit: usize) -> Value {
    let Value::Object(mut map) = payload else {
        return payload;
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        let Some(Value::Array(items)) = map.get(&k) else {
            continue;
        };
        if items.len() <= limit {
            continue;
        }
        let omitted = items.len() - limit;
        let kept: Vec<Value> = items.iter().take(limit).cloned().collect();
        map.insert(k.clone(), Value::Array(kept));
        map.insert(format!("{k}_omitted"), json!(omitted));
    }
    Value::Object(map)
}

/// Resolve a caller-supplied file path.
///
/// A BARE FILENAME lands in the shared export directory, which is bind-mounted
/// to the host. `export_session` and `ingest_file` previously took only
/// container-local paths, so a 173KB archive written by a tool call could not be
/// retrieved without `docker cp` -- the feature stopped one step short of being
/// usable. An absolute path is still honoured exactly as given, for a caller who
/// has mapped something themselves.
/// Where a container path shows up on the host, when conminer can tell.
///
/// Only the shared export directory is mapped, so that is the only path with an
/// honest answer; anything else says so rather than guessing at the operator's
/// bind mounts.
fn host_hint(path: &std::path::Path) -> serde_json::Value {
    match path.strip_prefix(EXPORT_DIR) {
        Ok(rest) => json!(format!("./exports/{}", rest.display())),
        Err(_) => json!(serde_json::Value::Null),
    }
}

fn shared_path(raw: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(raw);
    if p.is_absolute() || raw.contains('/') {
        return p;
    }
    std::path::PathBuf::from(EXPORT_DIR).join(raw)
}

/// Bind-mounted to ./exports on the host by compose, so a file written here is
/// reachable without reaching into the container.
const EXPORT_DIR: &str = "/exports";

fn broker_read_path(ctx: &Context, d: &DeviceRow) -> (std::path::PathBuf, String) {
    (
        conminer_core::broker::socket_path(&ctx.config().paths.run_dir),
        d.display_name().to_string(),
    )
}

/// Clear USB entries that are listed but no longer answer.
///
/// Presence is not liveness: a device whose descriptors the hub still caches
/// looks identical to a live one in `lsusb`, and any automation keying on
/// presence for "the board is up" or "the board is in EDL" is then wrong. Best
/// effort -- a device this dead may refuse a reset too, and the honest outcomes
/// are a count of what was cleared and a warning naming what was not.
/// QDL gadgets currently listed on the bus but not answering.
fn stale_qdl_on_bus() -> usize {
    conminer_core::usb::scan()
        .iter()
        .filter(|d| d.is_qdl() && d.is_zombie())
        .count()
}

fn sweep_usb_zombies() -> usize {
    let devices = conminer_core::usb::scan();
    let ghosts = conminer_core::usb::zombies(&devices);
    if ghosts.is_empty() {
        return 0;
    }
    let mut cleared = 0;
    for g in ghosts {
        if conminer_core::usb::clear_zombie(g) {
            tracing::info!(
                vid = format!("{:04x}", g.vendor_id),
                pid = format!("{:04x}", g.product_id),
                "cleared a stale USB entry left by a powered-off board"
            );
            cleared += 1;
        } else {
            tracing::warn!(
                vid = format!("{:04x}", g.vendor_id),
                pid = format!("{:04x}", g.product_id),
                bus = g.bus,
                address = g.address,
                "a stale USB entry will not clear; power-cycle the hub port (uhubctl) or replug"
            );
        }
    }
    cleared
}

/// The rest of an actuation that must not keep the caller waiting.
///
/// An `off` on a board found alive in EDL escalates to reset-then-off: two more
/// hook invocations and 33 s of settling, on top of the press already made.
/// Run inline, that is the one path of `power` that outlives every client
/// timeout -- report #12 was exactly that, and #14 was the caller giving up and
/// issuing the next actuation into the middle of it. So the caller gets its
/// answer when escalation is DECIDED, with the epoch already open, and this
/// continuation runs on its own thread holding the board's in-flight claim
/// until the effect is known. `actuation_status` reads what it leaves behind.
type Escalation = Box<dyn FnOnce(&Context, &[i64]) -> Value + Send + 'static>;

fn verify_power_effect(
    ctx: &Context,
    d: &DeviceRow,
    watched: &[DeviceRow],
    action: &str,
    hook: &conminer_core::config::ResolvedHook,
    timeout: std::time::Duration,
    // Did a pre-press poke get an answer? `Some(true)` means the board was
    // demonstrably alive a moment ago, which is what makes its silence
    // afterwards evidence rather than an absence.
    poked_alive: Option<bool>,
) -> (Value, Option<Escalation>) {
    use conminer_core::hooks;
    use std::time::{Duration, Instant};

    // How long to allow. A board that boots slowly is not a failure. All four
    // windows are config knobs (`[hooks]`), because they are hardware timings:
    // a bench with faster boards should not wait out ours, and a test rig should
    // not wait at all.
    let hk = &ctx.config().hooks;
    let edl_settle = Duration::from_secs(hk.edl_settle_s);
    let settle = Duration::from_secs(hk.verify_settle_s);
    let watch = match action {
        "off" => Duration::from_secs(hk.verify_off_watch_s),
        _ => Duration::from_secs(hk.verify_boot_watch_s),
    };

    // EVERY CONSOLE OF THE BOARD, not just the one whose hook ran.
    //
    // Measured on the IQ10 the moment target actuation shipped: `power {target}`
    // reported `verified: false` and escalated to a power cycle on a board that
    // had booted perfectly -- because verification watched the primary console
    // while a SIBLING was the one doing the talking. That is the same
    // epoch-stranding this whole feature exists to fix, one layer down: a board
    // responded if ANY of its consoles did, and the quiet one proves nothing.
    let bytes = |ctx: &Context| -> (u64, u64) {
        let mut total = 0u64;
        // The freshest console decides idleness: one silent port must not make a
        // talkative board look idle.
        let mut idle = u64::MAX;
        for dev in watched {
            if let Ok(f) = ctx.freshness(dev) {
                total += f
                    .get("bytes_this_boot")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                idle = idle.min(f.get("idle_ms").and_then(Value::as_u64).unwrap_or(u64::MAX));
            }
        }
        (total, if idle == u64::MAX { 0 } else { idle })
    };

    // THE BOARD'S OWN USB PORT IS THE BEST EVIDENCE OF "OFF" THERE IS.
    //
    // A board that loses power leaves the bus: measured on the Uno-Q, port
    // 2-2.4 is simply absent from sysfs while it is off. That is positive proof,
    // it costs a directory lookup, and it works on a controller with no power
    // sense at all -- where console silence proves nothing and a operator was
    // made to wait 80 seconds to be told so.
    let ports = declared_usb_ports(ctx, d);
    let port_before = board_ports_present(&ports);

    // Was the console SAYING anything before we acted?
    //
    // "off" is verified by the console going quiet -- but silence proves nothing
    // if it was already silent. Measured on the ADP: a `power off` issued while
    // the board sat in EDL (where the console is silent by design) reported
    // success while the board stayed enumerated and alive. The hook exited 0 and
    // nothing checked the effect, so the false positive was total.
    let before = bytes(ctx);
    // A poke that was answered IS the console talking, and it is better evidence
    // than the byte counter: it happened just now, on demand, rather than at
    // some point in the recent past.
    let was_talking = poked_alive.unwrap_or(before.0 > 0 && before.1 < 5_000);

    std::thread::sleep(settle);

    // ...and when it can answer, ask it FIRST and cheaply. Polling a path costs
    // nothing next to the console watch below, which for an already-silent
    // console cannot succeed at all.
    let evidence = off_evidence(was_talking, !ports.is_empty(), port_before);
    if action == "off" && evidence == OffEvidence::WatchUsb {
        let by = Instant::now() + watch;
        while Instant::now() < by {
            if !board_ports_present(&ports) {
                return (
                    json!({
                        "verified": true,
                        "action": action,
                        "escalated": false,
                        "why": format!(
                            "this board's own USB port(s) {} left the bus, which is what losing power \
                             looks like from the host -- confirmed without needing the console to say \
                             anything",
                            ports.join(", ")
                        ),
                        "evidence": {"usb_ports": ports, "present_before": true, "present_after": false},
                    }),
                    None,
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    // NO POINT WATCHING FOR SILENCE THAT WAS ALREADY THERE.
    //
    // "off" is confirmed by the console going quiet, and a console that was
    // already quiet cannot go quieter. Sitting out the full window changes
    // nothing about the answer and is most of why three power-offs read as
    // hangs: ~35 s of hook, then 12 s of watching for an event that cannot
    // occur, then a USB rescan.
    if action == "off" && evidence == OffEvidence::Impossible {
        return (
            json!({
                "verified": false,
                "action": action,
                "escalated": false,
                // SAY WHICH ONE. This used to offer both possibilities at once
                // -- "no `usb_ports` attributed, or it was already off the bus"
                // -- and then advise tagging the ports. On a board whose ports
                // ARE tagged that reads as a configuration mistake that does
                // not exist, and sends whoever reads it looking for the wrong
                // thing (it cost exactly that on uno-q, whose `usb_ports` tag
                // was present the whole time while its port simply was not on
                // the bus). The two cases need different actions, so they get
                // different sentences.
                "why": if ports.is_empty() {
                    "cannot confirm: this console was already silent before the action, and no \
                     `usb_ports` are attributed to this board, so there is no presence to watch \
                     leave the bus. The hook ran and reported success; nothing here can raise that \
                     to proof. Tag the board's ports, or use a controller with a power sense."
                        .to_string()
                } else {
                    format!(
                        "cannot confirm: this console was already silent before the action, and \
                         this board's port(s) {} were not on the bus beforehand either -- a board \
                         that shows no USB while it runs has no presence to lose. The hook ran and \
                         reported success; nothing here can raise that to proof. A controller with \
                         a power sense is the only thing that could.",
                        ports.join(", ")
                    )
                },
                "evidence": {"was_talking": false, "usb_ports": ports, "port_present_before": port_before},
            }),
            None,
        );
    }

    let deadline = Instant::now() + watch;
    let mut last = bytes(ctx);
    let mut ok = false;
    let mut polls = 0u32;
    let mut edl_during_watch = false;

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(750));
        let now = bytes(ctx);
        match action {
            // Off means the console goes and STAYS quiet: idle climbing while
            // the byte count holds still.
            "off" => {
                // Quiet AND staying quiet -- but only counted when there was
                // something to go quiet FROM. A board that was already silent
                // (in EDL, or idle at a prompt) cannot be confirmed off this way,
                // and saying so beats inventing a confirmation.
                ok = was_talking && now.1 > last.1 && now.0 <= last.0;
            }
            // Everything else means the board came back and SAID something.
            // An epoch that opens and captures nothing is the wedge signature.
            _ => {
                if now.0 > 0 {
                    ok = true;
                    break;
                }
                // ...unless it came back into EDL, where the console is silent
                // BY DESIGN and no amount of further waiting will produce a
                // byte. A reset with the EDL strap set used to sit here for the
                // full 30 s watch before answering, though the QDL gadget
                // enumerates within about five seconds of the reset. Watching
                // for the gadget as well as for bytes ends the wait when the
                // answer is actually available.
                polls += 1;
                if polls % 4 == 0
                    && conminer_core::usb::in_edl_on_ports(&conminer_core::usb::scan(), &ports)
                {
                    edl_during_watch = true;
                    break;
                }
            }
        }
        last = now;
    }

    // A SILENT CONSOLE IS NOT PROOF OF "OFF". Ask USB before believing it.
    //
    // Measured on the ADP: with `qcom_scm.download_mode=1` on the cmdline, a
    // long RESIN press ~15s into boot produces a WARM RESET INTO DOWNLOAD MODE
    // rather than a power-off. The console goes quiet exactly as it would if the
    // board had died, so byte-counting called it `verified: true` while the
    // board sat in EDL with a live QDL gadget -- diagnose proved it seconds
    // later. That is the worst possible result: an agent believes a board is off
    // and moves on, leaving it powered and in a flashing-capable state.
    //
    // conminer already owns the probe that settles this, so it must consult it
    // before claiming success, not only when the console-based check fails.
    //
    // And the probe has to WATCH, not sample: the warm reset drops the QDL
    // gadget for several seconds before it comes back, so a single scan taken in
    // that gap reports a clean bus for a board sitting in EDL. That gap is
    // exactly when this check runs, since it runs right after the press.
    let edl = if edl_during_watch {
        // Already proven live during the watch loop; nothing left to establish.
        conminer_core::usb::EdlProbe {
            in_edl: true,
            waited_ms: 0,
            settled: false,
            stale_qdl: 0,
        }
    } else if action == "off" {
        conminer_core::usb::watch_for_edl_on_ports(edl_settle, &ports)
    } else {
        conminer_core::usb::EdlProbe {
            in_edl: conminer_core::usb::in_edl_on_ports(&conminer_core::usb::scan(), &ports),
            waited_ms: 0,
            settled: false,
            stale_qdl: 0,
        }
    };
    if ok && action == "off" && edl.in_edl {
        return (
            json!({
                "verified": false,
                "action": action,
                "escalated": false,
                "edl_probe_ms": edl.waited_ms,
                "why": "the console went silent, but a live 05c6 QDL gadget is answering on USB: this \
                        board went into EDL, not off. With qcom_scm.download_mode=1 a long press during \
                        early boot warm-resets into download mode, and the silence looks identical to a \
                        power-off. Power off from steady state, or exit EDL with a reset first.",
            }),
            None,
        );
    }

    if ok {
        // The board is down; sweep the USB entries it left behind.
        //
        // Measured on the ADP: after a verified-off, its gadget stayed listed for
        // MINUTES with cached descriptors while every real read failed and the
        // hub never saw a detach (a faulty Type-C controller). A stale entry
        // makes the next EDL or flash session target a device that is not there,
        // so clearing it is conminer's job, not a note in a runbook.
        let cleared = sweep_usb_zombies();
        if cleared > 0 {
            return (
                json!({
                    "verified": true,
                    "action": action,
                    "escalated": false,
                    "usb_zombies_cleared": cleared,
                }),
                None,
            );
        }
        return (
            json!({"verified": true, "action": action, "escalated": false}),
            None,
        );
    }
    // An `off` that did not take because the board is in EDL: RECOVER, do not
    // just report it.
    //
    // Measured on the ADP: in EDL the PBL ignores the controller's 6s
    // PM_RESIN_N press, so `off` returns 0 and the board stays enumerated and
    // alive. The sequence that works -- and that a human had to know and run by
    // hand -- is `reset` (which exits EDL into a normal boot) and then `off`.
    // conminer knows the board is in EDL because the QDL gadget is answering, so
    // it can do that itself.
    if action == "off" && edl.in_edl {
        tracing::warn!(
            "power off did not take: the board is alive in EDL, where this controller's \
             press is ignored. Exiting EDL with a reset, then powering off."
        );
        let settle_s = format!("{}", hook.off_settle_s);
        let controller = hook.controller.clone().unwrap_or_default();
        // HOOKS GET THE CANONICAL PATH, never the display name.
        // A nickname is how a HUMAN or an agent selects a device; it is
        // not a hardware identifier. Substituting it into `{device}` fed
        // "adp-ventuno" to a hook that resolves an FTDI by its by-id
        // path, which then matched four devices and failed DEVICE_GONE --
        // so naming a board permanently broke its power control.
        let name = d.canonical.clone();
        let template = hook.template.clone();
        let action_owned = action.to_string();
        let ports = ports.clone();
        let expected_ms: u64 = 2 * timeout.as_millis() as u64 + 33_000;
        let cont: Escalation = Box::new(move |ctx: &Context, ids: &[i64]| {
            let run = |act: &str| {
                let _ = block_on(hooks::run(
                    &template,
                    &[
                        ("action", act),
                        ("device", &name),
                        ("controller", &controller),
                        ("off_settle", &settle_s),
                    ],
                    timeout,
                ));
            };
            let t0 = std::time::Instant::now();
            ctx.set_actuation_phase(ids, "escalation: reset (leaving EDL)");
            run("reset");
            let reset_ms = t0.elapsed().as_millis() as u64;
            // Let the board leave EDL and reach a state where the press is
            // honoured. A FLOOR, NOT A POLL: "the QDL gadget is gone" is not
            // the condition. With qcom_scm.download_mode=1 a long press during
            // EARLY boot warm-resets straight back into download mode
            // (measured on the ADP, above), so pressing as soon as EDL drops
            // would re-create the state this escalation exists to leave. The
            // board has to get past early boot first, and 25 s is what that
            // was measured to need.
            ctx.set_actuation_phase(ids, "escalation: waiting past early boot (25 s)");
            std::thread::sleep(Duration::from_secs(25));
            ctx.set_actuation_phase(ids, "escalation: off");
            let t1 = std::time::Instant::now();
            run("off");
            let off_ms = t1.elapsed().as_millis() as u64;
            ctx.set_actuation_phase(ids, "escalation: settling (8 s)");
            std::thread::sleep(Duration::from_secs(8));
            let still_edl =
                conminer_core::usb::in_edl_on_ports(&conminer_core::usb::scan(), &ports);
            json!({
                "verified": !still_edl,
                "action": action_owned,
                "escalated": true,
                "escalation": {
                    "kind": "reset-then-off",
                    "state": "done",
                    // Where the time went: two full hook invocations plus 33 s
                    // of settling on top of the original press.
                    "ms": {
                        "reset_hook": reset_ms,
                        "wait_past_early_boot": 25_000,
                        "off_hook": off_ms,
                        "settle": 8_000,
                        "total": t0.elapsed().as_millis() as u64,
                    },
                },
                "why": if still_edl {
                    "the board is STILL in EDL after a reset-then-off; its PMIC is not honouring \
                     the press. Power-cycle the hub port or pull the board's power."
                } else {
                    "the board was alive in EDL, where this controller's press is ignored; \
                     conminer exited EDL with a reset and powered off"
                },
            })
        });
        return (
            json!({
                // NOT `false`. `verified: false` means "checked, and the board is
                // NOT off" -- a terminal verdict. The escalation has not
                // concluded, so the honest value is "unknown yet". Report #18:
                // an off answered `verified: false` mid-escalation, the board
                // was visibly booting (the escalation's OWN reset), and the
                // agent concluded the off had failed and an on had won -- then
                // filed an overlap that never happened (the store shows no
                // accepted on during the escalation; the guard held). A pending
                // verdict plus "expect a reset" removes both false signals.
                "verified": Value::Null,
                "pending": true,
                "action": action,
                "escalated": true,
                "escalation": {
                    "kind": "reset-then-off",
                    "state": "running",
                    "expected_ms": expected_ms,
                    "poll": "actuation_status",
                    // Say it outright: this off REBOOTS the board before it
                    // powers it down. A boot appearing now is this workflow, not
                    // an independent power-on, and not a failed off.
                    "note": "completing this off REQUIRES resetting the board out of EDL first, \
                             so it will boot once more before it powers off; that boot is part of \
                             the off, not a new power-on",
                },
                "why": "the console went quiet but a live QDL gadget is answering: the board is \
                        alive in EDL, where this controller's press is ignored. conminer is exiting \
                        EDL with a reset and will power off once the board is past early boot. The \
                        off is NOT done and NOT failed -- it is pending (verified: null); the board \
                        stays claimed (ACTUATION_IN_FLIGHT) so no on/off can interleave, and \
                        actuation_status(device) carries the terminal outcome. Expect one more \
                        boot as part of this off.",
            }),
            Some(cont),
        );
    }

    // Name the case where verification was IMPOSSIBLE rather than failed: the
    // caller's next move differs completely.
    if action == "off" && !was_talking {
        // Say only what the probe actually established. "No gadget right now" and
        // "this board is not in EDL" are different claims, and an agent acts on
        // the difference: the second one tells it to stop looking.
        let edl_note =
            if edl.excludes_edl() && edl.stale_qdl > 0 {
                // A LISTED-BUT-DEAD gadget is neither of the two answers above, and
                // saying "no QDL gadget appeared" while one sits on the bus is the
                // same wrong-fact failure as claiming the board is not in EDL from a
                // scan taken mid-re-enumeration. Measured on the ADP: `off` from EDL
                // left 05c6:9008 enumerated and unresponsive.
                // REPORT WHAT IS ON THE BUS AFTERWARDS, not how many reset() calls
                // returned true. Measured on the ADP: the sweep said "cleared 0"
                // while `lsusb` showed the entry gone -- opening a zombie usually
                // fails, so that number counts attempts, not outcomes, and the
                // response contradicted the hardware. Counting exit codes instead of
                // checking effects is the exact bug this verifier exists to avoid.
                sweep_usb_zombies();
                let remaining = stale_qdl_on_bus();
                let outcome = if remaining == 0 {
                    "conminer cleared it; the bus is clean now".to_string()
                } else {
                    format!(
                    "{remaining} could not be cleared from userspace -- power-cycle the hub port \
                     (uhubctl) or replug, because a stale 9008 is what the next flash session \
                     would target"
                )
                };
                format!(
                "conminer then watched USB for {} s: no LIVE QDL gadget answered, so the board is \
                 not in EDL -- but {} stale QDL entr{} still listed on the bus (enumerated, not \
                 answering), which is what this class of board leaves behind when it drops out of \
                 EDL. {outcome}.",
                edl.waited_ms / 1000,
                edl.stale_qdl,
                if edl.stale_qdl == 1 { "y was" } else { "ies were" },
            )
            } else if edl.excludes_edl() {
                format!(
                "conminer then watched USB for {} s and no QDL gadget appeared, so the board is \
                 not in EDL.",
                edl.waited_ms / 1000
            )
            } else {
                format!(
                "conminer saw no QDL gadget in the {} ms it watched, which does NOT exclude EDL: \
                 a warm reset into download mode drops the gadget off the bus for several seconds \
                 before it re-enumerates. Re-check USB before concluding the board is off.",
                edl.waited_ms
            )
            };
        // CLEAR WHAT THE BOARD LEFT BEHIND, on this path too.
        //
        // The sweep used to run only when the off was confirmed, or when the
        // stale entry happened to be a QDL gadget. Measured on the ADP: powering
        // off a board that was idle at a login prompt takes the
        // already-silent path, so its `18d1:d002` ADB gadget stayed enumerated
        // and unanswering -- conminer reported `usb_zombies: 1` and left it
        // there. Detecting a mess for the operator and not clearing it is half a
        // job; the next flash or EDL session is the one that pays.
        let swept = sweep_usb_zombies();
        let ghosts_left = conminer_core::usb::zombies(&conminer_core::usb::scan()).len();
        // Measured on the ADP: a truly dead entry survives both a USBDEVFS reset
        // and the kernel's own logical disconnect, because that board's Type-C
        // controller never signals detach. Naming the remedy beats reporting a
        // number the caller cannot act on.
        let ghost_note = if ghosts_left > 0 {
            format!(
                " {ghosts_left} stale USB entr{} still listed after the sweep: userspace cannot \
                 clear those (both a port reset and a logical disconnect are refused by a device \
                 this dead) -- power-cycle the hub port with uhubctl, or replug the board.",
                if ghosts_left == 1 { "y is" } else { "ies are" }
            )
        } else {
            String::new()
        };
        return (
            json!({
                "verified": false,
                "action": action,
                "escalated": false,
                "edl_probe_ms": edl.waited_ms,
                "edl_excluded": edl.excludes_edl(),
                "stale_qdl_entries": edl.stale_qdl,
                "usb_zombies_cleared": swept,
                "usb_zombies_remaining": ghosts_left,
                "stale_qdl_remaining": stale_qdl_on_bus(),
                "why": format!(
                    "cannot confirm: this console was already silent before the action \
                     (a board idle at a prompt looks exactly like one that is off). Check the \
                     controller's power sense if it has one; {edl_note}{ghost_note}"
                ),
            }),
            None,
        );
    }

    // A BOARD IN EDL IS NOT A FAILURE, and must not be "recovered" from.
    //
    // Entering EDL is deliberate work: the strap is set, a reset is issued, and
    // the console goes silent BY DESIGN. Byte-counting saw silence, waited out
    // the full watch window (~70s), escalated to a power cycle nobody asked for,
    // and returned "it may need physical attention" -- while diagnose, called
    // seconds later, knew exactly that the board was in EDL. An unrequested
    // power cycle is a state-destroying action on someone else's rig.
    if conminer_core::usb::in_edl_on_ports(&conminer_core::usb::scan(), &ports) {
        return (
            json!({
                "verified": true,
                "action": action,
                "escalated": false,
                "why": "the board is in EDL (a live 05c6 QDL gadget is answering), where the console is \
                        silent by design. That is the expected result of a reset with the EDL strap set, \
                        not a failure -- so nothing was escalated.",
            }),
            None,
        );
    }

    // One escalation, and only where it is meaningful. A reset that produced a
    // dead board is recovered by a power cycle -- measured on the IQ10, which
    // wedged after eight resets and came back instantly from a cycle.
    let escalation = match action {
        // A reset that did nothing escalates to a power cycle. Measured on two
        // boards and it matters on both:
        //   * IQ10: wedges after ~8 consecutive resets; only a cycle recovers it.
        //   * NordAU RIDE SX: `reset` is a PMIC power-BUTTON tap and does not
        //     reset a running board AT ALL -- swept 0.3s..10s holds and
        //     MD_RESOUT_N never dipped. Without this escalation an automation
        //     loop would "reset" that board forever while nothing happened.
        "reset" | "on" => Some("cycle"),
        "off" => Some("off"), // try once more: a long-press that did not latch
        _ => None,
    };
    let Some(esc) = escalation else {
        return (
            json!({"verified": false, "action": action, "escalated": false,
                      "why": "the board did not do what the hook reported"}),
            None,
        );
    };

    tracing::warn!(
        device = %d.display_name(), action, escalate_to = esc,
        "power action reported success but the console disagrees; escalating once"
    );
    let settle_s = format!("{}", hook.off_settle_s);
    let controller = hook.controller.clone().unwrap_or_default();
    // HOOKS GET THE CANONICAL PATH, never the display name.
    // A nickname is how a HUMAN or an agent selects a device; it is
    // not a hardware identifier. Substituting it into `{device}` fed
    // "adp-ventuno" to a hook that resolves an FTDI by its by-id
    // path, which then matched four devices and failed DEVICE_GONE --
    // so naming a board permanently broke its power control.
    let name = d.canonical.clone();
    let _ = block_on(hooks::run(
        &hook.template,
        &[
            ("action", esc),
            ("device", &name),
            ("controller", &controller),
            ("off_settle", &settle_s),
        ],
        timeout,
    ));

    std::thread::sleep(Duration::from_secs(if esc == "off" { 6 } else { 20 }));
    let after = bytes(ctx);
    let recovered = if action == "off" {
        after.1 > 3_000
    } else {
        after.0 > 0
    };
    (
        json!({
            "verified": recovered,
            "action": action,
            "escalated": true,
            "escalated_to": esc,
            "why": if recovered {
                "the hook reported success but the board disagreed; recovered by escalating"
            } else {
                "the board did not respond to the action OR the escalation -- it may need              physical attention"
            },
        }),
        None,
    )
}

/// Advertise the tool surface.
///
/// `full` ships every schema; otherwise only [`CORE_TOOLS`], with `help` as the
/// documented way to reach the rest. Callers are NOT restricted by this -- any
/// registered tool still dispatches by name; this only controls what is pushed
/// into the agent's context up front.
pub fn advertise_profile(full: bool) -> Value {
    json!({
        "tools": registry()
            .iter()
            .filter(|t| full || CORE_TOOLS.contains(&t.name))
            .map(|t| json!({
                "name": t.name,
                "description": described(t),
                "inputSchema": (t.schema)(),
            }))
            .collect::<Vec<_>>()
    })
}

/// A tool's description with its required arguments appended.
///
/// DERIVED, not written by hand, and that is the point. A sweep of the surface
/// found seven tools whose prose named different arguments than their schema --
/// `get_records` said "limit" and meant `n`, `create_watch` read like it took a
/// pattern and wanted `name`/`until`, `bisect_report` said "build" and meant
/// `candidate` -- each costing an agent a round-trip to discover. Twenty-two
/// tools named none of their required arguments at all.
///
/// Hand-editing every description would fix today and drift by next week.
/// Generating the line from the schema means prose and schema CANNOT disagree,
/// and a tool added tomorrow is documented the moment it is registered.
pub fn described(t: &Tool) -> String {
    let schema = (t.schema)();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if required.is_empty() {
        return t.description.to_string();
    }
    let list = required
        .iter()
        .map(|r| format!("`{r}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} Required arguments: {list}.", t.description)
}

pub fn advertise() -> Value {
    advertise_profile(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_advertises_an_object_schema_with_no_extra_properties() {
        for t in registry() {
            let s = (t.schema)();
            assert_eq!(s["type"], "object", "{}", t.name);
            assert_eq!(
                s["additionalProperties"], false,
                "{} must reject unknown arguments so a typo is an error, not a silent no-op",
                t.name
            );
            assert!(!t.description.is_empty(), "{}", t.name);
            assert!(
                t.description.len() > 40,
                "{} needs a description an agent can choose from",
                t.name
            );
        }
    }

    /// The complete advertised surface: §8.1 read tools, §8.2 `follow`, §8.3
    /// interaction, §8.4 epochs, §15 lab integration.
    ///
    /// Pinned as a literal because `tools/list` is the *only* thing an agent
    /// sees — a registry that silently stopped short would leave every tool
    /// past the truncation point unreachable while every other test in this
    /// file still passed, since they all ask about tools by name.
    const CATALOG: &[&str] = &[
        "acquire",
        "actuation_status",
        "annotate_template",
        "attach_evidence",
        "backfill_versions",
        "bisect_report",
        "bisect_start",
        "bisect_status",
        "boot_mode",
        "boot_report",
        "boot_stages",
        "claim_exclusive",
        "classify_prompt",
        "confirm_report",
        "console_state",
        "create_watch",
        "decode",
        "delete_watch",
        "diagnose",
        "diff_boots",
        "diff_builds",
        "diff_sessions",
        "end_session",
        "evaluate_policy",
        "export_session",
        "flash",
        "follow",
        "forget_device",
        "get_context",
        "get_prompts",
        "get_recent",
        "get_records",
        "help",
        "identify",
        "ingest_file",
        "ingest_pstore",
        "learn_expectations",
        "list_baselines",
        "list_bisects",
        "list_boots",
        "list_devices",
        "list_metrics",
        "list_profiles",
        "list_reports",
        "list_sessions",
        "list_targets",
        "list_templates",
        "list_verdicts",
        "list_watches",
        "mark",
        "metric_series",
        "missing_in_boot",
        "name_device",
        "name_target",
        "noise",
        "peer_announce",
        "peer_poll",
        "peer_result",
        "peers",
        "pin_metric",
        "poll_watch",
        "power",
        "provenance",
        "prune",
        "pull_file",
        "push_file",
        "rebuild_templates",
        "release",
        "report_issue",
        "resolve_report",
        "run_command",
        "search",
        "search_raw",
        "selftest",
        "send",
        "set_baseline",
        "set_image",
        "set_line",
        "snapshot_dmesg",
        "stage_timings",
        "start_session",
        "stats",
        "symbolize",
        "tag_device",
        "target_context",
        "target_mark",
        "template_detail",
        "template_values",
        "timeline",
        "transfer_file",
        "unpin_metric",
    ];

    #[test]
    fn the_advertised_catalog_is_exactly_the_spec_surface() {
        // The FULL profile is the spec surface. `tools/list` advertises the core
        // profile by default (see CORE_TOOLS), because schemas were 65% of a
        // 45KB payload paid at the start of every session -- but every tool
        // still exists and still dispatches by name, so the catalog check has to
        // be made against the full set or it silently stops checking anything.
        let mut advertised: Vec<&'static str> = advertise_profile(true)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let s = t["name"].as_str().unwrap();
                // Leak-free: the names are &'static str in the registry, so
                // look each one back up rather than borrowing the temporary.
                find(s).unwrap().name
            })
            .collect();
        advertised.sort();
        assert_eq!(
            advertised, CATALOG,
            "the full profile must be exactly the spec's tool surface"
        );

        // And the core profile must be a strict subset that stays callable.
        for name in CORE_TOOLS {
            assert!(find(name).is_some(), "core tool {name} is not registered");
        }
    }

    #[test]
    fn tool_names_are_unique_and_stable() {
        let mut names: Vec<&str> = registry().iter().map(|t| t.name).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n);
    }

    #[test]
    fn the_uart_mcp_parity_tools_exist() {
        // §2: prompts written for uart-mcp must port trivially.
        assert!(find("search_raw").is_some(), "≈ query_serial_logs");
        assert!(find("get_recent").is_some(), "≈ get_recent_logs");
        assert!(find("list_devices").is_some(), "≈ get_serial_status");
        assert!(find("stats").is_some(), "≈ get_log_buffer_info");
    }

    #[test]
    fn mutating_tools_are_marked_so_the_lease_rule_can_be_enforced() {
        for name in [
            "ingest_file",
            "name_device",
            "tag_device",
            "mark",
            "export_session",
        ] {
            assert!(find(name).unwrap().mutating, "{name}");
        }
        for name in [
            "list_devices",
            "list_templates",
            "search",
            "get_context",
            "boot_report",
        ] {
            assert!(!find(name).unwrap().mutating, "{name}");
        }
    }

    #[test]
    fn advertise_is_valid_and_complete() {
        let a = advertise();
        let tools = a["tools"].as_array().unwrap();
        assert_eq!(tools.len(), registry().len());
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["inputSchema"]["type"] == "object");
        }
    }
}

#[cfg(test)]
mod verdict_order_tests {
    use super::console_verdict;
    use serde_json::json;

    fn probe(bytes: i64, open_failed: bool) -> serde_json::Value {
        json!({"connected": true, "bytes_received": bytes, "open_failed": open_failed})
    }

    /// EDL BEATS THE WEDGE, because in EDL there is nothing to wedge.
    ///
    /// Reported from a live flashing session on the Uno-Q: EDL entry succeeded,
    /// `edl=true` was published alongside -- and the verdict still said the
    /// console was wedged and needed a ser2net restart, because the failure
    /// banner is bytes and the wedge arm was reached first. An operator
    /// following that advice restarts ser2net for every console on the host,
    /// mid-flash, to fix nothing.
    #[test]
    fn a_board_in_edl_is_not_a_wedged_console() {
        let ep = "tcp://0.0.0.0:5017".to_string();
        let v = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(120, true)),
            true,
            None,
            "none",
        );
        assert!(v.contains("EDL"), "EDL must be the explanation: {v}");
        assert!(v.contains("Expected"), "...and named as expected: {v}");
        // The ADVICE, not the vocabulary: this sentence says "not a wedged
        // console" and "no restart is needed", so matching those words would
        // fail on the very text that is correct. What must never appear is the
        // wedge arm's instruction.
        assert!(
            !v.contains("needs a restart or"),
            "...and it must not send anybody restarting ser2net: {v}"
        );

        // The same shape WITHOUT EDL is still a wedge -- that verdict is right
        // and must survive; this is the difference the flag makes.
        let w = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(120, true)),
            false,
            None,
            "none",
        );
        // The INSTRUCTION, not the word: this verdict was reworded to stop
        // asserting contention it cannot see, and an assertion on vocabulary
        // duly failed on text that was already correct.
        assert!(
            w.contains("needs a restart"),
            "without EDL, a restart must still be offered as the remedy: {w}"
        );
        assert!(
            w.contains("Check power first"),
            "...after the cause that costs nothing to check: {w}"
        );
    }

    /// The rest of the ladder keeps its order: a silent board in EDL, a board
    /// that is off, and a live one are three different sentences.
    #[test]
    fn the_verdict_ladder_says_the_most_useful_true_thing_first() {
        let ep = "tcp://0.0.0.0:5017".to_string();
        let silent_edl = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(0, false)),
            true,
            None,
            "none",
        );
        assert!(silent_edl.contains("EDL"), "{silent_edl}");

        let off = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(0, false)),
            false,
            Some("off"),
            "none",
        );
        assert!(off.contains("POWERED OFF"), "{off}");

        let live = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(4096, false)),
            false,
            None,
            "none",
        );
        assert!(live.contains("delivering data"), "{live}");

        // An excluded row explains itself before anything about endpoints.
        let ignored = console_verdict(
            true,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(0, false)),
            true,
            None,
            "excluded",
        );
        assert_eq!(ignored, "excluded");
    }

    /// Report #19: a silent probe while the console sits at a recognised,
    /// commandable prompt must NOT read as "maybe powered off" -- an idle shell
    /// is silent by definition, and the verdict has the prompt in front of it.
    ///
    /// The controller here has no power sense (power_state is None), which is
    /// exactly when the old ladder fell through to "the board may be powered
    /// off" and contradicted the console block in the same response.
    #[test]
    fn a_silent_probe_at_a_commandable_prompt_is_idle_not_off() {
        let ep = "tcp://0.0.0.0:5017".to_string();
        for state in ["at_prompt", "at_prompt_with_traffic"] {
            let v = console_verdict(
                false,
                "listening",
                state,
                Some(&ep),
                Some(&probe(0, false)),
                false,
                None, // no power sense, as on the Uno-Q's Bughopper
                "none",
            );
            assert!(
                !v.contains("powered off") && !v.contains("may be"),
                "a prompt in the buffer must not read as maybe-off ({state}): {v}"
            );
            assert!(
                v.contains("idle") && v.contains("commandable prompt"),
                "it must name the idle-at-a-prompt reading ({state}): {v}"
            );
        }
    }

    /// And with no prompt, the honest fallback is unchanged: unknown power plus
    /// silence really can be off, and the verdict still says so.
    #[test]
    fn a_silent_probe_with_no_prompt_still_admits_maybe_off() {
        let ep = "tcp://0.0.0.0:5017".to_string();
        let v = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&probe(0, false)),
            false,
            None,
            "none",
        );
        assert!(v.contains("may be powered off"), "{v}");
    }
}

#[cfg(test)]
mod probe_bytes_tests {
    use super::console_verdict;
    use serde_json::json;

    /// SER2NET'S OWN ERROR TEXT IS NOT CONSOLE DATA.
    ///
    /// Reported from the bench while flashing: one diagnose response said the
    /// console was wedged AND reported bytes received, so it read as "conminer
    /// is calling ser2net's failure text delivered console data". It was: the
    /// probe counted the banner in `bytes_received`.
    ///
    /// The verdict must not fall through to "delivering data" for a banner-only
    /// read -- which is exactly what a zeroed byte count would cause if the
    /// ladder were ordered by bytes rather than by `open_failed`.
    #[test]
    fn a_banner_only_read_is_never_delivering_data() {
        let ep = "tcp://0.0.0.0:5017".to_string();
        // What the probe now produces for a banner: no board bytes, the text
        // moved aside, the flag set.
        let p = json!({
            "connected": true,
            "bytes_received": 0,
            "open_failed": true,
            "server_message": "Device open failure: No such file or directory",
        });
        let v = console_verdict(
            false,
            "listening",
            "listening",
            Some(&ep),
            Some(&p),
            false,
            None,
            "none",
        );
        assert!(
            !v.contains("delivering data"),
            "a console that delivered nothing must not read as delivering: {v}"
        );
        assert!(
            v.contains("nothing here came from the board"),
            "...and it must say whose bytes those were: {v}"
        );
        // It must also stop asserting contention it cannot see: a board that is
        // simply off produces this exact symptom.
        assert!(
            v.contains("board off"),
            "...and offer the cause an operator should check first: {v}"
        );
    }
}

#[cfg(test)]
mod off_verification_tests {
    use super::{off_evidence, ports_present_under, OffEvidence};

    /// EIGHTY SECONDS TO SAY "I CANNOT TELL" IS THE BUG.
    ///
    /// Measured on the Uno-Q: `power off` took 81s, 82s and 122s and answered
    /// `verified: false` every time. The hook is ~35s by design (it claims the
    /// FTDI and holds PM_RESIN_N for six seconds); the rest was conminer
    /// watching an already-silent console for twelve seconds -- an event that
    /// cannot occur -- and then rescanning twelve USB devices.
    ///
    /// The board's own USB port is the evidence that does exist: measured, port
    /// 2-2.4 is absent from sysfs while that board is off.
    #[test]
    fn an_off_waits_only_for_evidence_that_can_actually_arrive() {
        // The good case: the board is on the bus, so watch for it to leave.
        assert_eq!(off_evidence(false, true, true), OffEvidence::WatchUsb);
        // USB beats the console even when both are available: it is proof
        // rather than an absence, and it costs a directory lookup.
        assert_eq!(off_evidence(true, true, true), OffEvidence::WatchUsb);
        // No USB attribution, but the console was talking: silence means
        // something.
        assert_eq!(off_evidence(true, false, false), OffEvidence::WatchConsole);
        // Neither. THIS is the case that used to cost twelve seconds of watching
        // followed by "cannot confirm".
        assert_eq!(off_evidence(false, false, false), OffEvidence::Impossible);
        // Ports attributed but the board was already off the bus: nothing to
        // lose, so nothing to wait for.
        assert_eq!(off_evidence(false, true, false), OffEvidence::Impossible);
    }

    /// Presence is a path test, not a device open. `usb::scan()` opens every
    /// device on the host with a 300 ms control read -- seconds on a twelve
    /// device bench -- which is the wrong tool for "did this port go away".
    #[test]
    fn port_presence_is_a_directory_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("2-2.4")).unwrap();
        assert!(ports_present_under(root, &["2-2.4".to_string()]));
        assert!(!ports_present_under(root, &["3-1.2".to_string()]));
        // Any of the board's ports counts: a board may enumerate more than one.
        assert!(ports_present_under(
            root,
            &["3-1.2".to_string(), "2-2.4".to_string()]
        ));
        assert!(!ports_present_under(root, &[]));
    }
}

#[cfg(test)]
mod edl_verdict_tests {
    use super::*;

    const NO_ENDPOINT: &str = "no endpoint";

    fn probe(connected: bool, bytes: i64, open_failed: bool) -> Value {
        json!({"connected": connected, "bytes_received": bytes, "open_failed": open_failed})
    }

    fn verdict(p: &Value, edl: bool) -> &'static str {
        console_verdict(
            false,
            "present",
            "present",
            Some(&"127.0.0.1:5017".to_string()),
            Some(p),
            edl,
            None,
            NO_ENDPOINT,
        )
    }

    /// EDL EXPLAINS EVERY SHAPE OF SILENCE, INCLUDING A REFUSED CONNECTION.
    ///
    /// Reported twice. The first time the board was in EDL and ser2net was
    /// serving its device-open failure, which read as a wedged console; that rung
    /// was fixed. It came back one rung higher: when the tty is gone at config
    /// time the port is not served at all, the probe cannot connect, and the
    /// headline sent the reader to check ser2net while the board sat exactly
    /// where they had just put it.
    ///
    /// Executed, because the gate that was supposed to hold this was a grep over
    /// this file for the words "silent by design" -- which were still there, and
    /// still true, in a branch the ladder never reached.
    #[test]
    fn edl_explains_the_console_however_the_probe_fails() {
        for (name, p) in [
            ("refused connection", probe(false, 0, false)),
            ("device-open failure", probe(true, 0, true)),
            ("connected but silent", probe(true, 0, false)),
        ] {
            let v = verdict(&p, true);
            assert!(
                v.contains("EDL"),
                "{name} in EDL must be explained by EDL, not by ser2net: {v}"
            );
            assert!(
                !v.contains("is ser2net listening"),
                "{name}: EDL must not read as a broken ser2net: {v}"
            );
        }
    }

    /// ...and the generic advice still stands when EDL is NOT the reason, or the
    /// fix above would simply hide a real broken endpoint.
    #[test]
    fn a_refused_connection_without_edl_still_points_at_ser2net() {
        let v = verdict(&probe(false, 0, false), false);
        assert!(
            v.contains("is ser2net listening"),
            "a refused connection with no EDL evidence is still a ser2net question: {v}"
        );
    }
}
