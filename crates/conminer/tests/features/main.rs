//! Suite `features` (§F1-F10) — the round-5 feature specs.
//!
//! One module per feature, each asserting the SEMANTICS the spec asked for
//! rather than the presence of code. Acceptance on real boards is run
//! separately, on the rig; these are the gates that keep it working.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    h: Handler,
    /// Held so a test can advance time. Anything that measures a RATE is
    /// meaningless without control of the clock.
    clock: Arc<conminer_core::clock::StepClock>,
}

impl Rig {
    fn new() -> Self {
        Self::with_config(Config::default())
    }

    fn with_config(mut cfg: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx =
            Context::open(cfg, Arc::new(ProfileSet::builtin().unwrap()), clock.clone()).unwrap();
        Self {
            dir: dir.path().to_path_buf(),
            _dir: dir,
            h: Handler::new(ctx),
            clock,
        }
    }

    fn raw(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .unwrap();
        self.h.handle(req).expect("a reply").result.unwrap()
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args);
        assert_eq!(r["isError"], false, "{name} failed: {r}");
        r["structuredContent"].clone()
    }

    fn err(&self, name: &str, args: Value) -> Value {
        let r = self.raw(name, args);
        assert_eq!(r["isError"], true, "{name} unexpectedly succeeded: {r}");
        r["structuredContent"]["error"].clone()
    }

    fn registry(&self) -> Registry {
        Registry::open(&self.dir).unwrap()
    }

    /// A board: `n` consoles plus an ignored controller, all one target.
    fn board(&self, target: &str, consoles: &[&str], controller: &str) -> Vec<i64> {
        let mut reg = self.registry();
        let mut ids = Vec::new();
        for c in consoles {
            let d = reg
                .upsert_device(c, None, IdentityKind::ById, None, 0)
                .unwrap();
            reg.set_target(d.id, Some(target)).unwrap();
            ids.push(d.id);
        }
        let ctl = reg
            .upsert_device(controller, None, IdentityKind::ById, None, 0)
            .unwrap();
        reg.set_target(ctl.id, Some(target)).unwrap();
        reg.set_ignored(ctl.id, true).unwrap();
        ids
    }

    /// Ingest text as a device, returning its name.
    fn ingest_text(&self, text: &str) -> String {
        self.ingest_into(text, None)
    }

    /// Ingest into a NAMED device, so several ingests become several epochs on
    /// one console -- which is what a metric series is made of.
    fn ingest_into(&self, text: &str, device: Option<&str>) -> String {
        let path = self.dir.join(format!("in-{}.log", text.len()));
        std::fs::write(&path, text).unwrap();
        let mut args = json!({"path": path.display().to_string()});
        if let Some(d) = device {
            args["device"] = json!(d);
        }
        self.call("ingest_file", args)["device"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn lease(&self, device: &str) {
        self.call("acquire", json!({"device": device, "ttl_s": 600}));
    }
}

/// A config whose power hook is a harmless command, so actuation can be tested
/// without hardware: `true` succeeds and touches nothing.
fn cfg_with_hook() -> Config {
    let mut c = Config::default();
    c.hooks.power_timeout_s = 5;
    // The verification windows are hardware timings; a test rig has no hardware
    // to wait for, and 53 s of sleeping made this suite the slowest thing in the
    // workspace.
    c.hooks.verify_settle_s = 0;
    c.hooks.verify_off_watch_s = 0;
    c.hooks.verify_boot_watch_s = 0;
    c.hooks.edl_settle_s = 0;
    let over = conminer_core::config::DeviceOverride {
        hooks: conminer_core::config::DeviceHooks {
            // Harmless: proves the plumbing without touching hardware.
            power: Some("/bin/true {action} {device}".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    for key in [AP, SM] {
        c.devices.insert(key.to_string(), over.clone());
    }
    c
}

// ------------------------------------------------------------------- F1 -----

const AP: &str = "/dev/serial/by-id/usb-Rig_Board_X-if00-port0";
const SM: &str = "/dev/serial/by-id/usb-Rig_Board_X-if01-port0";
const CTL: &str = "/dev/serial/by-id/usb-Microchip_Bantam_RIGX-if00";

/// F1: actuating a board opens a LINKED epoch on every console of it.
///
/// Per-console epochs plus per-board hooks meant the power epoch landed on
/// whichever console was named while the boot evidence accrued on a sibling --
/// measured on the NordAU, where the epoch sat on a silent console.
#[test]
fn f1_target_actuation_opens_one_linked_epoch_per_console() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardx", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);

    let r = rig.call(
        "power",
        json!({"target": "boardx", "action": "on", "label": "f1"}),
    );
    let opened = r["opened"].as_array().expect("per-member epochs");
    assert_eq!(opened.len(), 2, "one epoch per console: {r}");

    // The controller is a member of the target but never gets an epoch.
    let devices: Vec<&str> = opened
        .iter()
        .map(|o| o["device"].as_str().unwrap())
        .collect();
    assert!(
        devices.contains(&AP) && devices.contains(&SM),
        "{devices:?}"
    );
    assert!(
        r["exempt_not_consoles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("Bantam")),
        "the controller must be named as exempt, not silently dropped: {r}"
    );

    // Both epochs carry the same group, so either console can find the other.
    let a = rig.call(
        "boot_report",
        json!({"device": AP, "boot": opened[0]["boot_id"]}),
    );
    let sibs = a["sibling_epochs"].as_array().expect("sibling epochs");
    assert_eq!(sibs.len(), 1, "the other console: {a}");
    assert_eq!(sibs[0]["device"], SM);
}

/// F1/N3: the lease error names every missing member, and never asks for one on
/// the controller.
#[test]
fn f1_a_missing_lease_names_the_console_it_is_missing_on() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardx", &[AP, SM], CTL);
    rig.lease(AP); // ...but not SM

    let e = rig.err("power", json!({"target": "boardx", "action": "on"}));
    assert_eq!(e["code"], "LEASE_REQUIRED");
    let missing: Vec<&str> = e["detail"]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert_eq!(missing, vec![SM], "name the one that is missing: {e}");
    assert!(
        e["detail"]["held"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == AP),
        "and say what IS held, so the caller can see the gap: {e}"
    );
    assert!(
        e["detail"]["exempt_not_consoles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x.as_str().unwrap().contains("Bantam")),
        "a lease on the controller is NOT required and must be named as exempt: {e}"
    );
    assert!(
        e["hint"].as_str().unwrap().contains("acquire("),
        "with a copy-pasteable fix: {e}"
    );
}

/// F1: the device form still works, and says what it did not do.
#[test]
fn f1_device_form_still_works_and_advises_about_siblings() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardx", &[AP, SM], CTL);
    rig.lease(AP);

    let r = rig.call("power", json!({"device": AP, "action": "on"}));
    assert!(r["boot_id"].is_i64(), "one epoch, as before: {r}");
    assert!(
        r["opened"].is_null(),
        "no per-member list for a device call"
    );
    let note = r["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("boardx") && note.contains("sibling"),
        "an agent aiming at one console of a board should be told: {r}"
    );
}

#[test]
fn f1_device_and_target_are_mutually_exclusive() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardx", &[AP, SM], CTL);
    let e = rig.err(
        "power",
        json!({"device": AP, "target": "boardx", "action": "on"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    assert!(e["message"]
        .as_str()
        .unwrap()
        .contains("mutually exclusive"));
}

// ------------------------------------------------------------------- F5 -----

/// F5: dry_run validates everything and actuates nothing.
///
/// The nickname-to-hook bug (N1) broke ADP power for two rounds; one dry run
/// would have shown `--device adp-ventuno` in the argv and ended it.
#[test]
fn f5_dry_run_shows_the_argv_and_changes_nothing() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardx", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);

    let before = rig.call("list_boots", json!({"device": AP}));
    let n_before = before["boots"].as_array().map(|b| b.len()).unwrap_or(0);

    let r = rig.call(
        "power",
        json!({"target": "boardx", "action": "off", "dry_run": true}),
    );
    assert_eq!(r["dry_run"], true);
    let cmd = r["hook"]["command"].to_string();
    assert!(cmd.contains("off"), "the action is in the argv: {cmd}");
    assert!(
        r["would_open_epochs"].as_array().unwrap().len() == 2,
        "it says what it WOULD do: {r}"
    );

    let after = rig.call("list_boots", json!({"device": AP}));
    assert_eq!(
        after["boots"].as_array().map(|b| b.len()).unwrap_or(0),
        n_before,
        "a dry run must not open an epoch"
    );
}

/// F5 + N1: the argv carries the PORT, never the label -- the regression guard
/// for the bug that broke ADP power control for two rounds.
#[test]
fn f5_dry_run_argv_uses_the_port_even_when_the_board_is_labelled() {
    let rig = Rig::with_config(cfg_with_hook());
    let ids = rig.board("boardx", &[AP, SM], CTL);
    rig.registry().set_nickname(ids[0], "adp-ventuno").unwrap();
    rig.lease(AP);
    rig.lease(SM);

    let r = rig.call(
        "power",
        json!({"target": "boardx", "action": "on", "dry_run": true}),
    );
    let cmd = r["hook"]["command"].to_string();
    assert!(
        cmd.contains("usb-Rig_Board_X-if00-port0"),
        "the hook resolves hardware by port: {cmd}"
    );
    assert!(
        !cmd.contains("adp-ventuno"),
        "a label must never reach a hook argv: {cmd}"
    );
}

// ------------------------------------------------------------------- F7 -----

use conminer_core::console::{derive, ConsoleState, Observation};
use conminer_core::live::CaptureState;
use conminer_core::runner::{Prompt, Prompts};
use conminer_core::store::DeviceStore;

fn shell_prompts() -> Prompts {
    Prompts(vec![Prompt {
        re: regex::Regex::new(r"root@[\w.-]+:[^\s]*[#$]\s*$").unwrap(),
        raw: r"root@.*[#$] $".into(),
        kind: conminer_core::framer::profile::PromptKind::Shell,
    }])
}

/// F7: a prompt under steady spam is BOTH facts, not either one.
///
/// The ADP prints USB gadget errors several times a second forever. Reporting a
/// clean `at_prompt` hides the spam; reporting `unstable` hides the shell that
/// is sitting there taking commands.
#[test]
fn f7_a_prompt_under_traffic_reports_both_and_stays_commandable() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("t.db"), "/dev/ttyUSB0", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    let boot = store.open_boot("power", None, 0, Some(session)).unwrap();

    // A minute of flap, then the prompt.
    for i in 0..40 {
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: i,
                    ts_wall: 60_000 + i * 1_000,
                    bytes: format!("[  {i}.0] usb usb3-port1: config error").as_bytes(),
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
    }
    store.set_pending_tail("root@adp:~# ", 100_500).unwrap();

    let obs = Observation {
        capture: CaptureState::Listening,
        now_ms: 101_000,
        hung_after_ms: 30_000,
        loop_min_epochs: 3,
        active_txn: None,
    };
    let state = derive(&store, &shell_prompts(), &obs).unwrap();
    match &state {
        ConsoleState::AtPromptWithTraffic { lines_per_min, .. } => {
            assert!(*lines_per_min >= 10, "the spam is reported: {state:?}");
        }
        other => panic!("a prompt under spam must say so: {other:?}"),
    }
    assert!(state.commandable(), "the shell is still a shell");
    assert!(state.not_commandable_because().is_none());
}

/// F7: `commandable: false` must say WHY -- the three reasons need three
/// different responses from the caller.
#[test]
fn f7_not_commandable_always_states_its_reason() {
    let cases = [
        (
            ConsoleState::LoginWait {
                pattern: "login: $".into(),
            },
            "credential_gate",
        ),
        (
            ConsoleState::AtUnknownPrompt {
                observed_line: "??? ".into(),
            },
            "unknown_prompt",
        ),
        (
            ConsoleState::Booting {
                stage: "kernel".into(),
            },
            "no_prompt_yet",
        ),
    ];
    for (state, expect) in cases {
        let why = state
            .not_commandable_because()
            .unwrap_or_else(|| panic!("{state:?} must state a reason"));
        assert!(why.starts_with(expect), "{state:?} -> {why}");
        assert!(!state.commandable());
    }
    // ...and a commandable state must NOT invent one.
    assert!(ConsoleState::AtPrompt {
        kind: "shell".into(),
        pattern: "p".into(),
        stage: "userspace".into()
    }
    .not_commandable_because()
    .is_none());
}

/// F7: the runner and the reporter share one source of prompt knowledge.
#[test]
fn f7_a_confirmed_prompt_is_recorded_where_console_state_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("p.db"), "/dev/ttyUSB1", true).unwrap();

    store
        .observe_prompt(r"root@.*[#$] $", "shell", None, 1_000, None)
        .unwrap();
    let rows = store.prompts(None).unwrap();
    let row = rows
        .iter()
        .find(|r| r.pattern == r"root@.*[#$] $")
        .expect("the confirmed prompt is stored");
    assert_eq!(row.observations, 1, "first confirmation");
    assert_eq!(row.provenance, "learned", "it belongs to the device now");

    // Confirming again counts, rather than duplicating.
    store
        .observe_prompt(r"root@.*[#$] $", "shell", None, 2_000, None)
        .unwrap();
    let rows = store.prompts(None).unwrap();
    let row = rows.iter().find(|r| r.pattern == r"root@.*[#$] $").unwrap();
    assert_eq!(row.observations, 2);
    assert_eq!(
        rows.iter()
            .filter(|r| r.pattern == r"root@.*[#$] $")
            .count(),
        1,
        "one row per pattern"
    );
}

// ------------------------------------------------------------------- F2 -----

/// F2: what is RUNNING, lifted from the banners the board already printed.
///
/// `provenance.running` returned `{}` in every call across four rounds while
/// the same store held BL31's fingerprint, OP-TEE's commit, the UEFI string and
/// the kernel's #build. The data was never missing; nothing extracted it.
#[test]
fn f2_provenance_reports_what_is_actually_running() {
    let rig = Rig::new();
    // A real boot's identifying lines, in the order a board prints them.
    let device = rig.ingest_text(
        "NOTICE:  BL1: v2.15.0(release):release-v0.1\n\
         NOTICE:  BL2: v2.15.0(release):release-v0.1\n\
         NOTICE:  BL31: v2.15.0(release):NORDFP-260705-162552\n\
         I/TC: OP-TEE version: c0661f073 (gcc version 13.2.0) #1 Sun Jul  5 16:28:29 UTC 2026\n\
         UEFI Ver : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n\
         [    0.000000] Linux version 7.1.0-rc6-NORDFP-260705-154955+ (build@lab) (gcc 14.2.0) #187 SMP PREEMPT iq10-bringup\n\
         [    0.100000] Machine model: Arduino VENTUNO Q\n",
    );

    let p = rig.call("provenance", json!({"device": device}));
    let running = &p["running"];
    for (component, expect) in [
        ("bl2", "v2.15.0"),
        ("bl31", "NORDFP-260705-162552"),
        ("optee", "c0661f073"),
        ("uefi", "KODIAKLA"),
        ("kernel", "7.1.0-rc6-NORDFP-260705-154955+"),
        ("machine", "Arduino VENTUNO Q"),
    ] {
        let got = running[component]["version"]
            .as_str()
            .unwrap_or_else(|| panic!("no {component} in {running}"));
        assert!(
            got.contains(expect),
            "{component}: expected {expect:?}, got {got:?}"
        );
    }
    // The kernel's build number is the difference between two images that share
    // a version string -- exactly the confusion the rig's fingerprint rule
    // exists to prevent, so it must be carried too.
    assert_eq!(
        running["kernel"]["build"].as_str().unwrap_or(""),
        "#187 SMP PREEMPT iq10-bringup"
    );
    // And the claim is checkable: it says which line it came from.
    assert!(running["bl31"]["line_id"].is_i64(), "{running}");
}

/// F2: a garbled banner is not a version.
#[test]
fn f2_a_truncated_banner_is_never_stored_as_a_version() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "NOTICE:  BL31: \n\
         I/TC: OP-TEE version: \n\
         [    0.000000] Linux version \n",
    );
    let p = rig.call("provenance", json!({"device": device}));
    let running = &p["running"];
    for c in ["bl31", "optee", "kernel"] {
        assert!(
            running[c].is_null(),
            "a banner with no version must store nothing, got {}",
            running[c]
        );
    }
}

// ------------------------------------------------------------------- F3 -----

/// F3: the XBL timing grammar survives templating.
///
/// `B - 12345 - sbl1_ddr_init` and `B - 67890 - sbl1_hw_init` differ in BOTH
/// fields, so the generic templatizer generalised both and every timing line in
/// the boot collapsed into one `D - <*> - <*>` -- 6,755 occurrences on the ADP,
/// and per-label timing could not be queried at all.
#[test]
fn f3_xbl_timing_labels_survive_templating() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "B -      1234 - sbl1_ddr_init\n\
         B -      5678 - sbl1_hw_init\n\
         D -      9012 - sbl1_ddr_init\n\
         B -      2345 - sbl1_ddr_init\n\
         S - DDR Frequency, 3196 MHz\n",
    );
    let toc = rig.call("list_templates", json!({"device": device, "limit": 50}));
    let texts: Vec<String> = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap_or_default().to_string())
        .collect();

    // Each LABEL gets its own template, with the microseconds in a slot. The two
    // `B` samples of sbl1_ddr_init disagree, so that one has a wildcard; the
    // labels seen once keep their literal value, which is the documented rule --
    // a wildcard means two observations disagreed, never a regex guess.
    let ddr_begin = texts
        .iter()
        .find(|t| t.contains("sbl1_ddr_init") && t.contains(" B "))
        .unwrap_or_else(|| panic!("no per-label template: {texts:?}"));
    assert!(
        ddr_begin.contains("<*>"),
        "two disagreeing samples make the microseconds a slot: {ddr_begin}"
    );
    assert!(
        texts.iter().any(|t| t.contains("sbl1_hw_init")),
        "a different label is a different template: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("sbl1_ddr_init") && t.contains(" D ")),
        "begin and done are different events, not two spellings of one: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("DDR Frequency")),
        "`S - key, value` keeps its key: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.starts_with("B - <*> - <*>")),
        "the collapsed-everything template is the bug: {texts:?}"
    );
}

/// F3: pinned metrics turn a template slot into a series.
#[test]
fn f3_a_pinned_metric_gives_a_series_across_epochs() {
    let rig = Rig::new();
    // Three boots with three values -- the numbers this rig actually measured.
    // One occurrence would produce NO slot at all, and correctly so: a wildcard
    // means two observations disagreed, never a regex guess.
    let device = rig.ingest_into("UEFI Total : 1016 ms\n", None);
    rig.ingest_into("UEFI Total : 1019 ms\n", Some(&device));
    rig.ingest_into("UEFI Total : 1472 ms\n", Some(&device));
    let toc = rig.call("list_templates", json!({"device": device, "limit": 50}));
    let t = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["text"]
                .as_str()
                .unwrap_or_default()
                .contains("UEFI Total")
        })
        .unwrap_or_else(|| panic!("no UEFI Total template: {toc}"));
    let tid = t["id"].as_i64().unwrap();
    // Which slot holds the number? The template has one wildcard.
    let vals = rig.call(
        "template_values",
        json!({"device": device, "template_id": tid}),
    );
    let slot = vals["slots"][0]["slot"].as_i64().unwrap_or(0);

    rig.call(
        "pin_metric",
        json!({"device": device, "name": "uefi_total_ms", "template_id": tid,
               "slot": slot, "unit": "ms"}),
    );
    let listed = rig.call("list_metrics", json!({"device": device}));
    assert_eq!(listed["metrics"][0]["name"], "uefi_total_ms");

    let series = rig.call(
        "metric_series",
        json!({"device": device, "name": "uefi_total_ms"}),
    );
    assert_eq!(
        series["stats"]["samples"], 3,
        "one value per epoch: {series}"
    );
    let vals: Vec<f64> = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["value"].as_f64().unwrap())
        .collect();
    assert_eq!(vals, vec![1016.0, 1019.0, 1472.0], "oldest first: {series}");
    assert_eq!(series["stats"]["max"], 1472.0);
    assert_eq!(series["unit"], "ms");

    // Unpinning forgets the question, not the data.
    rig.call(
        "unpin_metric",
        json!({"device": device, "name": "uefi_total_ms"}),
    );
    assert!(
        rig.call("list_metrics", json!({"device": device}))["metrics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let vals_after = rig.call(
        "template_values",
        json!({"device": device, "template_id": tid}),
    );
    assert!(vals_after["slots"][0]["samples"].as_array().is_some());
}

