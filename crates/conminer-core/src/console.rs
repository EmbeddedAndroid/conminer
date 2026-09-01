//! Perceived console state and the loop taxonomy (§8.5).
//!
//! The system tells the agent what it believes the console *is doing*, rather
//! than making the agent deduce it from raw output. Everything here is derived
//! from machinery that already exists — capture attestation, the epoch chain,
//! fingerprints, the stage machine, the prompt registry — so the state can never
//! disagree with the data it is supposedly summarising.
//!
//! The distinction that matters most: `no_signal` requires attestation. Without
//! a live capture claim the honest answer is `unknown`, and it is a *different*
//! state, not a softer version of the same one.

use crate::live::CaptureState;
use crate::runner::Prompts;
use crate::store::{BootRow, DeviceStore, RecordKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// What the console is doing, as best the system can tell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConsoleState {
    /// The port is open, RX is alive, and zero bytes have arrived.
    NoSignal,
    /// Output arrived but failed the GARBAGE_BURST threshold — a baud mismatch
    /// after a strap change looks exactly like this, and is named as such.
    Garbage {
        ratio_hint: f64,
    },
    Booting {
        stage: String,
    },
    AtPrompt {
        kind: String,
        pattern: String,
        stage: String,
    },
    /// At a prompt WHILE the board keeps printing (§F7).
    ///
    /// The ADP emits USB gadget errors several times a second forever. Its shell
    /// is sitting there, commandable, the whole time -- but calling that a clean
    /// `at_prompt` hides the spam an operator is about to be surprised by, and
    /// calling it `unstable` hides the shell. Both facts, or neither is honest.
    AtPromptWithTraffic {
        kind: String,
        pattern: String,
        stage: String,
        lines_per_min: i64,
    },
    /// `login:` / `Password:` matched. The board is up but **not commandable**;
    /// `run_command` refuses rather than typing into a login field.
    LoginWait {
        pattern: String,
    },
    /// Something is waiting for input and we do not recognise it. `prompt:true`
    /// does not fire, and the runner refuses unless forced.
    AtUnknownPrompt {
        observed_line: String,
    },
    InCommand {
        txn_id: String,
    },
    /// A power or boot-mode workflow is running on this console -- hook,
    /// verification, or an escalation still pressing buttons. Like
    /// `InCommand`, this outranks whatever the buffer shows: the prompt sitting
    /// in the store was printed by a board that is, right now, being reset or
    /// powered off. Measured on the Uno Q (report #16): `run_command` was
    /// issued while an escalation's `off` press was holding RESIN, on the
    /// strength of `at_prompt_with_traffic, commandable: true`.
    Actuating {
        tool: String,
        action: String,
        phase: String,
    },
    /// Steady non-boot output, no prompt.
    Streaming,
    /// QUIET, BUT NOT YET HUNG, AND NOT AT A PROMPT.
    ///
    /// Between "talking" and the hung threshold there is a real gap, and it used
    /// to fall through to `Streaming` -- whose explanation says "the board is
    /// still producing output". Measured on bravo: `state=streaming` with that
    /// sentence beside `idle_ms: 14567` in the same response, on a board that
    /// was boot looping. An operator reading that goes looking at the tool
    /// instead of their firmware, which is the opposite of the point.
    Quiet {
        silent_ms: i64,
    },
    Hung {
        stage: String,
        silent_ms: i64,
    },
    BootLooping {
        kind: LoopKind,
        count: usize,
        fingerprint: String,
    },
    /// Fingerprints diverging without progress: flaky, not deterministic.
    Unstable {
        distinct_fingerprints: usize,
    },
    /// The board is in EDL: its UART re-enumerates away, so there is no console
    /// to be at a prompt on. Distinct from silence and from a fault -- this is
    /// the one state in which flashing is possible.
    AwayInEdl,
    /// No live capture attestation. Not a claim about the board.
    Unknown,
}

