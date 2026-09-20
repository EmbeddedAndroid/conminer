//! Suite `boot-overrides`: what a board controller HOLDS across boots, and the
//! workflow that lets a board leave it.
//!
//! A strap-latching controller keeps a boot-mode line asserted until something
//! releases it. A flash depends on that. It is also how a board comes to sit in
//! ROM EDL with a silent console through any number of power cycles: conminer
//! could set that line and clear it, and could not show it.
//!
//! Every test here drives a fake controller that keeps its lines in a file and
//! logs every invocation, in order. That log is the point. "The board was not
//! cycled" and "the status read changed nothing" are claims about what did NOT
//! reach the controller, and only a record of what did can carry them.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_core::store::{IdentityKind, Registry};
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;

const AP: &str = "/dev/serial/by-id/usb-Ovr_Board-if00-port0";
const SM: &str = "/dev/serial/by-id/usb-Ovr_Board-if01-port0";
const CTL: &str = "/dev/serial/by-id/usb-Microchip_Bantam_OVR-if00";

/// A controller with four override lines, as a Bantam has.
///
/// Its whole state is a directory: one file per asserted line, `log` for every
/// invocation, and a few switches a test can flip to make it misbehave the ways
/// real ones do (a release that fails, a line that will not drop, a controller
/// that stops answering reads).
const FAKE: &str = r#"#!/bin/sh
D="$FAKE_CTL_DIR"
echo "$*" >> "$D/log"
LINES="MD_EDL SS_EDL UEFI FASTBOOT_MD"
case "$1" in
  boot-overrides)
    [ -f "$D/reads_fail" ] && { echo "controller did not answer" >&2; exit 1; }
    [ -f "$D/reads_slow" ] && sleep "$(cat "$D/reads_slow")"
    for l in $LINES; do
      if [ -f "$D/unreadable.$l" ]; then echo "$l=unknown"
      elif [ -f "$D/held.$l" ]; then echo "$l=1"
      else echo "$l=0"; fi
    done ;;
  mode)
    if [ "$2" = "clear" ]; then
      [ -f "$D/clear_fails" ] && { echo "MD_EDL := 0 but reads back '1'" >&2; exit 3; }
      for l in $LINES; do [ -f "$D/stuck.$l" ] || rm -f "$D/held.$l"; done
    else
      case "$2" in
        BOOT_MD_EDL) : > "$D/held.MD_EDL" ;;
        BOOT_SS_EDL) : > "$D/held.SS_EDL" ;;
        BOOT_UEFI)   : > "$D/held.UEFI" ;;
        MD_FASTBOOT) : > "$D/held.FASTBOOT_MD" ;;
      esac
    fi ;;
  power)
    [ -f "$D/power_slow" ] && sleep "$(cat "$D/power_slow")"
    : ;;
esac
exit 0
"#;

struct Rig {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    ctl: std::path::PathBuf,
    h: Handler,
    clock: Arc<conminer_core::clock::StepClock>,
}

impl Rig {
    fn new() -> Self {
        Self::build(true)
    }

    /// A latching controller with NO way to read its lines back.
    fn without_a_read_hook() -> Self {
        Self::build(false)
    }

    fn build(readable: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("ctl");
        std::fs::create_dir_all(&ctl).unwrap();
        let script = dir.path().join("fake-ctl.sh");
        std::fs::write(&script, FAKE).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The hook inherits mcpd's environment; tests in this binary run on
        // threads, so the state directory travels in the COMMAND, not in a
        // process-wide variable two tests would fight over.
        let run = format!(
            "/usr/bin/env FAKE_CTL_DIR={} {}",
            ctl.display(),
            script.display()
        );

        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.hooks.power_timeout_s = 10;
        // Hardware settling times; a test rig has no hardware to wait for.
        cfg.hooks.verify_settle_s = 0;
        cfg.hooks.verify_off_watch_s = 0;
        cfg.hooks.verify_boot_watch_s = 0;
        cfg.hooks.edl_settle_s = 0;
        cfg.controllers.retain(|c| c.name != "bantam");
        cfg.controllers
            .push(conminer_core::config::ControllerProfile {
                name: "fake-bantam".into(),
                match_glob: "*Bantam_OVR*".into(),
                controls: "*Ovr_Board*".into(),
                power: Some(format!("{run} power {{action}} --port {{controller}}")),
                boot_mode: Some(format!("{run} mode {{mode}} --port {{controller}}")),
                power_state: None,
                boot_overrides: readable
                    .then(|| format!("{run} boot-overrides --port {{controller}}")),
                boot_mode_release: None,
                flash: None,
                boot_modes: vec![
                    "BOOT_MD_EDL".into(),
                    "BOOT_SS_EDL".into(),
                    "BOOT_UEFI".into(),
                    "MD_FASTBOOT".into(),
                ],
                power_timeout_s: None,
                off_settle_s: 0.0,
                exclude_from_discovery: true,
                mode_enters_immediately: false,
            });

        let clock = Arc::new(conminer_core::clock::StepClock::default());
        let ctx =
            Context::open(cfg, Arc::new(ProfileSet::builtin().unwrap()), clock.clone()).unwrap();
        let rig = Self {
            dir: dir.path().to_path_buf(),
            _dir: dir,
            ctl,
            h: Handler::new(ctx),
            clock,
        };
        rig.board();
        rig
    }

