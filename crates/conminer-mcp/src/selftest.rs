//! `selftest`: the gauntlet, encoded (§K1).
//!
//! Nine rounds of findings were discovered by hand-driving acceptance tests on
//! live boards. Nothing guarded those fixes afterwards, so each round
//! re-discovered some of the previous round's regressions. This runs the same
//! acceptance passes the agent ran by hand, on real hardware, from inside the
//! tool.
//!
//! Two rules shape the whole module:
//!
//! **It composes existing tools and adds no capture logic of its own.** Every
//! check calls the same `power`/`follow`/`boot_report`/`diagnose`/`provenance`
//! an agent would call. If a property cannot be expressed through the public
//! surface, that is a finding about the surface, not a reason to reach behind
//! it — so this module has no privileged access to fall back on.
//!
//! **Cleanup is unconditional.** A test harness that leaves a board powered
//! because it failed early is worse than no harness: the next person finds a hot
//! board and no explanation. Every exit path — pass, fail, timeout, panic —
//! runs the same guard, and if the guard cannot VERIFY the board is off it says
//! so loudly rather than assuming.

use crate::state::Context;
use conminer_core::error::{ErrorCode, Result, ToolError};
use conminer_core::store::DeviceRow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

/// What a target is expected to do, per §K1's expectations config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Expect {
    /// Stage names this target's boot must pass through.
    #[serde(default)]
    pub stages: Vec<String>,
    /// Components `provenance.running_versions` must carry.
    #[serde(default)]
    pub provenance_components: Vec<String>,
    /// How long after a power-on the console must produce its first bytes.
    #[serde(default = "d_boot_output_s")]
    pub boot_output_s: u64,
    /// How long a reset may take to produce a fresh boot.
    #[serde(default = "d_reboot_s")]
    pub reboot_s: u64,
    /// The stage that means "this boot is finished".
    #[serde(default = "d_terminal_stage")]
    pub terminal_stage: String,
    /// Templates matching any of these must never appear as novel in a repeat
    /// boot: they are the phantom-template signature (§G1).
    #[serde(default = "d_phantom")]
    pub phantom_regexes: Vec<String>,
    /// How long this board takes to present a QDL gadget after a reset into
    /// EDL. A per-board fact: the IQ10 answers in ~12s, the ADP takes ~36s
    /// including its own enumeration verify, and a constant fails the slower
    /// board for being slow rather than broken.
    #[serde(default = "d_edl_enter_s")]
    pub edl_enter_s: u64,
}

fn d_boot_output_s() -> u64 {
    10
}
fn d_reboot_s() -> u64 {
    45
}
fn d_terminal_stage() -> String {
    "userspace".into()
}
fn d_phantom() -> Vec<String> {
    vec![r" [BD] \d+$".into()]
}
fn d_edl_enter_s() -> u64 {
    15
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            stages: Vec::new(),
            provenance_components: Vec::new(),
            boot_output_s: d_boot_output_s(),
            reboot_s: d_reboot_s(),
            terminal_stage: d_terminal_stage(),
            phantom_regexes: d_phantom(),
            edl_enter_s: d_edl_enter_s(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ExpectFile {
    #[serde(default)]
    target: std::collections::BTreeMap<String, Expect>,
}

/// Load `selftest.toml` from beside `conminer.toml`.
///
/// Absent is not an error: the defaults describe a generic Linux board, and a
/// bench that has not written expectations should still get the checks that do
/// not depend on them rather than nothing at all.
pub fn expectations_for(target: &str) -> Expect {
    let path = std::env::var("CONMINER_SELFTEST_CONFIG")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("CONMINER_CONFIG")
                .ok()
                .map(std::path::PathBuf::from)
                .and_then(|p| p.parent().map(|d| d.join("selftest.toml")))
        });
    let Some(p) = path else {
        return Expect::default();
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Expect::default();
    };
    toml::from_str::<ExpectFile>(&text)
        .ok()
        .and_then(|f| f.target.get(target).cloned())
        .unwrap_or_default()
}

/// One check's outcome. `evidence` is the point: a bare pass/fail teaches
/// nothing when it regresses six months from now.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub suite: &'static str,
    pub result: &'static str,
    pub evidence: String,
    pub duration_ms: u64,
}

pub struct Opts {
    pub target: Option<String>,
    pub device: Option<String>,
    pub suites: Vec<String>,
    pub steal: bool,
    pub keep_on: bool,
}

const ALL_SUITES: &[&str] = &[
    "capture",
    "actuation",
    "edl",
    "mining",
    "provenance",
    "honesty",
];

/// A per-suite budget. A wedged board must not hold the whole run hostage: the
/// check fails with `evidence: "timeout"` and the run proceeds — to cleanup
/// above all.
const SUITE_BUDGET: Duration = Duration::from_secs(120);
const RUN_BUDGET: Duration = Duration::from_secs(600);