impl ConsoleState {
    pub fn name(&self) -> &'static str {
        match self {
            ConsoleState::NoSignal => "no_signal",
            ConsoleState::Garbage { .. } => "garbage",
            ConsoleState::Booting { .. } => "booting",
            ConsoleState::AtPrompt { .. } => "at_prompt",
            ConsoleState::AtPromptWithTraffic { .. } => "at_prompt_with_traffic",
            ConsoleState::LoginWait { .. } => "login_wait",
            ConsoleState::AtUnknownPrompt { .. } => "at_unknown_prompt",
            ConsoleState::InCommand { .. } => "in_command",
            ConsoleState::Actuating { .. } => "actuating",
            ConsoleState::Streaming => "streaming",
            ConsoleState::Quiet { .. } => "quiet",
            ConsoleState::Hung { .. } => "hung",
            ConsoleState::BootLooping { .. } => "boot_looping",
            ConsoleState::Unstable { .. } => "unstable",
            ConsoleState::AwayInEdl => "away_in_edl",
            ConsoleState::Unknown => "unknown",
        }
    }

    /// Can an agent send a command right now?
    pub fn commandable(&self) -> bool {
        matches!(
            self,
            ConsoleState::AtPrompt { .. } | ConsoleState::AtPromptWithTraffic { .. }
        )
    }

    /// WHY not, when not (§F7).
    ///
    /// `commandable: false` on its own makes an agent guess, and the three
    /// reasons need three different responses: a credential gate needs
    /// credentials, an unknown prompt needs teaching, a booting board needs
    /// waiting. Stated, not implied by the state name.
    pub fn not_commandable_because(&self) -> Option<&'static str> {
        match self {
            ConsoleState::AtPrompt { .. } | ConsoleState::AtPromptWithTraffic { .. } => None,
            ConsoleState::LoginWait { .. } => Some(
                "credential_gate: the board is up and waiting for a login, not for a command",
            ),
            ConsoleState::AtUnknownPrompt { .. } => Some(
                "unknown_prompt: something is waiting for input and conminer does not recognise                  it -- teach it with classify_prompt",
            ),
            ConsoleState::InCommand { .. } => Some("in_command: a transaction is already running"),
            ConsoleState::Actuating { .. } => Some(
                "actuating: a power/boot_mode workflow is running on this console; what the \
                 buffer shows was printed before it. Poll actuation_status until it is free",
            ),
            ConsoleState::Booting { .. } | ConsoleState::Streaming => {
                Some("no_prompt_yet: the board is still producing output")
            }
            ConsoleState::Quiet { .. } => Some(
                "no_prompt_yet: the board has gone quiet without reaching a prompt, and has \
                 not been silent long enough to call hung",
            ),
            ConsoleState::Hung { .. } => Some("hung: no output and no prompt"),
            ConsoleState::NoSignal => Some("no_signal: the port is open and nothing has arrived"),
            ConsoleState::Garbage { .. } => Some("garbage: output is not decodable at this baud"),
            ConsoleState::BootLooping { .. } => Some("boot_looping: the board never reaches a prompt"),
            ConsoleState::Unstable { .. } => Some("no_prompt: nothing recognisable at the tail"),
            ConsoleState::AwayInEdl => Some(
                "away_in_edl: the board is in EDL and its UART is gone by design; there is \
                 nothing here to type at until it leaves",
            ),
            ConsoleState::Unknown => Some("unknown: no live capture attestation"),
        }
    }
}

/// The loop taxonomy of §8.5 — all from fingerprint-chain and stage analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    /// Identical fingerprints: a deterministic failure. Diff against a
    /// known-good boot is the move.
    Stable,
    /// Each epoch reaches a stage and *then* emits a crash record: it boots and
    /// then dies, which is a different bug from never booting.
    Crash,
    /// Epochs never pass a stage and emit no crash record — silent death.
    StageCapped,
    /// Reset attribution is a watchdog each time: it hangs and gets shot, rather
    /// than crashing.
    Watchdog,
    /// Fingerprints alternate or diverge: nondeterministic, race or marginal
    /// hardware.
    Flapping,
}

impl LoopKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LoopKind::Stable => "stable",
            LoopKind::Crash => "crash",
            LoopKind::StageCapped => "stage_capped",
            LoopKind::Watchdog => "watchdog",
            LoopKind::Flapping => "flapping",
        }
    }
}

/// Floor for "talking now", used when no hung threshold is configured.
///
/// The real window is `state.hung_after_s` (default 30 s), because that is
/// already the line this system draws between "quiet" and "not answering", and
/// boards differ: the IQ10 prints its GMU init message every 15 s, which a
/// two-second window read as silence between every pair of lines.
const TALKING_MS: i64 = 2_000;

/// Lines per minute above which a prompt is reported as "with traffic".
///
/// The ADP's USB gadget flap runs at about 22/min forever; a quiet shell that
/// prints a stray line or two is not the same thing, so the bar sits between.
const TRAFFIC_LINES_PER_MIN: i64 = 10;

/// How far back to look for a prompt when the recent tail is all noise.
///
/// Bounded so a quiet console never scans its whole history, deep enough to see
/// past a board that logs continuously: the ADP emits USB gadget errors about
/// four times a second, so its login prompt is hundreds of lines back within a
/// couple of minutes.
const PROMPT_LOOKBACK: usize = 400;

/// Inputs the state machine needs that are not in the store.
#[derive(Debug, Clone)]
pub struct Observation {
    pub capture: CaptureState,
    pub now_ms: i64,
    pub hung_after_ms: i64,
    pub loop_min_epochs: usize,
    /// Set while a runner transaction is in flight.
    pub active_txn: Option<String>,
}

