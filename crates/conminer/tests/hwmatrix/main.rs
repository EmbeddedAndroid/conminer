//! Suite `hwmatrix` — actuation, on real boards, from every node and every
//! control surface an operator or an agent actually uses.
//!
//! WHY THIS EXISTS SEPARATELY. Every other suite in this workspace runs against
//! fakes, temp dirs and `/bin/echo` hooks, and that is deliberate: they must be
//! runnable anywhere, in seconds, with no bench. But a fleet whose whole purpose
//! is driving hardware cannot be trusted on the strength of green tests that
//! never touched a board. This one closes that gap. It is skipped unless
//! `CONMINER_HW_MATRIX=1`, so `cargo test --workspace` stays hardware-free.
//!
//! WHAT IT PROVES, as a matrix rather than a demo:
//!
//!     for every NODE in the fleet            (charlie, alpha, bravo)
//!       for every MECHANISM                  (MCP tool, dashboard API, real UI click)
//!         for every FUNCTION                 (power off, power on, reset, EDL)
//!           N times                          (default 5)
//!
//! One green run of one path is an anecdote. A board is driven from three
//! machines through three different code paths, five times each, or it is not
//! reliable. The three mechanisms are not redundant: the MCP tool is what an
//! agent calls, the dashboard API is what the page calls, and the UI click is
//! what a person does -- and each has had its own bug (a selector that never
//! reached the endpoint, a button bound to no board, a lease nobody released).
//!
//! TWO RULES THAT MAKE THE RESULT MEAN SOMETHING:
//!
//!   1. ACTUATE THROUGH THE PATH UNDER TEST, VERIFY THROUGH THE OWNER.
//!      Every check reads `diagnose` from the node the board is CABLED to,
//!      directly, never through the node being tested. A routing or relay bug
//!      cannot then make a failed actuation look green, because the thing that
//!      reports success is not the thing being trusted.
//!
//!   2. A WITNESS BOARD ON THE SAME HOST MUST NOT MOVE.
//!      alpha has two strap controllers on one bench, and the failure mode
//!      that matters there is not "the button did nothing" but "the button
//!      powered the OTHER board" -- silent, and indistinguishable from success
//!      if you only look at the board you aimed at.
//!
//! POWER STATE IS READ FROM THE CONTROLLER, NEVER FROM THE DASHBOARD FIELD.
//! `diagnose` opens its own probe and asks the controller; the dashboard's
//! `power` is a cached snapshot that is seconds stale by construction, and a
//! matrix that graded itself on that would grade its own lag.
//!
//! Run it:
//!   CONMINER_HW_MATRIX=1 cargo test --test hwmatrix -- --nocapture
//! Narrow it while debugging:
//!   CONMINER_HW_SWEEPS=1 CONMINER_HW_MECHS=mcp CONMINER_HW_NODES=bravo ...

use serde_json::{json, Value};
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "../browser/cdp.rs"]
mod cdp;

// ------------------------------------------------------------------- config -

/// One conminer node: where its mcpd and its dashboard live.
#[derive(Clone, Debug)]
struct Node {
    name: String,
    mcp: String,
    dash: String,
}

/// One board under test, plus the board that must NOT move while it is driven.
#[derive(Clone, Debug)]
struct Board {
    label: String,
    /// Short tag, so two boards sharing one log can be told apart at a glance.
    tag: String,
    /// The node it is cabled to. Every verification goes here.
    owner: Node,
    /// Its id ON THE OWNER.
    canonical: String,
    /// The mode name that lands this silicon in EDL.
    edl_mode: String,
    /// Strap-latching controllers (Bantam) only ARM a mode; the board keeps
    /// running until it is reset. Sequencing controllers (TAC) do the whole
    /// thing. Stated per board rather than sniffed, because guessing wrong
    /// turns a real failure into a passing retry.
    edl_needs_reset: bool,
    /// A second board on the same host, whose power state must be unchanged
    /// after every actuation. Empty if the host has only one.
    witness: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn enabled() -> bool {
    std::env::var("CONMINER_HW_MATRIX").is_ok_and(|v| v == "1")
}

/// Which functions each cell drives, in order.
///
/// The full set by default; `CONMINER_HW_FUNCS=on,edl` narrows it when only one
/// mechanism is in question and 83 minutes of hardware is not worth spending to
/// answer it. ORDER IS THE CALLER'S: a board is left off by the previous run, so
/// an EDL-only pass has to be told to power it on first -- the sequence is the
/// test, not a set.
fn functions() -> Vec<String> {
    match std::env::var("CONMINER_HW_FUNCS") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect(),
        _ => ["off", "on", "reset", "edl"]
            .iter()
            .map(|f| f.to_string())
            .collect(),
    }
}