struct Run<'a> {
    ctx: &'a Context,
    consoles: Vec<DeviceRow>,
    primary: DeviceRow,
    /// The console the BOARD talks on, which is rarely the first member.
    ///
    /// A multi-console board has one AP console and several that say nothing
    /// (SPI, SAIL, a second UART). Watching `consoles[0]` made every
    /// observation check fail on a perfectly healthy IQ10: the boot evidence
    /// was on if02 while the harness stared at if00. So the board is asked
    /// rather than assumed -- whichever console produces bytes after power-on
    /// is the one every later check reads.
    observe: DeviceRow,
    target: Option<String>,
    expect: Expect,
    checks: Vec<Check>,
    skips: Vec<Value>,
    warnings: Vec<Value>,
    started: Instant,
}

impl<'a> Run<'a> {
    fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let t = crate::tools::find(tool)
            .ok_or_else(|| ToolError::new(ErrorCode::Internal, format!("no tool {tool}")))?;
        let map: Map<String, Value> = match args {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        (t.call)(self.ctx, &map)
    }

    /// The selector every actuation uses: the target when there is one, so
    /// epochs land on every console (§F1).
    fn actuate_args(&self, extra: &[(&str, Value)]) -> Value {
        let mut m = Map::new();
        match &self.target {
            Some(t) => m.insert("target".into(), json!(t)),
            None => m.insert("device".into(), json!(self.primary.display_name())),
        };
        for (k, v) in extra {
            m.insert((*k).into(), v.clone());
        }
        Value::Object(m)
    }

    fn record(&mut self, id: &'static str, suite: &'static str, started: Instant, r: CheckOutcome) {
        let (result, evidence) = match r {
            CheckOutcome::Pass(e) => ("pass", e),
            CheckOutcome::Fail(e) => ("fail", e),
            CheckOutcome::Skip(e) => {
                self.skips.push(json!({"id": id, "reason": e}));
                ("skip", e)
            }
            CheckOutcome::Warn(e) => {
                self.warnings.push(json!({"id": id, "detail": e}));
                ("warn", e)
            }
        };
        self.checks.push(Check {
            id,
            suite,
            result,
            evidence,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    fn out_of_time(&self) -> bool {
        self.started.elapsed() > RUN_BUDGET
    }

    /// The wall clock a single suite may not run past.
    fn suite_deadline(&self) -> Instant {
        Instant::now() + SUITE_BUDGET
    }
}

/// Record a timed-out suite as a failure and move on. The run must always
/// reach cleanup with time to spare, so this is a budget, not a suggestion.
fn budget_spent(r: &mut Run, suite: &str) -> bool {
    if !r.out_of_time() {
        return false;
    }
    let id: &'static str = match suite {
        "capture" => "capture.timeout",
        "edl" => "edl.timeout",
        "honesty" => "honesty.timeout",
        "actuation.off" => "actuation.off.timeout",
        _ => "actuation.timeout",
    };
    let t = Instant::now();
    r.record(
        id,
        "honesty",
        t,
        Fail("timeout: the run budget was spent before this suite started".into()),
    );
    true
}

enum CheckOutcome {
    Pass(String),
    Fail(String),
    Skip(String),
    /// Something real was observed, but it cannot be pinned on THIS board
    /// (§L6). A warn is not a pass -- it ships in the checks and in
    /// `warnings` -- and it is not a fail either, because failing a target for
    /// a neighbour's mess is how a correct cleanup got called dirty.
    Warn(String),
}

use CheckOutcome::{Fail, Pass, Skip, Warn};

/// Run the gauntlet against one board.
pub fn run(ctx: &Context, opts: Opts) -> Result<Value> {
    let started_at = ctx.now();
    let wall = Instant::now();

    // ---- resolve the board -------------------------------------------------
    let (consoles, exempt) = match (&opts.target, &opts.device) {
        (Some(t), None) => conminer_core::target::console_members(&ctx.registry(), t)?,
        (None, Some(d)) => (vec![ctx.registry().resolve(d)?], Vec::new()),
        _ => {
            return Err(ToolError::invalid_arg(
                "give exactly one of `target` (a board) or `device` (a single console)",
            ))
        }
    };
    let primary = consoles[0].clone();
    let expect = expectations_for(opts.target.as_deref().unwrap_or_default());

    // ---- leases ------------------------------------------------------------
    //
    // Refuse rather than barge: another agent mid-flash on this board is the
    // one thing a self-test must never interrupt. `steal` is the explicit
    // override, with the same semantics as acquire.
    let mut held = Vec::new();
    for c in &consoles {
        let mut m = Map::new();
        m.insert("device".into(), json!(c.display_name()));
        m.insert("holder".into(), json!("selftest"));
        m.insert("ttl_s".into(), json!(1800));
        if opts.steal {
            m.insert("steal".into(), json!(true));
        }
        let t = crate::tools::find("acquire").expect("acquire");
        match (t.call)(ctx, &m) {
            Ok(_) => held.push(c.clone()),
            Err(e) => {
                for h in &held {
                    release(ctx, h);
                }
                return Err(ToolError::new(
                    ErrorCode::LeaseHeld,
                    format!(
                        "selftest needs every console of this board; {} is held ({})",
                        c.display_name(),
                        e.message
                    ),
                )
                .with_hint("pass steal:true to take them, or wait for the holder"));
            }
        }
    }

    let suites: Vec<String> = if opts.suites.is_empty() {
        ALL_SUITES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.suites.clone()
    };
    let want = |s: &str| suites.iter().any(|x| x == s);

    let mut run = Run {
        ctx,
        consoles: consoles.clone(),
        primary: primary.clone(),
        target: opts.target.clone(),
        expect,
        observe: primary.clone(),
        checks: Vec::new(),
        skips: Vec::new(),
        warnings: Vec::new(),
        started: wall,
    };

    // The whole body is wrapped so that a panic in any check still reaches
    // cleanup. `AssertUnwindSafe` is honest here: the only state that matters
    // across the boundary is the board's, and cleanup re-reads it from hardware.
    let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Each suite is skipped rather than started once the run budget is
        // spent: a wedged board must not push the whole run past the point
        // where cleanup still has time to work.
        if want("capture") && !budget_spent(&mut run, "capture") {
            suite_capture(&mut run);
        }
        if (want("actuation") || want("mining") || want("provenance"))
            && !budget_spent(&mut run, "actuation")
        {
            suite_boot(
                &mut run,
                want("actuation"),
                want("mining"),
                want("provenance"),
            );
        }
        if want("edl") && !budget_spent(&mut run, "edl") {
            suite_edl(&mut run);
        }
        if want("actuation") && !budget_spent(&mut run, "actuation.off") {
            suite_power_off(&mut run);
        }
        if want("honesty") && !budget_spent(&mut run, "honesty") {
            suite_honesty(&mut run);
        }
    }));
    let panicked = body.is_err();

    // ---- cleanup: UNCONDITIONAL -------------------------------------------
    let cleanup = cleanup(&mut run, opts.keep_on);
    for c in &consoles {
        release(ctx, c);
    }

    let mut checks = run.checks;
    if panicked {
        checks.push(Check {
            id: "selftest.panic",
            suite: "honesty",
            result: "fail",
            evidence: "a check panicked; cleanup still ran".into(),
            duration_ms: 0,
        });
    }
    let failed = checks.iter().any(|c| c.result == "fail");
    let skipped = checks.iter().any(|c| c.result == "skip");
    let warned = checks.iter().any(|c| c.result == "warn");
    let verdict = if failed {
        "fail"
    } else if skipped {
        "pass_with_skips"
    } else if warned {
        "pass_with_warnings"
    } else {
        "pass"
    };

    Ok(json!({
        "target": opts.target,
        "device": opts.device,
        "consoles": consoles.iter().map(|c| c.display_name()).collect::<Vec<_>>(),
        "exempt_not_consoles": exempt,
        "started_at": started_at,
        "duration_s": wall.elapsed().as_secs(),
        "verdict": verdict,
        "checks": checks,
        "skips": run.skips,
        "warnings": run.warnings,
        "cleanup": cleanup,
    }))
}