/// Milliseconds since the console last produced any byte, or `None` if it never
/// has.
///
/// Bytes in the partial buffer are bytes the board sent: a console halfway
/// through printing a line is not silent, however long ago its last complete
/// line landed. Public because callers outside this module reason about the same
/// silence -- and two definitions of "quiet" would disagree at exactly the
/// moments that matter.
pub fn silence_ms(store: &DeviceStore, now_ms: i64) -> crate::error::Result<Option<i64>> {
    // §L7. A PARTIAL COUNTS AS OUTPUT ONLY WHILE IT IS FRESH.
    //
    // "There is text in the buffer" and "the board is talking" are different
    // facts, and reading the first as the second is what let a leftover partial
    // keep a dark console looking busy. Measured on the ADP with the cursor
    // provably not moving -- no bytes at all for three minutes -- the state
    // alternated between `streaming` and `unstable` on consecutive calls,
    // because the partial's timestamp sat right at the talking threshold.
    //
    // The framer closes a record after ~10 s of dead air, so a partial older
    // than a minute is not a console mid-line: it is a leftover. It stays
    // available for prompt classification (a prompt lives ONLY there, which
    // four rounds of findings established) -- it just stops counting as
    // evidence that bytes are flowing.
    const PARTIAL_IS_FRESH_MS: i64 = 60_000;
    let last = store.recent_lines(1)?.into_iter().next();
    let pending = store
        .pending_tail()?
        .map(|(_, ts)| ts)
        .filter(|ts| now_ms - ts <= PARTIAL_IS_FRESH_MS);
    let last_at = last.map(|l| l.ts_wall).max(pending);
    Ok(last_at.map(|ts| (now_ms - ts).max(0)))
}

/// Derive the console state from the store plus a live observation.
/// What this console is SITTING AT: recent lines plus the partial line the
/// capture loop is still holding, decoration stripped.
///
/// THE PROMPT IS NOT IN raw_lines. A prompt has no terminator -- the cursor sits
/// on the line waiting for input -- so a console idling at `root@iq10:~#` has
/// that text only in the capture loop's partial buffer, and it stays there until
/// enter is pressed or ten seconds of dead air close the record. Classifying
/// from stored lines alone looked straight past the prompt at the kernel chatter
/// above it and answered `unstable, commandable: false` at a healthy idle shell.
///
/// A carried-over partial is trusted only while the SCREEN CANNOT HAVE CHANGED
/// since it was taken. Restarting conminer does not change what a silent board
/// is displaying. Cutting power, resetting or entering a boot mode absolutely
/// does -- so an epoch opened by one of those, after the observation was taken,
/// invalidates it.
/// Could this line be something waiting for input?
///
/// Used only where the terminator is unknowable (an ingested file). Prompts end
/// in punctuation that invites typing; ordinary output does not, which is what
/// keeps `CONSOLE` from being reported as an unrecognised gate.
fn looks_like_a_gate(line: &str) -> bool {
    matches!(
        line.trim_end().chars().last(),
        Some('>' | '#' | '$' | ':' | '?' | '%')
    )
}

/// The UNTERMINATED text the console is sitting on, if any.
///
/// This is the difference between a prompt and output: a prompt has no
/// terminator, so the cursor rests on it and it lives only in the capture
/// loop's partial buffer. A line that ended is something the board SAID.
fn pending_partial(store: &DeviceStore) -> crate::error::Result<Option<String>> {
    // ANY actuation since the partial was seen invalidates it, not just the
    // newest epoch. Checking only the latest missed the ordinary shape
    // `power` then `session`: the reboot was one epoch back, the session
    // epoch on top of it is not an actuation, and a prompt from before the
    // power cycle survived as though the board had never moved.
    //
    // ...BUT AN EPOCH'S TIMESTAMP IS WHEN THE TOOL FINISHED, NOT WHEN THE BOARD
    // MOVED, and those are not the same event. `power` spends up to 8 s in its
    // off phase before the epoch is recorded, while this board boots in well
    // under a second -- so the board's own output is stamped BEFORE the epoch
    // that contains it. Measured on the Uno-Q, boot 483: partial `sirocco> `
    // seen at 1786896518281, epoch stamped 1786896528401, 10.1 s LATER. Purely
    // by time, the prompt the board was sitting at looked like a prompt from
    // before the reboot, and was thrown away -- which is why `follow` matched
    // nothing, `console_state` said hung and `boot_report` said hung, all from
    // this one discard.
    //
    // What actually distinguishes the two cases is whether the board has SPOKEN
    // inside the actuated epoch. Output in the epoch means the partial (always
    // the newest thing in the stream) came from this boot; an actuated epoch
    // with nothing in it means the board has said nothing since, and a prompt
    // from before it is stale.
    let boots = store.list_boots(BOOT_SCAN)?;
    let Some((partial, seen_at)) = store.pending_tail()? else {
        return Ok(None);
    };
    if partial.trim().is_empty() {
        return Ok(None);
    }
    // A partial is stale once the console has MOVED ON after it was seen. The
    // tell is an epoch -- of ANY kind -- that produced its OWN output after the
    // partial and did NOT update it:
    //   * an ACTUATION that produced NOTHING (bytes == 0): the board went down
    //     and has said nothing since, so a prompt from before it is gone (the
    //     original off case).
    //   * ANY epoch whose first line is stamped AFTER `seen_at`: fresh output
    //     arrived that is not this partial. When the SAME prompt is reprinted --
    //     a shell at a prompt under benign traffic, a capture reconnect that
    //     keeps showing `sirocco> ` -- the partial is republished and `seen_at`
    //     moves with it, so this does not fire. When DIFFERENT output arrives --
    //     report #21 (a cold boot reaching only `smp: bringup begin`), report
    //     #22 (EDL re-enumerates to a DevProg/Firehose console whose 34 KB of
    //     output lands in a SESSION epoch, not an actuation) -- `seen_at` is
    //     frozen at the old prompt and this fires.
    //
    // Session epochs are INCLUDED deliberately: #22's DevProg output is captured
    // in a `session` epoch (a reconnect after the UART re-enumerated), so the
    // old actuation-only rule let a pre-EDL `sirocco> ` survive into a board
    // that was being flashed -- one keystroke from Firehose.
    //
    // Boot 483 stays valid: its own banner precedes the `#` partial by ~2 ms, so
    // `first_line_ts <= seen_at + grace` and it is not read as a move.
    for b in &boots {
        if b.opened_at <= seen_at {
            continue;
        }
        // A generous margin over the commit-ordering skew: the pending tail is
        // timestamped when the buffer is published, a hair BEFORE its own lines
        // are flushed, so a partial and its OWN epoch's first line can differ by
        // a few ms (measured 2 ms on boot 483). Real new output is seconds away.
        const PARTIAL_COMMIT_SKEW_MS: i64 = 1_000;
        let empty_actuation = is_actuation(b) && b.bytes == 0;
        let spoke_after = store
            .boot_first_line_ts(b.id)?
            .is_some_and(|t| t > seen_at + PARTIAL_COMMIT_SKEW_MS);
        if empty_actuation || spoke_after {
            return Ok(None);
        }
    }
    Ok(Some(partial))
}