/// F3: an epoch with no stages is skipped, never zero-filled.
#[test]
fn f3_stage_timings_skip_epochs_with_no_stages_rather_than_zero_filling() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "NOTICE:  BL2: v2.15.0(release)\n\
         [    0.000000] Linux version 7.1.0 (b@l) (gcc) #1 SMP\n\
         [    5.000000] systemd[1]: Reached target Multi-User System.\n",
    );
    let t = rig.call("stage_timings", json!({"device": device, "last": 20}));
    let series = t["series"].as_array().unwrap();
    for row in series {
        assert!(
            !row["stages"].as_array().unwrap().is_empty(),
            "an epoch with no stages must not appear at all: {row}"
        );
    }
    // A trend needs more than two points: two always describe a perfect line.
    for st in t["stats"]["per_stage"].as_array().unwrap() {
        if st["samples"].as_u64().unwrap_or(0) < 3 {
            assert!(
                st["trend_ms_per_boot"].is_null(),
                "a confident trend from two points is invented: {st}"
            );
        }
    }
}

// ---------------------------------------------------------------- F6/F8/F9 --

/// F6: a follow can park ON a watch, so an agent babysitting a soak stops
/// polling on a timer.
#[test]
fn f6_follow_can_wait_for_a_watch_to_fire() {
    let rig = Rig::new();
    // The watch starts at the CURRENT head, so the line it should catch has to
    // arrive after it exists -- exactly like a soak on real hardware.
    let device = rig.ingest_text("[    0.1] booting\n");
    rig.call(
        "create_watch",
        json!({"device": device, "name": "flap", "until": {"pattern": "config error"}}),
    );
    rig.ingest_into("[    1.0] usb usb3-port1: config error\n", Some(&device));

    // NO poll_watch here, deliberately. Found on the IQ10: the scanner only ran
    // when a watch was POLLED, so a parked follow waited on a queue nothing was
    // filling and timed out at 60 s with `fired_total: 0` while the console was
    // talking throughout. A feature that still needs a poll does not replace
    // polling, which was its entire purpose.
    let f = rig.call(
        "follow",
        json!({"device": device, "until": {"watch": "flap"}, "timeout_s": 5}),
    );
    assert_eq!(f["follow"]["matched"], "watch:flap", "{f}");
    assert!(
        f["follow"]["evidence"]["watch"] == "flap"
            && f["follow"]["evidence"]["matched"].is_string(),
        "the firing itself is the evidence: {f}"
    );
    // A watch OF a watch would need the scanner to run in a defined order and
    // would double-record one event, so it is refused rather than accepted and
    // silently never firing.
    let e = rig.err(
        "create_watch",
        json!({"device": device, "name": "meta", "until": {"watch": "flap"}}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
    assert!(e["message"].as_str().unwrap().contains("watch"), "{e}");
}

/// F6: `fired_total` answers "did anything happen at all since I set this up?"
/// without consuming the queue.
#[test]
fn f6_list_watches_reports_pending_and_total() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    0.1] booting\n");
    rig.call(
        "create_watch",
        json!({"device": device, "name": "flap", "until": {"pattern": "config error"}}),
    );
    rig.ingest_into("[    1.0] usb usb3-port1: config error\n", Some(&device));
    rig.call(
        "poll_watch",
        json!({"device": device, "name": "flap", "peek": true}),
    );
    let l = rig.call("list_watches", json!({"device": device}));
    let w = &l["watches"][0];
    assert!(w["fired_total"].as_i64().unwrap_or(0) >= 1, "{l}");
    assert!(w["pending"].as_i64().is_some(), "{l}");
}

/// F8: the instructions carry the procedural knowledge, not just schema advice.
#[test]
fn f8_the_server_instructions_teach_the_rules_that_save_hours() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/handler.rs"
    ))
    .unwrap();
    for rule in [
        "actuate by TARGET",
        "boundary lag",
        "NEVER the \\\n                 dashboard power field",
        "next_offset",
        "frequency, not chronology",
        "dry_run",
    ] {
        assert!(src.contains(rule), "the instructions must carry: {rule:?}");
    }
}

/// F8: the costs an agent needs to plan with are machine-readable.
#[test]
fn f8_help_states_cost_and_precondition_for_the_expensive_tools() {
    let rig = Rig::new();
    for (tool, expect) in [
        ("run_command", "7 s"),
        ("power", "verification"),
        ("boot_mode", "LATCH"),
    ] {
        let h = rig.call("help", json!({"tool": tool}));
        let cost = h["cost"].as_str().unwrap_or_default();
        assert!(cost.contains(expect), "{tool}: {h}");
        assert!(
            h["precondition"].as_str().is_some(),
            "{tool} must say what has to be true first: {h}"
        );
    }
    // A cheap read does not need to claim a cost.
    let h = rig.call("help", json!({"tool": "list_templates"}));
    assert!(h["cost"].is_null(), "no noise for a database read: {h}");
}

/// F9: pruning reclaims BYTES and keeps the knowledge derived from them.
#[test]
fn f9_pruning_drops_raw_and_keeps_the_answers() {
    let rig = Rig::new();
    let mut text = String::new();
    for i in 0..200 {
        text.push_str(&format!("[    {i}.0] usb usb3-port1: config error {i}\n"));
    }
    let device = rig.ingest_text(&text);
    rig.lease(&device);

    let before = rig.call("stats", json!({"device": device}));
    let toc_before = rig.call("list_templates", json!({"device": device, "limit": 100}));
    let n_templates = toc_before["templates"].as_array().unwrap().len();
    assert!(before["retention"]["raw_bytes"].as_u64().unwrap() > 0);

    // A dry run must change nothing.
    let dry = rig.call(
        "prune",
        json!({"device": device, "keep_bytes": 100, "dry_run": true}),
    );
    assert_eq!(dry["lines_removed"], 0);
    assert_eq!(
        rig.call("stats", json!({"device": device}))["retention"]["raw_bytes"],
        before["retention"]["raw_bytes"],
        "a dry run must not reclaim anything"
    );

    let p = rig.call("prune", json!({"device": device, "keep_bytes": 100}));
    assert!(p["lines_removed"].as_u64().unwrap() > 0, "{p}");
    assert!(p["reclaimed_bytes"].as_u64().unwrap() > 0, "{p}");

    // THE POINT: the compressed knowledge survives.
    let toc_after = rig.call("list_templates", json!({"device": device, "limit": 100}));
    assert_eq!(
        toc_after["templates"].as_array().unwrap().len(),
        n_templates,
        "templates are the product and must survive pruning"
    );
    assert!(rig.call("boot_report", json!({"device": device}))["outcome"].is_string());

    // ...and a query into the pruned range says so, rather than returning less.
    let e = rig.err("get_context", json!({"device": device, "line_id": 1}));
    assert_eq!(e["code"], "PRUNED", "{e}");
    assert!(
        e["hint"].as_str().unwrap().contains("list_templates"),
        "{e}"
    );
}

// -------------------------------------------------------------- F4 / F10 ----

/// F4: dmesg flags reach a root shell, so they are allow-listed rather than
/// passed through.
#[test]
fn f4_snapshot_dmesg_refuses_arguments_that_are_not_allow_listed() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    0.1] booting\n");
    rig.lease(&device);
    let e = rig.err(
        "snapshot_dmesg",
        json!({"device": device, "args": "-T; rm -rf /"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
    assert!(
        e["detail"]["allowed"].is_array(),
        "and it must say what IS allowed: {e}"
    );
}

/// F4: the snapshot is framed into its OWN session, never the live stream.
///
/// Two thousand replayed kernel lines landing in the console's epoch would
/// inflate its byte counts and mint template occurrences that never crossed the
/// wire twice -- the store would then disagree with the board about what the
/// console actually said.
#[test]
fn f4_the_snapshot_is_a_separate_session_not_the_live_stream() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .unwrap();
    let tool = src
        .split("name: \"snapshot_dmesg\"")
        .nth(1)
        .expect("the tool exists");
    let body = &tool[..tool.find("name: \"prune\"").unwrap_or(tool.len())];
    assert!(
        body.contains("ingest_reader"),
        "it must go through the ingest pipeline, not the live one"
    );
    assert!(
        body.contains("SNAP-BEGIN-") && body.contains("SNAP-END-"),
        "delimiters make a truncated capture detectable"
    );
    assert!(
        body.contains("\"truncated\": truncated"),
        "and truncation must be reported, not hidden"
    );
    assert!(
        body.contains("bound_boot"),
        "the snapshot is bound to the epoch it describes"
    );
}

/// F10: the checksum a transfer is verified against must be one the BOARD can
/// compute for itself.
#[test]
fn f10_cksum_matches_the_posix_algorithm_boards_use() {
    // Checked against the real tool, not from memory:
    //   $ printf 'hello world' | cksum  →  1135714720 11
    //   $ printf '' | cksum             →  4294967295 0
    assert_eq!(
        conminer_core::transfer::cksum(b"hello world"),
        1_135_714_720,
        "this is the number `cksum` on the board will print"
    );
    assert_eq!(conminer_core::transfer::cksum(b""), 4_294_967_295);
}

/// F10: the port is claimed for the transfer and ALWAYS released -- round 1's
/// lesson was that a stuck claim costs a console until a human notices.
#[test]
fn f10_the_exclusive_claim_is_always_released() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .unwrap();
    let tool = src
        .split("name: \"transfer_file\"")
        .nth(1)
        .expect("the tool exists");
    let body = &tool[..tool.find("name: \"snapshot_dmesg\"").unwrap_or(tool.len())];
    let claim = body.find("claim_exclusive").expect("it claims the port");
    let release = body.find("release_exclusive").expect("and releases it");
    let unwrap = body
        .find("let bytes = result?;")
        .expect("the result is unwrapped");
    assert!(claim < release, "claim, then release");
    assert!(
        release < unwrap,
        "release BEFORE propagating a failure, or a failed transfer strands the port"
    );
    assert!(
        body.contains("command -v sz"),
        "the precondition is checked before the port is claimed"
    );
}

/// F1, found ON HARDWARE the moment target actuation shipped: `power {target}`
/// reported `verified: false` and escalated to an unrequested power cycle on an
/// IQ10 that had booted perfectly.
///
/// Verification watched the PRIMARY console while a sibling did the talking --
/// the same epoch-stranding this feature exists to fix, one layer down. A board
/// responded if ANY of its consoles did; the quiet one proves nothing.
#[test]
fn f1_effect_verification_watches_every_console_of_the_board() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .unwrap();
    // The set is now WIDER than the one this gate first pinned. `consoles` is
    // whose epoch opens -- deliberately just the named console for the device
    // form, or evidence lands on a sibling and is stranded. `watched` is whose
    // TRAFFIC proves the board answered, and that must cover the whole board:
    // aiming at a quiet interface made a healthy board look dead and got it
    // power-cycled. Same intent as before, applied to both call forms.
    // Whitespace-insensitive: the call spans several lines once it takes more
    // arguments, and an assertion that pins the formatting fails on a rustfmt
    // pass that changed nothing about the behaviour it guards.
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("verify_power_effect( ctx, d, &scope.watched,"),
        "the verifier must be given the whole board, not one console"
    );
    assert!(
        src.contains("let watched = board_siblings(ctx, &d);"),
        "…and the device form must widen it to the board's other consoles"
    );
    let f = src
        .split("fn verify_power_effect")
        .nth(1)
        .expect("the verifier exists");
    let bytes = f
        .find("let bytes = |ctx: &Context|")
        .expect("the byte probe");
    let loop_over = f[bytes..]
        .find("for dev in watched")
        .expect("it must sum across the watched consoles");
    assert!(loop_over < 400, "the probe itself iterates: {loop_over}");
    assert!(
        f.contains("idle = idle.min("),
        "the FRESHEST console decides idleness: one silent port must not make a \
         talkative board look idle"
    );
}

/// F4, found ON HARDWARE: the first version opened an ingest pipeline on the
/// CONSOLE'S OWN store, which takes the device writer lock — and minerd holds
/// that lock for every live console with `flock(LOCK_EX)`, which BLOCKS. The
/// call hung until the client gave up.
///
/// A sibling store is also the stronger form of the isolation F4 asks for: a
/// ring-buffer replay is not console output, so it must not touch the console's
/// templates, epoch byte counts or cursor.
#[test]
fn f4_the_snapshot_lands_in_its_own_store_not_the_live_one() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .unwrap();
    let tool = src
        .split("name: \"snapshot_dmesg\"")
        .nth(1)
        .expect("the tool exists");
    let body = &tool[..tool.find("name: \"prune\"").unwrap_or(tool.len())];
    assert!(
        body.contains("#dmesg"),
        "the snapshot needs a store of its own"
    );
    assert!(
        body.contains("&snap_dev.db_file") && body.contains("&snap_dev.canonical"),
        "the pipeline must open the SNAPSHOT store, never the console's: minerd \
         holds the console's writer lock and flock blocks"
    );
    assert!(
        !body.contains("ctx.data_dir().join(&d.db_file)"),
        "opening the live device's store here is the hang"
    );
    assert!(
        body.contains("\"snapshot_device\""),
        "and the caller must be told where it landed"
    );
}

/// Found ON HARDWARE: the IQ10 stuck in its GMU init loop prints one line every
/// 15 seconds. A two-second "talking" window read that as silence between every
/// pair of messages, so the epoch-chain fallback answered `unstable` about a
/// console that was visibly alive and printing.
///
/// How recent counts as recent is a property of the BOARD, not a constant. The
/// configured hung threshold is already the line this system draws between
/// "quiet" and "not answering".
#[test]
fn a_board_that_prints_every_fifteen_seconds_is_talking_not_flapping() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(&dir.path().join("gmu.db"), "/dev/ttyUSBg", true).unwrap();
    let session = store
        .begin_session(
            conminer_core::store::SessionSource::Live,
            0,
            None,
            None,
            None,
        )
        .unwrap();
    // Enough divergent epochs that the chain gives up and would say `unstable`.
    for i in 0..12 {
        let boot = store
            .open_boot("power", None, i * 100, Some(session))
            .unwrap();
        store
            .append_lines(
                session,
                Some(boot.id),
                &[conminer_core::store::PendingLine {
                    stage_id: None,
                    ts_mono: i,
                    ts_wall: 10_000 + i * 100,
                    bytes: format!("[  {i}.0] boot {i} says something different").as_bytes(),
                    terminator: conminer_core::linesplit::Terminator::Lf,
                    truncated: false,
                    continuation: false,
                }],
            )
            .unwrap();
        store
            .set_boot_summary(boot.id, Some(&format!("fp-{i}")), None)
            .unwrap();
    }
    // The live epoch: one GMU line, 15 s ago -- the real cadence.
    let boot = store
        .open_boot("power", None, 20_000, Some(session))
        .unwrap();
    store
        .append_lines(
            session,
            Some(boot.id),
            &[conminer_core::store::PendingLine {
                stage_id: None,
                ts_mono: 99,
                ts_wall: 100_000,
                bytes: b"[  100.1] platform 3d6a000.gmu: NORD JTAG-HOLD 90s: AO(1f888)=0x0",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
            }],
        )
        .unwrap();

    let state = derive(
        &store,
        &shell_prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 115_000, // 15 s after that line
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        !matches!(state, ConsoleState::Unstable { .. }),
        "a board printing every 15 s is alive, not flapping: {state:?}"
    );
    // ...and past the hung threshold it IS the epoch chain's turn again.
    let quiet = derive(
        &store,
        &shell_prompts(),
        &Observation {
            capture: CaptureState::Listening,
            now_ms: 100_000 + 45_000,
            hung_after_ms: 30_000,
            loop_min_epochs: 3,
            active_txn: None,
        },
    )
    .unwrap();
    assert!(
        matches!(
            quiet,
            ConsoleState::Unstable { .. } | ConsoleState::Hung { .. }
        ),
        "a console with nothing to say gets the history verdict: {quiet:?}"
    );
}

// ------------------------------------------------------------------ F11 -----

/// F11: an agent must be able to ask WHAT IS FLOODING the console, by rate.
///
/// `list_templates` answers "what has this board ever said, and how often" — a
/// lifetime count, which cannot tell a message that fired 6,000 times during a
/// boot two days ago from one firing four times a second right now. A board that
/// starts crash-looping produces the second kind, and an agent that reads raw
/// output without knowing spends its whole window on one repeated line.
#[test]
fn f11_noise_names_what_is_flooding_and_how_much_of_the_output_it_is() {
    let rig = Rig::new();
    // 200 copies of one message, 3 of something else: an ordinary flood.
    let mut text = String::new();
    for i in 0..200 {
        // The SAME message, as a real flood is. A line that varies in two places
        // has its words generalised away -- correctly, that is what a wildcard
        // means -- and then there is no literal left to recognise it by.
        text.push_str(&format!("[   {i}.0] cpu 3: watchdog: BUG: soft lockup\n"));
    }
    text.push_str("[  99.0] EXT4-fs (sda1): mounted filesystem\n");
    text.push_str("[  99.1] systemd[1]: Reached target Multi-User System.\n");
    text.push_str("[  99.2] random: crng init done\n");
    let device = rig.ingest_text(&text);

    let n = rig.call(
        "noise",
        json!({"device": device, "window_s": 86400, "limit": 3}),
    );
    let top = &n["top"][0];
    assert!(
        top["text"]
            .as_str()
            .unwrap_or_default()
            .contains("soft lockup"),
        "the flood must be named: {n}"
    );
    assert!(
        top["count_in_window"].as_i64().unwrap_or(0) >= 150,
        "with its count: {top}"
    );
    assert!(
        top["per_min"].as_f64().unwrap_or(0.0) > 0.0,
        "and a RATE, which is the question list_templates cannot answer: {top}"
    );
    assert!(
        top["share_of_output"].as_f64().unwrap_or(0.0) > 90.0,
        "and its share of the console's output: {top}"
    );
    assert!(
        n["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("annotate_template"),
        "the number alone does not say what to DO: {n}"
    );
}

/// F11: muting a template collapses it out of the raw views, and SAYS how many
/// it collapsed — a suppressed flood must never look like a quiet board.
#[test]
fn f11_a_muted_template_is_suppressed_from_raw_output_and_counted() {
    let rig = Rig::new();
    let mut text = String::new();
    for i in 0..60 {
        text.push_str(&format!("[   {i}.0] usb usb3-port1: config error {i}\n"));
    }
    text.push_str("[  99.0] Internal error: Oops: 96000045\n");
    let device = rig.ingest_text(&text);

    let toc = rig.call("list_templates", json!({"device": device, "limit": 50}));
    let flood = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["text"]
                .as_str()
                .unwrap_or_default()
                .contains("config error")
        })
        .expect("the flood template");
    let id = flood["id"].as_i64().unwrap();

    let before = rig.call("get_recent", json!({"device": device, "lines": 100}));
    let before_lines = before["count"].as_i64().unwrap_or(0);

    rig.call(
        "annotate_template",
        json!({"device": device, "template_id": id, "verdict": "benign",
               "note": "known flap, muted for reading"}),
    );

    let after = rig.call(
        "get_recent",
        json!({"device": device, "lines": 100, "suppress_noise": true}),
    );
    let after_lines = after["count"].as_i64().unwrap_or(0);
    assert!(
        after_lines < before_lines,
        "the flood must not fill the response: {before_lines} -> {after_lines}"
    );
    assert!(
        after["suppressed_noise_lines"].as_i64().unwrap_or(0) > 0,
        "and the count must be REPORTED: a suppressed flood is not a quiet board: {after}"
    );
    // The signal survives the mute.
    assert!(
        after["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Internal error"),
        "muting noise must never hide the crash: {after}"
    );
    // ...and the default view is unchanged for callers who did not ask.
    let untouched = rig.call("get_recent", json!({"device": device, "lines": 100}));
    assert_eq!(untouched["count"].as_i64().unwrap_or(0), before_lines);
}