fn sweeps() -> usize {
    env_or("CONMINER_HW_SWEEPS", "5").parse().unwrap_or(5)
}

/// The fleet, as deployed. Overridable so this is not welded to one bench:
/// `name=mcp_url|dash_url` entries, comma separated.
fn nodes() -> Vec<Node> {
    let spec = env_or(
        "CONMINER_HW_NODES",
        "charlie=http://127.0.0.1:8090|http://127.0.0.1:8080,\
         alpha=http://192.168.10.10:8090|http://192.168.10.10:8080,\
         bravo=http://192.168.10.11:8090|http://192.168.10.11:8080",
    );
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|e| {
            let (name, urls) = e.split_once('=')?;
            let (mcp, dash) = urls.split_once('|')?;
            Some(Node {
                name: name.trim().to_string(),
                mcp: mcp.trim().to_string(),
                dash: dash.trim().to_string(),
            })
        })
        .collect()
}

fn node_named(name: &str) -> Node {
    nodes()
        .into_iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("no node {name:?} configured"))
}

fn mechanisms() -> Vec<&'static str> {
    let want = env_or("CONMINER_HW_MECHS", "mcp,api,ui");
    ["mcp", "api", "ui"]
        .into_iter()
        .filter(|m| want.split(',').any(|w| w.trim() == *m))
        .collect()
}

/// Which nodes do the driving. All of them by default -- including the one with
/// no hardware of its own, which is the case that exercises pure proxying, and
/// the one that cannot dial anybody, which exercises the reverse channel.
fn driving_nodes() -> Vec<Node> {
    let want = env_or("CONMINER_HW_NODES_DRIVE", "");
    let all = nodes();
    if want.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|n| want.split(',').any(|w| w.trim() == n.name))
        .collect()
}

// ---------------------------------------------------------------- transport -

/// One HTTP call, via curl, so this suite adds no client dependency and behaves
/// exactly like the other black-box suites here.
fn http(args: &[&str], budget: Duration) -> Result<(u16, String), String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-o",
        "/dev/stdout",
        "-w",
        "\n%{http_code}",
        "--max-time",
    ])
    .arg(budget.as_secs().to_string())
    .args(args);
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    let body = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
    Ok((code.trim().parse().unwrap_or(0), body.to_string()))
}

/// A tool call against a node's mcpd, unwrapped to its structured content.
fn mcp(node: &Node, tool: &str, args: Value, budget: Duration) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": args}
    })
    .to_string();
    let url = format!("{}/mcp", node.mcp);
    let (code, text) = http(
        &[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: application/json, text/event-stream",
            "-d",
            &body,
            &url,
        ],
        budget,
    )?;
    if code != 200 {
        return Err(format!("{tool} on {}: HTTP {code}: {text}", node.name));
    }
    // The transport may answer as SSE; take the last data frame either way.
    let payload = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .next_back()
        .map(str::trim)
        .unwrap_or(text.trim());
    let v: Value = serde_json::from_str(payload)
        .map_err(|e| format!("{tool} on {}: unparsable reply ({e}): {payload}", node.name))?;
    let result = v.get("result").cloned().unwrap_or(Value::Null);
    let content = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(format!(
            "{tool} on {}: {}",
            node.name,
            content
                .get("error")
                .map(ToString::to_string)
                .unwrap_or_else(|| content.to_string())
        ));
    }
    Ok(content)
}

// -------------------------------------------------------------- observation -

/// What the OWNER's controller says, right now. The only source of truth here.
fn diagnose(board: &Board) -> Result<Value, String> {
    mcp(
        &board.owner,
        "diagnose",
        json!({"device": board.canonical, "wait_ms": 800}),
        Duration::from_secs(60),
    )
}

fn power_of(owner: &Node, canonical: &str) -> Result<String, String> {
    let v = mcp(
        owner,
        "diagnose",
        json!({"device": canonical, "wait_ms": 300}),
        Duration::from_secs(60),
    )?;
    Ok(v.get("power")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string())
}