/// How far back to look for the epoch that began the current boot.
///
/// One `session` epoch opens per capture reconnect, so a long-lived board
/// accumulates them between reboots; bounded so a device with a huge history
/// never scans all of it.
const BOOT_SCAN: usize = 200;

/// The board was actuated: this epoch is a genuinely new boot.
fn is_actuation(b: &BootRow) -> bool {
    matches!(
        b.opened_by.as_str(),
        "power" | "reset" | "boot_mode" | "flash"
    )
}

/// The epoch that began the boot the console is CURRENTLY in.
///
/// AN EPOCH IS NOT A BOOT. `session` and `mark` epochs open without the board
/// restarting -- a capture reconnect opens one, so every deploy adds another --
/// and the console carries straight across them. Anything asking "has THIS boot
/// reached its prompt?" has to mean "since the last actuation", or a board that
/// booted an hour ago and has been idle at its shell ever since looks like an
/// epoch that never said anything.
pub fn boot_floor(store: &DeviceStore) -> crate::error::Result<Option<i64>> {
    let boots = store.list_boots(BOOT_SCAN)?;
    // Newest first: the first actuation is the start of the current boot.
    Ok(boots
        .iter()
        .find(|b| is_actuation(b))
        .map(|b| b.id)
        .or_else(|| boots.last().map(|b| b.id)))
}

fn console_tail(store: &DeviceStore, lines: usize) -> crate::error::Result<String> {
    tail_text(store, None, lines)
}

/// The console tail as text, restricted to one epoch when `boot_id` is given.
///
/// `follow` needs the scoped form. Its `until:{prompt:true}` must never be
/// satisfied by the PREVIOUS epoch's prompt (§8.5, and the boundary-grace note
/// in `follow.rs`), but it must still see a prompt the current epoch is sitting
/// at. Scoping the stored lines gives both: the partial is always current by
/// construction, since `pending_partial` drops one that an actuated epoch
/// invalidated.
pub fn tail_text(
    store: &DeviceStore,
    since_boot: Option<i64>,
    lines: usize,
) -> crate::error::Result<String> {
    let rows = match since_boot {
        Some(id) => {
            // A FLOOR, NOT AN EQUALITY. An epoch is not a boot: `session` and
            // `mark` epochs open without the board restarting, so the lines of
            // the boot the console is still sitting in are in an EARLIER epoch
            // than the current one. Measured on the Uno-Q: epoch 473 was the
            // power-on that reached `sirocco> `, and 474-477 were four empty
            // `session` epochs opened by stack restarts. Scoped by equality to
            // 477 the console had said nothing, ever.
            let mut v = store.tail_since_boot(id, lines)?;
            v.reverse();
            v
        }
        None => store.recent_lines(lines)?,
    };
    let mut tail: String = rows
        .iter()
        .map(|l| l.lossy())
        .collect::<Vec<_>>()
        .join("\n");
    let pending = pending_partial(store)?;
    if let Some(partial) = &pending {
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str(partial);
    }
    // Match prompts through their decoration. A colourised shell prompt arrives
    // as `ESC[1;32mroot@iq10ESC[0m:~#`, which no prompt pattern an operator
    // would write is going to match.
    Ok(crate::strip_ansi(&tail))
}