/// F11: RATE IS A WINDOW, not a lifetime count — the whole reason this exists.
///
/// A message that fired 500 times two days ago and one firing four times a
/// second right now are indistinguishable in `list_templates`, and they call for
/// opposite responses: one is history, the other is what is burning the agent's
/// context. Driven through the real pipeline, because occurrences exist only
/// because MINING wrote them — a test that appends raw lines would be asserting
/// against an empty table and passing for the wrong reason.
#[test]
fn f11_an_old_flood_is_not_reported_as_flooding_now() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();

    // A clock we control: the whole question is "when did these lines arrive?".
    let day: i64 = 86_400_000;
    let clock = Arc::new(conminer_core::clock::StepClock::new(1_000_000, 0));
    let store = DeviceStore::open(&dir.path().join("noise.db"), "/dev/ttyUSBn", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSBn",
        None,
        clock.clone(),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();

    // Yesterday's flood.
    for _ in 0..200 {
        pipe.feed(b"[   1.0] ancient flood line\n").unwrap();
    }
    // ...and a small trickle a day later.
    clock.advance_ms(day);
    for _ in 0..3 {
        pipe.feed(b"[   2.0] live trickle line\n").unwrap();
    }
    pipe.tick().unwrap();
    let now = 1_000_000 + day;
    let st = pipe.store();

    let recent = st.noisiest_templates(now - 120_000, now, 5).unwrap();
    assert!(
        !recent
            .iter()
            .any(|r| r["text"].as_str().unwrap_or_default().contains("ancient")),
        "a flood from a day ago is history, not noise now: {recent:?}"
    );
    let all = st.noisiest_templates(now - 2 * day, now, 5).unwrap();
    assert!(
        all.iter()
            .any(|r| r["text"].as_str().unwrap_or_default().contains("ancient")),
        "over a window that covers it, it is exactly what you would ask about: {all:?}"
    );
}

/// F11: the rate is arithmetic an agent can check, not a vibe.
#[test]
fn f11_the_reported_rate_matches_the_window_it_was_measured_over() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let clock = Arc::new(conminer_core::clock::StepClock::new(1_000_000, 0));
    let store = DeviceStore::open(&dir.path().join("rate.db"), "/dev/ttyUSBr", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSBr",
        None,
        clock.clone(),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();

    // 120 copies of ONE message, one per second across two minutes.
    for _ in 0..120 {
        pipe.feed(b"[   1.0] cpu 3: watchdog: BUG: soft lockup\n")
            .unwrap();
        clock.advance_ms(1_000);
    }
    pipe.tick().unwrap();
    let now = 1_000_000 + 120_000;
    let top = pipe
        .store()
        .noisiest_templates(now - 120_000, now, 1)
        .unwrap();
    assert!(!top.is_empty(), "the flood must be found at all");
    let per_min = top[0]["per_min"].as_f64().unwrap();
    assert!(
        (per_min - 60.0).abs() <= 1.0,
        "120 lines over 2 minutes is 60/min, got {per_min}: {top:?}"
    );
}

/// F11: muting is per-device and does not leak to another board.
#[test]
fn f11_a_mute_belongs_to_the_board_it_was_set_on() {
    let rig = Rig::new();
    let a = rig.ingest_text(
        "[   1.0] usb usb3-port1: config error one\n[   2.0] usb usb3-port1: config error two\n",
    );
    let b = rig.ingest_into(
        "[   1.0] usb usb3-port1: config error one\n[   2.0] usb usb3-port1: config error two\n[   3.0] different board\n",
        None,
    );
    assert_ne!(a, b, "two devices");

    let toc = rig.call("list_templates", json!({"device": a, "limit": 20}));
    let id = toc["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["text"]
                .as_str()
                .unwrap_or_default()
                .contains("config error")
        })
        .expect("flood template")["id"]
        .as_i64()
        .unwrap();
    rig.call(
        "annotate_template",
        json!({"device": a, "template_id": id, "verdict": "benign"}),
    );

    let na = rig.call("noise", json!({"device": a, "window_s": 86400}));
    let nb = rig.call("noise", json!({"device": b, "window_s": 86400}));
    assert!(
        !na["muted_templates"].as_array().unwrap().is_empty(),
        "the board it was set on knows: {na}"
    );
    assert!(
        nb["muted_templates"].as_array().unwrap().is_empty(),
        "another board must not inherit somebody else's mute: {nb}"
    );
}

/// F11: a console with nothing to say is not "flooding with nothing".
#[test]
fn f11_a_quiet_console_says_so_rather_than_reporting_a_rate() {
    let rig = Rig::new();
    let device = rig.ingest_text("[   1.0] one quiet line\n");
    // Move on: the line is now well outside the window we ask about, which is
    // what "the console has said nothing lately" actually means.
    rig.clock.advance_ms(600_000);
    let n = rig.call("noise", json!({"device": device, "window_s": 5}));
    assert_eq!(n["lines_in_window"].as_i64().unwrap_or(-1), 0, "{n}");
    assert!(
        n["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("said nothing"),
        "silence is a different answer from calm: {n}"
    );
    assert!(n["top"].as_array().unwrap().is_empty(), "{n}");
}

/// F11: a BURST must be recognised however wide a window the caller asked about.
///
/// Found by the tests above: 200 lines arriving in 200 ms, asked about over 24
/// hours, averaged to 0.1/min and the advice said "ordinary output" — a real
/// flood talked out of existence by arithmetic. `per_min` is what the message
/// costs over the window; `burst_per_min` is how fast it comes when it comes,
/// and that is what decides whether an agent should mute it before reading.
#[test]
fn f11_a_burst_is_recognised_even_over_a_wide_window() {
    let rig = Rig::new();
    let mut text = String::new();
    for _ in 0..200 {
        text.push_str("[   1.0] cpu 3: watchdog: BUG: soft lockup\n");
    }
    text.push_str("[  99.0] EXT4-fs (sda1): mounted filesystem\n");
    let device = rig.ingest_text(&text);

    let wide = rig.call("noise", json!({"device": device, "window_s": 86400}));
    let top = &wide["top"][0];
    assert!(
        top["per_min"].as_f64().unwrap_or(0.0) < 10.0,
        "spread over a day, the average IS small -- that number is honest: {top}"
    );
    assert!(
        top["burst_per_min"].as_f64().unwrap_or(0.0) >= 60.0,
        "but the burst rate is what an agent needs: {top}"
    );
    assert!(
        wide["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("annotate_template"),
        "so the advice must still say to mute it: {wide}"
    );

    // A board that says something occasionally is NOT told to start muting.
    let calm = rig.ingest_text(
        "[   1.0] systemd[1]: Reached target Multi-User System.\n\
         [   2.0] random: crng init done\n\
         [   3.0] EXT4-fs (sda1): mounted filesystem\n",
    );
    let c = rig.call("noise", json!({"device": calm, "window_s": 86400}));
    assert!(
        !c["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("annotate_template"),
        "ordinary output must not be called a flood: {c}"
    );
}

/// F11, found ON HARDWARE the moment `noise` shipped: a window containing ONE
/// line reported `burst_per_min: 60000` and advised the agent to start muting.
///
/// One occurrence has no span between occurrences; dividing by the millisecond
/// floor turns it into an enormous rate that is an artefact of the clock, not a
/// property of the board. A false flood alarm is the same class of lie as a
/// false `verified: true`.
#[test]
fn f11_a_single_line_is_not_a_burst() {
    let rig = Rig::new();
    let device = rig.ingest_text("[   1.0] one lonely line\n");
    let n = rig.call("noise", json!({"device": device, "window_s": 86400}));
    let top = &n["top"][0];
    assert!(
        top["burst_per_min"].is_null(),
        "one sample has no burst rate to report: {top}"
    );
    assert!(
        !n["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("annotate_template"),
        "and it must not tell an agent to go muting one line: {n}"
    );

    // Five or more of the same message DOES have a measurable span.
    let mut text = String::new();
    for _ in 0..40 {
        text.push_str("[   1.0] cpu 3: watchdog: BUG: soft lockup\n");
    }
    let flood = rig.ingest_text(&text);
    let f = rig.call("noise", json!({"device": flood, "window_s": 86400}));
    assert!(
        f["top"][0]["burst_per_min"].as_f64().unwrap_or(0.0) > 0.0,
        "a real repeat has a real rate: {f}"
    );
}

/// F11: a STORM and a REPEATING MESSAGE need different advice.
///
/// Measured on the ADP mid-boot: 1,969 lines in two minutes with no single
/// message above 20% share. Calling that "ordinary output" undersells what an
/// agent is walking into; calling it a flood would cry wolf at every normal
/// boot. The move differs too -- you mute a repeating line, but you read
/// templates instead of lines when the whole console is loud.
#[test]
fn f11_a_broad_storm_is_advised_differently_from_one_repeating_line() {
    let rig = Rig::new();
    // Many genuinely DIFFERENT messages, fast: a boot.
    //
    // Different WORDS, not one sentence with a varying number -- lines that
    // differ only in a value are one template with a wildcard, correctly, and
    // that is the muting case rather than the storm case.
    let vocab = [
        "EXT4-fs (sda1): mounted filesystem with ordered data mode",
        "random: crng init done",
        "systemd[1]: Reached target Multi-User System",
        "usb 1-1: new high-speed USB device",
        "iommu: Adding device to group",
        "clk: Disabling unused clocks",
        "pci 0000:00:01.0: bridge window assigned",
        "thermal thermal_zone0: registered as sensor",
        "cfg80211: loading regulatory database",
        "input: gpio-keys as /devices/platform",
    ];
    let mut storm = String::new();
    for i in 0..900 {
        storm.push_str(&format!("[   {i}.0] {}\n", vocab[i % vocab.len()]));
    }
    let noisy_board = rig.ingest_text(&storm);
    let s = rig.call("noise", json!({"device": noisy_board, "window_s": 60}));
    let advice = s["advice"].as_str().unwrap_or_default();
    assert!(
        advice.contains("no single message dominates"),
        "a storm must be named as one: {s}"
    );
    assert!(
        advice.contains("list_templates") || advice.contains("templates, not lines"),
        "and it must say what to do instead of reading it: {advice}"
    );
    assert!(
        !advice.contains("annotate_template"),
        "muting one template would not help here: {advice}"
    );

    // One message, repeating: mute THAT.
    let mut flood = String::new();
    for _ in 0..900 {
        flood.push_str("[   1.0] cpu 3: watchdog: BUG: soft lockup\n");
    }
    let looping = rig.ingest_text(&flood);
    let f = rig.call("noise", json!({"device": looping, "window_s": 60}));
    assert!(
        f["advice"]
            .as_str()
            .unwrap_or_default()
            .contains("annotate_template"),
        "one repeating line is exactly what muting is for: {f}"
    );
}

/// F11, found ON HARDWARE: the ADP reported `share_of_output: 118.8%`.
///
/// Occurrences are rolled up per EPOCH, not per line, so a rollup that started
/// before the window still carried its whole count — 171 occurrences reported
/// inside a window holding 144 lines. That is not a rounding error, it is
/// counting the wrong thing, and an agent reading "118% of the output" learns
/// only that the number is wrong.
///
/// conminer keeps counts, not per-occurrence timestamps — that compression is
/// the point of the tool — so a straddling rollup is PRORATED over its own span
/// and the response says the number is an estimate.
#[test]
fn f11_a_share_of_output_can_never_exceed_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let clock = Arc::new(conminer_core::clock::StepClock::new(1_000_000, 0));
    let store = DeviceStore::open(&dir.path().join("share.db"), "/dev/ttyUSBs", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSBs",
        None,
        clock.clone(),
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();

    // 100 occurrences spread over ten minutes, in ONE epoch rollup...
    for _ in 0..100 {
        pipe.feed(b"[   1.0] usb usb3-port1: config error\n")
            .unwrap();
        clock.advance_ms(6_000);
    }
    pipe.tick().unwrap();
    let now = 1_000_000 + 600_000;
    let st = pipe.store();

    // ...asked about over the last two minutes, which holds ~20 of them.
    let window_start = now - 120_000;
    let rows = st.noisiest_templates(window_start, now, 5).unwrap();
    let lines = st.lines_since_ts(window_start).unwrap();
    let counted = rows[0]["count_in_window"].as_i64().unwrap();
    assert!(
        counted <= lines.max(1),
        "a template cannot occur more often than the console produced lines: \
         {counted} occurrences vs {lines} lines"
    );
    assert_eq!(
        rows[0]["approximate"], true,
        "a prorated count must be labelled an estimate, not presented as measured: {rows:?}"
    );

    // A rollup entirely inside the window is exact, and says so.
    let all = st.noisiest_templates(0, now, 5).unwrap();
    assert_eq!(all[0]["approximate"], false, "{all:?}");
    assert_eq!(all[0]["count_in_window"].as_i64().unwrap(), 100);
}

// =========================================================== G-series ======
//
// The feature gauntlet drove all three boards through the F-series and came
// back with eight findings. These are the gates for them: one per finding,
// each reproducing what the gauntlet actually saw.

/// G2: a slot that is not a slot must be REFUSED, not accepted in silence.
///
/// `pin_metric {slot: 0}` on a template whose number lives at token 3 was taken
/// without complaint and produced a permanently empty series. Nothing said why,
/// and an empty series reads exactly like "the board never printed it".
#[test]
fn g2_pinning_a_metric_to_a_non_slot_is_refused_with_the_slots_that_exist() {
    let rig = Rig::new();
    // Two boots so a value disagrees and a wildcard actually forms.
    let device = rig.ingest_text("[    1.0] uefi total 1018 ms\n[    2.0] idle\n");
    rig.ingest_into(
        "[    1.0] uefi total 1099 ms\n[    2.0] idle\n",
        Some(&device),
    );

    let ts = rig.call("list_templates", json!({"device": device, "limit": 50}));
    let t = ts["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["text"]
                .as_str()
                .unwrap_or_default()
                .contains("uefi total")
        })
        .expect("the uefi total template");
    let tid = t["id"].as_i64().unwrap();
    let text = t["text"].as_str().unwrap().to_string();
    assert!(
        text.contains("<*>"),
        "the value must have generalized: {text}"
    );

    // Token 0 is "uefi", never a slot.
    let e = rig.err(
        "pin_metric",
        json!({"device": device, "name": "bad", "template_id": tid, "slot": 0}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
    let hint = format!("{} {}", e["message"], e["hint"]);
    assert!(
        hint.contains("wildcard"),
        "the refusal must name what a slot IS: {hint}"
    );
    let slots = e["detail"]["wildcard_slots"].as_array().expect("slot list");
    assert!(
        !slots.is_empty(),
        "and it must list the slots that do exist: {e}"
    );

    // The listed slot works, and so does omitting it when there is only one.
    let good = rig.call(
        "pin_metric",
        json!({"device": device, "name": "uefi_total_ms",
               "template_id": tid, "slot": slots[0].clone()}),
    );
    assert_eq!(good["slot"], slots[0]);
    rig.call(
        "unpin_metric",
        json!({"device": device, "name": "uefi_total_ms"}),
    );
    let auto = rig.call(
        "pin_metric",
        json!({"device": device, "name": "auto", "template_id": tid}),
    );
    assert_eq!(
        auto["slot"], slots[0],
        "one wildcard and no preference: there is nothing to guess between"
    );
}

/// G3: the dmesg default must be sized from the console, not from taste.
///
/// A routine 65 KB dmesg hit `truncated: true, rc: null` at the old 120s
/// default. A console delivers ~11 KB/s.
#[test]
fn g3_the_dmesg_default_timeout_covers_a_routine_full_dmesg() {
    let cat = conminer_mcp::tools::registry();
    let t = cat
        .iter()
        .find(|t| t.name == "snapshot_dmesg")
        .expect("snapshot_dmesg");
    let schema = (t.schema)();
    let d = schema["properties"]["timeout_s"]["default"]
        .as_i64()
        .unwrap();
    assert!(
        d >= 300,
        "at ~11 KB/s a full dmesg needs minutes of streaming; {d}s truncates the \
         ordinary case"
    );
    let desc = schema["properties"]["timeout_s"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        desc.contains("KB/s"),
        "and the number must be justified where the next person will change it"
    );
}

/// G4: a snapshot must not break every substring selector for its console.
///
/// `snapshot_dmesg` materialises `<console>#dmesg`. On the rig that made
/// `AR40BYP4AU-if02` -- unique for weeks -- answer AMBIGUOUS_DEVICE.
#[test]
fn g4_a_dmesg_sub_device_does_not_shadow_its_consoles_selector() {
    let rig = Rig::new();
    // Two real stores whose canonical names are exactly the shape a snapshot
    // creates: the console, and `<console>#dmesg` beside it.
    let base = rig.dir.join("g4console.log");
    std::fs::write(&base, "[    1.0] hello\n").unwrap();
    let sub = rig.dir.join("g4console.log#dmesg");
    std::fs::write(&sub, "[    1.0] from the ring buffer\n").unwrap();
    let device = rig.call("ingest_file", json!({"path": base.display().to_string()}))["device"]
        .as_str()
        .unwrap()
        .to_string();
    rig.call("ingest_file", json!({"path": sub.display().to_string()}));
    assert!(
        device.ends_with("g4console.log"),
        "the console store is the one WITHOUT the suffix: {device}"
    );

    // A substring that matches both must still resolve to the console.
    let stem = "g4console.log".to_string();
    let got = rig.call("list_devices", json!({"filter": stem.clone()}));
    let names: Vec<String> = got["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["device"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        names.len(),
        1,
        "the sub-device must not compete for a substring selector: {names:?}"
    );
    assert!(
        !names[0].contains('#'),
        "and the winner is the port: {names:?}"
    );

    // Naming it explicitly still works.
    let explicit = rig.call("list_devices", json!({"filter": format!("{stem}#dmesg")}));
    assert_eq!(
        explicit["devices"].as_array().unwrap().len(),
        1,
        "a selector that says '#' means it"
    );
}

/// G6: a dry run is PLANNING. It reports lease problems instead of refusing,
/// so a read-only "what would this do" never takes a lease off another agent.
#[test]
fn g6_a_dry_run_reports_missing_leases_instead_of_refusing() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardg6", &[AP, SM], CTL);

    // No lease taken: this is the whole point.
    let plan = rig.call(
        "power",
        json!({"target": "boardg6", "action": "off", "dry_run": true}),
    );
    assert_eq!(plan["dry_run"], true);
    let lc = &plan["lease_check"];
    assert!(
        lc.get("missing").is_some(),
        "the lease problem must be REPORTED inside a successful dry run: {plan}"
    );
    assert!(
        lc["why"].as_str().unwrap_or_default().contains("dry run"),
        "and it must say the same call without dry_run would fail: {lc}"
    );

    // The real thing still refuses.
    let e = rig.err("power", json!({"target": "boardg6", "action": "off"}));
    assert_eq!(
        e["code"], "LEASE_REQUIRED",
        "an actuation without a lease must still be refused: {e}"
    );
}

/// G7: `stats` must say whether anything will ever reclaim the bytes.
///
/// A bench sitting at `pruned_before_offset: 0` because nothing is old enough
/// and one sitting there because no policy exists look identical and call for
/// opposite actions. The ADP reached 222k lines / 120 MB inside that ambiguity.
#[test]
fn g7_stats_says_whether_a_retention_policy_is_armed() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] hello\n[    2.0] world\n");

    let s = rig.call("stats", json!({"device": device}));
    let r = &s["retention"];
    for k in [
        "raw_bytes",
        "oldest_raw_ts",
        "pruned_before_offset",
        "cutoff_ts",
        "next_eligible_ts",
        "eligible_now",
        "policy",
        "armed",
        "enforced_by",
    ] {
        assert!(r.get(k).is_some(), "retention must report {k}: {r}");
    }
    assert_eq!(
        r["armed"], false,
        "the default config has no age or size rule, and saying so is the point"
    );
    assert!(
        r["enforced_by"]
            .as_str()
            .unwrap_or_default()
            .contains("no retention policy"),
        "an unarmed bench must be told in words: {r}"
    );

    // ...and an armed one reports the cutoff it would use.
    let mut cfg = Config::default();
    cfg.retention.raw_keep_days = 30;
    let rig2 = Rig::with_config(cfg);
    let d2 = rig2.ingest_text("[    1.0] hello\n");
    let r2 = rig2.call("stats", json!({"device": d2}));
    assert_eq!(r2["retention"]["armed"], true);
    assert!(
        r2["retention"]["cutoff_ts"].as_i64().is_some(),
        "an armed policy has a cutoff: {}",
        r2["retention"]
    );
    assert!(
        r2["retention"]["enforced_by"]
            .as_str()
            .unwrap_or_default()
            .contains("no background sweep"),
        "and it must not imply something runs on a timer: {}",
        r2["retention"]
    );
}

/// G8: the LOGIN_REQUIRED hint named a `login()` tool that does not exist.
#[test]
fn g8_the_login_hint_names_a_real_knob_and_not_a_missing_tool() {
    let hint = conminer_core::error::ErrorCode::LoginRequired.default_hint();
    assert!(
        !hint.contains("login()"),
        "there is no login tool; a hint that invents one sends the reader \
         hunting the tool list: {hint:?}"
    );
    assert!(
        hint.contains("credentials.file"),
        "it must name the knob that actually exists: {hint:?}"
    );
    // And that knob must really be the one in the config.
    let cfg = Config::default();
    let _ = &cfg.credentials.file;
}

/// G5(a): a boot's firmware chain does not respect epoch boundaries.
///
/// Measured on the rig: ONE boot arrived split across the boundary-lag pair --
/// kernel and machine in the power epoch, bl2/bl31/optee/uefi in the epoch
/// before it. `provenance` on either epoch showed half a chain, with nothing to
/// say the other half existed.
#[test]
fn g5_provenance_shows_the_whole_chain_across_the_boundary_lag() {
    let rig = Rig::new();
    // Epoch one: the early firmware banners.
    let device = rig.ingest_text(
        "NOTICE:  BL31: v2.10.0(release):NORDFP-260705-162552\n\
         I/TC: OP-TEE version: c0661f073 (gcc version 13.2.0)\n",
    );
    // Epoch two: the same boot's later half, as a separate ingest.
    rig.ingest_into(
        "[    0.000000] Linux version 6.12.0-rc1 (builder@host)\n\
         [    0.000000] Machine model: Qualcomm IQ-10 EVK\n",
        Some(&device),
    );

    let boots = rig.call("list_boots", json!({"device": device, "limit": 5}));
    let latest = boots["boots"][0]["id"].as_i64().expect("an epoch");
    assert!(
        boots["boots"].as_array().unwrap().len() >= 2,
        "the two ingests must be two epochs, or there is no boundary to cross: {}",
        boots["boots"]
    );
    let p = rig.call("provenance", json!({"device": device, "boot": latest}));
    let running = &p["running"];

    // The later half is this epoch's own, and carries no borrow label.
    let own: Vec<&String> = running
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.get("from_epoch").is_none())
        .map(|(k, _)| k)
        .collect();
    assert!(
        !own.is_empty(),
        "this epoch's own components must be present and unlabelled: {running}"
    );
    // ...and the earlier half is borrowed from the preceding epoch, LABELLED.
    let borrowed = running
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| v.get("from_epoch").is_some())
                .count()
        })
        .unwrap_or(0);
    assert!(
        borrowed > 0,
        "the chain from the preceding epoch must be folded in, or one call shows \
         half a boot: {running}"
    );
    for (k, v) in running.as_object().unwrap() {
        if let Some(from) = v.get("from_epoch") {
            assert_ne!(
                from.as_i64(),
                Some(latest),
                "{k} claims to be borrowed from this very epoch"
            );
            assert!(
                v.get("from").is_some(),
                "a borrowed component must say where it came from: {k} => {v}"
            );
        }
    }
}