    /// Two consoles and their controller on one USB branch, so the controller
    /// resolves by topology exactly as it does on the bench.
    fn board(&self) {
        let mut reg = Registry::open(&self.dir).unwrap();
        for (path, by_path, ignored) in [
            (AP, "pci-0000:00:14.0-usb-0:5.1.1:1.0", false),
            (SM, "pci-0000:00:14.0-usb-0:5.1.2:1.0", false),
            (CTL, "pci-0000:00:14.0-usb-0:5.1.3:1.0", true),
        ] {
            let d = reg
                .upsert_device(path, Some(by_path), IdentityKind::ById, None, 0)
                .unwrap();
            reg.set_target(d.id, Some("ovr")).unwrap();
            reg.set_ignored(d.id, ignored).unwrap();
            reg.set_state(d.id, "listening").unwrap();
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

    fn lease(&self) {
        self.call("acquire", json!({"target": "ovr", "ttl_s": 600}));
    }

    /// Somebody ELSE holds the board.
    ///
    /// Taken in the registry, not through `acquire {holder}`: that argument
    /// renames THIS connection, so every later call in the test would run as the
    /// holder and the refusal under test could never happen.
    fn leased_by_someone_else(&self) {
        let mut reg = Registry::open(&self.dir).unwrap();
        for path in [AP, SM] {
            let d = reg.resolve(path).unwrap();
            reg.acquire_lease(
                d.id,
                "another-agent",
                1_577_836_800_000,
                3_600,
                86_400,
                false,
            )
            .unwrap();
        }
    }

    // ---- the controller, as the test sees it --------------------------------

    fn hold(&self, line: &str) {
        std::fs::write(self.ctl.join(format!("held.{line}")), "").unwrap();
    }

    fn held(&self, line: &str) -> bool {
        self.ctl.join(format!("held.{line}")).exists()
    }

    fn switch(&self, name: &str, value: &str) {
        std::fs::write(self.ctl.join(name), value).unwrap();
    }

    /// Every invocation the controller received, first word(s) only:
    /// `["mode clear", "boot-overrides", "power cycle"]`.
    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.ctl.join("log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                match w.first().copied() {
                    Some("boot-overrides") => "boot-overrides".to_string(),
                    Some(_) => w.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                    None => String::new(),
                }
            })
            .collect()
    }

    fn forget_log(&self) {
        let _ = std::fs::remove_file(self.ctl.join("log"));
    }
}

// --------------------------------------------------------------- visibility ---

/// The reported fault, made visible: a held EDL line is named, by the controller.
#[test]
fn a_held_override_is_read_from_the_controller_and_named() {
    let rig = Rig::new();
    rig.hold("MD_EDL");
    let r = rig.call("boot_overrides", json!({"device": AP}));
    let o = &r["boot_overrides"];
    assert_eq!(o["supported"], true, "{r}");
    assert_eq!(o["state"], "latched", "{r}");
    assert_eq!(o["asserted"], json!(["MD_EDL"]), "{r}");
    assert_eq!(
        o["overrides"],
        json!({"MD_EDL": 1, "SS_EDL": 0, "UEFI": 0, "FASTBOOT_MD": 0}),
        "every line is reported, held or not: {r}"
    );
    assert_eq!(o["source"], "controller_read", "{r}");
    assert!(
        o["effect"]
            .as_str()
            .unwrap_or_default()
            .contains("power cycles"),
        "and it must say what that MEANS for the next boot: {r}"
    );
}

#[test]
fn a_board_holding_nothing_reads_clear() {
    let rig = Rig::new();
    let r = rig.call("boot_overrides", json!({"target": "ovr"}));
    assert_eq!(r["boot_overrides"]["state"], "clear", "{r}");
    assert_eq!(r["boot_overrides"]["asserted"], json!([]), "{r}");
}

/// A read that failed is unknown, never clear.
///
/// The controller not answering is the one situation in which assuming a normal
/// boot is least justified, and "nothing held" is exactly what a failed read
/// looks like to code that treats absence as zero.
#[test]
fn a_controller_that_does_not_answer_reads_unknown_never_clear() {
    let rig = Rig::new();
    rig.hold("MD_EDL");
    rig.switch("reads_fail", "1");
    let r = rig.call("boot_overrides", json!({"device": AP}));
    let o = &r["boot_overrides"];
    assert_eq!(o["state"], "unknown", "{r}");
    assert_ne!(o["state"], "clear", "{r}");
    assert!(
        o["error"].is_string(),
        "the reason must travel with it: {r}"
    );
    assert!(
        o["effect"]
            .as_str()
            .unwrap_or_default()
            .contains("NOT evidence"),
        "and it must refuse to be read as good news: {r}"
    );
}