/// Poll until `f` is satisfied by a fresh `diagnose`, or give up saying what it
/// actually saw. A bare sleep would either be slow or would grade the bench on
/// how fast it happened to be that minute.
fn wait_until(
    board: &Board,
    what: &str,
    budget: Duration,
    f: impl Fn(&Value) -> bool,
) -> Result<Value, String> {
    let deadline = Instant::now() + budget;
    #[allow(unused_assignments)]
    let mut last = Value::Null;
    loop {
        match diagnose(board) {
            Ok(v) => {
                if f(&v) {
                    return Ok(v);
                }
                last = v;
            }
            Err(e) => last = json!({"diagnose_failed": e}),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{what}: not seen within {}s; last state power={} edl={} verdict={}",
                budget.as_secs(),
                last.get("power").unwrap_or(&Value::Null),
                last.get("edl").unwrap_or(&Value::Null),
                last.get("verdict").unwrap_or(&Value::Null),
            ));
        }
        std::thread::sleep(Duration::from_millis(1200));
    }
}

/// How the board is named ON the driving node: its own id, or the peer form.
fn selector_on(driver: &Node, board: &Board) -> String {
    if driver.name == board.owner.name {
        board.canonical.clone()
    } else {
        format!("peer:{}/{}", board.owner.name, board.canonical)
    }
}

/// …confirmed against what that node actually lists, so a selector this suite
/// invented can never be the reason a cell fails.
fn confirm_selector(driver: &Node, board: &Board) -> Result<String, String> {
    let want = selector_on(driver, board);
    let v = mcp(driver, "list_devices", json!({}), Duration::from_secs(30))?;
    let found = v
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("device").and_then(Value::as_str))
                .any(|d| d == want)
        })
        .unwrap_or(false);
    if !found {
        return Err(format!(
            "{} does not list {want:?}; the fleet view is incomplete, so nothing below would \
             mean anything",
            driver.name
        ));
    }
    Ok(want)
}

// ----------------------------------------------------------------- actuation -