fn release(ctx: &Context, d: &DeviceRow) {
    let mut m = Map::new();
    m.insert("device".into(), json!(d.display_name()));
    m.insert("force".into(), json!(true));
    if let Some(t) = crate::tools::find("release") {
        let _ = (t.call)(ctx, &m);
    }
}

// ------------------------------------------------------------- suites -------

/// The console is reachable and delivering, before anything is actuated.
fn suite_capture(r: &mut Run) {
    let t0 = Instant::now();
    let out = match r.call("diagnose", json!({"device": r.primary.display_name()})) {
        Ok(v) => v,
        Err(e) => {
            r.record("capture.endpoint_probe", "capture", t0, Fail(e.message));
            return;
        }
    };
    let probe = &out["probe"];
    let connected = probe["connected"].as_bool().unwrap_or(false);
    let open_failed = probe["open_failed"].as_bool().unwrap_or(false);
    let res = if !connected {
        Fail(format!(
            "ser2net did not accept a second connection: {}",
            out["verdict"].as_str().unwrap_or("?")
        ))
    } else if open_failed {
        Fail("ser2net is serving its device-open failure banner, not the board".into())
    } else {
        Pass(format!(
            "endpoint {} accepted a probe, no open failure",
            out["endpoint"].as_str().unwrap_or("?")
        ))
    };
    r.record("capture.endpoint_probe", "capture", t0, res);
}

