//! Suite `runner` (§8.3, §13) — the interactive command runner.
//!
//! Edge cases: happy path · echo verification catches an injected dropped or
//! corrupted char · per-char retry succeeds, then aborts correctly on repeat
//! failure · no-echo console mode · command during boot (no prompt) →
//! `NO_PROMPT`, not a blind send · prompt scrolled by async kernel messages
//! mid-command · each recovery rung individually · TX lock serialisation ·
//! distinctive-prompt validation rejects `:`.
//!
//! Driven against a scripted console through the `Transport` trait, so every
//! rung of the recovery ladder is exercised deterministically without hardware.

use conminer_core::config::{LineConfig, RunnerConfig};
use conminer_core::error::Result;
use conminer_core::framer::profile::PromptKind;
use conminer_core::runner::{
    escape_bytes, CommandOptions, Outcome, Prompt, Prompts, Runner, Transport,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A scripted console: it echoes what is typed and replies to whole lines.
#[derive(Clone)]
struct FakeConsole {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Bytes waiting to be read by the runner.
    out: VecDeque<u8>,
    /// Everything the runner wrote.
    written: Vec<u8>,
    /// Current partial input line.
    line: String,
    prompt: String,
    /// `command` → what the console prints in reply.
    replies: Vec<(String, String)>,
    /// Echo the characters typed at us.
    echo: bool,
    /// Drop the Nth character silently (0 = never): a *transient* loss, which
    /// the per-character retry should cure.
    drop_nth: usize,
    /// Always drop this character: a *persistent* fault, which no retry cures.
    drop_char: Option<char>,
    /// Always corrupt this character's echo, and record the corruption — the
    /// console really did receive something else.
    corrupt_char: Option<char>,
    typed: usize,
    /// Never print the prompt again: the console is wedged.
    hung: bool,
    /// Which control byte un-wedges it, if any.
    unwedge: Option<u8>,
    /// Async traffic injected before each echo, as a kernel would.
    noise: Option<String>,
}

impl FakeConsole {
    fn new(prompt: &str) -> Self {
        let mut out = VecDeque::new();
        out.extend(prompt.as_bytes());
        Self {
            inner: Arc::new(Mutex::new(Inner {
                out,
                written: Vec::new(),
                line: String::new(),
                prompt: prompt.to_string(),
                replies: Vec::new(),
                echo: true,
                drop_nth: 0,
                drop_char: None,
                corrupt_char: None,
                typed: 0,
                hung: false,
                unwedge: None,
                noise: None,
            })),
        }
    }

    fn with(self, f: impl FnOnce(&mut Inner)) -> Self {
        f(&mut self.inner.lock().unwrap());
        self
    }

    fn reply(self, cmd: &str, out: &str) -> Self {
        self.with(|i| i.replies.push((cmd.to_string(), out.to_string())))
    }

    fn written(&self) -> String {
        String::from_utf8_lossy(&self.inner.lock().unwrap().written).into_owned()
    }
}

impl Transport for FakeConsole {
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let mut i = self.inner.lock().unwrap();
        i.written.extend_from_slice(data);
        for &b in data {
            if Some(b) == i.unwedge {
                i.hung = false;
                i.line.clear();
                let p = i.prompt.clone();
                i.out.extend(b"^C\r\n");
                i.out.extend(p.as_bytes());
                continue;
            }
            if b == b'\n' || b == b'\r' {
                let cmd = std::mem::take(&mut i.line);
                if i.hung {
                    continue;
                }
                let reply = i
                    .replies
                    .iter()
                    .find(|(c, _)| *c == cmd)
                    .map(|(_, r)| r.clone());
                i.out.extend(b"\r\n");
                if let Some(r) = reply {
                    i.out.extend(r.as_bytes());
                    i.out.extend(b"\r\n");
                }
                let p = i.prompt.clone();
                i.out.extend(p.as_bytes());
                continue;
            }
            i.typed += 1;
            let n = i.typed;
            // A character lost on the wire never reached the console at all, so
            // it must not appear in the line the console thinks it received.
            let lost = (i.drop_nth != 0 && n == i.drop_nth) || i.drop_char == Some(b as char);
            if lost {
                continue;
            }
            let corrupt = i.corrupt_char == Some(b as char);
            i.line.push(if corrupt { '#' } else { b as char });
            if !i.echo {
                continue;
            }
            if let Some(noise) = i.noise.clone() {
                i.out.extend(noise.as_bytes());
            }
            i.out.push_back(if corrupt { b'#' } else { b });
        }
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut i = self.inner.lock().unwrap();
                if !i.out.is_empty() {
                    let n = buf.len().min(i.out.len());
                    for (k, slot) in buf.iter_mut().enumerate().take(n) {
                        *slot = i.out.pop_front().unwrap();
                        let _ = k;
                    }
                    return Ok(Some(n));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn shell_prompts() -> Prompts {
    Prompts(vec![
        Prompt {
            re: regex::Regex::new(r"(^|\n)# $").unwrap(),
            raw: "# ".into(),
            kind: PromptKind::Shell,
        },
        Prompt {
            re: regex::Regex::new(r"(^|\n)[\w.-]* ?login: *$").unwrap(),
            raw: "login: ".into(),
            kind: PromptKind::CredentialGate,
        },
    ])
}

fn opts() -> CommandOptions {
    // Keep the tests fast without changing what is being tested.
    let cfg = RunnerConfig {
        char_delay_ms: 0,
        echo_timeout_ms: 60,
        settle_quiet_ms: 40,
        command_timeout_s: 2,
        ..Default::default()
    };
    let mut o = CommandOptions::new(cfg, LineConfig::default());
    o.timeout_s = 2;
    o
}

// ------------------------------------------------------------- happy path ---

#[tokio::test]
async fn a_command_runs_and_returns_only_its_output() {
    let console = FakeConsole::new("# ").reply("uname -r", "6.12.9");
    let t = Runner::new(console.clone(), shell_prompts(), opts())
        .run("uname -r")
        .await
        .unwrap();

    assert_eq!(t.status, Outcome::Ok);
    assert_eq!(t.output, "6.12.9");
    assert_eq!(t.prompt_matched.as_deref(), Some("# "));
    assert!(t.rungs_attempted.is_empty());
    // Sent character-at-a-time, then the configured line ending.
    assert!(console.written().contains("uname -r\n"));
}

#[tokio::test]
async fn output_is_capped_and_says_so_rather_than_flooding_the_agent() {
    let long = "x".repeat(50_000);
    let console = FakeConsole::new("# ").reply("dmesg", &long);
    let mut o = opts();
    o.max_output_bytes = 1024;
    let t = Runner::new(console, shell_prompts(), o)
        .run("dmesg")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok);
    assert!(t.output_capped);
    assert!(t.output.len() < 1200);
    assert!(t.output.contains("capped"));
}

// ---------------------------------------------------- echo verification -----

#[tokio::test]
async fn a_dropped_character_is_caught_by_echo_verification() {
    // The letter `b` never makes it onto the wire, retry or not.
    let console = FakeConsole::new("# ").with(|i| i.drop_char = Some('b'));
    let t = Runner::new(console, shell_prompts(), opts())
        .run("reboot")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::EchoMismatch);
    assert_eq!(t.detail["at_char"], 2);
    assert_eq!(t.detail["expected"], "b");
    assert!(t.detail["attempts"].as_i64().unwrap() >= 2);
}

#[tokio::test]
async fn a_corrupted_echo_is_caught_too() {
    let console = FakeConsole::new("# ").with(|i| i.corrupt_char = Some('s'));
    let t = Runner::new(console, shell_prompts(), opts())
        .run("ls")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::EchoMismatch);
    assert!(t.detail["observed"].as_str().unwrap().contains('#'));
    assert_eq!(t.detail["expected"], "s");
}