/// Drive one action through one mechanism. Returns only when the far side has
/// answered, so a failure here is the endpoint's, not a race.
#[allow(clippy::too_many_arguments)]
fn actuate(
    driver: &Node,
    selector: &str,
    control: &str,
    mech: &str,
    tool: &str,
    arg: &str,
    browser: Option<&cdp::Browser>,
) -> Result<(), String> {
    let budget = Duration::from_secs(120);
    match mech {
        // What an agent does.
        "mcp" => {
            // Steal, because the dashboard takes the lease for its own presses
            // and this suite drives all three surfaces at the same board.
            mcp(
                driver,
                "acquire",
                json!({"device": selector, "steal": true, "holder": "hwmatrix", "ttl_s": 120}),
                Duration::from_secs(60),
            )?;
            let args = if tool == "power" {
                json!({"device": selector, "action": arg})
            } else {
                json!({"device": selector, "mode": arg})
            };
            mcp(driver, tool, args, budget).map(|_| ())
        }
        // What the dashboard page calls.
        "api" => {
            let url = format!(
                "{}/api/{}/{}/{}",
                driver.dash,
                tool,
                urlencode(selector),
                urlencode(arg)
            );
            let (code, body) = http(&["-X", "POST", &url], budget)?;
            if code != 200 {
                return Err(format!("api {tool}/{arg}: HTTP {code}: {body}"));
            }
            if body.contains("\"error\"") {
                return Err(format!("api {tool}/{arg}: {body}"));
            }
            Ok(())
        }
        // What a person does: a real click, on a real button, in a real browser.
        "ui" => {
            let b = browser.ok_or("no browser")?;
            let want = if tool == "power" {
                arg.to_string()
            } else {
                format!("mode:{arg}")
            };
            let js = format!(
                r#"
                (async () => {{
                  const dev = {dev}, act = {act}, stem = {stem}, ctl = {ctl};
                  // The owner's own answer first: the exact row the page bound
                  // this board's controls to. The stem is only a fallback for a
                  // board whose owner names no controller, and it is kept tight
                  // (`stem + "-if"`) so it can never match a longer serial that
                  // merely starts the same way -- that would be a click on a
                  // different board.
                  const find = () => {{
                    const buttons = [...document.querySelectorAll("button[data-power]")]
                      .filter(b => b.offsetParent !== null);
                    let mine = buttons.filter(b => b.dataset.device === ctl);
                    if (!mine.length) {{
                      mine = buttons.filter(b => (b.dataset.device || "").startsWith(stem + "-if"));
                    }}
                    return {{all: buttons, btn: mine.find(b => b.dataset.power === act)}};
                  }};

                  // WAIT FOR THE CONTROL TO BE CLICKABLE, as a person would.
                  //
                  // `actOn` disables every action button for the duration of an
                  // action and re-enables them 1600 ms after it finishes. The
                  // page is no longer reloaded between clicks -- a press does not
                  // reload it either -- so the next click can arrive while the
                  // controls are still disabled, and `click()` on a disabled
                  // button does nothing at all: no request, no error, just a
                  // silent timeout blaming the endpoint.
                  const ready = await new Promise(res => {{
                    const t0 = Date.now();
                    const iv = setInterval(() => {{
                      const f = find();
                      if (f.btn && !f.btn.disabled) {{ clearInterval(iv); res(f.btn); }}
                      else if (Date.now() - t0 > 30000) {{ clearInterval(iv); res(null); }}
                    }}, 150);
                  }});
                  if (!ready) {{
                    const f = find();
                    return JSON.stringify({{error: f.btn ? "control stayed disabled" : "no button",
                      stem: stem, dev: dev,
                      have: f.all.map(b => b.dataset.power + "@" + b.dataset.device).slice(0, 12)}});
                  }}

                  // ONLY THE ACTUATION'S OWN REQUEST COUNTS. The page polls
                  // /api/devices on a timer through this same wrapper; recording
                  // whichever fetch resolved first would let a poll's 200 stand
                  // in for the press, which is a PASS for a click that may have
                  // done nothing.
                  if (!window.__conminerWrapped) {{
                    window.__conminerWrapped = true;
                    const real = window.fetch;
                    window.fetch = (...a) => {{
                      const url = String(a[0]);
                      const p = real(...a);
                      if (url.includes("/api/power/") || url.includes("/api/boot_mode/")) {{
                        p.then(r => {{ window.__done = {{status: r.status, url: url}}; }})
                         .catch(e => {{ window.__done = {{error: String(e)}}; }});
                      }}
                      return p;
                    }};
                  }}
                  window.__done = null;
                  ready.click();
                  // Wait for the REQUEST to come back, not for a fixed delay:
                  // tearing the browser down mid-flight would cancel the very
                  // actuation being tested.
                  return await new Promise(r => {{
                    const t0 = Date.now();
                    const iv = setInterval(() => {{
                      if (window.__done || Date.now() - t0 > 110000) {{
                        clearInterval(iv);
                        r(JSON.stringify(window.__done || {{error: "no response in 110s"}}));
                      }}
                    }}, 200);
                  }});
                }})()
                "#,
                dev = json!(selector),
                act = json!(want),
                stem = json!(board_stem(selector)),
                ctl = json!(control),
            );
            let out = b.eval_within(
                &format!("{}/?nostream=1", driver.dash),
                Duration::from_millis(2500),
                &js,
                Duration::from_secs(130),
            );
            let text = out.as_str().unwrap_or_default();
            if text.is_empty() {
                return Err("the browser returned nothing; CDP evaluate failed".into());
            }
            let v: Value = serde_json::from_str(text).unwrap_or(json!({"raw": text}));
            if v.get("status").and_then(Value::as_u64) == Some(200) {
                Ok(())
            } else {
                Err(format!("ui click {tool}/{arg}: {v}"))
            }
        }
        other => Err(format!("unknown mechanism {other:?}")),
    }
}