/// G5(b): the verdict PROSE must hold the same line the fields do.
///
/// `set_image` records what somebody SAYS is on the board. Calling that "the
/// last flash pushed" turns an assertion into evidence, on a rig where "was
/// this reflashed?" is a forensic question.
#[test]
fn g5_a_hand_bound_image_is_never_described_as_a_flash() {
    let rig = Rig::new();
    let device =
        rig.ingest_text("NOTICE:  BL31: v2.10.0(release):NORDFP-111111-111111\n[    1.0] up\n");
    rig.call("acquire", json!({"device": device}));
    rig.call(
        "set_image",
        json!({"device": device, "name": "NORDFP-999999-999999"}),
    );
    let p = rig.call("provenance", json!({"device": device}));
    let why = p["why"].as_str().unwrap_or_default();

    assert!(
        p["bound_by_hand"].is_object(),
        "set_image records a binding: {p}"
    );
    assert!(p["last_flashed"].is_null(), "and NOT a flash: {p}");
    assert!(
        !why.contains("flash"),
        "the prose must not call a hand binding a flash: {why:?}"
    );
    assert!(
        why.contains("binding") || why.contains("bound"),
        "it must say what actually happened: {why:?}"
    );
}

/// G5(c): a board that identifies itself with no epoch open must still be heard.
///
/// Extraction used to require an open epoch, so a hand power-cycle or a
/// watchdog reset threw the whole chain away as it went past. Measured on the
/// ADP: `UEFI Ver : ...KODIAKLA-1` sat in the store as ordinary text while
/// provenance reported nothing running.
#[test]
fn g5_version_banners_are_kept_even_with_no_epoch_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.paths.data_dir = dir.path().to_path_buf();
    let clock = Arc::new(conminer_core::clock::StepClock::default());
    let store = DeviceStore::open(&dir.path().join("g5.db"), "/dev/ttyUSBg5", true).unwrap();
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        Arc::new(conminer_core::framer::ProfileSet::builtin().unwrap()),
        cfg,
        "/dev/ttyUSBg5",
        None,
        clock,
    )
    .unwrap();
    pipe.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    let boot = pipe.boot_id().expect("the session's epoch");
    // Close it: from here on nothing is open, exactly as after a hand reset.
    pipe.close_epoch().unwrap();

    pipe.feed(b"UEFI Ver    : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n")
        .unwrap();
    pipe.tick().unwrap();

    let versions = pipe.store().versions_in_boot(boot).unwrap();
    assert!(
        versions.iter().any(|(c, _)| c == "uefi"),
        "the banner must be recorded against the most recent epoch rather than \
         dropped: {versions:?}"
    );
}

/// G5(c), the other half: a store captured BEFORE the banner patterns existed
/// keeps the text and knows nothing about it. A rebuild is the moment to fix
/// that, because it already walks every record and it is the operation that
/// exists to make derived views a pure function of the raw.
#[test]
fn g5_rebuilding_templates_backfills_version_banners() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "UEFI Ver    : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n\
         S - QC_IMAGE_VERSION_STRING=BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n",
    );
    // Wipe what mining extracted, leaving exactly the state of a store filled
    // before the patterns shipped: the raw lines, and no versions.
    rig.registry();
    let path = {
        let reg = rig.registry();
        let d = reg.device_by_canonical(&device).unwrap().unwrap();
        reg.device_db_path(&d)
    };
    {
        let store = DeviceStore::open(&path, &device, true).unwrap();
        store.forget_versions_for_test().unwrap();
        let boots = store.list_boots(5).unwrap();
        assert!(
            store.versions_in_boot(boots[0].id).unwrap().is_empty(),
            "the fixture must start with nothing extracted"
        );
    }

    rig.call("acquire", json!({"device": device}));
    rig.call("rebuild_templates", json!({"device": device}));

    let p = rig.call("provenance", json!({"device": device}));
    let running = &p["running"];
    assert!(
        running.get("uefi").is_some(),
        "a rebuild must recover the banners the raw already held: {running}"
    );
}

/// FOUND while verifying G1 on hardware: a pinned metric made the whole store
/// un-rebuildable.
///
/// §F3's `pin_metric` added a fifth table referencing `templates(id)`, and
/// `rebuild_templates` -- which deletes and re-mints every template -- was never
/// taught about it. `DELETE FROM templates` hit the constraint and took the
/// entire rebuild down with `FOREIGN KEY constraint failed`. Measured: the ADP
/// (one metric pinned) could not be rebuilt at all, while the IQ10 (nothing
/// pinned) rebuilt fine, so it read as a property of the store rather than of
/// the feature that had been used on it.
#[test]
fn a_pinned_metric_survives_a_rebuild_instead_of_blocking_it() {
    let rig = Rig::new();
    // Two ingests so the value disagrees and a real wildcard forms.
    let device = rig.ingest_text("[    1.0] uefi total 1018 ms\n[    2.0] done\n");
    rig.ingest_into(
        "[    1.0] uefi total 1099 ms\n[    2.0] done\n",
        Some(&device),
    );

    let ts = rig.call("list_templates", json!({"device": device, "limit": 50}));
    let t = ts["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["text"]
                .as_str()
                .unwrap_or_default()
                .contains("uefi total")
        })
        .expect("the uefi total template");
    let tid = t["id"].as_i64().unwrap();

    rig.call("acquire", json!({"device": device}));
    let pinned = rig.call(
        "pin_metric",
        json!({"device": device, "name": "uefi_total_ms", "template_id": tid, "unit": "ms"}),
    );
    let before = rig.call(
        "metric_series",
        json!({"device": device, "name": "uefi_total_ms"}),
    );
    let samples_before = before["stats"]["samples"].as_i64().unwrap_or(0);
    assert!(
        samples_before > 0,
        "the metric must read something first: {before}"
    );

    // The rebuild must SUCCEED -- this is the part that failed on hardware.
    let re = rig.call("rebuild_templates", json!({"device": device}));
    assert!(
        re["after"].as_i64().is_some(),
        "the rebuild must complete: {re}"
    );

    // ...and the metric must still answer, on whatever id the template took.
    let after = rig.call(
        "metric_series",
        json!({"device": device, "name": "uefi_total_ms"}),
    );
    assert_eq!(
        after["stats"]["samples"].as_i64().unwrap_or(0),
        samples_before,
        "a pinned metric must be carried across the re-mint by TEXT, exactly as a \
         verdict is; template ids do not survive: {after}"
    );
    let list = rig.call("list_metrics", json!({"device": device}));
    let m = list["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "uefi_total_ms")
        .expect("the metric is still pinned");
    assert_eq!(
        m["slot"], pinned["slot"],
        "and it must still point at the slot holding the number"
    );
}

// =========================================================== H-series ======

/// H1: the dmesg capture must not be cut by a limit nobody asked for, and when
/// it IS cut, the report must name the limit that actually did it.
///
/// Measured twice on the IQ10: a full dmesg truncated at exactly 65,574 bytes
/// -- the same byte count both rounds, which is the signature of a buffer and
/// not of a clock -- while the call returned in 23s against a 300s budget. The
/// cap was `CommandOptions::new`'s 64 KB default, right for a shell command and
/// absurd for a ring buffer, which this call never raised. The previous round's
/// "fix" raised the TIMEOUT and taught the hint to blame it, so the tool then
/// asserted a cause that could not have been true.
#[test]
fn h1_the_dmesg_capture_is_not_cut_at_the_shell_command_default() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let f = src
        .split(r#"name: "snapshot_dmesg","#)
        .nth(1)
        .expect("snapshot_dmesg");
    let body = &f[..f.find("\n        Tool {").unwrap_or(f.len())];
    assert!(
        body.contains("opts.max_output_bytes = snapshot_cap as usize"),
        "the capture must be allowed to reach api.max_snapshot_bytes, where the \
         honest IngestTooLarge refusal already lives -- otherwise it stops at the \
         64 KB shell default with no error at all"
    );
    // And the cause must be MEASURED, never assumed.
    assert!(
        body.contains("txn.output_capped"),
        "which limit was hit is something the runner reports; guessing it is how \
         a 23s capture got told it had run out of a 300s budget"
    );
    for arm in ["output_cap", "timeout", "console_stopped"] {
        assert!(
            body.contains(arm),
            "the three ways a capture ends short are different problems with \
             different fixes; {arm} must be one of them"
        );
    }
}

/// H1, behavioural: the runner's own cap is what decides, and it is honest
/// about having applied it.
#[test]
fn h1_a_capture_over_the_cap_is_reported_as_capped_not_as_a_timeout() {
    use conminer_core::runner::CommandOptions;
    let cfg = Config::default();
    let mut opts = CommandOptions::new(cfg.runner_for("x"), cfg.line_for("x"));
    assert_eq!(
        opts.max_output_bytes,
        64 * 1024,
        "the shell default is 64 KB -- this is the value snapshot_dmesg must not \
         inherit, and pinning it here is what makes the override meaningful"
    );
    opts.max_output_bytes = cfg.api.max_snapshot_bytes as usize;
    assert!(
        opts.max_output_bytes >= 4 * 1024 * 1024,
        "a ring-buffer capture needs megabytes, not kilobytes"
    );
}

/// H3: when the chain cannot be merged, say where the rest of it is.
///
/// The merge stops at readings too OLD to be this boot's lag (§L4 replaced the
/// one-epoch depth rule, which declined chains the tool could see). Observed on
/// the IQ10 after a mode-clear plus reset: the banners were an epoch chain away,
/// the merge correctly declined, and the chain split across two calls with
/// nothing saying so.
#[test]
fn h3_a_chain_that_could_not_be_merged_names_the_epoch_holding_the_rest() {
    let rig = Rig::new();
    // Epoch 1: the firmware banners.
    let device = rig.ingest_text(
        "NOTICE:  BL31: v2.10.0(release):NORDFP-260705-162552\n\
         UEFI Ver : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n",
    );
    // Epoch 2: an unrelated epoch in between (the mode-clear).
    rig.ingest_into("[    1.0] nothing identifying here\n", Some(&device));
    // ...and then time passes. Ten minutes later those banners describe a boot
    // that ended, not the lag of the one starting now.
    rig.clock.advance_ms(600_000);
    // Epoch 3: the kernel, well outside its own firmware's window.
    rig.ingest_into(
        "[    0.000000] Machine model: Qualcomm IQ-10 EVK\n",
        Some(&device),
    );

    let p = rig.call("provenance", json!({"device": device}));
    let running = &p["running"];
    assert!(
        running.get("uefi").is_none(),
        "the merge must decline a reading this old; borrowing it would attribute a \
         finished boot's firmware to this one: {running}"
    );
    let hint = &p["chain_continues_in"];
    assert!(
        hint.is_object(),
        "...but a pointer is safe where a merge is not: {p}"
    );
    let comps: Vec<&str> = hint["components"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert!(
        comps.contains(&"uefi") || comps.contains(&"bl31"),
        "and it must name what is over there: {hint}"
    );
    assert!(
        hint["epoch"].as_i64().is_some(),
        "with the epoch to ask: {hint}"
    );
}

/// H2: this rig's retention policy is armed, and arming it deletes nothing.
///
/// The policy was visible in `stats` but all zeros, so the only answer it could
/// give was "nothing will ever reclaim these bytes" -- while one board's store
/// sat at 140 MB.
///
/// It is armed in the DEPLOYMENT, not in the compiled defaults and not in
/// conminer.toml. Those stay off deliberately: a lab host that has been
/// capturing for a month must not start discarding bytes as a side effect of an
/// upgrade, and conminer.toml is contractually the documented defaults.
#[test]
fn h2_the_rig_deployment_arms_a_retention_policy() {
    // The compiled default stays OFF -- upgrading must never start deleting.
    let d = Config::default();
    assert_eq!(d.retention.raw_keep_days, 0);
    assert_eq!(d.retention.raw_keep_bytes, 0);
    assert!(d.retention.protect_baselines);

    // ...and the shipped file still documents exactly those defaults.
    let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../conminer.toml"))
        .expect("conminer.toml");
    let shipped = Config::from_toml_str(&toml).expect("the shipped config must parse");
    assert_eq!(shipped.retention, d.retention);

    // The rig arms it by environment, which is what the deployment sets.
    let mut cfg = Config::default();
    cfg.apply_env(&[
        (
            "CONMINER_RETENTION_RAW_KEEP_DAYS".to_string(),
            "30".to_string(),
        ),
        (
            "CONMINER_RETENTION_RAW_KEEP_BYTES".to_string(),
            "536870912".to_string(),
        ),
    ]);
    assert_eq!(
        cfg.retention.raw_keep_days, 30,
        "the env override must land"
    );
    assert_eq!(cfg.retention.raw_keep_bytes, 536_870_912);

    // And the deployment really does set them.
    let compose = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker-compose.yaml"
    ))
    .expect("docker-compose.yaml");
    // IN THE SERVICE THAT READS IT. Asserting it appears anywhere in the file
    // is what let the first attempt through: it was set on the `x-common`
    // anchor, and a service declaring its own `environment:` REPLACES the
    // anchor's outright -- YAML merge keys do not deep-merge a mapping. So the
    // policy reached only discoveryd, which never reads it, and `stats` still
    // said `armed: false` after a clean deploy.
    let mcpd = compose.split("\n  mcpd:").nth(1).expect("an mcpd service");
    let mcpd = &mcpd[..mcpd.find("\n  dashd:").unwrap_or(mcpd.len())];
    for key in [
        "CONMINER_RETENTION_RAW_KEEP_DAYS",
        "CONMINER_RETENTION_RAW_KEEP_BYTES",
    ] {
        assert!(
            mcpd.contains(key),
            "mcpd answers `stats` and runs `prune`, so {key} has to be in ITS \
             environment -- anywhere else and the policy is decoration"
        );
    }

    // Arming it is not a licence to delete: pruning happens only when prune()
    // is called, and nothing in the daemons calls it.
    for f in ["src/minerd.rs", "src/main.rs", "src/dash.rs", "src/app.rs"] {
        let path = format!("{}/{f}", env!("CARGO_MANIFEST_DIR"));
        if let Ok(src) = std::fs::read_to_string(&path) {
            assert!(
                !src.contains("prune_before(") && !src.contains("enforce_size_cap("),
                "{f} prunes on its own; retention must stay a deliberate call"
            );
        }
    }
}

// =========================================================== K-series ======

/// K5a: an epoch that has entered a stage and is still talking is BOOTING.
///
/// The rule is exact: >=1 stage entered, no terminal stage, and the console
/// spoke within 15s. The `why` must name the stage, because "booting" alone
/// does not tell an operator whether the board is at bl2 or at the kernel.
#[test]
fn k5a_an_in_progress_boot_is_booting_and_names_its_stage() {
    let rig = Rig::new();
    // A boot that reaches the kernel and stops there: a stage, no userspace.
    let device = rig.ingest_text(
        "NOTICE:  BL2: v2.10.0\n\
         [    0.000000] Linux version 6.12.0-rc1 (b@h)\n\
         [    0.512000] smp: Bringing up secondary CPUs\n",
    );
    let boots = rig.call("list_boots", json!({"device": device, "limit": 3}));
    let b = boots["boots"][0]["id"].as_i64().unwrap();
    let rep = rig.call("boot_report", json!({"device": device, "boot": b}));

    assert_eq!(rep["outcome"], "booting", "{}", rep["why"]);
    let why = rep["why"].as_str().unwrap();
    assert!(
        why.contains("kernel") || why.contains("bl2"),
        "the stage must be named: {why:?}"
    );
    assert!(
        why.contains("boot in progress"),
        "and it must say what that means: {why:?}"
    );
}

/// K5a: the history heuristic must never reach `outcome` or `why`.
///
/// `unstable` counted DISTINCT fingerprints across recent epochs -- a property
/// of the bench's past, not of this boot -- and it was the fallback that
/// labelled a healthy in-progress boot a failure because earlier boots differed.
#[test]
fn k5a_distinct_fingerprints_never_appear_as_an_outcome() {
    let rig = Rig::new();
    // Several epochs with deliberately different content: the exact shape that
    // used to produce `outcome: unstable`.
    let device = rig.ingest_text("[    1.0] alpha one\n");
    for n in 0..4 {
        rig.ingest_into(
            &format!("[    1.0] variant {n} of the boot\n"),
            Some(&device),
        );
    }
    let boots = rig.call("list_boots", json!({"device": device, "limit": 8}));
    for b in boots["boots"].as_array().unwrap() {
        let id = b["id"].as_i64().unwrap();
        let rep = rig.call("boot_report", json!({"device": device, "boot": id}));
        assert_ne!(
            rep["outcome"], "unstable",
            "epoch {id} reported a history verdict as its own outcome: {}",
            rep["why"]
        );
        let why = rep["why"].as_str().unwrap_or_default();
        assert!(
            !why.contains("distinct fingerprints"),
            "epoch {id}: {why:?}"
        );
    }
    // ...and it is still REPORTED, in the field that measures it.
    let latest = boots["boots"][0]["id"].as_i64().unwrap();
    let rep = rig.call("boot_report", json!({"device": device, "boot": latest}));
    assert!(
        rep["distinct_fingerprints_recent"].is_i64(),
        "the measurement stays, it just stops pretending to be a verdict: {rep}"
    );
}