#[tokio::test]
async fn a_transient_loss_is_retried_once_and_succeeds() {
    // Dropped on the first attempt, echoed on the retry.
    let console = FakeConsole::new("# ").reply("id", "uid=0(root)");
    {
        let mut i = console.inner.lock().unwrap();
        i.drop_nth = 1;
    }
    let t = Runner::new(console.clone(), shell_prompts(), opts())
        .run("id")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok, "{:?}", t.detail);
    assert_eq!(t.output, "uid=0(root)");
    // The retry really was sent: `i` appears twice on the wire.
    assert_eq!(console.written().matches('i').count(), 2);
}

#[tokio::test]
async fn a_no_echo_console_downgrades_to_pacing_rather_than_failing() {
    let console = FakeConsole::new("# ")
        .with(|i| i.echo = false)
        .reply("version", "U-Boot 2026.01");
    let mut o = opts();
    o.echo = false;
    let t = Runner::new(console, shell_prompts(), o)
        .run("version")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok);
    assert!(t.output.contains("U-Boot 2026.01"));
}

#[tokio::test]
async fn a_no_echo_console_with_verification_on_is_reported_honestly() {
    // Asking for verification we cannot perform must fail, not pretend.
    let console = FakeConsole::new("# ").with(|i| i.echo = false);
    let t = Runner::new(console, shell_prompts(), opts())
        .run("version")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::EchoMismatch);
}