#[test]
fn one_unreadable_line_makes_the_whole_reading_unknown() {
    let rig = Rig::new();
    rig.switch("unreadable.UEFI", "1");
    let r = rig.call("boot_overrides", json!({"device": AP}));
    let o = &r["boot_overrides"];
    assert_eq!(o["state"], "unknown", "{r}");
    assert_eq!(o["unknown"], json!(["UEFI"]), "{r}");
    assert_eq!(
        o["overrides"]["UEFI"],
        Value::Null,
        "unknown renders as null, never 0: {r}"
    );
}

#[test]
fn a_board_whose_controller_cannot_be_read_says_unsupported_not_clear() {
    let rig = Rig::without_a_read_hook();
    let r = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(r["boot_overrides"]["supported"], false, "{r}");
    assert_eq!(r["boot_overrides"]["state"], "unsupported", "{r}");
}

/// Status refresh is strictly read-only.
///
/// A status path that could move a line would be a way to knock a board out of
/// the EDL a flash is relying on from a page reload. Asserted on what REACHED the
/// controller: across the status tool and diagnose, with a line held, nothing
/// but queries, and the line still held afterwards.
#[test]
fn reading_status_never_sends_the_controller_anything_but_a_query() {
    let rig = Rig::new();
    rig.hold("MD_EDL");
    rig.hold("UEFI");
    for _ in 0..3 {
        rig.call("boot_overrides", json!({"device": AP, "max_age_s": 0}));
        rig.call("boot_overrides", json!({"target": "ovr", "max_age_s": 0}));
        rig.call("diagnose", json!({"device": AP, "wait_ms": 100}));
        rig.clock.advance_ms(60_000);
    }
    let log = rig.log();
    assert!(
        !log.is_empty(),
        "precondition: the controller really was read"
    );
    assert!(
        log.iter().all(|l| l == "boot-overrides"),
        "a status read sent the controller something other than a query: {log:?}"
    );
    assert!(
        rig.held("MD_EDL") && rig.held("UEFI"),
        "and the lines are still held"
    );
}

/// No lease, because it changes nothing: an operator looking at a board must
/// not have to take it from the agent that is flashing it.
#[test]
fn reading_status_needs_no_lease() {
    let rig = Rig::new();
    rig.leased_by_someone_else();
    // Precondition: the lease really does belong to somebody else, or "needs no
    // lease" would pass simply by holding it.
    let refused = rig.err("boot_mode", json!({"device": AP, "mode": "clear"}));
    assert_eq!(refused["code"], "LEASE_HELD", "{refused}");
    let r = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(r["boot_overrides"]["supported"], true, "{r}");
}

/// A reading is shared inside its window and always says how old it is.
///
/// Every dashboard on the fleet asks for this on a timer and a read holds a
/// single-session controller for seconds, so askers may accept some age. What
/// they may not get is a cached reading dressed as a fresh one.
#[test]
fn a_cached_reading_is_labelled_and_a_forced_one_reads_again() {
    let rig = Rig::new();
    let first = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(first["boot_overrides"]["source"], "controller_read");

    rig.clock.advance_ms(7_000);
    let second = rig.call("boot_overrides", json!({"device": SM}));
    assert_eq!(
        second["boot_overrides"]["source"], "cached_controller_read",
        "the board's other console shares its controller, so it shares the reading: {second}"
    );
    // The step clock ticks on every read, so "about seven seconds", stated.
    let age = second["boot_overrides"]["age_ms"].as_i64().unwrap_or(-1);
    assert!(
        (7_000..7_200).contains(&age),
        "the reading must say how old it is, truthfully: {second}"
    );
    assert_eq!(rig.log().len(), 1, "one real read so far: {:?}", rig.log());

    let forced = rig.call("boot_overrides", json!({"device": AP, "max_age_s": 0}));
    assert_eq!(forced["boot_overrides"]["source"], "controller_read");
    assert_eq!(rig.log().len(), 2);

    // Past the window, the default asks the controller again.
    rig.clock.advance_ms(25_000);
    let late = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(
        late["boot_overrides"]["source"], "controller_read",
        "{late}"
    );
}

/// A change made THROUGH conminer is never hidden behind the cache.
#[test]
fn setting_a_mode_replaces_the_shared_reading_at_once() {
    let rig = Rig::new();
    rig.lease();
    let before = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(before["boot_overrides"]["state"], "clear");

    let set = rig.call("boot_mode", json!({"target": "ovr", "mode": "BOOT_MD_EDL"}));
    assert_eq!(
        set["boot_overrides"]["asserted"],
        json!(["MD_EDL"]),
        "the response carries a readback taken AFTER the change: {set}"
    );
    // Well inside the window the earlier "clear" would still have been served.
    let after = rig.call("boot_overrides", json!({"device": AP}));
    assert_eq!(
        after["boot_overrides"]["state"], "latched",
        "a cached `clear` from before the change must not survive it: {after}"
    );
}