/// K5b: history gets its versions back without any template churn.
///
/// The whole point of the standalone: `rebuild_templates` also refreshes these
/// but renumbers every template doing it, and an operator asking "what was
/// running last month" should not have to pay that.
#[test]
fn k5b_backfill_fills_versions_and_renumbers_nothing() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "UEFI Ver    : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n\
         [    0.000000] Linux version 7.1.0-rc4-g107224669d45-dirty (b@h) (gcc 14) #3 SMP\n",
    );
    // Take a full census of templates BEFORE, ids and text together.
    let before: Vec<(i64, String)> = rig
        .call("list_templates", json!({"device": device, "limit": 500}))["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_i64().unwrap(),
                t["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(!before.is_empty());

    // The state of a store filled before the extractors existed.
    {
        let reg = rig.registry();
        let row = reg.device_by_canonical(&device).unwrap().unwrap();
        let path = reg.device_db_path(&row);
        let store = DeviceStore::open(&path, &device, true).unwrap();
        store.forget_versions_for_test().unwrap();
    }
    let empty = rig.call("provenance", json!({"device": device}));
    assert!(
        empty["running"]
            .as_object()
            .map(|o| o.is_empty())
            .unwrap_or(true),
        "the fixture must start with nothing extracted: {}",
        empty["running"]
    );

    rig.call("acquire", json!({"device": device}));
    let r = rig.call("backfill_versions", json!({"device": device}));
    assert!(
        r["versions_written"].as_i64().unwrap_or(0) >= 2,
        "both banners must be recovered: {r}"
    );
    assert!(r["epochs_filled"].as_i64().unwrap_or(0) >= 1, "{r}");

    let p = rig.call("provenance", json!({"device": device}));
    let running = &p["running"];
    assert!(running.get("uefi").is_some(), "{running}");
    assert!(running.get("kernel").is_some(), "{running}");

    // NOT ONE TEMPLATE MOVED. This is the difference from a rebuild, and the
    // reason the tool exists: template ids are cited in verdicts, metrics and
    // an agent's notes, and renumbering them to recover a version string would
    // cost more than it returns.
    let after: Vec<(i64, String)> = rig
        .call("list_templates", json!({"device": device, "limit": 500}))["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_i64().unwrap(),
                t["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(before, after, "backfill must not touch templates at all");
}

/// K1: cleanup is unconditional, and that is the whole safety property.
///
/// A harness that leaves a board powered because it failed early is worse than
/// no harness: the next person finds a hot board and no explanation. So the
/// guard must run on EVERY exit path -- pass, fail, panic -- and it must
/// VERIFY the board is off rather than assume the hook worked.
#[test]
fn k1_selftest_cleanup_runs_on_every_exit_path() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/selftest.rs"
    ))
    .expect("selftest.rs");

    // The suites run inside a catch_unwind, so a panicking check still reaches
    // cleanup instead of taking the process down with the board still on.
    let body = src
        .find("catch_unwind")
        .expect("the suites must be wrapped");
    let cleanup = src
        .find("let cleanup = cleanup(&mut run, opts.keep_on);")
        .expect("cleanup must be called");
    assert!(body < cleanup, "cleanup must come after the guarded body");
    assert!(
        src[cleanup..].contains("release(ctx, c);"),
        "and every lease must be handed back after it"
    );

    // Cleanup asks the hardware; it never trusts the hook's exit code.
    let f = src.split("fn cleanup(").nth(1).expect("fn cleanup");
    assert!(
        f.contains("diagnose"),
        "board_off_verified must come from a probe, not from the power call"
    );
    assert!(
        f.contains("usb_zombies"),
        "silence alone is not evidence of off: the bus is checked too. (This \
         gate itself once asserted `live_gadgets`, a field diagnose does not \
         return -- it would have locked in a check that could never fail.)"
    );
    assert!(
        f.contains("keep_on"),
        "the one way to leave a board on must be explicit and reported"
    );
}

/// K1: the engine composes the public tools and owns no capture path.
///
/// If a check cannot be expressed through the tools an agent has, that is a
/// finding about the tool surface -- so the harness must not have a private
/// back door that hides it.
#[test]
fn k1_selftest_composes_tools_and_owns_no_capture_logic() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/selftest.rs"
    ))
    .expect("selftest.rs");
    assert!(
        src.contains("crate::tools::find(tool)"),
        "checks must dispatch through the public tool registry"
    );
    for forbidden in ["Runner::new", "BrokeredTransport", "Pipeline::new"] {
        assert!(
            !src.contains(forbidden),
            "the harness must not open its own {forbidden}: that is capture logic, \
             and a second implementation of it is how the harness starts passing \
             while the product is broken"
        );
    }
}

/// K1: the rig's expectations parse, and describe the boards that exist.
#[test]
fn k1_the_shipped_expectations_describe_this_rig() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../selftest.toml"))
        .expect("selftest.toml");
    let parsed: toml::Value = toml::from_str(&text).expect("selftest.toml must parse");
    let targets = parsed["target"].as_table().expect("a [target] table");
    for t in ["3.1", "3.2", "3.3"] {
        let e = &targets[t];
        assert!(
            !e["stages"].as_array().unwrap().is_empty(),
            "{t} must say what stages its boot passes through"
        );
        assert!(
            !e["provenance_components"].as_array().unwrap().is_empty(),
            "{t} must say what its firmware chain looks like"
        );
    }
    // The ADP runs a production UEFI: expecting TF-A stages there would make the
    // selftest fail on a healthy board, which is worse than not checking.
    let adp: Vec<&str> = targets["3.3"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        !adp.contains(&"bl31"),
        "the ADP has no TF-A stages to find: {adp:?}"
    );
}

/// K1: a check must not read a field the tool does not return.
///
/// The honesty suite originally asserted `usb.live_gadgets == 0`, and `diagnose`
/// returns no such field: the value was always 0 and the check passed no matter
/// what the bus held. A vacuous check is worse than a missing one, because it
/// reports safety it never established -- and this one guarded the "board is
/// really off" claim.
#[test]
fn k1_selftest_reads_only_fields_diagnose_actually_returns() {
    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let self_src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/selftest.rs"
    ))
    .expect("selftest.rs");

    // Every key the harness reads off a diagnose response must be one diagnose
    // emits.
    for key in ["edl", "usb_zombies", "power", "probe"] {
        assert!(
            tools.contains(&format!("\"{key}\":")),
            "diagnose must actually return {key}"
        );
    }
    // Code only: the comment above the fix quotes the old field name to explain
    // why it was wrong, and a gate that reads prose fails on the documentation
    // of the very bug it guards.
    let code: String = self_src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("live_gadgets"),
        "`live_gadgets` is not a diagnose field; reading it made the off-board \
         honesty check pass unconditionally"
    );
    assert!(
        code.contains("[\"usb_zombies\"]"),
        "the harness must read the field that exists"
    );
}

/// K3: one call, several stores, and every hit says where it came from.
#[test]
fn k3_a_cross_device_search_attributes_every_hit() {
    let rig = Rig::new();
    let a = rig.ingest_text("[    1.0] GMU firmware initialization timed out\n[    2.0] idle\n");
    let b = rig.ingest_text("[    1.0] GMU firmware initialization timed out\n[    3.0] other\n");
    assert_ne!(a, b);

    let r = rig.call(
        "search",
        json!({"devices": "all", "query": "GMU firmware initialization timed out",
               "include_derived": true}),
    );
    let hits = r["hits"].as_array().expect("hits");
    assert!(hits.len() >= 2, "both stores must answer: {r}");
    for h in hits {
        assert!(
            h["device"].is_string(),
            "a cross-device hit that does not say which device is a string in a list: {h}"
        );
    }
    let devices: std::collections::BTreeSet<&str> =
        hits.iter().filter_map(|h| h["device"].as_str()).collect();
    assert!(devices.len() >= 2, "hits came from only {devices:?}");

    // ...and the per-device summary is there to drill from.
    let by = r["by_device"].as_array().expect("by_device");
    assert!(by.len() >= 2, "{by:?}");
    for row in by {
        assert!(
            row["device"].is_string() && row["hits"].is_number(),
            "{row}"
        );
    }
}

/// K3: a device-scoped id cannot mean anything across devices, and saying so
/// beats searching the wrong thing.
#[test]
fn k3_session_and_boot_are_refused_with_a_device_set() {
    let rig = Rig::new();
    rig.ingest_text("[    1.0] hello\n");
    for k in ["session", "boot"] {
        let e = rig.err(
            "search",
            json!({"devices": "all", "query": "hello", k: 1, "include_derived": true}),
        );
        assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
        assert!(
            e["message"].as_str().unwrap_or_default().contains(k),
            "the error must name the offending argument: {e}"
        );
    }
    // And `device` + `devices` together is a contradiction, not a merge.
    let e = rig.err(
        "search",
        json!({"devices": "all", "device": "nope", "query": "hello"}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
}

/// K3: derived stores are not boards, and a bench-wide question means boards.
#[test]
fn k3_derived_sub_devices_stay_out_unless_asked_for() {
    let rig = Rig::new();
    let base = rig.dir.join("k3console.log");
    std::fs::write(&base, "[    1.0] shared marker line\n").unwrap();
    let sub = rig.dir.join("k3console.log#dmesg");
    std::fs::write(&sub, "[    1.0] shared marker line\n").unwrap();
    rig.call("ingest_file", json!({"path": base.display().to_string()}));
    rig.call("ingest_file", json!({"path": sub.display().to_string()}));

    // `file:` devices are excluded by the same rule, so the default search over
    // "all" finds nothing here -- which is itself the assertion.
    let plain = rig.raw(
        "search",
        json!({"devices": "all", "query": "shared marker line"}),
    );
    let hit_devices: Vec<String> = plain["structuredContent"]["hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| h["device"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !hit_devices.iter().any(|d| d.contains('#')),
        "a derived sub-device answered a bench-wide search: {hit_devices:?}"
    );

    let with = rig.call(
        "search",
        json!({"devices": "all", "query": "shared marker line", "include_derived": true}),
    );
    let devs: Vec<&str> = with["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["device"].as_str())
        .collect();
    assert!(
        devs.iter().any(|d| d.contains('#')),
        "include_derived must actually include them: {devs:?}"
    );
}

/// K3: one chatty board must not starve the rest.
#[test]
fn k3_results_are_interleaved_so_a_loud_board_cannot_crowd_out_a_quiet_one() {
    let rig = Rig::new();
    let mut loud = String::new();
    for i in 0..40 {
        loud.push_str(&format!("[    {i}.0] config error on the noisy board\n"));
    }
    let a = rig.ingest_text(&loud);
    let b = rig.ingest_text("[    1.0] config error on the quiet board\n");
    assert_ne!(a, b);

    let r = rig.call(
        "search",
        json!({"devices": "all", "query": "config error", "max_results": 6,
               "include_derived": true}),
    );
    let devs: std::collections::BTreeSet<&str> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["device"].as_str())
        .collect();
    assert_eq!(
        devs.len(),
        2,
        "the quiet board's single occurrence is usually the interesting one, and \
         a first-come result set buries it: {devs:?}"
    );
}

/// K4: the allowlist is mandatory, and it refuses at CREATION.
///
/// A watch payload carries console content. On a LAN-open endpoint an arbitrary
/// URL is an exfiltration primitive anybody who can reach the API could arm --
/// so this is a hard refusal naming the config key, not a warning.
#[test]
fn k4_a_notify_url_outside_the_allowlist_is_refused_by_name() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] hello\n");
    rig.call("acquire", json!({"device": device}));

    let e = rig.err(
        "create_watch",
        json!({"device": device, "name": "exfil", "until": {"pattern": "hello"},
               "notify": {"url": "http://evil.example.com/collect"}}),
    );
    assert_eq!(e["code"], "INVALID_ARGUMENT", "{e}");
    assert!(
        e["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("[notify] allow"),
        "the refusal must name the key that governs it: {e}"
    );
    assert!(
        e["detail"]["allow"].is_array(),
        "and show what IS allowed: {e}"
    );

    // The default allowlist covers loopback, which is where a sink lives.
    let ok = rig.call(
        "create_watch",
        json!({"device": device, "name": "local", "until": {"pattern": "hello"},
               "notify": {"url": "http://127.0.0.1:9111/hook", "secret": "s3cret",
                          "min_interval_s": 60}}),
    );
    assert!(ok["watch"]["watch_id"].is_i64(), "{ok}");

    // The secret is never echoed back: a response that returns it turns every
    // log of a tool call into a credential leak.
    let list = rig.call("list_watches", json!({"device": device}));
    let w = list["watches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["watch"])
        .find(|w| w["name"] == "local")
        .expect("the watch");
    let blob = serde_json::to_string(w).unwrap();
    assert!(
        !blob.contains("s3cret"),
        "the secret leaked into a response: {blob}"
    );
    assert_eq!(
        w["notify"]["signed"], true,
        "but it must say one is set: {w}"
    );
    assert!(w["delivery"]["delivered"].is_number(), "{w}");
}

/// K4: a delivery that is not acknowledged must not consume the firing.
///
/// `poll_watch` stays the source of truth. A receiver that was down must not
/// cost the operator the evidence -- the whole point of a durable watch is that
/// the record outlives the listener.
#[test]
fn k4_an_unacknowledged_delivery_never_consumes_a_firing() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] usb 1-1: config error\n");
    rig.call("acquire", json!({"device": device}));
    rig.call(
        "create_watch",
        json!({"device": device, "name": "flap", "until": {"pattern": "config error"},
               "from": "start",
               // A port with nothing listening: every attempt will fail.
               "notify": {"url": "http://127.0.0.1:1/hook", "min_interval_s": 1}}),
    );

    // Whatever push does or does not manage, polling still returns the firing.
    let p = rig.call("poll_watch", json!({"device": device, "name": "flap"}));
    assert!(
        p["returned"].as_i64().unwrap_or(0) >= 1,
        "the firing must be readable regardless of delivery: {p}"
    );
}

/// K4: the signature is over the body a receiver actually gets.
#[test]
fn k4_the_signature_verifies_against_the_posted_bytes() {
    let firings = vec![json!({"id": 1, "at": 10, "matched": "flap"})];
    let body = conminer_mcp::push::body("flap", "/dev/ttyUSB0", &firings, 60);
    let bytes = serde_json::to_vec(&body).unwrap();
    let sig = conminer_mcp::push::signature("s3cret", &bytes);

    assert!(sig.starts_with("sha256="), "{sig}");
    // A receiver verifies the BYTES, not a re-serialisation of the fields:
    // any other JSON writer would order or space them differently.
    assert_eq!(sig, conminer_mcp::push::signature("s3cret", &bytes));
    assert_ne!(sig, conminer_mcp::push::signature("other", &bytes));
    let mut tampered = bytes.clone();
    tampered.push(b' ');
    assert_ne!(
        sig,
        conminer_mcp::push::signature("s3cret", &tampered),
        "a changed body must change the signature"
    );
}

/// K4: coalescing is what keeps a flapping board from DoSing the receiver.
#[test]
fn k4_a_windows_worth_of_firings_is_one_post_that_says_how_many() {
    let many: Vec<serde_json::Value> = (0..40).map(|i| json!({"id": i, "at": i * 2000})).collect();
    let b = conminer_mcp::push::body("flap", "/dev/ttyUSB0", &many, 60);
    assert_eq!(b["firings"].as_array().unwrap().len(), 40);
    let note = b["pending_note"].as_str().unwrap();
    assert!(
        note.contains("coalesced 40 firings"),
        "a receiver seeing one message must know it stands for forty: {note:?}"
    );
    // The retry schedule is the documented one, not an ad-hoc loop.
    assert_eq!(conminer_mcp::push::BACKOFF_S, [5, 25, 125]);
}

/// K4: the allowlist keeps console content off the PUBLIC internet, not off the
/// bench's own network.
///
/// Loopback-only was the first cut and it was wrong in practice: a sink almost
/// never runs inside mcpd's own network namespace, so the feature was
/// untestable before it was safe. The line that matters is routable vs private.
#[test]
fn k4_the_allowlist_admits_the_bench_and_still_refuses_the_internet() {
    let n = Config::default().notify;
    for ok in [
        "http://127.0.0.1:9111/hook",
        "http://192.168.10.10:9111/hook",
        "http://10.1.2.3:8080/x",
        "http://172.17.0.1:9111/hook",
    ] {
        assert!(n.url_allowed(ok), "a bench address must be allowed: {ok}");
    }
    for bad in [
        "http://evil.example.com/collect",
        "http://8.8.8.8/x",
        "https://example.com/x",
        "http://172.99.0.1/x",
    ] {
        assert!(
            !n.url_allowed(bad),
            "a routable address must still be a deliberate decision: {bad}"
        );
    }
}

/// K4: the delivery sweep must not open a store just to learn there is nothing
/// to deliver.
///
/// Opening a store is not free -- `open_sqlite` sets `journal_mode=WAL`, which
/// takes a brief WRITE lock. The first sweep probed every device every 5s, and
/// on a bench being actuated at the same time a `power on` came back "database
/// is locked": the selftest failed a board that was working perfectly, because
/// the notification machinery was drumming on the write lock behind it.
#[test]
fn k4_the_sweep_does_not_touch_stores_it_has_no_reason_to() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/push.rs"
    ))
    .expect("push.rs");
    let f = src.split("pub async fn sweep(").nth(1).expect("fn sweep");

    assert!(
        f.contains("device_may_have_armed_watch(ctx, &d)"),
        "the sweep must skip devices with no armed watch BEFORE opening their store"
    );
    let guard = f.find("device_may_have_armed_watch").expect("the guard");
    let open = f.find("watches_to_deliver").expect("the store read");
    assert!(
        guard < open,
        "the guard must come first, or it buys nothing"
    );

    // And the answer is cached, or the guard just moves the same drumming.
    assert!(
        src.contains("ARMED_TTL"),
        "the armed-device set must be cached rather than rebuilt every sweep"
    );
    // ...while a newly armed watch still starts delivering promptly.
    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(
        tools.contains("crate::push::invalidate_armed_cache();"),
        "creating a pushing watch must invalidate the cache, or it waits out the TTL"
    );
}

/// The two controller kinds enter a boot mode differently, and assuming one
/// sequence broke the other.
///
/// A Bantam LATCHES a strap: the board enters on its next boot, so a reset is
/// still owed and the strap must be cleared afterwards. A Bughopper drives
/// FORCED_USB_BOOT_N and pulses reset ITSELF while holding the strap across the
/// sampling window -- setting the mode IS the entry, and a reset afterwards
/// boots the board straight back out.
///
/// Measured on the ADP: `boot_mode EDL` followed by the reset a Bantam needs
/// left no QDL gadget, and read as "EDL is broken on this board" when the
/// second step had simply undone the first.
#[test]
fn a_boot_mode_says_whether_the_board_is_in_it_or_merely_armed_for_it() {
    let cfg = Config::default();
    let bughopper = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bughopper")
        .expect("the bughopper profile");
    let bantam = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bantam")
        .expect("the bantam profile");
    assert!(
        bughopper.mode_enters_immediately,
        "the bughopper sequences its own reset while holding the strap"
    );
    assert!(
        !bantam.mode_enters_immediately,
        "the bantam latches a strap and needs the board rebooted into it"
    );

    // The distinction must reach the CALLER, not just the harness: an agent
    // cannot guess which kind of controller it is talking to.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(
        src.contains("\"entered\": immediate && !clearing"),
        "boot_mode must report whether the board is IN the mode"
    );
    assert!(
        src.contains("Do not reset -- that boots it back out."),
        "and must say what NOT to do next, which is the part that cost a round"
    );

    // ...and the selftest must follow that report rather than assume.
    let st = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/selftest.rs"
    ))
    .expect("selftest.rs");
    assert!(
        st.contains("let entered_already = set"),
        "the EDL check must read `entered` instead of always resetting"
    );
}

// =========================================================== L-series ======