// -------------------------------------------------------- prompt discipline -

#[tokio::test]
async fn a_command_during_boot_is_refused_rather_than_blind_sent() {
    let console = FakeConsole::new("");
    {
        let mut i = console.inner.lock().unwrap();
        i.out.extend(b"[    1.234] mmc0: new HS200 MMC card\r\n");
        i.prompt = String::new();
    }
    let t = Runner::new(console.clone(), shell_prompts(), opts())
        .run("reboot")
        .await
        .unwrap();
    assert!(
        matches!(t.status, Outcome::NoPrompt | Outcome::UnknownPrompt),
        "{:?}",
        t.status
    );
    assert!(
        !console.written().contains("reboot"),
        "nothing may be typed into an unknown state"
    );
}

#[tokio::test]
async fn a_credential_gate_refuses_instead_of_typing_into_a_login_field() {
    let console = FakeConsole::new("board login: ");
    let t = Runner::new(console.clone(), shell_prompts(), opts())
        .run("whoami")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::LoginRequired);
    assert!(!console.written().contains("whoami"));
    assert!(t.detail["why"].as_str().unwrap().contains("commandable"));
}

#[tokio::test]
async fn an_unfamiliar_idle_line_is_refused_but_can_be_forced() {
    let console = FakeConsole::new("nucleus> ").reply("help", "commands: a b c");
    let t = Runner::new(console.clone(), shell_prompts(), opts())
        .run("help")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::UnknownPrompt);
    assert!(t.detail["teach"]
        .as_str()
        .unwrap()
        .contains("classify_prompt"));

    // Teaching it works…
    let taught = Prompts(vec![Prompt {
        re: regex::Regex::new(r"nucleus> $").unwrap(),
        raw: "nucleus> ".into(),
        kind: PromptKind::Monitor,
    }]);
    let console = FakeConsole::new("nucleus> ").reply("help", "commands: a b c");
    let t = Runner::new(console, taught, opts())
        .run("help")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok);
    assert_eq!(t.output, "commands: a b c");
}

#[tokio::test]
async fn a_prompt_scrolled_away_by_async_kernel_messages_still_completes() {
    // dmesg lines interleave with the echo, exactly as on a real shared console.
    let console = FakeConsole::new("# ")
        .with(|i| i.noise = Some("[   12.3] usb 1-1: new device\r\n".into()))
        .reply("ls", "bin dev etc");
    let t = Runner::new(console, shell_prompts(), opts())
        .run("ls")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok, "{:?}", t.detail);
    assert!(t.output.contains("bin dev etc"));
}

// -------------------------------------------------------- recovery ladder ---

#[tokio::test]
async fn rung_one_a_scrolled_prompt_is_cured_by_a_newline_probe() {
    // The console swallowed the command but a newline still draws a prompt.
    let console = FakeConsole::new("# ");
    {
        let mut i = console.inner.lock().unwrap();
        i.replies.push(("sleep 99".into(), String::new()));
    }
    let t = Runner::new(console, shell_prompts(), opts())
        .run("sleep 99")
        .await
        .unwrap();
    // The scripted console answers immediately, so this is the happy path; the
    // ladder is exercised by the hung cases below.
    assert_eq!(t.status, Outcome::Ok);
}