/// The prompt this console is waiting at, if any.
///
/// THE ONE PLACE THAT ANSWERS THIS. `boot_report` used to answer it from silence
/// alone -- an epoch quiet past the hung threshold was `hung`, full stop -- and
/// so it called an idle RTOS shell hung while `console_state`, reading these very
/// bytes, said `at_prompt`. Two verdicts from one evidence base is not a
/// disagreement worth having, so there is one implementation and both call it.
///
/// A PROMPT DOES NOT SCROLL AWAY. When the recent tail says nothing, look
/// further back for the last line that could BE a prompt: measured on the ADP,
/// whose `login:` gate had long since been pushed out of the eight-line window
/// by USB gadget errors, leaving a taught pattern unusable in exactly the case
/// it was taught for. Lazy on purpose -- a console with its prompt in the last
/// eight lines never pays for the deeper read.
pub fn prompt_at_tail<'a>(
    store: &DeviceStore,
    prompts: &'a Prompts,
) -> crate::error::Result<Option<&'a crate::runner::Prompt>> {
    prompt_at_tail_in_boot(store, prompts, None)
}

/// `prompt_at_tail`, restricted to one epoch's stored lines.
///
/// The same one implementation, so `follow` cannot disagree with
/// `console_state` about whether a board is at a prompt. Measured on the
/// Uno-Q, boot 473: the board printed `CONSOLE` and then `\r\nsirocco> ` with
/// no terminator, so the prompt existed only in the partial buffer.
/// `console_state` said `at_prompt` and `commandable`; `follow` -- which
/// scanned stored lines alone -- timed out after 30 s and called the console
/// hung, and only a `run_command` that forced a newline made the same prompt
/// appear. An agent lost the board to a defect in the reader, not the board.
pub fn prompt_at_tail_in_boot<'a>(
    store: &DeviceStore,
    prompts: &'a Prompts,
    since_boot: Option<i64>,
) -> crate::error::Result<Option<&'a crate::runner::Prompt>> {
    if let Some(p) = prompts.classify(&tail_text(store, since_boot, 8)?) {
        return Ok(Some(p));
    }
    Ok(prompts.classify(&tail_text(store, since_boot, PROMPT_LOOKBACK)?))
}

/// Did THIS EPOCH reach a commandable prompt, in what it recorded?
///
/// The historic question. Answered from the epoch's own stored lines, so it is
/// valid for a closed epoch months later and says nothing about what the console
/// is doing now.
pub fn prompt_reached_in_boot<'a>(
    store: &DeviceStore,
    prompts: &'a Prompts,
    boot_id: i64,
) -> crate::error::Result<Option<&'a crate::runner::Prompt>> {
    for l in store.tail_of_boot(boot_id, PROMPT_SCAN)? {
        if let Some(p) = prompts.commandable(&l.lossy()) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// How far back through an epoch's own lines to look for the prompt it reached.
const PROMPT_SCAN: usize = 40;

/// THE ONE ANSWER FOR AN EPOCH: did it reach a prompt, or is it at one now?
///
/// Two questions used to be answered in four places -- `follow` scanned stored
/// lines with its own matcher, `boot_report` scanned them again with a third,
/// and each then called into here for the live half, one of them behind an
/// arbitrary 30 s silence gate. Every defect that produced this function was two
/// of those disagreeing about one console: `console_state` at a prompt while
/// `follow` timed out, `follow` matched while `boot_report` said the epoch had
/// not finished.
///
/// A HISTORIC EPOCH IS DESCRIBED BY WHAT IT RECORDED, never by what the console
/// happens to be showing now -- so the live tail is consulted only for the boot
/// the console is actually in.
pub fn prompt_for_boot<'a>(
    store: &DeviceStore,
    prompts: &'a Prompts,
    boot_id: i64,
    boot_is_open: bool,
) -> crate::error::Result<Option<(&'a crate::runner::Prompt, PromptSource)>> {
    if let Some(p) = prompt_reached_in_boot(store, prompts, boot_id)? {
        return Ok(Some((p, PromptSource::Recorded)));
    }
    let live = boot_is_open && boot_floor(store)?.is_some_and(|floor| boot_id >= floor);
    if !live {
        return Ok(None);
    }
    Ok(prompt_at_tail_in_boot(store, prompts, Some(boot_id))?
        .filter(|p| p.kind.is_commandable())
        .map(|p| (p, PromptSource::LiveTail)))
}

/// WHICH of the two questions answered -- callers report it, because the two
/// mean different things to an agent. A prompt the epoch RECORDED is a line the
/// board sent and can be quoted back; one read from the live tail has no
/// newline yet, exists only in the capture loop's partial buffer, and is the
/// state the console is in right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSource {
    /// A stored line inside the epoch.
    Recorded,
    /// The unterminated tail the cursor is resting on.
    LiveTail,
}

impl PromptSource {
    pub fn is_unterminated(self) -> bool {
        matches!(self, PromptSource::LiveTail)
    }
}