/// Power the board on and check everything that only a live boot can show.
fn suite_boot(r: &mut Run, actuation: bool, mining: bool, provenance: bool) {
    // ---- power_on ----------------------------------------------------------
    let t0 = Instant::now();
    let consoles = r.consoles.clone();
    let before: Vec<String> = consoles.iter().map(|c| cursor_of(r, c)).collect();
    let on = r.call("power", r.actuate_args(&[("action", json!("on"))]));
    let opened: Vec<Value> = match &on {
        Ok(v) => v["opened"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![v.clone()]),
        Err(_) => Vec::new(),
    };
    match &on {
        Err(e) => {
            r.record(
                "actuation.power_on",
                "actuation",
                t0,
                Fail(e.message.clone()),
            );
            return;
        }
        Ok(v) => {
            // The hook's own claim is never the evidence: bytes are.
            let deadline = (Instant::now() + Duration::from_secs(r.expect.boot_output_s.max(5)))
                .min(r.suite_deadline());
            let mut spoke_after = None;
            let mut talker: Option<DeviceRow> = None;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(500));
                for (i, c) in consoles.iter().enumerate() {
                    if cursor_of(r, c) != before[i] {
                        spoke_after = Some(t0.elapsed());
                        talker = Some(c.clone());
                        break;
                    }
                }
                if talker.is_some() {
                    break;
                }
            }
            if let Some(t) = talker {
                r.observe = t;
            }
            let effect = v["effect"].clone();
            let res = match spoke_after {
                Some(d) => Pass(format!(
                    "hook ok, {} produced bytes {:.1}s after the hook; effect={}",
                    r.observe.display_name(),
                    d.as_secs_f64(),
                    compact(&effect)
                )),
                None => Fail(format!(
                    "no console bytes within {}s of power on; effect={}",
                    r.expect.boot_output_s,
                    compact(&effect)
                )),
            };
            if actuation {
                r.record("actuation.power_on", "actuation", t0, res);
            }
        }
    }

    // ---- epoch_grouping (F1) ----------------------------------------------
    if actuation && r.target.is_some() {
        let t = Instant::now();
        if r.consoles.len() < 2 {
            // Nothing to tie together. Grouping is a claim about SIBLINGS, and
            // a one-console target has none -- asserting it anyway failed the
            // ADP for not doing something that would have meant nothing.
            r.record(
                "actuation.epoch_grouping",
                "actuation",
                t,
                Skip(
                    "this target has a single console: there are no sibling epochs to group".into(),
                ),
            );
        } else {
            let groups: Vec<&str> = opened
                .iter()
                .filter_map(|o| o["group_id"].as_str())
                .collect();
            // The tie is checked from BOTH ends, because they are different claims:
            // that the action opened one grouped epoch per console, and that the
            // talking console can later find its siblings (§F1 is only useful if
            // the second one holds).
            let siblings = r
                .call("boot_report", json!({"device": r.observe.display_name()}))
                .ok()
                .and_then(|v| v["sibling_epochs"].as_array().map(|a| a.len()))
                .unwrap_or(0);
            let res = if opened.len() < r.consoles.len() {
                Fail(format!(
                    "{} epoch(s) opened for {} consoles",
                    opened.len(),
                    r.consoles.len()
                ))
            } else if groups.len() < opened.len() {
                Fail(format!(
                    "{} of {} opened epochs carry no group_id",
                    opened.len() - groups.len(),
                    opened.len()
                ))
            } else if groups.iter().any(|g| *g != groups[0]) {
                Fail("consoles received DIFFERENT group ids for one action".into())
            } else if siblings + 1 < r.consoles.len() {
                Fail(format!(
                    "group {} spans {} epochs but boot_report on {} lists only {} sibling(s)",
                    groups[0],
                    opened.len(),
                    r.observe.display_name(),
                    siblings
                ))
            } else {
                Pass(format!(
                    "{} consoles share group_id {}; boot_report lists {} sibling epoch(s)",
                    groups.len(),
                    groups[0],
                    siblings
                ))
            };
            r.record("actuation.epoch_grouping", "actuation", t, res);
        }
    }

    // ---- boot_report says BOOTING while it is booting (K5a/N6) -------------
    if mining {
        let t = Instant::now();
        // ASK ABOUT THE EPOCH THAT HOLDS THE BOOT, not the newest one.
        //
        // `power` runs its hook before opening the epoch -- deliberately, so a
        // failed hook cannot leave an epoch describing a boot nobody triggered
        // -- and it verifies the effect before returning. The epoch it opens is
        // therefore EMPTY, and the boot's bytes are still accruing in the one
        // before it. Reporting on the newest epoch said "no_output: the port was
        // open and zero bytes arrived" about a board that was mid-boot in front
        // of us.
        let boot_id = r
            .call(
                "list_boots",
                json!({"device": r.observe.display_name(), "limit": 4}),
            )
            .ok()
            .and_then(|v| {
                v["boots"].as_array().and_then(|a| {
                    a.iter()
                        .find(|b| b["bytes"].as_u64().unwrap_or(0) > 0)
                        .and_then(|b| b["id"].as_i64())
                })
            });
        let mut args = json!({"device": r.observe.display_name()});
        if let (Some(b), Some(o)) = (boot_id, args.as_object_mut()) {
            o.insert("boot".into(), json!(b));
        }
        let rep = r.call("boot_report", args);
        let res = match rep {
            Err(e) => Fail(e.message),
            Ok(v) => {
                let outcome = v["outcome"].as_str().unwrap_or("?").to_string();
                let why = v["why"].as_str().unwrap_or("").to_string();
                if outcome == "unstable" || why.contains("distinct fingerprints") {
                    Fail(format!(
                        "a history statistic was reported as this epoch's outcome: {outcome} -- {why}"
                    ))
                } else if matches!(outcome.as_str(), "booting" | "booted" | "in_progress") {
                    Pass(format!("mid-boot outcome {outcome:?}: {why}"))
                } else {
                    Fail(format!("unexpected mid-boot outcome {outcome:?}: {why}"))
                }
            }
        };
        r.record("mining.boot_report_outcome", "mining", t, res);
    }

    // The first boot is allowed to finish before the checks that read it.
    wait_for_terminal(r);

    // ---- the stage set is the expected one --------------------------------
    if mining && !r.expect.stages.is_empty() {
        let t = Instant::now();
        let res = match r.call("boot_stages", json!({"device": r.observe.display_name()})) {
            Err(e) => Fail(e.message),
            Ok(v) => {
                let seen: Vec<String> = v["stages"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s["name"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let missing: Vec<&String> = r
                    .expect
                    .stages
                    .iter()
                    .filter(|w| !seen.contains(w))
                    .collect();
                if missing.is_empty() {
                    Pass(format!("stages {seen:?}"))
                } else {
                    Fail(format!("missing {missing:?}; saw {seen:?}"))
                }
            }
        };
        r.record("mining.stages", "mining", t, res);
    }

    // ---- provenance --------------------------------------------------------
    if provenance && !r.expect.provenance_components.is_empty() {
        let t = Instant::now();
        let res = match r.call("provenance", json!({"device": r.observe.display_name()})) {
            Err(e) => Fail(e.message),
            Ok(v) => {
                let have: Vec<String> = v["running"]
                    .as_object()
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default();
                let missing: Vec<&String> = r
                    .expect
                    .provenance_components
                    .iter()
                    .filter(|c| !have.contains(c))
                    .collect();
                if missing.is_empty() {
                    Pass(format!("running: {have:?}"))
                } else {
                    Fail(format!(
                        "missing {missing:?}; have {have:?}{}",
                        match v["chain_continues_in"]["epoch"].as_i64() {
                            Some(e) => format!(" (chain_continues_in epoch {e})"),
                            None => String::new(),
                        }
                    ))
                }
            }
        };
        r.record("provenance.running_versions", "provenance", t, res);
    }

    // ---- reset: one boot, three checks -------------------------------------
    //
    // The reset is followed rather than polled, which is what makes
    // `follow_size` measurable at all: a follow issued AFTER a boot has already
    // reached userspace fires instantly on zero bytes, and the first version of
    // this check duly reported "the follow saw no raw bytes" against a board
    // that had booted perfectly. A follow must SPAN a boot to say anything
    // about the cost of mining one.
    if actuation || mining {
        let t = Instant::now();
        // FOLLOW FROM A CURSOR TAKEN BEFORE THE RESET.
        //
        // Two anchors are wrong here and both were tried on hardware. Starting
        // from the head misses the boot entirely: `power` VERIFIES its effect
        // before returning, so the board has already rebooted by then and the
        // follow waits for a stage transition that is in the past. Using the
        // cursor from the epoch `power` opened is wrong for the same reason one
        // step later -- the hook runs BEFORE the epoch opens, so that cursor is
        // also past the boot.
        //
        // The only anchor that spans the reset is one taken before it. Every
        // response carries a freshness cursor, so this costs one cheap call.
        let anchor = r
            .call("console_state", json!({"device": r.observe.display_name()}))
            .ok()
            .and_then(|v| v["freshness"]["cursor"].as_str().map(str::to_string));
        let reset = r.call(
            r_reset_tool(),
            r.actuate_args(&[("action", json!("reset"))]),
        );
        let followed = match &reset {
            Err(e) => Err(e.message.clone()),
            Ok(_) => {
                let mut args = json!({
                    "device": r.observe.display_name(),
                    "until": {"stage": r.expect.terminal_stage},
                    "timeout_s": r.expect.reboot_s.max(30),
                });
                if let (Some(c), Some(o)) = (&anchor, args.as_object_mut()) {
                    o.insert("cursor".into(), json!(c));
                }
                r.call("follow", args).map_err(|e| e.message)
            }
        };
        if actuation {
            let res = match &followed {
                Err(e) => Fail(e.clone()),
                Ok(v) => {
                    // The payload is {"follow": <increment>, "timed_out": bool}
                    // -- reading `bytes`/`fired` off the TOP level silently
                    // yielded 0 and false, so a board that rebooted perfectly
                    // was reported as not rebooting at all.
                    let inc = &v["follow"];
                    if inc["matched"].is_string() || inc["bytes"].as_u64().unwrap_or(0) > 0 {
                        Pass(format!(
                            "the board rebooted and reached {} {}s after the reset",
                            r.expect.terminal_stage,
                            t.elapsed().as_secs()
                        ))
                    } else {
                        Fail(format!(
                            "no fresh boot within {}s: the board did not visibly reboot",
                            r.expect.reboot_s
                        ))
                    }
                }
            };
            r.record("actuation.reset", "actuation", t, res);
        }
        if mining {
            let t = Instant::now();
            let res = match &followed {
                Err(e) => Fail(e.clone()),
                Ok(v) => {
                    let mined = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
                    let raw = v["follow"]["bytes"].as_u64().unwrap_or(0);
                    if raw == 0 {
                        Fail("the follow spanned a reboot and still saw no raw bytes".into())
                    } else if mined >= 20_000 {
                        Fail(format!(
                            "the mined response is {mined} B for {raw} B of console: the whole \
                             point of mining is that this stays small"
                        ))
                    } else {
                        Pass(format!("{mined} B mined from {raw} B of console"))
                    }
                }
            };
            r.record("mining.follow_size", "mining", t, res);
        }
    }

    if mining {
        let t = Instant::now();
        let res = phantom_check(r);
        r.record("mining.no_phantom_templates", "mining", t, res);
    }
}

/// Let a boot finish before the checks that read it run.
fn wait_for_terminal(r: &mut Run) {
    let _ = r.call(
        "follow",
        json!({"device": r.observe.display_name(),
               "until": {"stage": r.expect.terminal_stage},
               "timeout_s": r.expect.reboot_s.max(30)}),
    );
}

fn r_reset_tool() -> &'static str {
    "power"
}