/// Selecting a mode asserts its own line and releases nothing.
///
/// Nothing about the call suggests that, and an EDL line decides the boot in
/// ROM before UEFI or fastboot ever run: BOOT_UEFI on a board still holding
/// MD_EDL boots into EDL and reads as the mode not working.
#[test]
fn a_second_mode_on_top_of_a_held_one_is_called_out() {
    let rig = Rig::new();
    rig.lease();
    rig.call("boot_mode", json!({"target": "ovr", "mode": "BOOT_MD_EDL"}));
    let second = rig.call("boot_mode", json!({"target": "ovr", "mode": "BOOT_UEFI"}));
    assert_eq!(
        second["boot_overrides"]["asserted"],
        json!(["MD_EDL", "UEFI"]),
        "{second}"
    );
    let warning = second["warning"].as_str().unwrap_or_default();
    assert!(
        warning.contains("MD_EDL")
            && warning.contains("UEFI")
            && warning.contains("releases nothing"),
        "two held lines must be named, with why: {second}"
    );
}

/// diagnose reports the controller's intent BESIDE the USB observation.
#[test]
fn diagnose_names_a_held_override_and_the_way_out() {
    let rig = Rig::new();
    rig.hold("MD_EDL");
    let d = rig.call("diagnose", json!({"device": AP, "wait_ms": 100}));
    assert_eq!(d["boot_overrides"]["asserted"], json!(["MD_EDL"]), "{d}");
    assert!(
        d.get("edl").is_some(),
        "the USB observation stays its own field: {d}"
    );
    let verdict = d["verdict"].as_str().unwrap_or_default();
    assert!(
        verdict.contains("MD_EDL") && verdict.contains("normal_boot"),
        "the verdict is what gets read first, so it must name the line and the exit: {verdict}"
    );
    // ...and says nothing of the kind on a board that holds nothing.
    let clean = Rig::new();
    let d = clean.call("diagnose", json!({"device": AP, "wait_ms": 100}));
    assert!(
        !d["verdict"]
            .as_str()
            .unwrap_or_default()
            .contains("HOLDING"),
        "{d}"
    );
}

// -------------------------------------------------------------- normal boot ---

/// The workflow, in order: release, prove it, and only then cycle.
#[test]
fn a_normal_boot_releases_verifies_then_cycles_in_that_order() {
    let rig = Rig::new();
    rig.lease();
    rig.hold("MD_EDL");
    rig.hold("SS_EDL");
    let r = rig.call("normal_boot", json!({"target": "ovr"}));

    assert_eq!(
        rig.log(),
        vec!["mode clear", "boot-overrides", "power cycle"],
        "release, THEN an independent readback, THEN the cycle, and nothing else"
    );
    assert!(!rig.held("MD_EDL") && !rig.held("SS_EDL"));
    assert_eq!(r["normal_boot"]["boot_overrides"]["state"], "clear", "{r}");
    assert_eq!(
        r["normal_boot"]["boot_overrides"]["source"], "controller_read",
        "the proof must be a read taken after the release, never a cached one: {r}"
    );
    assert!(
        r["boot_id"].is_i64() || r["boot_id"].is_u64(),
        "the cycle opens its epoch: {r}"
    );
    assert!(
        r["effect"].is_object(),
        "and is verified like any other power action: {r}"
    );
}

/// A cached `clear` from BEFORE the release is the wrong evidence entirely.
#[test]
fn the_verification_read_is_never_served_from_the_cache() {
    let rig = Rig::new();
    rig.lease();
    // Warm the cache with a clean reading...
    rig.call("boot_overrides", json!({"device": AP}));
    // ...then a line is latched by something outside conminer, and sticks.
    rig.hold("MD_EDL");
    rig.switch("stuck.MD_EDL", "1");
    let e = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(
        e["code"], "NORMAL_BOOT_ABORTED",
        "a cached `clear` would have waved this straight through to the cycle: {e}"
    );
}

/// Abort before cycling when the release fails.
#[test]
fn a_release_that_fails_aborts_before_any_power_action() {
    let rig = Rig::new();
    rig.lease();
    rig.hold("MD_EDL");
    rig.switch("clear_fails", "1");
    let e = rig.err("normal_boot", json!({"target": "ovr"}));

    assert_eq!(e["code"], "NORMAL_BOOT_ABORTED", "{e}");
    assert_eq!(e["detail"]["step"], "clear_overrides", "{e}");
    assert_eq!(e["detail"]["power_cycled"], false, "{e}");
    assert!(
        !rig.log().iter().any(|l| l.starts_with("power")),
        "NO power action may reach the controller after a failed release: {:?}",
        rig.log()
    );
    assert_eq!(
        e["detail"]["boot_overrides"]["asserted"],
        json!(["MD_EDL"]),
        "and the caller is told what the controller holds NOW: {e}"
    );
    assert!(
        e["message"]
            .as_str()
            .unwrap_or_default()
            .contains("NOT cycled"),
        "{e}"
    );
}