pub fn derive(
    store: &DeviceStore,
    prompts: &Prompts,
    obs: &Observation,
) -> crate::error::Result<ConsoleState> {
    // A transaction in flight outranks everything: we know exactly what the
    // console is doing because we are the ones doing it.
    if let Some(txn) = &obs.active_txn {
        return Ok(ConsoleState::InCommand {
            txn_id: txn.clone(),
        });
    }

    if obs.capture == CaptureState::NotListening {
        // "I do not know" is a state, and it is not silence.
        return Ok(ConsoleState::Unknown);
    }

    let boots = store.list_boots(obs.loop_min_epochs.max(3) * 4)?;
    let latest = boots.first();

    let bytes_this_boot = latest.map(|b| b.bytes).unwrap_or(0);
    if bytes_this_boot == 0 && store.line_count()? == 0 {
        // Only claimable because capture health was attested above.
        return Ok(ConsoleState::NoSignal);
    }

    if obs.capture == CaptureState::Garbage {
        return Ok(ConsoleState::Garbage { ratio_hint: 0.0 });
    }

    // EDL OUTRANKS THE TAIL, because the tail is a photograph of a console that
    // no longer exists. Entering EDL re-enumerates the board's USB and takes the
    // UART with it: no new bytes can arrive to contradict the prompt still
    // sitting in the store, so classification happily reported `at_prompt,
    // commandable: true` for a board that had just been put into the one mode
    // where there is nothing to type at. Reported from a live flashing session,
    // in the response of the very call that did it.
    if obs.capture == CaptureState::AwayInEdl {
        return Ok(ConsoleState::AwayInEdl);
    }

    let tail = console_tail(store, 8)?;
    let stage = store
        .stages(None, latest.map(|b| b.id))?
        .last()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "unknown".into());

    // WHAT THE CONSOLE IS DOING RIGHT NOW OUTRANKS WHAT IT DID BEFORE.
    //
    // Epoch-chain analysis used to run first, on the reasoning that a board
    // which has looped four hundred times is one line of state. True -- but on a
    // rig where boards are power-cycled all day, `loop_state` fires on almost
    // every call, so `classify` never ran and `login_wait` was UNREACHABLE. A
    // prompt that conminer was explicitly taught (classify_prompt) could never
    // show up in the state it was taught for, which made the teaching useless.
    //
    // A recognised prompt is also a stronger claim than churn: the board is
    // alive, has stopped, and is WAITING FOR SOMETHING. `login_wait` in
    // particular is the difference between "this board is broken" and "this
    // board wants a password" -- an operator's next action, not a statistic.
    // Loop analysis still runs, immediately below, whenever the tail says
    // nothing recognisable. The deep lookback lives in `prompt_at_tail`, which
    // is also what `boot_report` asks, so the two cannot drift apart.
    let matched = prompt_at_tail(store, prompts)?;
    if let Some(p) = matched {
        // How talkative has this console been in the last minute? A prompt under
        // steady spam is a different situation from a quiet one, and an agent
        // planning a run_command needs to know its output will be interleaved.
        let lines_per_min = store.lines_since(obs.now_ms - 60_000)?;
        return Ok(match p.kind {
            crate::framer::profile::PromptKind::CredentialGate => ConsoleState::LoginWait {
                pattern: p.raw.clone(),
            },
            crate::framer::profile::PromptKind::Ignore => ConsoleState::Streaming,
            k if lines_per_min >= TRAFFIC_LINES_PER_MIN => ConsoleState::AtPromptWithTraffic {
                kind: k.as_str().to_string(),
                pattern: p.raw.clone(),
                stage,
                lines_per_min,
            },
            k => ConsoleState::AtPrompt {
                kind: k.as_str().to_string(),
                pattern: p.raw.clone(),
                stage,
            },
        });
    }

    let silent_ms = silence_ms(store, obs.now_ms)?;
    // "Talking" means "has said something recently ENOUGH", and how recent that
    // is depends on the board, not on a constant. Measured on the IQ10 stuck in
    // its GMU init loop: it prints every 15 s, so a 2 s window called it silent
    // between messages and the epoch-chain fallback answered `unstable` about a
    // console that was visibly alive. The configured hung threshold is already
    // the line between "quiet" and "not answering"; reusing it keeps the states
    // consistent instead of inventing a second timescale.
    let talking_now = silent_ms.is_some_and(|ms| ms < obs.hung_after_ms.max(TALKING_MS));

    // Nothing recognisable at the tail, so the epoch chain is the best available
    // answer: looping, unstable, or steady.
    //
    // ...with one exception, measured on the IQ10 while it was mid-boot and
    // printing 6 KB in 8 seconds: `unstable` is not an observation, it is the
    // fallback the epoch chain returns when recent boots produced too many
    // different fingerprints to conclude anything. Reporting that as the state
    // of a console that is visibly emitting output tells an agent the board is
    // flapping when the board is simply talking. A LOOP verdict is different --
    // `boot_looping` is a positive, evidence-backed claim about epochs that
    // really do repeat, and repeated boots are noisy by nature, so that one
    // stands whatever the console is doing right now.
    match loop_state(store, &boots, obs)? {
        Some(ConsoleState::Unstable { .. }) if talking_now => {}
        Some(state) => return Ok(state),
        None => {}
    }

    // An idle line we do not recognise: say so rather than guessing. The agent
    // can teach it with classify_prompt, which turns each unfamiliar console
    // into a one-time event instead of a recurring misclassification.
    if let Some(ms) = silent_ms {
        if ms >= obs.hung_after_ms {
            // AN UNKNOWN PROMPT MUST BE SOMETHING THAT IS ACTUALLY WAITING.
            //
            // This used to take the last non-empty line, whatever it was, so any
            // quiet console whose last output happened to be `CONSOLE`, or a
            // kernel message, or a test result, was reported as "something is
            // waiting for input and conminer does not recognise it". Reported
            // from the bench on a board that had printed `CONSOLE` and had not
            // yet reached its prompt: the state invited an operator to teach a
            // pattern for a line that is not a prompt, and teaching it would
            // have made every later `follow {until: prompt}` fire on ordinary
            // output.
            //
            // The evidence is the TERMINATOR. A prompt has none -- the cursor
            // sits on it, which is the entire reason the partial buffer exists.
            // A line that ended is something the board said, and silence after
            // it is silence, which the branch below already names honestly.
            let idle = match pending_partial(store)? {
                Some(p) => crate::strip_ansi(&p).trim_end_matches('\n').to_string(),
                // NO PARTIAL AT ALL is a different situation from an empty one:
                // an INGESTED file has no capture loop, so its last line carries
                // no terminator information and could well be a prompt. Fall
                // back to it -- but only when it looks like something waiting
                // for input, which is what separates `nucleus> ` from `CONSOLE`.
                None => {
                    let last = tail.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
                    if looks_like_a_gate(last) {
                        last.to_string()
                    } else {
                        String::new()
                    }
                }
            };
            let complete = matches!(stage.as_str(), "userspace" | "android");
            return Ok(if !idle.trim().is_empty() && !complete {
                ConsoleState::AtUnknownPrompt {
                    observed_line: idle,
                }
            } else {
                ConsoleState::Hung {
                    stage,
                    silent_ms: ms,
                }
            });
        }
    }

    if stage == "unknown" {
        // Only call it streaming if it is ACTUALLY streaming. Past the talking
        // window with nothing arriving, the honest answer is that it went quiet.
        match silent_ms {
            Some(ms) if ms > TALKING_MS => Ok(ConsoleState::Quiet { silent_ms: ms }),
            _ => Ok(ConsoleState::Streaming),
        }
    } else {
        Ok(ConsoleState::Booting { stage })
    }
}