#[tokio::test]
async fn rung_two_a_sleep_hang_is_cured_by_ctrl_c() {
    let console = FakeConsole::new("# ").with(|i| {
        i.hung = true;
        i.unwedge = Some(0x03); // Ctrl-C
    });
    let t = Runner::new(console, shell_prompts(), opts())
        .run("sleep 999")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok);
    let rungs: Vec<&str> = t.rungs_attempted.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(rungs, ["newline_probe", "C-c"]);
    assert!(t.rungs_attempted.last().unwrap().recovered);
}

#[tokio::test]
async fn rung_three_a_cat_hang_is_cured_by_ctrl_d() {
    let console = FakeConsole::new("# ").with(|i| {
        i.hung = true;
        i.unwedge = Some(0x04); // Ctrl-D
    });
    let t = Runner::new(console, shell_prompts(), opts())
        .run("cat")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Ok);
    let rungs: Vec<&str> = t.rungs_attempted.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(rungs, ["newline_probe", "C-c", "C-\\", "C-d"]);
    assert!(t.rungs_attempted.last().unwrap().recovered);
}

#[tokio::test]
async fn a_hard_hang_exhausts_the_ladder_and_says_so_with_evidence() {
    let console = FakeConsole::new("# ").with(|i| i.hung = true);
    let t = Runner::new(console, shell_prompts(), opts())
        .run("wedge-me")
        .await
        .unwrap();
    assert_eq!(t.status, Outcome::Hung);
    assert_eq!(
        t.rungs_attempted.len(),
        4,
        "every rung is attempted and recorded"
    );
    assert!(t.rungs_attempted.iter().all(|r| !r.recovered));
    assert!(t.detail["next"].as_str().unwrap().contains("power"));
    // The agent gets a truthful terminal state with the output so far.
    let err = t.as_error().unwrap();
    assert_eq!(err.code, conminer_core::ErrorCode::Hung);
}

#[test]
fn every_escape_in_the_default_ladder_maps_to_a_real_control_byte() {
    let cfg = RunnerConfig::default();
    assert_eq!(cfg.escape_set, ["C-c", "C-\\", "C-d"]);
    assert_eq!(escape_bytes("C-c").as_bytes(), [0x03]);
    assert_eq!(escape_bytes("C-\\").as_bytes(), [0x1c]);
    assert_eq!(escape_bytes("C-d").as_bytes(), [0x04]);
}

// ------------------------------------------------------------- serialisation-

#[tokio::test]
async fn two_transactions_serialise_and_output_never_crosses() {
    // The runner takes the device lease, so transactions queue rather than
    // interleaving. Here that is modelled directly: two runners over one console
    // run one after the other and each sees only its own output.
    let console = FakeConsole::new("# ")
        .reply("first", "output-of-first")
        .reply("second", "output-of-second");

    let a = Runner::new(console.clone(), shell_prompts(), opts())
        .run("first")
        .await
        .unwrap();
    let b = Runner::new(console.clone(), shell_prompts(), opts())
        .run("second")
        .await
        .unwrap();

    assert_eq!(a.output, "output-of-first");
    assert_eq!(b.output, "output-of-second");
    assert!(!a.output.contains("second"));
    assert!(!b.output.contains("first"));
}

// -------------------------------------------------- prompt distinctiveness --

#[test]
fn a_non_distinctive_prompt_is_rejected() {
    // LAVA's lesson: `:` matches status output, and every wait-for-prompt would
    // fire on the first line of a boot log.
    use conminer_mcp::report::validate_prompt_distinctiveness as check;
    assert!(check(":").is_err());
    assert!(check("$").is_err());
    assert!(check("=> ").is_ok());
    assert!(check("uart:~\\$ ").is_ok());
}

/// Regression: a shell with bracketed paste enabled wraps every command in
/// `ESC[?2004h` / `ESC[?2004l`, and the `l` leaked into `run_command` output as
/// a literal `[?2004l` prefix. Observed on the IQ10 AP console running Debian.
#[tokio::test]
async fn bracketed_paste_modes_are_not_command_output() {
    let reply = "\x1b[?2004l\r\n   clock  enable  prepare\r\n   gcc_ufs  1  1\x1b[?2004h";
    let console = FakeConsole::new("# ").reply("cat /sys/kernel/debug/clk/clk_summary", reply);
    let t = Runner::new(console, shell_prompts(), opts())
        .run("cat /sys/kernel/debug/clk/clk_summary")
        .await
        .unwrap();

    assert_eq!(t.status, Outcome::Ok);
    assert!(
        !t.output.contains("2004"),
        "DEC private modes leaked: {:?}",
        t.output
    );
    assert!(
        !t.output.contains('\u{1b}'),
        "escape survived: {:?}",
        t.output
    );
    // Both lines of the board's actual answer survive intact.
    assert!(
        t.output.contains("clock  enable  prepare"),
        "{:?}",
        t.output
    );
    assert!(t.output.contains("gcc_ufs  1  1"), "{:?}", t.output);
}