/// Abort before cycling when the release "succeeds" but a line is still held.
///
/// The hook's exit code says the command ran. Only the readback says the line
/// dropped, and a board cycled on the exit code alone goes straight back to EDL
/// with a response that said it would not.
#[test]
fn a_line_that_will_not_release_aborts_before_any_power_action() {
    let rig = Rig::new();
    rig.lease();
    rig.hold("MD_EDL");
    rig.switch("stuck.MD_EDL", "1");
    let e = rig.err("normal_boot", json!({"target": "ovr"}));

    assert_eq!(e["code"], "NORMAL_BOOT_ABORTED", "{e}");
    assert_eq!(e["detail"]["step"], "verify_overrides", "{e}");
    assert_eq!(e["detail"]["power_cycled"], false, "{e}");
    assert_eq!(
        e["detail"]["boot_overrides"]["asserted"],
        json!(["MD_EDL"]),
        "{e}"
    );
    assert_eq!(
        rig.log(),
        vec!["mode clear", "boot-overrides"],
        "the workflow must stop at the readback: {:?}",
        rig.log()
    );
}

/// Abort before cycling when the readback itself fails: unknown is not clear.
#[test]
fn a_readback_that_fails_aborts_before_any_power_action() {
    let rig = Rig::new();
    rig.lease();
    rig.switch("reads_fail", "1");
    let e = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(e["code"], "NORMAL_BOOT_ABORTED", "{e}");
    assert_eq!(e["detail"]["step"], "verify_overrides", "{e}");
    assert_eq!(e["detail"]["boot_overrides"]["state"], "unknown", "{e}");
    assert!(
        !rig.log().iter().any(|l| l.starts_with("power")),
        "{:?}",
        rig.log()
    );
}

#[test]
fn one_unreadable_line_is_enough_to_abort() {
    let rig = Rig::new();
    rig.lease();
    rig.switch("unreadable.FASTBOOT_MD", "1");
    let e = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(e["code"], "NORMAL_BOOT_ABORTED", "{e}");
    assert!(
        !rig.log().iter().any(|l| l.starts_with("power")),
        "{:?}",
        rig.log()
    );
}

/// An abort leaves the board FREE and says so in actuation_status.
#[test]
fn an_aborted_normal_boot_releases_its_claim_and_records_why() {
    let rig = Rig::new();
    rig.lease();
    rig.switch("clear_fails", "1");
    rig.err("normal_boot", json!({"target": "ovr"}));

    let st = rig.call("actuation_status", json!({"target": "ovr"}));
    assert!(
        st["in_flight"].is_null(),
        "an abort must not strand the claim: {st}"
    );
    assert_eq!(st["last"]["tool"], "normal_boot", "{st}");
    assert_eq!(st["last"]["aborted_at"], "clear_overrides", "{st}");
    assert_eq!(st["last"]["power_cycled"], false, "{st}");

    // ...and the very next actuation goes through.
    std::fs::remove_file(rig.ctl.join("clear_fails")).unwrap();
    rig.call("normal_boot", json!({"target": "ovr"}));
}

/// A latching controller that cannot be read back cannot be VERIFIED, and this
/// tool's whole promise is that it verifies.
#[test]
fn a_latching_controller_with_no_readback_is_refused_outright() {
    let rig = Rig::without_a_read_hook();
    rig.lease();
    let e = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(e["code"], "HOOK_NOT_CONFIGURED", "{e}");
    assert!(
        rig.log().is_empty(),
        "nothing may reach the controller: {:?}",
        rig.log()
    );
}

// ------------------------------------------------------ leases, concurrency ---