/// Classify the epoch chain, if it is looping at all.
/// Does this epoch represent a BOOT THE BOARD ACTUALLY PERFORMED?
///
/// Epochs are opened for several reasons and only some of them are the board
/// restarting. `session` is an agent attaching, `mark` is a marker dropped in
/// the stream, `ingest` is a file. None of them says anything about how the
/// board behaves, and an epoch that recorded ZERO bytes has no behaviour to
/// compare at all.
///
/// Counting them is a false boot-loop generator. Measured on the Uno-Q: its last
/// epochs were three `session` attaches of zero bytes each, which naturally
/// share one fingerprint -- the hash of nothing happening -- and three is
/// exactly `loop_min`. So a board sitting happily at its prompt, answering
/// echo-verified commands, was reported as `boot_looping`, and the same rows
/// made `boot_report` call its history `flapping` with ten "distinct
/// fingerprints". A board that genuinely reboots itself still opens `reset` or
/// `power` epochs with output, so real loops are untouched.
fn is_boot_attempt(b: &BootRow) -> bool {
    // Stated as an EXCLUSION, not an allowlist. An allowlist of
    // power/reset/flash also threw away `ingest` epochs -- a replayed log is a
    // recording of real boots, and loop analysis on an ingested corpus stopped
    // working entirely. `session` and `mark` are the only ones that are not the
    // board running.
    !matches!(b.opened_by.as_str(), "session" | "mark") && b.bytes > 0
}

