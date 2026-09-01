//! External command hooks (§15.2, §15.3): power, flash, reset.
//!
//! conminer is not a flashing tool and not a PDU driver. Invoking `fastboot`,
//! `qdl`, `labgrid power cycle` or a rack PDU is the lab's business — what
//! conminer owns is the *epoch machinery* around it: a hook invocation opens a
//! boot epoch, is recorded inside it, and binds image identity to the boots that
//! follow. That is what makes "first boot after flash" answerable.
//!
//! A hook that times out is a structured error, never a hang.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    On,
    Off,
    Cycle,
    /// Pulse the reset line. Universal enough to belong here rather than in a
    /// rig-specific hook: every board has one, and it is the action a bring-up
    /// loop reaches for most.
    Reset,
}

impl PowerAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerAction::On => "on",
            PowerAction::Off => "off",
            PowerAction::Cycle => "cycle",
            PowerAction::Reset => "reset",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "on" => PowerAction::On,
            "off" => PowerAction::Off,
            "cycle" => PowerAction::Cycle,
            "reset" => PowerAction::Reset,
            other => {
                return Err(ToolError::invalid_arg(format!(
                    "power action must be on|off|cycle|reset, got {other:?}"
                )))
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// Substitute `{action}` / `{image}` / `{device}` into a hook template.
///
/// Values are passed as separate argv entries rather than interpolated into a
/// shell string, so an image name containing a space or a semicolon is data, not
/// syntax.
pub fn render(template: &str, subs: &[(&str, &str)]) -> Vec<String> {
    shell_split(template)
        .into_iter()
        .map(|tok| {
            let mut t = tok;
            for (k, v) in subs {
                t = t.replace(&format!("{{{k}}}"), v);
            }
            t
        })
        .collect()
}

/// Split on whitespace, honouring single and double quotes.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match c {
            '\'' | '"' if quote.is_none() => quote = Some(c),
            c if Some(c) == quote => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// ONE HOOK AT A TIME PER CONTROLLER.
///
/// A board controller is a single-session device: a Bantam, a TAC, a Bughopper
/// all refuse a second opener, and `bantam-power` says so out loud --
/// "another action is already driving <port>", exit 1. conminer already routes
/// every actuation through mcpd so there is ONE path to the board, but one path
/// is not the same as one at a time: the dashboards probe power on a five-second
/// timer, and on a three-node fleet all three probes plus any actuation land on
/// the owner's mcpd. Measured during the hardware matrix: two collisions in ~150
/// actuations, each surfacing as a hard HOOK_FAILED on a board that was fine.
///
/// Keyed on the CONTROLLER, not the console: every console of a board resolves
/// the same controller, and two consoles of one board must not be actuated in
/// parallel either. Falls back to the device when a hook names no controller,
/// which keeps controller-less hooks independent of each other.
/// The last answer a READ-ONLY probe got from this controller, and when.
///
/// `None` after any actuation: a reading taken before the board was touched says
/// nothing about it afterwards, and serving one across that boundary is exactly
/// the "power state must be exact" rule breaking quietly.
type Controller = tokio::sync::Mutex<Option<(std::time::Instant, HookResult)>>;

fn controller_lock(key: &str) -> std::sync::Arc<Controller> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Controller>>>,
    > = std::sync::OnceLock::new();
    let map = LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(None)))
        .clone()
}

/// The lock key a set of substitutions implies: the controller, else the device.
fn lock_key(subs: &[(&str, &str)]) -> Option<String> {
    subs.iter()
        .find(|(k, v)| *k == "controller" && !v.is_empty())
        .or_else(|| subs.iter().find(|(k, v)| *k == "device" && !v.is_empty()))
        .map(|(_, v)| v.to_string())
}

/// Run a READ-ONLY probe, sharing an answer that arrived while we queued.
///
/// Every dashboard on the fleet asks the owner for each board's power every five
/// seconds, and they all funnel through the owner's mcpd onto one single-session
/// controller. Serialising them (which correctness demands) turned that into a
/// queue: with three nodes the controller was busy most of the time, and an
/// actuation waited behind probes that were all asking the same question.
///
/// So a caller that finds an answer produced AFTER IT ASKED takes that one
/// instead of running the hook again. That is not a cache: nothing is served
/// that predates the request, so no caller can be told about a board as it was
/// before it enquired. N concurrent probes cost one hook run; a probe that
/// arrives later still gets its own.
pub async fn probe(template: &str, subs: &[(&str, &str)], timeout: Duration) -> Result<HookResult> {
    let arrived = std::time::Instant::now();
    let Some(key) = lock_key(subs) else {
        return run(template, subs, timeout).await;
    };
    let cell = controller_lock(&key);
    let mut slot = cell.lock().await;
    if let Some((done_at, result)) = slot.as_ref() {
        if *done_at >= arrived {
            return Ok(result.clone());
        }
    }
    let result = spawn_hook(template, subs, timeout).await?;
    *slot = Some((std::time::Instant::now(), result.clone()));
    Ok(result)
}