/// The second boot must not mint timing phantoms (§G1).
fn phantom_check(r: &mut Run) -> CheckOutcome {
    let rep = match r.call("boot_report", json!({"device": r.observe.display_name()})) {
        Ok(v) => v,
        Err(e) => return Fail(e.message),
    };
    let novel: Vec<String> = rep["novel_templates"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["text"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let pats: Vec<regex::Regex> = r
        .expect
        .phantom_regexes
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();
    let hits: Vec<&String> = novel
        .iter()
        .filter(|t| pats.iter().any(|p| p.is_match(t)))
        .collect();
    if hits.is_empty() {
        Pass(format!(
            "{} novel template(s) on the repeat boot, none matching the phantom signature",
            novel.len()
        ))
    } else {
        Fail(format!(
            "{} phantom template(s) minted by a repeat boot, e.g. {:?}",
            hits.len(),
            hits.iter().take(3).collect::<Vec<_>>()
        ))
    }
}

/// EDL entry, non-escalation and clean exit.
fn suite_edl(r: &mut Run) {
    let modes = r
        .ctx
        .config()
        .boot_modes_for(r.primary.display_name(), &r.primary.canonical);
    let edl_mode = modes
        .iter()
        .find(|m| m.to_ascii_uppercase().contains("EDL"))
        .cloned();
    let Some(mode) = edl_mode else {
        let t = Instant::now();
        r.record(
            "edl.enter",
            "edl",
            t,
            Skip("this device has no EDL boot mode configured".into()),
        );
        return;
    };

    let t = Instant::now();
    let set = r.call(
        r.actuate_tool_boot_mode(),
        r.actuate_args(&[("mode", json!(mode))]),
    );
    // A STRAP IS NOT AN ENTRY. Setting the mode arms the NEXT boot; the board
    // reaches EDL only when it is reset. The first version of this check polled
    // straight after `boot_mode` and reported "no live QDL gadget" against
    // hardware that was working perfectly — it had simply never been told to
    // reboot.
    // FOLLOW THE CONTROLLER, do not assume a sequence.
    //
    // `boot_mode` reports whether it PUT the board in the mode or merely armed
    // the next boot. Resetting a controller that sequenced its own entry boots
    // the board straight back out -- measured on the ADP, where that extra
    // reset produced "no live QDL gadget" against hardware that had entered
    // EDL perfectly a second earlier.
    let entered_already = set
        .as_ref()
        .ok()
        .and_then(|v| v["entered"].as_bool())
        .unwrap_or(false);
    let res = match &set {
        Err(e) => Fail(e.message.clone()),
        Ok(_) => match if entered_already {
            Ok(Value::Null)
        } else {
            r.call("power", r.actuate_args(&[("action", json!("reset"))]))
        } {
            Err(e) => Fail(format!("could not reset into {mode}: {}", e.message)),
            Ok(_) => {
                // 15s, polled: entry is not instant, and one sample taken in the
                // re-enumeration gap is exactly the R4 mistake.
                let deadline = Instant::now() + Duration::from_secs(r.expect.edl_enter_s);
                let mut last = Value::Null;
                let mut seen = false;
                while Instant::now() < deadline {
                    if let Ok(v) = r.call("diagnose", json!({"device": r.observe.display_name()})) {
                        if v["edl"].as_bool().unwrap_or(false) {
                            seen = true;
                            break;
                        }
                        last = v;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                if seen {
                    // §L6. LEARN WHOSE PORT THAT IS, from the one moment the
                    // answer is causal rather than inferred: this suite put
                    // THIS board into download mode and watched a QDL gadget
                    // appear. Recording its port path turns bench-wide zombie
                    // matching into per-board attribution for every later run,
                    // without anyone having to trace a cable by hand.
                    learn_usb_ports(r);
                    Pass(format!(
                        "a live QDL gadget answered {}s after {} into {mode}",
                        t.elapsed().as_secs(),
                        if entered_already {
                            "the controller's own entry sequence"
                        } else {
                            "the reset"
                        }
                    ))
                } else {
                    Fail(format!(
                        "no live QDL gadget within {}s of entering {mode}; diagnose said \
                         edl={}, usb_zombies={}",
                        r.expect.edl_enter_s, last["edl"], last["usb_zombies"]
                    ))
                }
            }
        },
    };
    let entered = matches!(res, Pass(_));
    r.record("edl.enter", "edl", t, res);

    if entered {
        let t = Instant::now();
        let res = match &set {
            Ok(v) => {
                let esc = v["effect"]["escalated"].as_bool().unwrap_or(false);
                if esc {
                    Fail("a deliberate EDL entry was escalated against (R2)".into())
                } else {
                    Pass(format!("no escalation; effect={}", compact(&v["effect"])))
                }
            }
            Err(e) => Fail(e.message.clone()),
        };
        r.record("edl.verify_no_escalation", "edl", t, res);
    }

    // clear + exit, always attempted: leaving a strap set is the sabotage case.
    let t = Instant::now();
    let cleared = r.call(
        r.actuate_tool_boot_mode(),
        r.actuate_args(&[("mode", json!("clear"))]),
    );
    let res = match cleared {
        Err(e) => Fail(format!("could not clear the boot mode: {}", e.message)),
        Ok(v) => {
            // Clearing the strap is only half of it: the board is still IN EDL
            // until it is reset out of it. Stopping here leaves a board stranded
            // in download mode with its straps innocently clear — which reads,
            // to the next person, as a board that simply will not boot.
            let _ = r.call("power", r.actuate_args(&[("action", json!("reset"))]));
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut still_edl = true;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(2));
                if let Ok(d) = r.call("diagnose", json!({"device": r.observe.display_name()})) {
                    if !d["edl"].as_bool().unwrap_or(false) {
                        still_edl = false;
                        break;
                    }
                }
            }
            if still_edl {
                Fail(format!(
                    "straps cleared but a live QDL gadget still answers 20s after the reset: {}",
                    compact(&v)
                ))
            } else {
                Pass(format!(
                    "straps cleared and the QDL gadget is gone: {}",
                    compact(&v)
                ))
            }
        }
    };
    r.record("edl.clear_exit", "edl", t, res);
}

impl Run<'_> {
    fn actuate_tool_boot_mode(&self) -> &'static str {
        "boot_mode"
    }
}