/// WHICH CONTROL ON THE PAGE DRIVES THIS BOARD, asked of its owner.
///
/// The dashboard puts one set of power controls per board, bound to the row that
/// actually carries the hook -- which on a Bantam bench is the CONTROLLER's own
/// tty, a different USB device from the console entirely, and on a TAC is the
/// GPIO channel of the same FTDI part. Guessing from the console's name finds
/// the first on one bench and nothing on the other. The owner already publishes
/// the answer as `controls.controller_port`; use it, and fall back to the
/// console itself only when the owner names no controller.
fn control_device(driver: &Node, board: &Board, selector: &str) -> String {
    let port = mcp(
        &board.owner,
        "list_devices",
        json!({"detail": true}),
        Duration::from_secs(30),
    )
    .ok()
    .and_then(|v| {
        v.get("devices")?
            .as_array()?
            .iter()
            .find(|d| d.get("device").and_then(Value::as_str) == Some(board.canonical.as_str()))?
            .get("controls")?
            .get("controller_port")?
            .as_str()
            .map(str::to_string)
    });
    match port {
        Some(p) if driver.name == board.owner.name => p,
        Some(p) => format!("peer:{}/{p}", board.owner.name),
        None => selector.to_string(),
    }
}

/// The part of a by-id name that identifies the BOARD, not the console.
///
/// `…usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0` -> `…usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q`.
/// One FTDI device, one board, however many interfaces it exposes. Used only to
/// find the chassis control that drives it; every verification still names the
/// exact console.
fn board_stem(selector: &str) -> String {
    match selector.rfind("-if") {
        Some(i) => selector[..i].to_string(),
        None => selector.to_string(),
    }
}

/// Percent-encode a selector for one path segment. The ids contain `/`, and a
/// raw one would split the route and address a different device (or none).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// -------------------------------------------------------------------- sweep -

struct Cell {
    driver: String,
    mech: String,
    func: String,
    ok: usize,
    failures: Vec<String>,
}

/// One board, driven every way, `sweeps()` times.
fn run_matrix(board: &Board) {
    if !enabled() {
        eprintln!("SKIP: set CONMINER_HW_MATRIX=1 to drive real hardware");
        return;
    }
    let n = sweeps();
    let mut cells: Vec<Cell> = Vec::new();
    let chrome = cdp::chromium_bin();
    println!(
        "\n=== {} ({} on {}) ===",
        board.label, board.canonical, board.owner.name
    );

    // Where the witness starts. Not a fixed expectation: whatever it is, it must
    // still be that at the end of every actuation of the OTHER board.
    let witness_before = if board.witness.is_empty() {
        None
    } else {
        power_of(&board.owner, &board.witness).ok()
    };
    if let Some(w) = &witness_before {
        println!("    witness {} starts {w}", board.witness);
    }

    for driver in driving_nodes() {
        let selector = match confirm_selector(&driver, board) {
            Ok(s) => s,
            Err(e) => {
                cells.push(Cell {
                    driver: driver.name.clone(),
                    mech: "-".into(),
                    func: "resolve".into(),
                    ok: 0,
                    failures: vec![e],
                });
                continue;
            }
        };
        let control = control_device(&driver, board, &selector);
        for mech in mechanisms() {
            // One browser per cell, not per click: chromium start-up dwarfs the
            // actuation and would triple the wall clock of an already long run.
            let browser = if mech == "ui" {
                match chrome.as_deref().and_then(cdp::Browser::launch) {
                    Some(b) => Some(b),
                    None => {
                        cells.push(Cell {
                            driver: driver.name.clone(),
                            mech: mech.into(),
                            func: "launch".into(),
                            ok: 0,
                            failures: vec!["no chromium with a debugging port".into()],
                        });
                        continue;
                    }
                }
            } else {
                None
            };
            let mut per_func: Vec<Cell> = functions()
                .iter()
                .map(|f| Cell {
                    driver: driver.name.clone(),
                    mech: mech.to_string(),
                    func: f.to_string(),
                    ok: 0,
                    failures: Vec::new(),
                })
                .collect();

            for sweep in 1..=n {
                for cell in per_func.iter_mut() {
                    let started = Instant::now();
                    let r = match cell.func.as_str() {
                        "off" => {
                            step_off(&driver, &selector, &control, mech, board, browser.as_ref())
                        }
                        "on" => {
                            step_on(&driver, &selector, &control, mech, board, browser.as_ref())
                        }
                        "reset" => {
                            step_reset(&driver, &selector, &control, mech, board, browser.as_ref())
                        }
                        _ => step_edl(&driver, &selector, &control, mech, board, browser.as_ref()),
                    };
                    let r = r.and_then(|()| witness_unmoved(board, witness_before.as_deref()));
                    let problem = match r {
                        Ok(()) => {
                            cell.ok += 1;
                            None
                        }
                        Err(e) => {
                            cell.failures.push(format!("sweep {sweep}: {e}"));
                            Some(e)
                        }
                    };
                    println!(
                        "    [{:<10}] {:<8} {:<4} {:<6} sweep {sweep}/{n} {:<4} {:>5.1}s{}",
                        board.tag,
                        driver.name,
                        mech,
                        cell.func,
                        if problem.is_some() { "FAIL" } else { "ok" },
                        started.elapsed().as_secs_f32(),
                        problem.map(|e| format!("  {e}")).unwrap_or_default(),
                    );
                }
            }
            cells.append(&mut per_func);
        }
    }

    // Leave the board where an operator would want it: powered, straps clear.
    let _ = mcp(
        &board.owner,
        "acquire",
        json!({"device": board.canonical, "steal": true, "holder": "hwmatrix", "ttl_s": 60}),
        Duration::from_secs(30),
    );
    let _ = mcp(
        &board.owner,
        "boot_mode",
        json!({"device": board.canonical, "mode": "clear"}),
        Duration::from_secs(60),
    );
    let _ = mcp(
        &board.owner,
        "power",
        json!({"device": board.canonical, "action": "on"}),
        Duration::from_secs(120),
    );

    report(board, &cells, n);
}