fn loop_state(
    store: &DeviceStore,
    boots: &[BootRow],
    obs: &Observation,
) -> crate::error::Result<Option<ConsoleState>> {
    // Only epochs that are actually boots: see `is_boot_attempt`.
    let boots: Vec<&BootRow> = boots.iter().filter(|b| is_boot_attempt(b)).collect();
    if boots.len() < obs.loop_min_epochs {
        return Ok(None);
    }
    let fingerprints: Vec<&str> = boots
        .iter()
        .filter_map(|b| b.fingerprint.as_deref())
        .collect();
    if fingerprints.len() < obs.loop_min_epochs {
        return Ok(None);
    }

    let head = fingerprints[0];
    let run = fingerprints.iter().take_while(|f| **f == head).count();
    let distinct: std::collections::BTreeSet<&&str> = fingerprints.iter().collect();

    if run < obs.loop_min_epochs {
        // Diverging without progress is a different problem from a stable loop,
        // and needs a different fix.
        if distinct.len() > 2 {
            return Ok(Some(ConsoleState::Unstable {
                distinct_fingerprints: distinct.len(),
            }));
        }
        return Ok(None);
    }

    // A stable chain: name *which* kind, because "boots then dies" and "never
    // boots" call for opposite next steps.
    let mut crashed = 0usize;
    let mut watchdog = 0usize;
    let mut deepest: Vec<String> = Vec::new();
    for b in boots.iter().take(run) {
        let crashes = store.records_in_boot(b.id, Some(RecordKind::Crash), 5)?;
        if !crashes.is_empty() {
            crashed += 1;
        }
        for r in store.records_in_boot(b.id, None, 200)? {
            let text = store.record_text(r.id).unwrap_or_default();
            if crate::framer::generic::is_watchdog(&text) {
                watchdog += 1;
                break;
            }
        }
        if let Some(s) = store.stages(None, Some(b.id))?.last() {
            deepest.push(s.name.clone());
        }
    }

    let kind = if watchdog * 2 >= run {
        LoopKind::Watchdog
    } else if crashed * 2 >= run {
        LoopKind::Crash
    } else if distinct.len() > 2 {
        LoopKind::Flapping
    } else if deepest
        .iter()
        .all(|s| !matches!(s.as_str(), "userspace" | "android"))
    {
        LoopKind::StageCapped
    } else {
        LoopKind::Stable
    };

    Ok(Some(ConsoleState::BootLooping {
        kind,
        count: run,
        fingerprint: head.to_string(),
    }))
}

/// The JSON form carried in the freshness envelope and in notifications.
pub fn to_json(s: &ConsoleState) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("commandable".into(), json!(s.commandable()));
        if let Some(why) = s.not_commandable_because() {
            o.insert("not_commandable_because".into(), json!(why));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_stable_name_and_only_at_prompt_is_commandable() {
        let cases = [
            ConsoleState::NoSignal,
            ConsoleState::Garbage { ratio_hint: 0.4 },
            ConsoleState::Booting {
                stage: "bl31".into(),
            },
            ConsoleState::AtPrompt {
                kind: "shell".into(),
                pattern: "# ".into(),
                stage: "userspace".into(),
            },
            ConsoleState::LoginWait {
                pattern: "login: ".into(),
            },
            ConsoleState::AtUnknownPrompt {
                observed_line: "?".into(),
            },
            ConsoleState::InCommand {
                txn_id: "t1".into(),
            },
            ConsoleState::Actuating {
                tool: "power".into(),
                action: "off".into(),
                phase: "escalation: off".into(),
            },
            ConsoleState::Streaming,
            ConsoleState::Hung {
                stage: "bl31".into(),
                silent_ms: 60_000,
            },
            ConsoleState::BootLooping {
                kind: LoopKind::Stable,
                count: 40,
                fingerprint: "abc".into(),
            },
            ConsoleState::Unstable {
                distinct_fingerprints: 5,
            },
            ConsoleState::Unknown,
        ];
        let mut names = std::collections::BTreeSet::new();
        for c in &cases {
            assert!(names.insert(c.name()), "duplicate state name {}", c.name());
            assert_eq!(
                c.commandable(),
                matches!(c, ConsoleState::AtPrompt { .. }),
                "{}",
                c.name()
            );
        }
        assert_eq!(names.len(), 13);
    }

    #[test]
    fn login_wait_is_a_distinct_state_from_at_prompt() {
        // The board is up but not commandable. Conflating these is how an agent
        // ends up typing a command into a password field.
        let gate = ConsoleState::LoginWait {
            pattern: "login: ".into(),
        };
        assert!(!gate.commandable());
        assert_ne!(gate.name(), "at_prompt");
    }

    #[test]
    fn unknown_is_not_no_signal() {
        // Without capture attestation we do not get to claim the board is quiet.
        assert_ne!(ConsoleState::Unknown.name(), ConsoleState::NoSignal.name());
    }

    #[test]
    fn the_loop_taxonomy_names_every_class() {
        let all = [
            LoopKind::Stable,
            LoopKind::Crash,
            LoopKind::StageCapped,
            LoopKind::Watchdog,
            LoopKind::Flapping,
        ];
        let names: std::collections::BTreeSet<&str> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn the_json_form_tells_an_agent_whether_it_may_send() {
        let v = to_json(&ConsoleState::AtPrompt {
            kind: "shell".into(),
            pattern: "# ".into(),
            stage: "userspace".into(),
        });
        assert_eq!(v["state"], "at_prompt");
        assert_eq!(v["commandable"], true);
        let v = to_json(&ConsoleState::LoginWait {
            pattern: "login: ".into(),
        });
        assert_eq!(v["commandable"], false);
    }
}