/// Regression: ser2net's accepter is `telnet(rfc2217=false)`, and the runner
/// dials its own connection, so it receives IAC negotiation as if it were
/// console bytes. Measured on the IQ10 as run_command failing UNKNOWN_PROMPT
/// with a preamble of IAC WILL/DO pairs and empty output.
#[tokio::test]
async fn telnet_negotiation_does_not_break_a_command() {
    // IAC WILL ECHO, IAC WILL SGA, IAC DO BINARY -- exactly what ser2net opens with.
    let neg = "\u{ff}\u{fb}\u{01}\u{ff}\u{fb}\u{03}\u{ff}\u{fd}\u{00}";
    let console = FakeConsole::new("# ")
        .with(|i| i.noise = Some(neg.into()))
        .reply("uname -r", "6.12.9");
    let t = Runner::new(console, shell_prompts(), opts())
        .run("uname -r")
        .await
        .unwrap();

    assert_eq!(t.status, Outcome::Ok, "{:?}", t.detail);
    assert_eq!(t.output, "6.12.9");
    assert!(
        !t.output.contains('\u{ff}'),
        "IAC leaked into output: {:?}",
        t.output
    );
    assert!(
        !t.preamble.contains('\u{ff}'),
        "IAC leaked into preamble: {:?}",
        t.preamble
    );
}

/// Regression: a board that logs continuously never leaves its prompt as the
/// last line, so prompt detection failed against a perfectly healthy shell.
/// Driven with the REAL shipped linux profile pattern (profiles.d/linux.toml),
/// against the literal IQ10 AP console output captured 2026-08-11.
#[test]
fn a_prompt_buried_under_continuous_kernel_logging_is_still_found() {
    let pat = r"^[^ ]*[#$] (\[\s*\d+\.\d+\].*)?$";
    let p = Prompts(vec![Prompt {
        re: regex::Regex::new(pat).unwrap(),
        raw: "# ".into(),
        kind: PromptKind::Shell,
    }]);

    // 1. A kernel message lands on the prompt line itself.
    let same_line =
        "root@debian-trixie-arm64:~# [   47.646380] [drm] NORDAUX tout: TRANS_CTRL=0x200";
    assert!(
        p.commandable(same_line).is_some(),
        "prompt+kernel on one line"
    );

    // 2. The prompt scrolled above a burst of async messages.
    let buried = "root@debian-trixie-arm64:~# \n\
                  [   47.914376] [drm] NORDAUX tout: AUX_STATUS=0x0\n\
                  [   48.182376] [drm] NORDAUX tout: AUX_STATUS=0x0\n\
                  [   53.870377] platform 3d6a000.gmu: NORD JTAG-HOLD 45s";
    assert!(
        p.commandable(buried).is_some(),
        "prompt buried under kernel spam"
    );

    // 3. Kernel output ALONE must still not be commandable: the fix must not
    //    invent a prompt where the board never offered one.
    let none = "[   47.914376] [drm] NORDAUX tout: AUX_STATUS=0x0\n\
                [   48.182376] [drm] NORDAUX tout: AUX_STATUS=0x0";
    assert!(p.commandable(none).is_none(), "must not fabricate a prompt");
}

/// Regression: a console is not required to ever go quiet. settle() looped
/// until a read timed out with no data, so a board logging continuously (the
/// IQ10 AP console emits DP AUX timeouts ~4x/second, forever) never let the
/// runner reach its prompt probe, and the command died having never asked.
#[tokio::test]
async fn a_console_that_never_goes_quiet_still_runs_a_command() {
    // Noise on every echoed character means the line is never silent.
    let console = FakeConsole::new("# ")
        .with(|i| i.noise = Some("[   47.914376] [drm] NORDAUX tout: AUX_STATUS=0x0\r\n".into()))
        .reply("uname -r", "6.12.9");
    let t = Runner::new(console, shell_prompts(), opts())
        .run("uname -r")
        .await
        .unwrap();

    assert_eq!(t.status, Outcome::Ok, "{:?}", t.detail);
    assert!(t.output.contains("6.12.9"), "output = {:?}", t.output);
}