fn report(board: &Board, cells: &[Cell], n: usize) {
    println!("\n--- {} matrix ---", board.label);
    let mut failed = 0;
    for c in cells {
        let bad = c.ok < n;
        if bad {
            failed += 1;
        }
        println!(
            "  {:<8} {:<4} {:<6} {}/{}{}",
            c.driver,
            c.mech,
            c.func,
            c.ok,
            n,
            if bad { "   <-- FAILED" } else { "" }
        );
        for f in &c.failures {
            println!("        {f}");
        }
    }
    assert_eq!(
        failed, 0,
        "{} cells did not pass every sweep for {}",
        failed, board.label
    );
}

/// The other board on this host must be exactly where it was.
fn witness_unmoved(board: &Board, before: Option<&str>) -> Result<(), String> {
    let (Some(before), false) = (before, board.witness.is_empty()) else {
        return Ok(());
    };
    let now = power_of(&board.owner, &board.witness)?;
    if now != before {
        return Err(format!(
            "the witness board {} moved from {before} to {now}: that actuation reached the \
             WRONG hardware",
            board.witness
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------- steps -

fn step_off(
    d: &Node,
    sel: &str,
    ctl: &str,
    mech: &str,
    b: &Board,
    br: Option<&cdp::Browser>,
) -> Result<(), String> {
    actuate(d, sel, ctl, mech, "power", "off", br)?;
    let v = wait_until(b, "power off", Duration::from_secs(45), |v| {
        v.get("power").and_then(Value::as_str) == Some("off")
    })?;
    // THE TWO SIGNALS MUST NOT CONTRADICT EACH OTHER.
    //
    // The controller saying "off" is one measurement; the console still
    // delivering bytes is another, and a board cannot be both. Checking only the
    // controller is how a sense line that reads the wrong pin passes a power test
    // for ever -- the reading is self-consistent and wrong. A quiet board when
    // powered ON is legitimate (some firmware simply does not print), so the
    // corroboration is asserted in this direction only.
    let talking = v
        .get("probe")
        .and_then(|p| p.get("bytes_received"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if talking > 0 {
        return Err(format!(
            "the controller reports the board OFF while its console is still delivering              {talking} bytes: one of those two measurements is wrong"
        ));
    }
    Ok(())
}

fn step_on(
    d: &Node,
    sel: &str,
    ctl: &str,
    mech: &str,
    b: &Board,
    br: Option<&cdp::Browser>,
) -> Result<(), String> {
    actuate(d, sel, ctl, mech, "power", "on", br)?;
    wait_until(b, "power on", Duration::from_secs(60), |v| {
        v.get("power").and_then(Value::as_str) == Some("on")
    })?;
    Ok(())
}

/// A reset has to be observable, or "it ran" is all we learn.
///
/// The evidence is a NEW EPOCH: `power reset` opens one, and the console's
/// `boot_id` is how the store names it. Checking power alone would pass on a
/// hook that did nothing at all, since the board is on before and after.
fn step_reset(
    d: &Node,
    sel: &str,
    ctl: &str,
    mech: &str,
    b: &Board,
    br: Option<&cdp::Browser>,
) -> Result<(), String> {
    let before = boot_id(b);
    actuate(d, sel, ctl, mech, "power", "reset", br)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let now = boot_id(b);
        if now != before && now.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "reset opened no new epoch (boot_id stayed {before:?}); the hook ran and the \
                 board did not restart"
            ));
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    wait_until(b, "power on after reset", Duration::from_secs(60), |v| {
        v.get("power").and_then(Value::as_str) == Some("on")
    })?;
    Ok(())
}

fn boot_id(b: &Board) -> Option<i64> {
    mcp(
        &b.owner,
        "console_state",
        json!({"device": b.canonical}),
        Duration::from_secs(30),
    )
    .ok()?
    .get("freshness")?
    .get("boot_id")?
    .as_i64()
}

/// EDL, end to end: arm it, land in it, prove it, and come back out.
///
/// Coming back out is part of the test, not cleanup. A bench left with a latched
/// strap boots into EDL for ever afterwards, and the next sweep would "pass"
/// against a board that never left.
fn step_edl(
    d: &Node,
    sel: &str,
    ctl: &str,
    mech: &str,
    b: &Board,
    br: Option<&cdp::Browser>,
) -> Result<(), String> {
    actuate(d, sel, ctl, mech, "boot_mode", &b.edl_mode, br)?;
    if b.edl_needs_reset {
        // A strap-latching controller only ARMS the mode; the board keeps
        // running until it is reset. Doing this through the same mechanism keeps
        // the cell honest about the path being tested.
        actuate(d, sel, ctl, mech, "power", "reset", br)?;
    }
    wait_until(b, "EDL", Duration::from_secs(60), |v| {
        v.get("edl").and_then(Value::as_bool) == Some(true)
    })?;

    // Out again: clear the strap, cycle, and confirm it really left EDL.
    actuate(d, sel, ctl, mech, "boot_mode", "clear", br)?;
    actuate(d, sel, ctl, mech, "power", "cycle", br)?;
    wait_until(b, "back out of EDL", Duration::from_secs(90), |v| {
        v.get("edl").and_then(Value::as_bool) == Some(false)
            && v.get("power").and_then(Value::as_str) == Some("on")
    })?;
    Ok(())
}

// -------------------------------------------------------------------- cases -

/// The IQ8 EVK on bravo: a TAC (Alpaca) controller, which SEQUENCES boot modes.
/// bravo is upstream of the mesh NAT, so two of the three drivers reach it only
/// over the reverse channel -- which is exactly why it is in this matrix.
#[test]
fn the_iq8_on_node_b_is_driveable_from_every_node_by_every_mechanism() {
    run_matrix(&Board {
        label: "IQ8 EVK / TAC".into(),
        tag: "IQ8".into(),
        owner: node_named(&env_or("CONMINER_HW_IQ8_NODE", "bravo")),
        canonical: env_or(
            "CONMINER_HW_IQ8",
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0",
        ),
        edl_mode: "EDL".into(),
        edl_needs_reset: false,
        witness: env_or("CONMINER_HW_IQ8_WITNESS", ""),
    });
}

/// The IQ10 EVK on alpha: a Bantam, which LATCHES straps, on a bench that has
/// a second Bantam board beside it. The witness is the point.
#[test]
fn the_iq10_on_node_a_is_driveable_from_every_node_by_every_mechanism() {
    run_matrix(&Board {
        label: "IQ10 EVK / Bantam".into(),
        tag: "IQ10".into(),
        owner: node_named(&env_or("CONMINER_HW_IQ10_NODE", "alpha")),
        // if02, because that is the interface the board actually TALKS on. The
        // FT4232H exposes four; if00/if01 are silent on this EVK, and aiming at
        // one of them made every `power on` run its full escalation (74 s) before
        // reporting unverified -- a slow, wrong answer about working hardware.
        canonical: env_or(
            "CONMINER_HW_IQ10",
            "/dev/serial/by-id/usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0",
        ),
        edl_mode: env_or("CONMINER_HW_IQ10_EDL", "BOOT_MD_EDL"),
        edl_needs_reset: true,
        witness: env_or(
            "CONMINER_HW_IQ10_WITNESS",
            "/dev/serial/by-id/usb-FTDI_NordAU_RIDE_SX_879X_UART_AI41BI4U0R-if00-port0",
        ),
    });
}