/// L3: a repeat delta that is neither novel nor severe is two integers.
///
/// The follow payload went over the 20 KB budget on BOTH boards (21.5 KB for
/// 45 KB of console), and the deltas were the bulk of it: full template text
/// re-sent for messages the caller had already seen, at every poll. The budget
/// is not the thing that gives -- the payload is.
#[test]
fn l3_repeat_deltas_drop_the_text_they_were_re_sending() {
    let rig = Rig::new();
    let mut log = String::new();
    for i in 0..200 {
        log.push_str(&format!("[    1.{i:03}] mmc0: card is busy, retrying\n"));
        log.push_str(&format!("[    2.{i:03}] usb 1-3: device descriptor read\n"));
        log.push_str(&format!(
            "[    3.{i:03}] mmc0: error -110 whilst initialising\n"
        ));
    }
    // First pass teaches the templates; the SECOND is where a repeat is a
    // repeat -- which is the only place the payload shape matters.
    let device = rig.ingest_text(&log);
    let start = rig.call("follow", json!({"device": device, "max_lines": 1}))["follow"]["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    // The same messages again: from `start`, every one of them is a REPEAT, and
    // repeats are what the payload was spending its budget on.
    rig.ingest_into(&log, Some(&device));
    let inc = rig.call(
        "follow",
        json!({"device": device, "cursor": start, "max_lines": 5}),
    );

    let deltas = inc["follow"]["template_deltas"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !deltas.is_empty(),
        "this corpus must produce repeat deltas or the gate proves nothing: {inc}"
    );
    for d in &deltas {
        let severe = d["severity"]
            .as_str()
            .is_some_and(|s| matches!(s, "emerg" | "alert" | "crit" | "err" | "warn"));
        assert!(
            d["count"].is_i64() && d["template_id"].is_i64(),
            "a delta is at minimum an id and a count: {d}"
        );
        if !severe {
            assert!(
                d["text"].is_null(),
                "an ordinary repeat must not re-send its text: {d}"
            );
        }
    }
    // ...and severity is still readable where it matters: the warn-or-worse
    // rows keep their text so an agent can act without a second call.
    if let Some(sev) = deltas
        .iter()
        .find(|d| d["severity"].as_str() == Some("err"))
    {
        assert!(
            sev["text"].is_string(),
            "a severe repeat keeps its text: {sev}"
        );
    }
}

/// L3: the budget did not move.
#[test]
fn l3_the_follow_budget_is_still_twenty_kilobytes() {
    let src = std::fs::read_to_string(format!(
        "{}/../conminer-core/src/follow.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert!(
        src.contains("MAX_DELTAS: usize = 200"),
        "the delta cap is the knob that moved, and it is 200 with novel+severe first"
    );
    let selftest = std::fs::read_to_string(format!(
        "{}/../conminer-mcp/src/selftest.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert!(
        selftest.contains("mined >= 20_000"),
        "relaxing the budget because the harness went red is the failure this project \
         keeps catching in itself: the check stays at 20 KB"
    );
}

/// L4: one boot's banner chain, split across more than one epoch boundary.
///
/// Reproduced from the ADP under selftest churn: a reset and a power press 178
/// ms apart put uefi two epochs behind kernel, the one-step look-back declined,
/// and `chain_continues_in` pointed straight at the epoch the merge refused to
/// read. One rule, two answers.
#[test]
fn l4_provenance_merges_a_chain_split_across_two_boundaries() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "NOTICE:  BL31: v2.10.0(release):NORDFP-260705-162552\n\
         UEFI Ver : 6.0.260212.BOOT.MXF.1.0.c1-00460-KODIAKLA-1\n",
    );
    // The churn epoch: a second actuation landed 178 ms into the same boot.
    rig.ingest_into("[    1.0] nothing identifying here\n", Some(&device));
    rig.ingest_into(
        "[    0.000000] Machine model: Qualcomm IQ-10 EVK\n",
        Some(&device),
    );

    let boots = rig.call("list_boots", json!({"device": device, "limit": 5}));
    let latest = boots["boots"][0]["id"].as_i64().unwrap();
    let p = rig.call("provenance", json!({"device": device, "boot": latest}));

    let running = &p["running_versions"];
    assert!(
        running["machine"].is_string(),
        "this epoch's own reading must be there: {p}"
    );
    assert!(
        running["uefi"].is_string(),
        "the uefi banner is two epochs back with nothing contradicting it, and the tool \
         demonstrably has it -- declining it while pointing at it is the L4 defect: {p}"
    );
    let from = &p["running"]["uefi"];
    assert!(
        from["from_epoch"].is_i64() && from["epochs_back"].is_i64(),
        "a borrowed component must name where it came from and how far: {from}"
    );
}

/// L5: a boot that grew out of a transient garbage span is not `garbage`.
///
/// Real UART noise at a strap or power transition is normal on these boards --
/// the outcome text says so itself -- and one quarantined span at the reset
/// buried five clean stages behind it.
#[test]
fn l5_transient_garbage_that_resolves_is_not_the_outcome() {
    let rig = Rig::new();
    // A quarantined span at the strap transition: control bytes, long enough to
    // trip the detector's window, and NOT a reset -- the whole point is that it
    // lands in the same epoch as the boot that follows it.
    let mut log: String = std::iter::repeat_n('\u{1}', 600).collect();
    log.push('\n');
    // ...then enough clean console for the detector to stand down again, which
    // is what "transient" means: the span ENDS.
    for i in 0..40 {
        log.push_str(&format!(
            "[    0.{i:03}] clean console output after the transition\n"
        ));
    }
    // ...and the boot marches on through its stages to a prompt.
    log.push_str(
        "NOTICE:  BL2: v2.10.0\n\
         [    0.900000] Linux version 6.12.0-rc1 (b@h)\n\
         [    2.000000] Run /sbin/init as init process\n\
         root@board:~# \n",
    );
    let device = rig.ingest_text(&log);

    let boots = rig.call("list_boots", json!({"device": device, "limit": 3}));
    let b = boots["boots"][0]["id"].as_i64().unwrap();
    let rep = rig.call("boot_report", json!({"device": device, "boot": b}));

    assert_ne!(
        rep["outcome"], "garbage",
        "the boot recovered and marched through its stages: {}",
        rep["why"]
    );
    // ...and the gate is only meaningful if the span is really there: the
    // evidence stays in the response, it just stops being the headline.
    assert!(
        rep["garbage_spans"].as_i64().unwrap_or(0) > 0,
        "no quarantined span in this epoch means this gate proves nothing: {rep}"
    );
}

/// L6: a zombie belongs to a port path, not to the bench.
#[test]
fn l6_usb_attribution_is_by_port_path() {
    use conminer_core::usb::{on_ports, Liveness, UsbDevice};
    let mine = UsbDevice {
        vendor_id: 0x18d1,
        product_id: 0xd002,
        bus: 3,
        address: 69,
        port_path: Some("3-3.4".into()),
        liveness: Liveness::Dead,
    };
    let ports = vec!["3-3".to_string()];
    assert!(
        on_ports(&mine, &ports),
        "a hub path covers what is under it"
    );
    // The sibling trap: `3-3` must not match `3-30`.
    let other = UsbDevice {
        port_path: Some("3-30.1".into()),
        ..mine.clone()
    };
    assert!(!on_ports(&other, &ports), "prefix matching is by segment");
    let unknown = UsbDevice {
        port_path: None,
        ..mine.clone()
    };
    assert!(
        !on_ports(&unknown, &ports),
        "no path is not attribution: it must never match"
    );
}

/// L6: without a port map, `diagnose` says the count is unattributed rather
/// than blaming whoever asked.
#[test]
fn l6_unattributed_zombies_say_so() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] hello\n");
    let d = rig.call("diagnose", json!({"device": device}));
    let scope = &d["usb_zombies_scope"];
    assert!(
        scope["attribution"]
            .as_str()
            .is_some_and(|s| s.contains("unknown")),
        "with no usb_ports configured the count is bench-wide and must say so: {d}"
    );
    assert!(
        d["usb_zombies_elsewhere"].is_i64(),
        "and what fell outside this board's ports is reported, not dropped: {d}"
    );
}

/// L6: the config path works, and it is what turns the count into a claim.
#[test]
fn l6_configured_ports_scope_the_count() {
    let mut cfg = Config::default();
    let over = conminer_core::config::DeviceOverride {
        usb_ports: vec!["2-3.1".into(), "3-3.4".into()],
        ..Default::default()
    };
    cfg.devices.insert("board-under-test".into(), over);
    assert_eq!(
        cfg.usb_ports_for(&["board-under-test"]),
        vec!["2-3.1".to_string(), "3-3.4".to_string()]
    );
    assert!(
        cfg.usb_ports_for(&["some-other-board"]).is_empty(),
        "unset means unattributed, never 'everything'"
    );
}