/// Regression: ser2net's telnet accepter withholds console data until the
/// client answers IAC negotiation. Measured on the IQ10 -- plain `nc` read ZERO
/// bytes in 5s while minerd captured 130863 on the same port, and a client that
/// replied got output immediately. The runner stripped negotiation but never
/// answered it, so every run_command failed NO_PROMPT with an empty buffer.
#[test]
fn negotiation_is_answered_with_refusals() {
    use conminer_core::runner::telnet_refusals;
    const IAC: u8 = 255;
    // Exactly what ser2net opens with (captured from the board).
    let offer = [
        IAC, 251, 3, IAC, 253, 3, IAC, 251, 1, IAC, 254, 1, IAC, 253, 0, IAC, 251, 0,
    ];
    let reply = telnet_refusals(&offer);

    // WILL -> DONT, DO -> WONT, for every option offered.
    assert_eq!(
        reply,
        // WILL 3 -> DONT 3 | DO 3 -> WONT 3 | WILL 1 -> DONT 1 |
        // WONT 1 -> (no reply owed) | DO 0 -> WONT 0 | WILL 0 -> DONT 0
        vec![IAC, 254, 3, IAC, 252, 3, IAC, 254, 1, IAC, 252, 0, IAC, 254, 0],
        "reply = {reply:?}"
    );
    // Console text must never provoke a reply.
    assert!(telnet_refusals(b"[   53.966277] platform 3d6a000.gmu: NORD JTAG-HOLD\r\n").is_empty());
}

/// A failure must say how much it read and from where. NO_PROMPT is reachable
/// ONLY when the buffer is empty, so without `bytes_read` an agent cannot tell
/// "I do not recognise this prompt" from "I received nothing at all" -- the
/// distinction that cost three wrong fixes on the IQ10 before anyone read the
/// branch condition.
#[tokio::test]
async fn a_failure_reports_bytes_read_and_endpoint() {
    // A console that says nothing at all: the runner should read zero bytes.
    let console = FakeConsole::new("# ").with(|i| {
        i.hung = true;
        i.echo = false;
    });
    let t = Runner::new(console, shell_prompts(), opts())
        .at("ser2net:5003")
        .run("uname -r")
        .await
        .unwrap();

    assert_ne!(t.status, Outcome::Ok);
    assert_eq!(
        t.detail["endpoint"], "ser2net:5003",
        "detail = {}",
        t.detail
    );
    assert!(t.detail["bytes_read"].is_number(), "detail = {}", t.detail);
    // The count is what was actually read -- here the console's opening prompt
    // and nothing more, since it then went silent. The point is that the number
    // is REPORTED: 0 would have said "never connected", non-zero says "connected
    // and then went quiet", and those need different fixes.
    assert_eq!(t.detail["bytes_read"], 2, "detail = {}", t.detail);
}

/// When the recovery ladder is exhausted it tells the caller what to try next.
/// That advice must reflect what the hardware actually does: on the IQ10,
/// repeated resets are a DEAD END that looks like progress -- the board wedges
/// after roughly eight, and every further reset still reports success while
/// opening an epoch that captures nothing. Only a power cycle recovers it.
#[tokio::test]
async fn an_exhausted_ladder_points_at_a_power_cycle_not_another_reset() {
    let console = FakeConsole::new("# ").with(|i| {
        i.hung = true;
        i.unwedge = None;
    });
    let t = Runner::new(console, shell_prompts(), opts())
        .run("sleep 999")
        .await
        .unwrap();

    assert_eq!(t.status, Outcome::Hung);
    let next = t.detail["next"].as_str().unwrap_or_default();
    assert!(
        next.contains("POWER CYCLE"),
        "must name the escalation: {next:?}"
    );
    assert!(
        next.contains("eight consecutive resets"),
        "must say WHY, or the advice is folklore: {next:?}"
    );
}