/// Run a hook with a deadline.
pub async fn run(template: &str, subs: &[(&str, &str)], timeout: Duration) -> Result<HookResult> {
    // Serialised per controller (see `controller_lock`). Derived from the
    // substitutions rather than passed in, so every call site -- actuation,
    // boot mode, the power-state probe, the escalation -- is covered without
    // each having to remember.
    let key = lock_key(subs);
    let _guard = match &key {
        Some(k) => {
            let cell = controller_lock(k);
            let mut slot = cell.clone().lock_owned().await;
            // Belt and braces. The exactness guarantee is the arrival-time rule
            // in `probe` -- nothing older than the request is ever served, so a
            // caller arriving after this actuation cannot be told what the board
            // looked like before it. Clearing here costs nothing and keeps that
            // guarantee if anyone later adds a time-window cache, which would
            // otherwise reintroduce exactly the bug the rule prevents.
            *slot = None;
            Some(slot)
        }
        None => None,
    };
    spawn_hook(template, subs, timeout).await
}

/// Spawn the hook itself. Callers hold the controller lock.
async fn spawn_hook(
    template: &str,
    subs: &[(&str, &str)],
    timeout: Duration,
) -> Result<HookResult> {
    let argv = render(template, subs);
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| ToolError::new(ErrorCode::HookNotConfigured, "hook command is empty"))?;

    let started = std::time::Instant::now();
    let child = tokio::process::Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            ToolError::new(
                ErrorCode::HookFailed,
                format!("cannot run hook {prog:?}: {e}"),
            )
            .with_detail(json!({"command": argv}))
        })?;

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => {
            return Err(ToolError::new(
                ErrorCode::HookTimeout,
                format!("hook {prog:?} did not finish within {:?}", timeout),
            )
            .with_hint("raise hooks.*_timeout_s, or fix the hook")
            .with_detail(json!({"command": argv})))
        }
        Ok(Err(e)) => {
            return Err(ToolError::new(
                ErrorCode::HookFailed,
                format!("hook {prog:?} failed: {e}"),
            ))
        }
        Ok(Ok(o)) => o,
    };

    let result = HookResult {
        command: argv.join(" "),
        exit_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        duration_ms: started.elapsed().as_millis() as u64,
    };

    if !out.status.success() {
        // A failing hook must not open an epoch: an epoch that never happened
        // would make the next `boot_report` describe a boot nobody triggered.
        return Err(ToolError::new(
            ErrorCode::HookFailed,
            format!("hook exited with {:?}", result.exit_code),
        )
        .with_detail(json!({"hook": result})));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    /// TWO HOOKS FOR ONE CONTROLLER MUST NOT OVERLAP.
    ///
    /// Asserted by RUNNING them, not by reading the source: a first version of
    /// this gate checked that the locking code was present, and went on passing
    /// when that code was bypassed. A board controller is single-session, so the
    /// only thing that matters is whether two processes are ever inside it at
    /// the same moment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_hooks_for_one_controller_never_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hook.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'in %s\\n' \"$(date +%s%N)\" >> \"$1\"\nsleep 1\nprintf 'out %s\\n' \"$(date +%s%N)\" >> \"$1\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.path().join("log");
        let tmpl = format!("{} {}", script.display(), log.display());

        let one = super::run(
            &tmpl,
            &[("controller", "/dev/ctlA")],
            super::Duration::from_secs(20),
        );
        let two = super::run(
            &tmpl,
            &[("controller", "/dev/ctlA")],
            super::Duration::from_secs(20),
        );
        let (a, b) = tokio::join!(one, two);
        a.expect("first hook");
        b.expect("second hook");

        let text = std::fs::read_to_string(&log).unwrap();
        let marks: Vec<&str> = text.lines().collect();
        assert_eq!(marks.len(), 4, "both hooks must have run: {text}");
        // Serialised means the file reads in / out / in / out. Overlap would
        // interleave as in / in / out / out.
        let kinds: Vec<&str> = marks
            .iter()
            .map(|l| l.split_whitespace().next().unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec!["in", "out", "in", "out"],
            "two hooks drove one controller at the same time: {text}"
        );
    }

    /// CONCURRENT PROBES COST ONE HOOK RUN.
    ///
    /// Every dashboard asks the owner for each board's power every five seconds,
    /// all through one single-session controller. Serialising them is required
    /// for correctness and turned them into a queue; sharing the answer that
    /// arrives while you wait removes the queue without inventing anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_probes_share_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("probe.sh");
        // Counts its own invocations, and answers with the count.
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'x' >> \"$1.n\"\nsleep 1\nwc -c < \"$1.n\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.path().join("p");
        let tmpl = format!("{} {}", script.display(), log.display());
        let subs = [("controller", "/dev/ctlP")];
        let (a, b, c) = tokio::join!(
            super::probe(&tmpl, &subs, super::Duration::from_secs(20)),
            super::probe(&tmpl, &subs, super::Duration::from_secs(20)),
            super::probe(&tmpl, &subs, super::Duration::from_secs(20)),
        );
        a.expect("a");
        b.expect("b");
        c.expect("c");
        let runs = std::fs::read_to_string(format!("{}.n", log.display()))
            .map(|t| t.len())
            .unwrap_or(0);
        assert_eq!(
            runs, 1,
            "three simultaneous probes must cost one hook run, not {runs}"
        );
    }

    /// A PROBE AFTER AN ACTUATION ASKS THE HARDWARE AGAIN.
    ///
    /// A reading taken before the board was touched says nothing about it
    /// afterwards. This holds by the arrival-time rule alone -- the stored
    /// answer predates the later probe's request, so it is never served -- which
    /// is why it survives removing the explicit invalidation. That is the point:
    /// the guarantee rests on one rule, not on remembering to clear a cache.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_after_an_actuation_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("probe.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'x' >> \"$1.n\"\nwc -c < \"$1.n\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.path().join("q");
        let tmpl = format!("{} {}", script.display(), log.display());
        let subs = [("controller", "/dev/ctlQ")];
        super::probe(&tmpl, &subs, super::Duration::from_secs(20))
            .await
            .unwrap();
        // Something drives the board.
        super::run(&tmpl, &subs, super::Duration::from_secs(20))
            .await
            .unwrap();
        // …so the next probe must ask the hardware again, not reuse anything.
        super::probe(&tmpl, &subs, super::Duration::from_secs(20))
            .await
            .unwrap();
        let runs = std::fs::read_to_string(format!("{}.n", log.display()))
            .map(|t| t.len())
            .unwrap_or(0);
        assert_eq!(
            runs, 3,
            "probe, actuate, probe must be three real runs, not {runs}"
        );
    }

    /// …but two DIFFERENT controllers must NOT share a lock.
    ///
    /// A global lock would serialise the whole bench: powering one board would
    /// wait behind an unrelated board's power probe. Asserted on the lock's
    /// IDENTITY rather than on whether two processes happened to overlap --
    /// a timing race is exactly the kind of test that fails on a busy machine
    /// and teaches everyone to re-run it.
    #[test]
    fn each_controller_gets_its_own_lock() {
        let a1 = super::controller_lock("/dev/ctlA");
        let a2 = super::controller_lock("/dev/ctlA");
        let b = super::controller_lock("/dev/ctlB");
        assert!(
            std::sync::Arc::ptr_eq(&a1, &a2),
            "one controller must map to one lock, or two callers never see each other"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&a1, &b),
            "two controllers must map to different locks, or one board waits on another"
        );
    }

    use super::*;

    #[test]
    fn substitution_keeps_values_as_separate_argv_entries() {
        let argv = render("pdu-ctl {action} --outlet 4", &[("action", "cycle")]);
        assert_eq!(argv, ["pdu-ctl", "cycle", "--outlet", "4"]);
    }

    #[test]
    fn a_hostile_image_name_is_data_not_shell_syntax() {
        let argv = render("flash.sh {image}", &[("image", "a b; rm -rf /")]);
        assert_eq!(argv, ["flash.sh", "a b; rm -rf /"]);
    }

    #[test]
    fn quoted_arguments_survive_splitting() {
        assert_eq!(
            shell_split("sh -c 'echo hello world'"),
            ["sh", "-c", "echo hello world"]
        );
    }

    #[tokio::test]
    async fn a_successful_hook_reports_its_output() {
        let r = run("echo powered-on", &[], Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert!(r.stdout.contains("powered-on"));
    }

    #[tokio::test]
    async fn a_failing_hook_is_a_structured_error_carrying_its_output() {
        let err = run("sh -c 'echo nope >&2; exit 3'", &[], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::HookFailed);
        let hook = &err.detail.unwrap()["hook"];
        assert_eq!(hook["exit_code"], 3);
        assert!(hook["stderr"].as_str().unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn a_hook_that_hangs_times_out_rather_than_blocking_the_lab() {
        let err = run("sleep 30", &[], Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::HookTimeout);
        assert!(err.hint.contains("timeout_s"));
    }

    #[tokio::test]
    async fn a_missing_hook_binary_is_reported_not_swallowed() {
        let err = run("definitely-not-a-real-binary", &[], Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::HookFailed);
    }

    #[test]
    fn power_actions_parse_and_reject_nonsense() {
        assert_eq!(PowerAction::parse("cycle").unwrap(), PowerAction::Cycle);
        assert_eq!(
            PowerAction::parse("explode").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