/// L2: every store transaction takes its write lock up front.
///
/// `busy_timeout` does not cover a DEFERRED transaction upgrading read->write:
/// SQLite fails that immediately, which is how a raw "database is locked"
/// became the RESULT of three selftest checks.
#[test]
fn l2_store_transactions_are_immediate() {
    for f in ["store/device.rs", "store/registry.rs", "store/mod.rs"] {
        let path = format!("{}/../conminer-core/src/{f}", env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(&path).unwrap();
        for (n, line) in src.lines().enumerate() {
            assert!(
                !line.contains(".transaction()"),
                "{f}:{} opens a DEFERRED transaction; busy_timeout does not cover its \
                 read->write upgrade",
                n + 1
            );
        }
    }
}

/// L2: concurrent writers wait for each other instead of erroring.
#[test]
fn l2_concurrent_writers_do_not_surface_database_is_locked() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] first line\n");
    let dev = rig.registry().resolve(&device).unwrap();
    let path = rig.dir.join(&dev.db_file);
    let canonical = dev.canonical.clone();

    let mut threads = Vec::new();
    for t in 0..4 {
        let path = path.clone();
        let canonical = canonical.clone();
        threads.push(std::thread::spawn(move || {
            let mut st = conminer_core::store::DeviceStore::open(&path, &canonical, false).unwrap();
            for i in 0..25 {
                // A read followed by a write in one transaction: the exact
                // shape that used to fail instantly instead of waiting.
                let _ = st.line_count().unwrap();
                st.learn_prompt(&format!("p{t}-{i}"), "shell", "learned", None, 0)
                    .map_err(|e| format!("{e:?}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    for h in threads {
        h.join()
            .unwrap()
            .expect("a contended store must wait, not fail");
    }
}

/// L1: the armed-watch cache is invalidated only once the write is committed.
///
/// A sweep that read the store mid-transaction cached "no armed watches" and
/// trusted it for the full TTL, so a real firing sat undelivered for minutes.
#[test]
fn l1_arming_a_watch_invalidates_the_cache_after_the_commit() {
    let src = std::fs::read_to_string(format!(
        "{}/../conminer-mcp/src/tools.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let at = src
        .find("invalidate_armed_cache()")
        .expect("create_watch must invalidate the armed cache");
    let before = &src[..at];
    let opened = before.matches("ctx.with_store(").count();
    let closed = before.matches("})?;").count() + before.matches("},\n").count();
    assert!(
        opened <= closed,
        "invalidate_armed_cache() is called inside a with_store closure: the sweep can \
         then cache 'not armed' from data the transaction has not committed"
    );
    let push = std::fs::read_to_string(format!(
        "{}/../conminer-mcp/src/push.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert!(
        push.contains("ARMED_TTL: Duration = Duration::from_secs(30)"),
        "and the cache's blind window stays short enough that a stale answer costs \
         seconds, not minutes"
    );
}

/// L5: `boot_looping` is a claim about the present tense; an OFF board is not
/// making it.
///
/// The verdict comes from the epoch chain, and a chain keeps its shape long
/// after the power goes away -- so a verifiably dark board reported
/// `boot_looping` from fingerprints minted an hour earlier. The controller was
/// one question away.
#[test]
fn l5_console_state_on_a_board_the_controller_says_is_off_is_no_signal() {
    let mut cfg = Config::default();
    cfg.controllers
        .push(conminer_core::config::ControllerProfile {
            name: "test-ctl".into(),
            match_glob: "*Test_Ctl*".into(),
            controls: "*Test_Board*".into(),
            power_state: Some("/bin/echo off".into()),
            power: None,
            boot_mode: None,
            boot_overrides: None,
            boot_mode_release: None,
            flash: None,
            boot_modes: Vec::new(),
            power_timeout_s: None,
            off_settle_s: 0.0,
            exclude_from_discovery: true,
            mode_enters_immediately: false,
        });
    let rig = Rig::with_config(cfg);
    let console = "/dev/serial/by-id/usb-Test_Board-if00-port0";
    let ids = rig.board("t", &[console], "/dev/serial/by-id/usb-Test_Ctl-if00");
    // `no_signal` REQUIRES attestation, so the console has to be one conminer
    // is actually listening to -- otherwise the answer is `unknown` and this
    // gate would be testing the wrong sentence.
    rig.registry().set_state(ids[0], "listening").unwrap();

    // Three byte-identical epochs: the shape that earns a `boot_looping`.
    for _ in 0..4 {
        rig.ingest_into("[    1.0] boot loops here\n", Some(console));
    }
    let looping = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        looping["console"]["state"], "boot_looping",
        "the corpus must produce the verdict this gate is about: {looping}"
    );

    // Now the board goes quiet for longer than `hung_after_s`, and the
    // controller says it is off.
    rig.clock.advance_ms(120_000);
    let state = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        state["console"]["state"], "no_signal",
        "the controller says the board is off and nothing has arrived since: a history \
         verdict must not outrank that: {state}"
    );
}

/// L3: the response FITS the budget, whatever the corpus does.
///
/// A count cap is a proxy for size and proxies drift: 200 two-integer deltas
/// plus one boot's novel templates measured 20,279 B on the ADP's 129 KB epoch,
/// against a 20,000 B budget. The number that must hold is the byte count.
#[test]
fn l3_a_follow_response_is_trimmed_to_its_budget_and_says_what_it_dropped() {
    let rig = Rig::new();
    // Hundreds of DISTINCT templates: the shape that makes a delta list long.
    let mut log = String::new();
    for i in 0..400 {
        // Distinct LEADING tokens and distinct lengths: Drain keys on both, so
        // this is 400 templates rather than one template with a wildcard. They
        // are ERRORS, which keep their text (§L3) -- so the count cap alone
        // cannot bring this under budget and the byte trim has to.
        let words: Vec<String> = (0..(i % 30) + 3).map(|j| format!("w{j}k{i}")).collect();
        log.push_str(&format!(
            "[    1.{i:03}] err{i}: error -110 whilst initialising {}\n",
            words.join(" ")
        ));
    }
    let device = rig.ingest_text(&log);
    let start = rig.call("follow", json!({"device": device, "max_lines": 1}))["follow"]["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    rig.ingest_into(&log, Some(&device));
    let inc = rig.call(
        "follow",
        json!({"device": device, "cursor": start, "max_lines": 50}),
    );

    let size = serde_json::to_string(&inc).unwrap().len();
    assert!(
        size <= 20_000,
        "the follow response is {size} B, over the 20 KB budget the whole mining story \
         rests on"
    );
    let omitted = inc["follow"]["deltas_omitted"].as_i64().unwrap_or(0);
    assert!(
        omitted > 0,
        "this corpus must overflow the budget or the gate proves nothing: {} deltas, \
         {size} B",
        inc["follow"]["template_deltas"]
            .as_array()
            .map_or(0, Vec::len)
    );
}

/// L1: a dead receiver must not hold up a live one.
///
/// The retry backoff used to be three sleeps inside the sweep, so one
/// unreachable endpoint parked the notifier for up to 155 s while every other
/// board's watch waited behind it. Measured on the rig: a live watch's first
/// post landed 32 s after its firing because a dead endpoint was still being
/// retried ahead of it.
#[test]
fn l1_watch_delivery_backoff_is_scheduled_not_slept() {
    let src = std::fs::read_to_string(format!(
        "{}/../conminer-mcp/src/push.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let sweep = src
        .split("pub async fn sweep")
        .nth(1)
        .expect("push.rs must have a sweep");
    assert!(
        !sweep.contains("sleep("),
        "the sweep sleeps: one unreachable receiver then blocks every other watch on the \
         bench behind its retries"
    );
    assert!(
        sweep.contains("failed_streak") || sweep.contains("fails"),
        "the backoff has to come from somewhere durable, or a restart retries instantly \
         forever"
    );
    // ...and the streak has to reset, or a watch that failed once backs off for
    // ever after it recovers.
    let store = std::fs::read_to_string(format!(
        "{}/../conminer-core/src/store/device.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert!(
        store.contains("delivery_fail_streak = 0"),
        "a successful delivery must clear the streak"
    );
}

/// A daemon that is OOM-killed mid-call cannot run its own cleanup.
///
/// Measured on the rig: mcpd's 128 MB limit predated the 4 MB `dmesg` snapshot
/// cap (§H1) and the 50,000-line follow scan. A selftest against the bench's
/// biggest store pushed it to ~130 MB, the kernel killed it mid-run, and the
/// caller got an EMPTY BODY -- not an error -- with the board still powered and
/// its lease still held.
#[test]
fn mcpd_has_room_for_the_answers_it_is_asked_to_build() {
    let compose = std::fs::read_to_string(format!(
        "{}/../../docker-compose.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let limit_of = |service: &str| -> u64 {
        let block = compose
            .split(&format!("\n  {service}:"))
            .nth(1)
            .unwrap_or_else(|| panic!("no {service} service"));
        let line = block
            .lines()
            .find(|l| l.trim_start().starts_with("mem_limit:"))
            .unwrap_or_else(|| panic!("{service} has no mem_limit"));
        let v = line.split(':').nth(1).unwrap().trim();
        let n: u64 = v.trim_end_matches(['m', 'g']).parse().unwrap();
        if v.ends_with('g') {
            n * 1024
        } else {
            n
        }
    };
    // The multiplier is measured, not intuited: with a 4 MB snapshot cap the
    // process peaked near 130 MB, because building an answer costs several
    // copies of it (capture buffer, escaped JSON, response body) on top of the
    // stores and template trees already resident. 8x "felt" safe and would have
    // passed the very limit that got the daemon killed -- so the floor is the
    // measurement with headroom, not a guess.
    let snapshot_mb = Config::default().api.max_snapshot_bytes / (1024 * 1024);
    assert!(
        limit_of("mcpd") >= snapshot_mb * 64,
        "mcpd may be asked for a {snapshot_mb} MB snapshot and was OOM-killed mid-call at \
         128 MB building one; its limit has to leave room to build, serialise and send it"
    );
    assert!(
        limit_of("minerd") >= 1024,
        "minerd holds a ring buffer and a template tree per device; it was measured at 97% \
         of a 512 MB limit with this bench's stores warm"
    );
}

/// A page cache is PER CONNECTION, and a daemon holds one per device.
///
/// The 64 MB setting reads as "64 MB" and behaves as 64 MB x 18 stores on this
/// bench. That is a cache doing its job -- but it is only safe while the
/// container ceilings clear it. They did not: measured across fifteen selftest
/// runs, minerd climbed 209 MB -> 751 MB with no plateau, on a bench where mcpd
/// had already been OOM-killed mid-call at 128 MB. The cache stays; the
/// arithmetic is now written down and the limits are held above it.
#[test]
fn the_page_cache_ceiling_is_known_and_the_limits_clear_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let st = conminer_core::store::DeviceStore::open(&path, "dev", false).unwrap();

    // SQLite's convention: negative is KiB.
    let steady_kib = -st.cache_size().unwrap();
    st.use_bulk_cache().unwrap();
    let bulk_kib = -st.cache_size().unwrap();
    assert!(
        bulk_kib > steady_kib,
        "an ingest must be able to ask for more than the steady-state cache"
    );

    // What a daemon holding a bench of stores can reach, in MB.
    const STORES_ON_A_BIG_BENCH: i64 = 24;
    let ceiling_mb = (steady_kib / 1024) * STORES_ON_A_BIG_BENCH;

    let compose = std::fs::read_to_string(format!(
        "{}/../../docker-compose.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let limit_mb = |service: &str| -> i64 {
        let block = compose
            .split(&format!("\n  {service}:"))
            .nth(1)
            .unwrap_or_else(|| panic!("no {service} service"));
        let line = block
            .lines()
            .find(|l| l.trim_start().starts_with("mem_limit:"))
            .unwrap_or_else(|| panic!("{service} has no mem_limit"));
        let v = line.split(':').nth(1).unwrap().trim();
        let n: i64 = v.trim_end_matches(['m', 'g']).parse().unwrap();
        if v.ends_with('g') {
            n * 1024
        } else {
            n
        }
    };
    assert!(
        limit_mb("minerd") >= ceiling_mb * 2,
        "minerd holds a page cache per device: {ceiling_mb} MB of cache alone on a busy \
         bench, before ring buffers and template trees. Its limit is {} MB",
        limit_mb("minerd")
    );
    // mcpd builds multi-megabyte answers on top of the same per-store caches --
    // a `dmesg` snapshot alone is capped at 4 MB (§H1) and costs several copies.
    let snapshot_mb = (Config::default().api.max_snapshot_bytes / (1024 * 1024)) as i64;
    assert!(
        limit_mb("mcpd") >= snapshot_mb * 64,
        "mcpd was OOM-killed mid-call at 128 MB building one; its limit is {} MB",
        limit_mb("mcpd")
    );

    // The ingest path is where the bigger cache is asked for.
    let tools = std::fs::read_to_string(format!(
        "{}/../conminer-mcp/src/tools.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert!(
        tools.contains("store.use_bulk_cache()"),
        "a bulk ingest walks several large indexes at once and should say so"
    );
}
/// L7: the partial line is stamped with WHEN THE BOARD SPOKE, not with now.
///
/// `console_state` reads that timestamp as "the console last produced a byte" --
/// a partial line is output like any other. Stamping it at PUBLISH time made a
/// board that had gone quiet look like it was still talking, because publishing
/// happens on a tick and ticks keep coming. Measured on the ADP after a verified
/// power-off: last real line 378 s old, pending-tail stamp 70 s old, and the
/// console reported `streaming` with the explanation "the board is still
/// producing output".
#[test]
fn l7_the_partial_line_is_stamped_when_the_bytes_arrived() {
    use conminer_core::pipeline::Pipeline;
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(conminer_core::clock::StepClock::default());
    let store =
        conminer_core::store::DeviceStore::open(&dir.path().join("d.db"), "dev", false).unwrap();
    let mut pipe = Pipeline::new(
        store,
        std::sync::Arc::new(ProfileSet::builtin().unwrap()),
        Config::default(),
        "dev",
        None,
        clock.clone(),
    )
    .unwrap();
    pipe.begin_session(
        conminer_core::store::SessionSource::Live,
        Some("l7"),
        None,
        None,
    )
    .unwrap();

    // The board prints a line and then an unterminated prompt, and stops.
    pipe.feed(b"[    0.043] boot line\nroot@board:~# ").unwrap();
    let spoke_at = {
        use conminer_core::clock::Clock;
        clock.now_wall_ms()
    };
    // The partial publishes once it has been STABLE across a tick -- and by then
    // five minutes of silence have gone by.
    pipe.tick().unwrap();
    clock.advance_ms(300_000);
    pipe.tick().unwrap();

    let store = pipe.into_store();
    let (text, ts) = store.pending_tail().unwrap().expect("a published partial");
    assert!(
        text.contains("root@board"),
        "the prompt is what got published: {text:?}"
    );
    assert!(
        (ts - spoke_at).abs() < 5_000,
        "the partial was stamped {} s after the bytes arrived: a console that has been \
         silent since then reads as talking",
        (ts - spoke_at) / 1000
    );

    // And the silence the console layer reads agrees with the wall clock.
    use conminer_core::clock::Clock;
    let now = clock.now_wall_ms();
    let silence = conminer_core::console::silence_ms(&store, now)
        .unwrap()
        .expect("some silence");
    assert!(
        silence >= 300_000,
        "five minutes of quiet measured as {silence} ms"
    );
}

/// L7: a leftover partial line must not keep a silent console looking busy.
///
/// Measured on the ADP with the capture cursor provably frozen -- no bytes for
/// three minutes -- `console_state` alternated between `streaming` and
/// `unstable` on consecutive calls, because a partial stamped at PUBLISH time
/// sat right at the talking threshold. Two different wrong answers from
/// identical inputs.
///
/// Stamping fixes new writes; this rule bounds the damage from the ones already
/// on disk, which is the state every deployed bench starts from.
#[test]
fn l7_a_stale_partial_is_not_evidence_that_bytes_are_flowing() {
    use conminer_core::pipeline::Pipeline;
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(conminer_core::clock::StepClock::default());
    let store =
        conminer_core::store::DeviceStore::open(&dir.path().join("d.db"), "dev", false).unwrap();
    let mut pipe = Pipeline::new(
        store,
        std::sync::Arc::new(ProfileSet::builtin().unwrap()),
        Config::default(),
        "dev",
        None,
        clock.clone(),
    )
    .unwrap();
    pipe.begin_session(
        conminer_core::store::SessionSource::Live,
        Some("l7b"),
        None,
        None,
    )
    .unwrap();
    // The board says one line, then goes quiet for good.
    pipe.feed(b"[    0.043] boot line\n").unwrap();
    let mut store = pipe.into_store();

    use conminer_core::clock::Clock;
    // Five minutes later something republishes the partial with a stamp of its
    // own -- exactly what a pre-fix build wrote every time a process restarted
    // or a connection dropped, and exactly what is sitting in the stores today.
    clock.advance_ms(300_000);
    let republished_at = clock.now_wall_ms();
    store
        .set_pending_tail("root@board:~# ", republished_at)
        .unwrap();
    clock.advance_ms(100_000);

    let now = clock.now_wall_ms();
    let silence = conminer_core::console::silence_ms(&store, now)
        .unwrap()
        .expect("some silence");
    assert!(
        silence >= 300_000,
        "no bytes have arrived for 400 s, but the silence reads as {silence} ms: a stale \
         partial is still counted as console activity"
    );
}

/// M1: "the board is up and waiting for a login" must not outlive the board.
///
/// The last lying surface. A credential gate seen once stays in the buffer, so a
/// board powered off an hour ago still reported `login_wait` with
/// `not_commandable_because: "the board is up and waiting for a login"`. The
/// claim decays to `no_signal` once silence AND off-evidence accumulate -- and
/// what it used to say rides along as `last_known`, so nothing is lost.
#[test]
fn m1_a_login_gate_decays_when_the_board_goes_dark() {
    let mut cfg = Config::default();
    cfg.controllers
        .push(conminer_core::config::ControllerProfile {
            name: "m1-ctl".into(),
            match_glob: "*M1_Ctl*".into(),
            controls: "*M1_Board*".into(),
            power_state: Some("/bin/echo off".into()),
            power: None,
            boot_mode: None,
            boot_overrides: None,
            boot_mode_release: None,
            flash: None,
            boot_modes: Vec::new(),
            power_timeout_s: None,
            off_settle_s: 0.0,
            exclude_from_discovery: true,
            mode_enters_immediately: false,
        });
    let rig = Rig::with_config(cfg);
    let console = "/dev/serial/by-id/usb-M1_Board-if00-port0";
    let ids = rig.board("m1", &[console], "/dev/serial/by-id/usb-M1_Ctl-if00");
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into(
        "[    2.000] Booting Linux\nqcs9100 login: \n",
        Some(console),
    );

    let live = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        live["console"]["state"], "login_wait",
        "the corpus must produce the claim this gate is about: {live}"
    );

    // The board goes dark: nothing arrives, and the controller says off.
    rig.clock.advance_ms(120_000);
    let dark = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        dark["console"]["state"], "no_signal",
        "a login gate on a board the controller reports off is a memory, not a state: {}",
        dark["console"]
    );
    assert_eq!(
        dark["console"]["last_known"]["state"], "login_wait",
        "...and what it used to say must ride along: {}",
        dark["console"]
    );
    assert!(
        dark["console"]["last_known"]["silent_ms"]
            .as_i64()
            .unwrap_or(0)
            >= 120_000,
        "with the silence that killed it: {}",
        dark["console"]["last_known"]
    );
}

/// M1: a quiet shell on a board nobody says is off is still a shell.
///
/// The decay needs BOTH silence and off-evidence. An idle root prompt is the
/// single most common state on this bench, and four earlier rounds were spent
/// making `console_state` report it correctly -- decaying it on silence alone
/// would undo exactly that.
#[test]
fn m1_an_idle_shell_does_not_decay_on_silence_alone() {
    let rig = Rig::new();
    let console = "/dev/serial/by-id/usb-M1_Live-if00-port0";
    let ids = rig.board("m1b", &[console], "/dev/serial/by-id/usb-M1_LiveCtl-if00");
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into(
        "[    2.000] Run /sbin/init as init process\nroot@board:~# \n",
        Some(console),
    );
    let before = rig.call("console_state", json!({"device": console}));
    let state = before["console"]["state"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        state.starts_with("at_prompt"),
        "the corpus must produce a prompt state: {before}"
    );

    rig.clock.advance_ms(600_000);
    let after = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        after["console"]["state"], state,
        "ten minutes of quiet at a shell is a quiet shell, not a dark board: {}",
        after["console"]
    );
}

/// M2: the loopback trap is surfaced where the TOOL speaks, not only in prose.
///
/// The first attempt at this put the sentence in conminer.toml and the README --
/// neither of which an agent arming a watch ever reads. What it reads is the
/// tool description, the response, and the error hint. `http://127.0.0.1:*` was
/// also the FIRST entry in the shipped allowlist: the one address class that can
/// never reach a host-side receiver from inside the container, presented as the
/// example.
#[test]
fn m2_a_loopback_webhook_is_flagged_where_the_caller_will_see_it() {
    // 1. The description, which is what an agent reads before it calls.
    let t = conminer_mcp::tools::find("create_watch").expect("create_watch");
    let schema = (t.schema)();
    let notify = schema["properties"]["notify"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        notify.contains("127.0.0.1") && notify.contains("container"),
        "the notify description must say what 127.0.0.1 means from inside the container: \
         {notify:?}"
    );

    // 2. The response: allowed is not the same as reachable.
    let rig = Rig::new();
    let device = rig.ingest_text("[    1.0] hello\n");
    rig.lease(&device);
    let made = rig.call(
        "create_watch",
        json!({"device": device, "name": "m2-loop", "until": {"pattern": "boom"},
               "notify": {"url": "http://127.0.0.1:8099/hook"}}),
    );
    let note = made["watch"]["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("conminer container") && note.contains("LAN address"),
        "a loopback webhook must come back with a note saying it cannot reach a host-side \
         receiver: {made}"
    );

    // ...and a LAN URL must NOT be nagged about.
    let clean = rig.call(
        "create_watch",
        json!({"device": device, "name": "m2-lan", "until": {"pattern": "boom"},
               "notify": {"url": "http://192.168.10.10:8099/hook"}}),
    );
    assert!(
        clean["watch"]["note"].is_null(),
        "a reachable URL needs no note: {clean}"
    );

    // 3. The rejection hint, read by somebody about to reach for loopback.
    let err = rig.err(
        "create_watch",
        json!({"device": device, "name": "m2-bad", "until": {"pattern": "boom"},
               "notify": {"url": "http://evil.example.com/x"}}),
    );
    let detail = err["detail"].to_string();
    assert!(
        detail.contains("127.0.0.1") && detail.contains("container"),
        "the allowlist rejection must warn about loopback before the caller tries it: {err}"
    );

    // 4. And loopback is not the example in the shipped allowlist.
    let allow = Config::default().notify.allow;
    assert!(
        !allow[0].contains("127.0.0.1"),
        "the first entry is the one people copy: {allow:?}"
    );
    assert!(
        allow.iter().any(|a| a.contains("127.0.0.1")),
        "it stays allowed -- a bare-metal install is a real deployment: {allow:?}"
    );
}

/// M1: when the claim cannot be decayed, it must at least carry its age.
///
/// A controller with no sense line cannot tell "off" from "waiting at a login
/// gate" -- both are silence -- so the claim stands, which is honest. What is
/// not honest is presenting it as timeless: "the board is up and waiting for a
/// login" reads the same at two seconds and at two hours.
#[test]
fn m1_an_undecayable_claim_still_says_how_old_it_is() {
    let rig = Rig::new();
    let console = "/dev/serial/by-id/usb-M1_NoSense-if00-port0";
    let ids = rig.board(
        "m1c",
        &[console],
        "/dev/serial/by-id/usb-M1_NoSenseCtl-if00",
    );
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into(
        "[    2.000] Booting Linux\nqcs9100 login: \n",
        Some(console),
    );
    rig.clock.advance_ms(120_000);

    let v = rig.call("console_state", json!({"device": console}));
    let c = &v["console"];
    assert_eq!(
        c["state"], "login_wait",
        "with no way to know the board is off, the observation stands: {c}"
    );
    assert!(
        c["silent_ms"].as_i64().unwrap_or(0) >= 120_000,
        "...but it must say how long ago it was observed: {c}"
    );
    assert!(
        c["staleness"]
            .as_str()
            .is_some_and(|s| s.contains("cannot measure power")),
        "...and why it cannot be settled: {c}"
    );
}

/// A device you could not forget.
///
/// conminer deliberately never deletes a device row: a `gone` board is one whose
/// cable fell out, and its port assignment, nickname and capture history are
/// exactly what make it the same board when it comes back. But that left no way
/// to remove a row that was never a board at all -- measured on the bravo bench,
/// where five EDL entries left a `pci-…-usb-0:2:1.0-port0` console holding a
/// ser2net port, mined into 995 bytes across six sessions, that was the QDL
/// flash gadget rather than a console.
#[test]
fn a_row_that_was_never_a_board_can_be_forgotten_and_a_live_one_cannot() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    0.1] boot\n");

    // Present means no: forgetting a live row deletes what discovery is about to
    // recreate, and takes its port and nickname with it.
    let err = rig.err("forget_device", json!({"device": device}));
    assert_eq!(err["code"], "INVALID_ARGUMENT", "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("not gone"),
        "the refusal must say why: {err}"
    );
    let before = rig.call("list_devices", json!({}))["devices"]
        .as_array()
        .unwrap()
        .len();

    // Mark it gone the way discovery would when the device stops being there.
    {
        let mut reg = rig.registry();
        let row = reg.device_by_canonical(&device).unwrap().unwrap();
        reg.set_state(row.id, "gone").unwrap();
    }

    let out = rig.call("forget_device", json!({"device": device}));
    assert_eq!(out["was"], "gone");
    assert_eq!(out["data_dropped"], false, "the store survives by default");
    assert_eq!(
        rig.call("list_devices", json!({}))["devices"]
            .as_array()
            .unwrap()
            .len(),
        before - 1,
        "the row must be gone from the listing"
    );
}

// -------------------------------- what firmware ran, and which fingerprint ---

/// TWO DIFFERENT THINGS WERE BOTH CALLED A FINGERPRINT (report #5).
///
/// An agent read `fingerprint: null` from a boot whose own banner said
/// `build=f0a276f875fba3d6` -- on a line the search tools had already found --
/// and filed a defect against firmware detection. Nothing was broken: it was
/// reading the epoch SHAPE signature, and the field it actually wanted, what
/// the board said it was running, did not exist on this tool at all.
#[test]
fn boot_report_says_what_the_board_said_it_was_running() {
    let rig = Rig::new();
    let device = rig.ingest_text(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         Sirocco version 0.1.0-sirocco-unoq-ext4release-shell (mojo 1.1.0.dev2026081405) \
         #1 SMP PREEMPT aarch64 build=f0a276f875fba3d6\n\
         BUILD fp=f0a276f875fba3d6\n\
         CONSOLE\n",
    );
    let boots = rig.call("list_boots", json!({"device": device, "limit": 3}));
    let b = boots["boots"][0]["id"].as_i64().unwrap();
    let rep = rig.call("boot_report", json!({"device": device, "boot": b}));

    let fps = rep["build_fingerprints"]
        .as_array()
        .unwrap_or_else(|| panic!("boot_report must report what the board printed: {rep}"));
    assert!(
        fps.iter().any(|v| v == "f0a276f875fba3d6"),
        "the board printed build=f0a276f875fba3d6 twice; boot_report reported {fps:?}"
    );
    // ...and the two fingerprints must stay distinguishable, which is the whole
    // point: one is the shape of the epoch, the other is the firmware.
    assert_ne!(
        rep["fingerprint"], rep["build_fingerprints"][0],
        "the shape signature and the firmware build must not be conflated: {rep}"
    );
    assert!(
        rep["fingerprint_is"]
            .as_str()
            .unwrap_or_default()
            .contains("shape"),
        "`fingerprint` must say which of the two it is: {rep}"
    );
}

/// ...and when the shape signature is null, say why instead of returning a bare
/// null next to `outcome`. It is sealed when the epoch closes, so an epoch that
/// is still running has none yet -- which reads as a defect if nothing says so.
#[test]
fn an_open_epoch_explains_its_missing_shape_fingerprint() {
    let rig = Rig::new();
    let device = rig.ingest_text("[    0.1] booting\n");
    // A mark opens a fresh epoch that is still open and has produced nothing.
    rig.call("acquire", json!({"device": device}));
    rig.call("mark", json!({"device": device, "label": "still-running"}));
    let rep = rig.call("boot_report", json!({"device": device}));

    assert!(
        rep["fingerprint"].is_null(),
        "the fixture must leave the epoch open and unsealed: {rep}"
    );
    let why = rep["fingerprint_pending"].as_str().unwrap_or_default();
    assert!(
        why.contains("still open"),
        "a null shape signature must explain itself: {rep}"
    );
}

/// FLAPPING PROMISES ALTERNATION. A BENCH BEING REFLASHED IS NOT ALTERNATING.
///
/// `history` counted DISTINCT fingerprints, so every board under active
/// development was "flapping" forever. Measured on the Uno-Q: boot 473 came
/// back `booted` with `history: flapping` and 19 recent fingerprints each seen
/// EXACTLY ONCE. Nothing repeated; the board was simply reflashed between
/// boots. The word sent an agent looking for marginal hardware.
#[test]
fn a_board_reflashed_between_boots_is_varied_not_flapping() {
    let rig = Rig::new();
    let mut text = String::new();
    for i in 0..6 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        // A line unique to each epoch, so no two boots share a template
        // sequence and every fingerprint is a singleton.
        text.push_str(&format!("probe unit_{i} attached\n"));
        text.push_str(&format!(
            "[    0.000000] Linux version 6.12.{i} (b@h) (gcc)\n"
        ));
    }
    let device = rig.ingest_text(&text);
    let rep = rig.call("boot_report", json!({"device": device}));

    // NON-VACUITY: the chain must really be all-singletons, or this is testing
    // some other branch.
    let distinct = rep["distinct_fingerprints_recent"].as_i64().unwrap();
    assert!(
        distinct > 2,
        "the fixture must produce a churning chain: {rep}"
    );
    assert_eq!(
        rep["history"], "varied",
        "every recent epoch differs and none recurs, so nothing is alternating: {rep}"
    );
}

/// ...and the same rule the other way. `A B A B A B` is a board alternating
/// between two outcomes -- the most flapping thing a bench can do -- and the
/// old `> 2 distinct` test called it `steady`, because there were only two.
#[test]
fn a_board_alternating_between_two_shapes_is_flapping() {
    let rig = Rig::new();
    let mut text = String::new();
    for i in 0..6 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        if i % 2 == 0 {
            text.push_str("[    0.000000] Linux version 6.12.9 (b@h) (gcc)\n");
            text.push_str("mmc0: card is ready\n");
        } else {
            text.push_str("[    0.000000] Linux version 6.12.9 (b@h) (gcc)\n");
            text.push_str("mmc0: timeout waiting for hardware interrupt\n");
            text.push_str("mmc0: giving up\n");
        }
    }
    let device = rig.ingest_text(&text);
    let rep = rig.call("boot_report", json!({"device": device}));

    // NON-VACUITY: a fingerprint must really recur, and the chain must not be
    // one single repeated shape (that is `looping`, a different verdict).
    let distinct = rep["distinct_fingerprints_recent"].as_i64().unwrap();
    assert!(
        distinct >= 2,
        "the fixture must produce more than one shape: {rep}"
    );
    assert_eq!(
        rep["history"], "flapping",
        "the board alternates between shapes it has been in before: {rep}"
    );
}

/// THE UART IS GONE BY DESIGN, SO THE RESPONSE MUST NOT OFFER IT (report #7).
///
/// `boot_mode EDL` returned `entered=true` alongside
/// `freshness.console.state=at_prompt, kind=rtos_shell, commandable=true`, while
/// a `diagnose` issued immediately afterwards correctly said `edl=true,
/// device_state=gone, capture_state=not_listening`. The envelope is derived from
/// the store, and the store still held the prompt the board was sitting at --
/// the capture loop cannot know the device vanished until it next fails to read
/// it. So the one call that KNOWS the console just went away was the one still
/// advertising it as commandable.
#[test]
fn boot_mode_edl_does_not_answer_with_a_commandable_console() {
    let mut cfg = conminer_core::config::Config::default();
    cfg.controllers
        .push(conminer_core::config::ControllerProfile {
            name: "edl-ctl".into(),
            match_glob: "*Edl_Ctl*".into(),
            controls: "*Edl_Board*".into(),
            power_state: Some("/bin/echo on".into()),
            power: Some("/bin/echo powered".into()),
            boot_mode: Some("/bin/echo entered".into()),
            boot_overrides: None,
            boot_mode_release: None,
            flash: None,
            boot_modes: vec!["EDL".into()],
            power_timeout_s: None,
            off_settle_s: 0.0,
            exclude_from_discovery: true,
            // This controller sequences the entry itself: after this call the
            // board IS in EDL and its console has re-enumerated away.
            mode_enters_immediately: true,
        });
    let rig = Rig::with_config(cfg);
    let console = "/dev/serial/by-id/usb-Edl_Board-if00-port0";
    let ids = rig.board("edlboard", &[console], "/dev/serial/by-id/usb-Edl_Ctl-if00");
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into("APP admit\nCONSOLE\nsirocco> \n", Some(console));
    rig.call(
        "classify_prompt",
        json!({"device": console, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    rig.call("acquire", json!({"device": console}));

    // NON-VACUITY: before EDL the console must really read as commandable, or
    // this gate would pass on a board that was never at a prompt.
    let before = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        before["console"]["commandable"], true,
        "the fixture must start from a commandable prompt: {before}"
    );

    let entered = rig.call("boot_mode", json!({"device": console, "mode": "EDL"}));
    assert_eq!(
        entered["entered"], true,
        "the fixture must enter EDL: {entered}"
    );
    assert_ne!(
        entered["freshness"]["console"]["commandable"], true,
        "this call just sequenced the board into EDL; its own answer must not \
         invite a command into a console that no longer exists: {entered}"
    );
    // §W4: the envelope's capture_state field itself must be LIVE. The handler
    // published `away_in_edl` mid-call; the response is built from the row
    // resolved at entry (still `listening`), so only a live re-read in
    // freshness() can show the new value. This is the exact class behind report
    // #7, and it is now killed by re-reading rather than by patching the row.
    assert_eq!(
        entered["freshness"]["capture_state"], "away_in_edl",
        "the envelope must report the capture health this very call published, \
         not the value its row was resolved with: {entered}"
    );

    // And the same holds for a SEPARATE reader whose row was resolved before the
    // state changed: publish a fresh capture state out-of-band, then read.
    {
        let mut reg = rig.registry();
        conminer_core::live::publish_capture_state(
            &mut reg,
            ids[0],
            conminer_core::live::CaptureState::Listening,
        )
        .unwrap();
    }
    let back = rig.call("console_state", json!({"device": console}));
    assert_eq!(
        back["freshness"]["capture_state"], "listening",
        "a later reader must see the live column, not a cached row: {back}"
    );
}

/// AN EPOCH LEGITIMATELY CONTAINS LINES OLDER THAN ITS OWN `opened_at`.
///
/// `opened_at` is stamped when the actuation TOOL finished; the boundary is the
/// stream offset taken when the button was pressed. A power hook holds its line
/// for seconds with verification after it, and a fast board boots in between --
/// so its output is timestamped before the epoch that contains it. Two separate
/// agents read that as corruption and filed it, the second against a boot whose
/// attribution was correct by then. The response now says so itself.
#[test]
fn boot_report_says_opened_at_is_not_when_the_boot_began() {
    let rig = Rig::new();
    let device = rig.ingest_text("APP admit\nCONSOLE\nBUILD fp=deadbeefcafe1\n");
    let boots = rig.call("list_boots", json!({"device": device, "limit": 3}));
    let b = boots["boots"][0]["id"].as_i64().unwrap();
    let rep = rig.call("boot_report", json!({"device": device, "boot": b}));

    assert!(
        rep["began_at"].is_i64(),
        "the epoch must report when its first line actually arrived: {rep}"
    );
    let says = rep["opened_at_is"].as_str().unwrap_or_default();
    assert!(
        says.contains("COMPLETED"),
        "`opened_at` must say it is the actuation's completion time: {rep}"
    );
}

/// AN EMPTY EPOCH POINTS AT THE ONE THAT HOLDS THE OUTPUT (report #11).
///
/// A `session` marker records nothing, so provenance for it has no evidence of
/// its own and walks BACKWARDS by epoch id for firmware banners. Once actuation
/// epochs are back-dated to their pre-hook stream mark, the epoch that covers a
/// marker's position can have a HIGHER id and a LOWER offset -- measured on the
/// Uno-Q: epoch 631 is an empty marker at offset 5605020 while the boot covering
/// it, 633, sits at 5605018, two ids later and holding `BUILD fp=1eae21255070069f`.
/// An agent asked 631 about that fingerprint and was told the epoch showed only
/// Qualcomm firmware strings, because the walk looked the wrong way.
#[test]
fn provenance_on_an_empty_epoch_names_the_epoch_that_holds_the_output() {
    let rig = Rig::new();
    let device = rig.ingest_text("NOTICE:  BL1: v2.11\nBUILD fp=1eae21255070069f\nCONSOLE\n");
    rig.call("acquire", json!({"device": device}));
    // A marker opened after that output: it records nothing of its own.
    rig.call("mark", json!({"device": device, "label": "empty-marker"}));
    let boots = rig.call("list_boots", json!({"device": device, "limit": 5}));
    let marker = boots["boots"][0]["id"].as_i64().unwrap();

    let prov = rig.call("provenance", json!({"device": device, "boot": marker}));

    // NON-VACUITY: the queried epoch must really be the empty one.
    assert_eq!(
        prov["recorded_nothing"], true,
        "the fixture must query an epoch with no output: {prov}"
    );
    let covered = &prov["covered_by"];
    assert!(
        covered["boot_id"].is_i64(),
        "an epoch that recorded nothing must name the one that covers its position: {prov}"
    );
    assert_ne!(
        covered["boot_id"].as_i64(),
        Some(marker),
        "and that must not be itself: {prov}"
    );
    assert!(
        covered["bytes"].as_i64().unwrap_or(0) > 0,
        "the covering epoch must be one that actually recorded output: {prov}"
    );
}

// ------------------------------------------- one actuation per board at a time

/// A config whose power hook takes a while, so two calls can overlap, and
/// whose boards also have a boot_mode hook, so a strap change can collide too.
fn cfg_with_slow_hook(seconds: u32) -> Config {
    let mut c = cfg_with_hook();
    c.hooks.power_timeout_s = seconds as u64 + 5;
    let over = conminer_core::config::DeviceOverride {
        hooks: conminer_core::config::DeviceHooks {
            power: Some(format!("/bin/sleep {seconds}")),
            ..Default::default()
        },
        ..Default::default()
    };
    for key in [AP, SM] {
        c.devices.insert(key.to_string(), over.clone());
    }
    c.controllers
        .push(conminer_core::config::ControllerProfile {
            name: "rigx-ctl".into(),
            match_glob: "*Bantam_RIGX*".into(),
            controls: "*Rig_Board_X*".into(),
            power_state: None,
            power: None,
            boot_mode: Some("/bin/true {mode} {device}".into()),
            boot_overrides: None,
            boot_mode_release: None,
            flash: None,
            boot_modes: vec!["EDL".into()],
            power_timeout_s: None,
            off_settle_s: 0.0,
            exclude_from_discovery: true,
            mode_enters_immediately: false,
        });
    c
}

/// Report #14 (bravo, build 24665a87d065): a `power off` on the Uno Q escalated
/// (reset-then-off, ~60 s of pressing and settling); the agent's client gave up,
/// the agent issued `power on`, conminer ACCEPTED it, the board booted, and the
/// escalation's final press then reset that kernel 28 s later -- recorded as a
/// power epoch nobody had asked for. Two actuations on one board at once is a
/// race with the hardware, not a queue: the second must be refused, by name.
#[test]
fn a_second_actuation_on_a_board_mid_actuation_is_refused_not_raced() {
    let rig = Rig::with_config(cfg_with_slow_hook(2));
    rig.board("boardrace", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);

    let started = std::time::Instant::now();
    std::thread::scope(|s| {
        let first = s.spawn(|| rig.call("power", json!({"target": "boardrace", "action": "off"})));
        // Let the first call reach its hook.
        std::thread::sleep(std::time::Duration::from_millis(400));

        // The whole-target form and the single-console form both collide.
        for args in [
            json!({"target": "boardrace", "action": "on"}),
            json!({"device": SM, "action": "reset"}),
            json!({"device": AP, "mode": "EDL"}),
        ] {
            let tool = if args.get("mode").is_some() {
                "boot_mode"
            } else {
                "power"
            };
            let e = rig.err(tool, args.clone());
            assert_eq!(
                e["code"], "ACTUATION_IN_FLIGHT",
                "{tool} {args} must be refused while the off is running: {e}"
            );
            assert_eq!(
                e["detail"]["action"], "off",
                "it must say WHAT is running: {e}"
            );
            assert_eq!(e["detail"]["tool"], "power");
            assert!(
                e["detail"]["running_for_ms"].is_i64() || e["detail"]["running_for_ms"].is_u64(),
                "and for how long: {e}"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_millis(1800),
                "the refusal must be immediate, not queued behind the hook"
            );
        }
        let done = first.join().unwrap();
        assert!(
            done["duration_ms"].as_u64().unwrap_or(0) >= 2000,
            "the first call reports how long the whole workflow held the board: {done}"
        );
    });

    // Once the first workflow has returned, the board is free again.
    let again = rig.call("power", json!({"target": "boardrace", "action": "on"}));
    assert!(
        again["boot_id"].is_i64() || again["boot_id"].is_u64(),
        "{again}"
    );
}

/// The claim must not outlive a workflow that FAILED. A hook that exits
/// non-zero returns HOOK_FAILED; the very next call must see a free board, not a
/// phantom actuation.
#[test]
fn a_failed_actuation_releases_the_board() {
    let mut c = cfg_with_hook();
    let over = conminer_core::config::DeviceOverride {
        hooks: conminer_core::config::DeviceHooks {
            power: Some("/bin/false {action}".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    for key in [AP, SM] {
        c.devices.insert(key.to_string(), over.clone());
    }
    let rig = Rig::with_config(c);
    rig.board("boardfail", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);

    let e = rig.err("power", json!({"target": "boardfail", "action": "off"}));
    assert_eq!(e["code"], "HOOK_FAILED", "{e}");
    let e2 = rig.err("power", json!({"target": "boardfail", "action": "off"}));
    assert_eq!(
        e2["code"], "HOOK_FAILED",
        "the failed call must have released the board; a phantom in-flight claim \
         would answer ACTUATION_IN_FLIGHT here: {e2}"
    );
}

/// A dry run plans; it must never claim the board or be refused by a claim.
#[test]
fn a_dry_run_neither_takes_nor_respects_the_actuation_claim() {
    let rig = Rig::with_config(cfg_with_slow_hook(2));
    rig.board("boarddry", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);
    std::thread::scope(|s| {
        let first = s.spawn(|| rig.call("power", json!({"target": "boarddry", "action": "off"})));
        std::thread::sleep(std::time::Duration::from_millis(400));
        let plan = rig.call(
            "power",
            json!({"target": "boarddry", "action": "on", "dry_run": true}),
        );
        assert_eq!(plan["dry_run"], true, "{plan}");
        first.join().unwrap();
    });
}

/// `actuation_status` is how a caller that gave up on a long `power` call finds
/// out what happened: what is running now (and in which phase), then the
/// finished workflow's effect. Read-only, no lease.
#[test]
fn actuation_status_reports_the_running_workflow_and_then_its_outcome() {
    let rig = Rig::with_config(cfg_with_slow_hook(2));
    rig.board("boardstat", &[AP, SM], CTL);
    rig.lease(AP);
    rig.lease(SM);

    // Nothing yet: free, no history.
    let idle = rig.call("actuation_status", json!({"target": "boardstat"}));
    assert_eq!(idle["board_free"], true, "{idle}");
    assert!(idle["last"].is_null(), "{idle}");

    std::thread::scope(|s| {
        let first = s.spawn(|| rig.call("power", json!({"target": "boardstat", "action": "off"})));
        std::thread::sleep(std::time::Duration::from_millis(400));
        // No lease needed: ask by a single console of the board.
        let st = rig.call("actuation_status", json!({"device": SM}));
        assert_eq!(st["board_free"], false, "{st}");
        assert_eq!(st["in_flight"]["tool"], "power", "{st}");
        assert_eq!(st["in_flight"]["action"], "off", "{st}");
        assert_eq!(
            st["in_flight"]["phase"], "hook",
            "the hook is what is running: {st}"
        );
        assert!(st["in_flight"]["running_for_ms"].is_number(), "{st}");
        first.join().unwrap();
    });

    let after = rig.call("actuation_status", json!({"target": "boardstat"}));
    assert_eq!(after["board_free"], true, "{after}");
    assert!(after["in_flight"].is_null(), "{after}");
    let last = &after["last"];
    assert_eq!(last["tool"], "power", "{after}");
    assert_eq!(last["action"], "off", "{after}");
    assert!(
        last["effect"].is_object(),
        "the finished effect is kept: {after}"
    );
    assert!(last["finished_ms"].is_number(), "{after}");
    assert!(
        last["boot_id"].is_number(),
        "and which epoch it opened: {after}"
    );
}

/// Report #15 (bravo, Uno Q, build 24665a87d065): after an `off` that conminer
/// itself VERIFIED, console_state kept asserting the pre-off `sirocco> ` as
/// commandable -- the epoch was not "quiet" (the board had booted out of EDL
/// mid-workflow and printed a full log) and the Bughopper has no sense line, so
/// neither existing decay condition held. Three run_commands then got zero
/// bytes. The evidence was in the store the whole time: the actuation's own
/// `power` event, `{action: off, effect: {verified: true}}`, recorded after the
/// console's last byte. This is the rule at store level; the `actuation` suite
/// drives the same thing through the real tool and its escalation.
#[test]
fn a_verified_power_off_retires_the_pre_off_prompt_claim() {
    let rig = Rig::with_config(cfg_with_hook());
    rig.board("boardoff", &[AP, SM], CTL);
    let ids = {
        let reg = rig.registry();
        [AP, SM]
            .iter()
            .map(|c| reg.device_by_canonical(c).unwrap().unwrap().id)
            .collect::<Vec<_>>()
    };
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into("APP admit\nCONSOLE\nsirocco> \n", Some(AP));
    rig.call(
        "classify_prompt",
        json!({"device": AP, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    let before = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        before["console"]["commandable"], true,
        "precondition: {before}"
    );

    // What `power` records when an off verifies: the same event, same shape.
    {
        let dev = rig.registry().device_by_canonical(AP).unwrap().unwrap();
        let path = rig.dir.join(&dev.db_file);
        let mut st = conminer_core::store::DeviceStore::open(&path, AP, true).unwrap();
        let session = st.latest_session().unwrap().map(|s| s.id);
        let boot = st.latest_boot().unwrap().map(|b| b.id);
        st.append_event(
            session,
            boot,
            1_000,
            "power",
            &json!({"action": "off", "hook": {"exit_code": 0},
                    "effect": {"verified": true, "action": "off", "escalated": false}}),
        )
        .unwrap();
    }

    let after = rig.call("console_state", json!({"device": AP}));
    assert_eq!(after["console"]["commandable"], false, "{after}");
    assert_eq!(after["console"]["state"], "no_signal", "{after}");
    assert!(
        after["console"]["last_known"]["decayed_because"]
            .as_str()
            .unwrap_or_default()
            .contains("power off was verified"),
        "{after}"
    );
    assert_eq!(
        after["console"]["last_known"]["state"], "at_prompt",
        "{after}"
    );

    // Bytes after the off mean the board is talking, whatever the event says.
    rig.ingest_into("sirocco> \n", Some(AP));
    let talking = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        talking["console"]["commandable"], true,
        "a byte after the off supersedes it: {talking}"
    );
}

/// Report #16 (bravo, Uno Q, build eeab0746a7f0): `run_command version` was
/// issued at 23:59:50Z, while the escalation of a `power off` (started 23:59:23,
/// finished 00:00:04) had its off press on the button. console_state said
/// `at_prompt_with_traffic, commandable: true` from the prompt the board printed
/// before the press; run_command probed a board being powered down, got zero
/// bytes, and the agent filed a console defect. Only power/boot_mode honoured
/// the in-flight claim. Now the console says what is happening to it, and the
/// tools that put characters on the line refuse until it is over.
#[test]
fn a_console_mid_actuation_says_so_and_refuses_commands() {
    let rig = Rig::with_config(cfg_with_slow_hook(2));
    let ids = rig.board("boardact", &[AP, SM], CTL);
    rig.registry().set_state(ids[0], "listening").unwrap();
    rig.ingest_into("APP admit\nCONSOLE\nsirocco> \n", Some(AP));
    rig.call(
        "classify_prompt",
        json!({"device": AP, "pattern": "^sirocco> $", "kind": "rtos_shell"}),
    );
    rig.lease(AP);
    rig.lease(SM);
    let before = rig.call("console_state", json!({"device": AP}));
    assert_eq!(
        before["console"]["commandable"], true,
        "precondition: {before}"
    );

    std::thread::scope(|s| {
        let first = s.spawn(|| rig.call("power", json!({"target": "boardact", "action": "off"})));
        std::thread::sleep(std::time::Duration::from_millis(400));

        let mid = rig.call("console_state", json!({"device": AP}));
        assert_eq!(mid["console"]["state"], "actuating", "{mid}");
        assert_eq!(mid["console"]["commandable"], false, "{mid}");
        assert_eq!(mid["console"]["action"], "off", "{mid}");
        assert!(
            mid["console"]["not_commandable_because"]
                .as_str()
                .unwrap_or_default()
                .starts_with("actuating"),
            "{mid}"
        );
        // The freshness envelope other tools carry agrees.
        let env = rig.call("list_boots", json!({"device": AP}));
        assert_eq!(env["freshness"]["console"]["state"], "actuating", "{env}");

        let e = rig.err(
            "run_command",
            json!({"device": AP, "command": "version", "timeout_s": 2}),
        );
        assert_eq!(e["code"], "ACTUATION_IN_FLIGHT", "{e}");
        assert_eq!(e["detail"]["action"], "off", "{e}");
        let e = rig.err("send", json!({"device": AP, "data": "\n"}));
        assert!(
            e["code"] == "ACTUATION_IN_FLIGHT" || e["code"] == "SEND_DISABLED",
            "send must not reach the line either: {e}"
        );
        first.join().unwrap();
    });

    // Free again, and the console is described from its evidence once more.
    let after = rig.call("console_state", json!({"device": AP}));
    assert_ne!(after["console"]["state"], "actuating", "{after}");
}

/// A row this node holds on a peer's behalf, exactly as peering creates it:
/// the canonical id carries the owner, which is what makes it remote.
fn peer_console(rig: &Rig, node: &str, remote_path: &str) -> String {
    let canonical = format!("peer:{node}/{remote_path}");
    let mut reg = rig.registry();
    let d = reg
        .upsert_device(&canonical, None, IdentityKind::ById, None, 0)
        .unwrap();
    reg.set_remote_route(d.id, node, Some("127.0.0.1"), remote_path, None, None, 1)
        .unwrap();
    canonical
}

/// §P1. One peer-owned console must not take the whole bench-wide search down.
///
/// `all_devices` includes the rows this node holds for its peers, and
/// `resolve_device_set` handed them straight to the local store path. There is
/// no store here for a board another node mines, so `with_store` refused with
/// INTERNAL "owned by node ... there is no local store to read", and the fan-out
/// died on the first one instead of answering from the stores it does have.
/// search_raw({devices:"all"}) failed on the first `peer:<node>/...` row while
/// every local console sat there unread.
#[test]
fn a_peer_owned_console_does_not_break_a_bench_wide_search() {
    let rig = Rig::new();
    rig.ingest_text("[    1.0] shared marker line\n");
    peer_console(
        &rig,
        "beta",
        "/dev/serial/by-id/usb-Arduino_Bughopper_SN000001-if00",
    );

    let r = rig.call(
        "search",
        json!({"devices": "all", "query": "shared marker line", "include_derived": true}),
    );
    assert!(
        !r["hits"].as_array().expect("hits").is_empty(),
        "the local stores must still answer: {r}"
    );
}

/// And the half that makes the answer honest.
///
/// Quietly dropping the peer's boards would turn "has this appeared anywhere on
/// the bench" into "anywhere on this node", and an empty result then reads as
/// "it never happened". That is a false negative in the one tool whose job is to
/// find the occurrence, so what was NOT looked at is part of the answer.
#[test]
fn a_partial_bench_search_says_which_boards_it_could_not_read() {
    let rig = Rig::new();
    rig.ingest_text("[    1.0] shared marker line\n");
    let peer = peer_console(
        &rig,
        "beta",
        "/dev/serial/by-id/usb-Arduino_Bughopper_SN000001-if00",
    );

    let r = rig.call(
        "search",
        json!({"devices": "all", "query": "shared marker line", "include_derived": true}),
    );
    let skipped = r["not_searched"]
        .as_array()
        .unwrap_or_else(|| panic!("a partial search must name what it skipped: {r}"));
    assert_eq!(skipped.len(), 1, "{r}");
    assert_eq!(skipped[0]["node"], "beta", "and say whose it is: {r}");
    assert!(
        peer.contains(skipped[0]["device"].as_str().unwrap_or("\0")),
        "named by the console it could not read: {r}"
    );
    assert!(
        r["partial_because"].as_str().is_some_and(|w| !w.is_empty()),
        "and say so in words the caller reads: {r}"
    );
}

/// An all-remote selector is an error, not an empty result.
///
/// "No hits" and "I never looked" are different answers, and only one of them
/// means the line is not there.
#[test]
fn a_selector_matching_only_peer_boards_refuses_rather_than_answering_none() {
    let rig = Rig::new();
    peer_console(
        &rig,
        "beta",
        "/dev/serial/by-id/usb-Arduino_Bughopper_SN000001-if00",
    );

    let e = rig.err(
        "search",
        json!({"devices": "all", "query": "anything at all", "include_derived": true}),
    );
    assert_eq!(e["code"], "UNKNOWN_DEVICE", "{e}");
    assert!(
        e["hint"].as_str().unwrap_or_default().contains("beta/"),
        "the hint must name the federating form: {e}"
    );
}