#[test]
fn a_normal_boot_needs_the_lease_like_any_actuation() {
    let rig = Rig::new();
    let e = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(e["code"], "LEASE_REQUIRED", "{e}");
    assert!(
        rig.log().is_empty(),
        "nothing may reach the controller: {:?}",
        rig.log()
    );

    rig.leased_by_someone_else();
    let held_by_other = rig.err("normal_boot", json!({"target": "ovr"}));
    assert_eq!(held_by_other["code"], "LEASE_REQUIRED", "{held_by_other}");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

/// A dry run plans: three commands, in order, and nothing run.
#[test]
fn a_dry_run_shows_the_three_steps_and_touches_nothing() {
    let rig = Rig::new();
    rig.hold("MD_EDL");
    let r = rig.call("normal_boot", json!({"target": "ovr", "dry_run": true}));
    let steps: Vec<&str> = r["sequence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["step"].as_str())
        .collect();
    assert_eq!(
        steps,
        vec!["clear_overrides", "verify_overrides", "power_cycle"],
        "{r}"
    );
    assert!(
        rig.log().is_empty(),
        "a dry run must not reach the controller: {:?}",
        rig.log()
    );
    assert!(rig.held("MD_EDL"));
}

/// One claim for the whole workflow.
///
/// Two separate calls would leave a window between the release and the cycle in
/// which another caller could latch a line again, and the cycle would then boot
/// the board into it having "verified" it clear.
#[test]
fn nothing_else_can_actuate_the_board_while_a_normal_boot_runs() {
    let rig = Rig::new();
    rig.lease();
    rig.hold("MD_EDL");
    // Stretch the readback so the workflow is observably mid-flight.
    rig.switch("reads_slow", "2");

    std::thread::scope(|s| {
        let first = s.spawn(|| rig.raw("normal_boot", json!({"target": "ovr"})));
        std::thread::sleep(std::time::Duration::from_millis(700));

        for (tool, args) in [
            ("boot_mode", json!({"target": "ovr", "mode": "BOOT_MD_EDL"})),
            ("power", json!({"target": "ovr", "action": "off"})),
            ("normal_boot", json!({"target": "ovr"})),
            ("boot_mode", json!({"device": SM, "mode": "clear"})),
        ] {
            let e = rig.err(tool, args.clone());
            assert_eq!(
                e["code"], "ACTUATION_IN_FLIGHT",
                "{tool} {args} must be refused while the normal boot runs: {e}"
            );
            assert_eq!(
                e["detail"]["tool"], "normal_boot",
                "and say what is running: {e}"
            );
        }
        let done = first.join().unwrap();
        assert_eq!(done["isError"], false, "{done}");
    });

    assert_eq!(
        rig.log(),
        vec!["mode clear", "boot-overrides", "power cycle"],
        "the refused calls must not have reached the controller at all"
    );
    assert!(
        !rig.held("MD_EDL"),
        "and the line the intruder tried to latch is not held"
    );
}

/// A normal boot is refused while something else holds the board, too.
#[test]
fn a_normal_boot_is_refused_while_another_actuation_runs() {
    let rig = Rig::new();
    rig.lease();
    rig.switch("power_slow", "2");
    std::thread::scope(|s| {
        let first = s.spawn(|| rig.raw("power", json!({"target": "ovr", "action": "off"})));
        std::thread::sleep(std::time::Duration::from_millis(600));
        let e = rig.err("normal_boot", json!({"target": "ovr"}));
        assert_eq!(e["code"], "ACTUATION_IN_FLIGHT", "{e}");
        first.join().unwrap();
    });
    assert!(
        !rig.log().iter().any(|l| l == "mode clear"),
        "a refused normal boot must not have released anything: {:?}",
        rig.log()
    );
}

// ------------------------------------------------- what must NOT have changed ---

/// A generic power action releases nothing. A flash depends on it.
///
/// EDL latching is deliberate: the board has to come back into EDL after every
/// reset for as long as the flash needs. A `power cycle` that quietly released
/// the line would be a kindness to the person who forgot it and a broken flash
/// for everyone else.
#[test]
fn a_plain_power_action_never_releases_a_held_override() {
    let rig = Rig::new();
    rig.lease();
    rig.call("boot_mode", json!({"target": "ovr", "mode": "BOOT_MD_EDL"}));
    rig.forget_log();
    for action in ["cycle", "reset", "off", "on"] {
        rig.call("power", json!({"target": "ovr", "action": action}));
    }
    assert!(
        rig.held("MD_EDL"),
        "the EDL line must survive every power action"
    );
    assert!(
        !rig.log().iter().any(|l| l.starts_with("mode")),
        "a power action sent the controller a mode command: {:?}",
        rig.log()
    );
}

/// `boot_mode clear` still works on its own, and proves itself.
#[test]
fn clear_on_its_own_releases_and_reports_the_readback() {
    let rig = Rig::new();
    rig.lease();
    rig.hold("MD_EDL");
    let r = rig.call("boot_mode", json!({"target": "ovr", "mode": "clear"}));
    assert_eq!(r["boot_overrides"]["state"], "clear", "{r}");
    assert!(r.get("warning").is_none(), "{r}");
    assert!(
        !rig.log().iter().any(|l| l.starts_with("power")),
        "clear is not a power action: {:?}",
        rig.log()
    );

    // ...and says so when the release did not take.
    rig.hold("SS_EDL");
    rig.switch("stuck.SS_EDL", "1");
    let r = rig.call("boot_mode", json!({"target": "ovr", "mode": "clear"}));
    assert!(
        r["warning"].as_str().unwrap_or_default().contains("SS_EDL"),
        "a release that left a line held must say which: {r}"
    );
}

// ------------------------------------------------ the real hook, on a real tty ---

/// A Bantam, as far as `bantam-power` can tell: a line-oriented command
/// processor on a tty that answers `NAME ?` with the line's level, applies
/// `NAME 0|1` silently, and here also RECORDS every line it is sent.
///
/// A pty rather than a pipe, because the hook configures the port with `stty`
/// and reads it with a bounded `cat`, and neither means anything on a pipe.
struct FakeBantam {
    pty: Arc<conminer_testkit::pty::Pty>,
    lines: Arc<std::sync::Mutex<Vec<String>>>,
    levels: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    _lock_dir: tempfile::TempDir,
    lock: std::path::PathBuf,
}

impl FakeBantam {
    fn start(initial: &[(&str, &str)]) -> Option<Self> {
        let pty = Arc::new(conminer_testkit::pty::Pty::open().ok()?);
        let fd = pty.master_fd();
        // SAFETY: fd is the live master owned by `pty`, which outlives the thread
        // through the Arc the thread holds.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let levels: std::collections::HashMap<String, String> = initial
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let levels = Arc::new(std::sync::Mutex::new(levels));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (p, l, lv, st) = (pty.clone(), lines.clone(), levels.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut pending = Vec::new();
            let mut buf = [0u8; 256];
            while !st.load(std::sync::atomic::Ordering::Relaxed) {
                // SAFETY: a plain read into a stack buffer of the stated length.
                let n = unsafe {
                    libc::read(
                        p.master_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    // EAGAIN, or EIO while no process holds the slave open
                    // between the hook's commands. Both mean "nothing yet".
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                pending.extend_from_slice(&buf[..n as usize]);
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&raw).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    l.lock().unwrap().push(line.clone());
                    let mut words = line.split_whitespace();
                    let (Some(name), Some(arg)) = (words.next(), words.next()) else {
                        continue;
                    };
                    if arg == "?" {
                        let v = lv.lock().unwrap().get(name).cloned().unwrap_or_default();
                        let _ = p.write_all(format!("{v}\r\n").as_bytes());
                    } else {
                        lv.lock().unwrap().insert(name.to_string(), arg.to_string());
                    }
                }
            }
        });
        let lock_dir = tempfile::tempdir().ok()?;
        let lock = lock_dir.path().join("bantam.lock");
        Some(Self {
            pty,
            lines,
            levels,
            stop,
            _lock_dir: lock_dir,
            lock,
        })
    }

    /// Run the shipped hook against this controller.
    fn run(&self, args: &[&str]) -> std::process::Output {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/bantam-power");
        std::process::Command::new("sh")
            .arg(script)
            .args(args)
            .arg("--port")
            .arg(self.pty.slave_path())
            .env("BANTAM_LOCK", &self.lock)
            .output()
            .expect("run bantam-power")
    }

    fn sent(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn level(&self, name: &str) -> String {
        self.levels
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
}

impl Drop for FakeBantam {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The shipped hook's status read is a read: every line it sends is a query.
///
/// Asserted on the controller's side of the wire. A status path that could move
/// a line would let a page reload knock a board out of the EDL a flash is
/// relying on, and a test that only inspected the hook's OUTPUT could not see it.
#[test]
fn the_shipped_hook_reads_the_overrides_without_sending_a_single_set() {
    let Some(bantam) = FakeBantam::start(&[
        ("MD_EDL", "1"),
        ("SS_EDL", "0"),
        ("UEFI", "0"),
        ("FASTBOOT_MD", "0"),
    ]) else {
        eprintln!("SKIP: no pty in this environment");
        return;
    };
    let out = bantam.run(&["boot-overrides"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let parsed = conminer_core::overrides::BootOverrides::parse(&stdout);
    assert_eq!(
        parsed.asserted(),
        vec!["MD_EDL"],
        "what the hook printed: {stdout}"
    );
    assert_eq!(
        parsed.signals.len(),
        4,
        "every override is reported: {stdout}"
    );

    let sent = bantam.sent();
    assert_eq!(
        sent.len(),
        4,
        "one query per override, nothing else: {sent:?}"
    );
    assert!(
        sent.iter().all(|l| l.ends_with(" ?")),
        "the status read sent the controller something that is not a query: {sent:?}"
    );
    assert_eq!(
        bantam.level("MD_EDL"),
        "1",
        "and the held line is still held"
    );
}

/// A controller that answers with something other than 0 or 1 has not said the
/// line is released, and the hook must not say it for it.
#[test]
fn the_shipped_hook_reports_an_unclean_answer_as_unknown() {
    let Some(bantam) = FakeBantam::start(&[
        ("MD_EDL", "ERR"),
        ("SS_EDL", "0"),
        // UEFI missing entirely: the controller answers with an empty line.
        ("FASTBOOT_MD", "0"),
    ]) else {
        eprintln!("SKIP: no pty in this environment");
        return;
    };
    let out = bantam.run(&["boot-overrides"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed = conminer_core::overrides::BootOverrides::parse(&stdout);
    assert_eq!(parsed.unknown(), vec!["MD_EDL", "UEFI"], "{stdout}");
    assert!(
        !parsed.all_released(),
        "an unclean read must never pass the normal-boot gate"
    );
}

/// The release and the read agree on what "the overrides" are.
///
/// They used to be two literal lists in one script. A release that dropped three
/// lines while the read reported four would verify a board as normal with one
/// line still held, so both now come from a single list, and this checks the
/// release against the read on the wire.
#[test]
fn the_shipped_hook_releases_exactly_the_lines_it_reports() {
    let Some(bantam) = FakeBantam::start(&[
        ("MD_EDL", "1"),
        ("SS_EDL", "1"),
        ("UEFI", "1"),
        ("FASTBOOT_MD", "1"),
        ("PWR_OFF", "0"),
    ]) else {
        eprintln!("SKIP: no pty in this environment");
        return;
    };
    let out = bantam.run(&["mode", "clear"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let read = bantam.run(&["boot-overrides"]);
    let parsed =
        conminer_core::overrides::BootOverrides::parse(&String::from_utf8_lossy(&read.stdout));
    assert!(
        parsed.all_released(),
        "after a release every REPORTED line reads released"
    );

    let released: std::collections::BTreeSet<String> = bantam
        .sent()
        .iter()
        .filter(|l| l.ends_with(" 0"))
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_string())
        .collect();
    let reported: std::collections::BTreeSet<String> =
        parsed.signals.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(
        released, reported,
        "the release and the read must cover the same lines"
    );
    assert_eq!(
        bantam.level("PWR_OFF"),
        "0",
        "and a release touches nothing but overrides"
    );
}

/// The mode table is true. For every mode the shipped profile offers, set it
/// with the shipped hook and read it back with the shipped hook: a mode the
/// hook says holds a line must be the mode that lit it, and a mode it does not
/// mention must have held nothing.
///
/// This is what lets a dashboard light a button from the read alone. A table
/// that named the wrong mode would light UEFI on a board held in EDL.
#[test]
fn the_shipped_hook_names_the_mode_that_holds_each_line() {
    use conminer_core::overrides::{BootOverrides, Level};
    let modes = Config::default()
        .controllers
        .iter()
        .find(|c| c.name == "bantam")
        .expect("the shipped bantam profile")
        .boot_modes
        .clone();
    assert!(modes.len() >= 4, "{modes:?}");

    let results: Vec<Option<(String, BootOverrides)>> = std::thread::scope(|s| {
        let handles: Vec<_> = modes
            .iter()
            .map(|mode| {
                s.spawn(move || {
                    let bantam = FakeBantam::start(&[
                        ("MD_EDL", "0"),
                        ("SS_EDL", "0"),
                        ("UEFI", "0"),
                        ("FASTBOOT_MD", "0"),
                    ])?;
                    let set = bantam.run(&["mode", mode]);
                    assert!(
                        set.status.success(),
                        "{mode}: {}",
                        String::from_utf8_lossy(&set.stderr)
                    );
                    let read = bantam.run(&["boot-overrides"]);
                    Some((
                        mode.clone(),
                        BootOverrides::parse(&String::from_utf8_lossy(&read.stdout)),
                    ))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut declared = 0;
    for result in results {
        let Some((mode, read)) = result else {
            eprintln!("SKIP: no pty in this environment");
            return;
        };
        // WHICH line: the controller's firmware sequence of the same name
        // leaves the line at 0 and the board boots normally, so each mode has
        // to drive this strap and no other.
        let wired = [
            ("BOOT_MD_EDL", "MD_EDL"),
            ("BOOT_SS_EDL", "SS_EDL"),
            ("BOOT_UEFI", "UEFI"),
            ("MD_FASTBOOT", "FASTBOOT_MD"),
        ];
        match read.mode_level(&mode) {
            Some(level) => {
                declared += 1;
                assert_eq!(level, Level::Asserted, "{mode} was set: {read:?}");
                let line = wired
                    .iter()
                    .find(|(m, _)| *m == mode)
                    .map(|(_, l)| *l)
                    .unwrap_or_else(|| panic!("{mode} latches a line nobody has measured"));
                assert_eq!(
                    read.asserted(),
                    vec![line],
                    "{mode} holds exactly its own line: {read:?}"
                );
                for (other, _) in read.modes.iter().filter(|(m, _)| *m != mode) {
                    assert_eq!(
                        read.mode_level(other),
                        Some(Level::Released),
                        "setting {mode} must not light {other}: {read:?}"
                    );
                }
            }
            None => assert!(
                read.asserted().is_empty(),
                "{mode} held a line the hook's table does not admit to: {read:?}"
            ),
        }
    }
    assert_eq!(declared, 4, "the four latching modes are all declared");
}

/// The shipped release lets go of ONE line, and refuses a mode that holds none
/// without touching anything.
#[test]
fn the_shipped_hook_releases_one_mode_and_only_its_line() {
    let Some(bantam) = FakeBantam::start(&[
        ("MD_EDL", "1"),
        ("SS_EDL", "1"),
        ("UEFI", "1"),
        ("FASTBOOT_MD", "1"),
    ]) else {
        eprintln!("SKIP: no pty in this environment");
        return;
    };
    let out = bantam.run(&["mode-release", "BOOT_UEFI"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sets: Vec<String> = bantam
        .sent()
        .into_iter()
        .filter(|l| !l.ends_with(" ?"))
        .collect();
    assert_eq!(sets, vec!["UEFI 0"], "one line driven, and driven low");
    for held in ["MD_EDL", "SS_EDL", "FASTBOOT_MD"] {
        assert_eq!(bantam.level(held), "1", "{held} must not move");
    }

    let before = bantam.sent().len();
    for bad in [
        &["mode-release", "SS_MD_FASTBOOT"][..],
        &["mode-release"][..],
    ] {
        let out = bantam.run(bad);
        assert!(!out.status.success(), "{bad:?} must be refused");
    }
    assert_eq!(
        bantam.sent().len(),
        before,
        "a refused release sent the controller something: {:?}",
        bantam.sent()
    );
    assert_eq!(bantam.level("MD_EDL"), "1");
}