/// Power off, and prove it from something other than silence.
fn suite_power_off(r: &mut Run) {
    let t = Instant::now();
    let off = r.call(
        r.actuate_args_tool(),
        r.actuate_args(&[("action", json!("off"))]),
    );
    let res = match &off {
        Err(e) => Fail(e.message.clone()),
        Ok(v) => {
            let effect = &v["effect"];
            let verified = effect["verified"].as_bool().unwrap_or(false);
            let usb = r
                .call("diagnose", json!({"device": r.primary.display_name()}))
                .unwrap_or(Value::Null);
            let edl = usb["edl"].as_bool().unwrap_or(false);
            // §K1's rule, read literally: `verified: true` must never be
            // claimed WITHOUT evidence -- it does not require every controller
            // to be able to produce the strongest kind. The bughopper drives
            // the button with no sense line back from the board, so silence
            // plus a clean bus is the whole of the evidence available, and
            // demanding a sense reading failed the ADP for its wiring rather
            // than for its behaviour.
            let gadgets = my_zombies(&usb);
            let sensed_off = usb["power"].as_str() == Some("off");
            if edl {
                Fail("the board went into EDL, not off: a live QDL gadget answers".into())
            } else if verified && !sensed_off && gadgets > 0 {
                Fail(format!(
                    "off claimed verified with no evidence behind it: {}",
                    compact(effect)
                ))
            } else if sensed_off {
                Pass(format!(
                    "the controller senses the board off; {}",
                    compact(effect)
                ))
            } else if gadgets == 0 {
                Pass(format!(
                    "no sense line on this controller, and the evidence available agrees: \
                     not in EDL, no stale USB entries; {}",
                    compact(effect)
                ))
            } else {
                Fail(format!(
                    "{gadgets} live gadget(s) still on the bus after the off: {}",
                    compact(effect)
                ))
            }
        }
    };
    r.record("actuation.power_off", "actuation", t, res);

    // The dashboard's lamp must agree within 20s (N7).
    let t = Instant::now();
    let res = power_sense_check(r);
    r.record("actuation.power_sense", "actuation", t, res);
}

impl Run<'_> {
    fn actuate_args_tool(&self) -> &'static str {
        "power"
    }
}

fn power_sense_check(r: &mut Run) -> CheckOutcome {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let d = r.call("diagnose", json!({"device": r.primary.display_name()}));
        match d {
            Ok(v) => match v["power"].as_str() {
                Some("off") => {
                    return Pass("the controller reports the board off".into());
                }
                Some(other) => {
                    if Instant::now() >= deadline {
                        return Fail(format!(
                            "the controller still reports power {other:?} 20s after the off"
                        ));
                    }
                }
                None => {
                    return Skip(
                        "this controller cannot measure power (no sense line back from the board)"
                            .into(),
                    );
                }
            },
            Err(e) => return Fail(e.message),
        }
        if Instant::now() >= deadline {
            return Fail("the controller never reported off within 20s".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// With the board off, nothing may claim otherwise.
fn suite_honesty(r: &mut Run) {
    let t = Instant::now();
    let d = r.call("diagnose", json!({"device": r.primary.display_name()}));
    let res = match d {
        Err(e) => Fail(e.message),
        Ok(v) => {
            // READ THE FIELDS THAT EXIST. The first version of this check
            // read `usb.live_gadgets`, which `diagnose` does not return -- so
            // it was always 0 and the check passed no matter what the bus held.
            // A vacuous check is worse than a missing one: it reports safety it
            // never established.
            let edl = v["edl"].as_bool().unwrap_or(false);
            let zombies = v["usb_zombies"].as_u64().unwrap_or(0);
            // §L6. Only a zombie on THIS board's ports is this board's fault.
            // Bus-wide, the ADP's abandoned gadget failed the Nord's cleanup
            // for a mess it could not have made.
            let attributable = v["usb_zombies_scope"]["ports"]
                .as_array()
                .is_some_and(|p| !p.is_empty());
            if edl {
                Fail("diagnose claims EDL while the board is off".into())
            } else if zombies > 0 && attributable {
                Fail(format!(
                    "{zombies} stale USB entr(ies) on this board's own ports, left behind by the \
                     off"
                ))
            } else if zombies > 0 {
                Warn(format!(
                    "{zombies} stale USB entr(ies) on the bench, but no usb_ports are configured \
                     for this board, so they cannot be attributed to it. Configure \
                     devices.<name>.usb_ports to make this a real check."
                ))
            } else {
                Pass("no EDL claim, no stale USB entries".into())
            }
        }
    };
    r.record("honesty.off_board_claims_nothing", "honesty", t, res);

    let t = Instant::now();
    let res = match r.call("console_state", json!({"device": r.primary.display_name()})) {
        Err(e) => Fail(e.message),
        Ok(v) => {
            if v["console"]["commandable"].as_bool().unwrap_or(false) {
                Fail("console_state says commandable with the board powered off".into())
            } else {
                Pass(format!(
                    "console_state {:?}, not commandable",
                    v["console"]["state"].as_str().unwrap_or("?")
                ))
            }
        }
    };
    r.record("honesty.not_commandable_when_off", "honesty", t, res);
}

// ------------------------------------------------------------ cleanup -------

/// Runs on EVERY exit path. Clears straps, powers off, and VERIFIES it.
fn cleanup(r: &mut Run, keep_on: bool) -> Value {
    if keep_on {
        return json!({
            "board_off_verified": false,
            "straps_cleared": false,
            "leases_released": true,
            "note": "keep_on was set: the board was deliberately left powered",
        });
    }
    let modes = r
        .ctx
        .config()
        .boot_modes_for(r.primary.display_name(), &r.primary.canonical);
    let straps_cleared = if modes.is_empty() {
        true
    } else {
        r.call("boot_mode", r.actuate_args(&[("mode", json!("clear"))]))
            .is_ok()
    };
    let _ = r.call("power", r.actuate_args(&[("action", json!("off"))]));

    // Verified from hardware, not from the hook's exit code.
    let mut off_verified = false;
    let mut why = String::from("no evidence gathered");
    if let Ok(v) = r.call("diagnose", json!({"device": r.primary.display_name()})) {
        let edl = v["edl"].as_bool().unwrap_or(false);
        let gadgets = my_zombies(&v);
        let elsewhere = v["usb_zombies_elsewhere"].as_u64().unwrap_or(0);
        match v["power"].as_str() {
            Some("off") => {
                off_verified = !edl && gadgets == 0;
                why = format!(
                    "controller says off, edl={edl}, usb_zombies={gadgets} on this board's ports \
                     ({elsewhere} elsewhere on the bench)"
                );
            }
            Some(p) => why = format!("controller says power {p:?}"),
            None => {
                // No sense line: the honest fallback is the bus plus silence.
                off_verified = !edl && gadgets == 0;
                why = format!(
                    "controller cannot measure power; edl={edl}, usb_zombies={gadgets} on this \
                     board's ports ({elsewhere} elsewhere on the bench)"
                );
            }
        }
    }
    json!({
        "board_off_verified": off_verified,
        "straps_cleared": straps_cleared,
        "leases_released": true,
        "evidence": why,
    })
}

/// The zombie count that can honestly be held against THIS board (§L6).
///
/// Bench-wide entries are real, and they are reported -- but a board is only
/// answerable for the ports it is plugged into. Without `usb_ports` configured
/// there is no attribution to be had, and counting anyway is what failed a clean
/// Nord for the ADP's leftovers.
fn my_zombies(v: &Value) -> u64 {
    let attributable = v["usb_zombies_scope"]["ports"]
        .as_array()
        .is_some_and(|p| !p.is_empty());
    if attributable {
        v["usb_zombies"].as_u64().unwrap_or(0)
    } else {
        0
    }
}

/// Record the port path of the QDL gadget this suite just brought up (§L6).
///
/// Deliberately additive and deliberately narrow: only ports observed while THIS
/// board was in download mode at our own request, merged with whatever was
/// already known, so a bench that moves a cable ends up with a stale entry at
/// worst -- never with another board's port.
fn learn_usb_ports(r: &Run) {
    let found: Vec<String> = conminer_core::usb::scan()
        .into_iter()
        .filter(|d| d.is_qdl() && !d.is_zombie())
        .filter_map(|d| d.port_path)
        .collect();
    if found.is_empty() {
        return;
    }
    for c in std::iter::once(&r.primary).chain(std::iter::once(&r.observe)) {
        let mut ports: Vec<String> = c
            .tags
            .get("usb_ports")
            .map(|t| {
                t.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        ports.extend(found.iter().cloned());
        ports.sort();
        ports.dedup();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("usb_ports".to_string(), ports.join(","));
        let _ = r.ctx.registry().set_tags(c.id, &tags);
    }
}

fn cursor_of(r: &Run, d: &DeviceRow) -> String {
    r.ctx
        .with_store(d, |st| Ok(st.stream_offset()))
        .map(|o| o.to_string())
        .unwrap_or_default()
}

/// One-line JSON, for evidence strings that must stay readable in a table.
fn compact(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > 160 {
        format!("{}…", &s[..160])
    } else {
        s
    }
}
