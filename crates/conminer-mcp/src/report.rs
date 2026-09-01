//! Derived reports: `boot_report`, `diff_sessions`, `get_prompts` (§8, §8.4, §8.5).
//!
//! These are the answers an agent actually wants — "what happened?", "what
//! changed?", "what should I expect to see?" — assembled from the store rather
//! than asked of a human.

use crate::state::Context;
use conminer_core::error::{ErrorCode, Result, ToolError};
use conminer_core::framer::profile::PromptKind;
use conminer_core::store::{DeviceRow, RecordKind, Severity, TemplateQuery};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// How recently the console must have spoken for an epoch to count as still
/// booting (§K5a).
///
/// Deliberately shorter than `state.hung_after_s`: "is this boot alive right
/// now" and "has this console hung" are different questions with different
/// answers, and borrowing the hung threshold let a 25s-silent epoch still claim
/// to be booting.
const BOOTING_IDLE_MS: i64 = 15_000;

/// Classify one boot epoch (§8.4).
///
/// Every outcome is stated with its evidence, and the difference between "no
/// output" and "I do not know" is never blurred: a device with no live capture
/// attestation reports `unknown_capture`, not `no_output`.
pub fn boot_report(ctx: &Context, dev: &DeviceRow, boot: Option<i64>) -> Result<Value> {
    let loop_min = ctx.config().state.loop_min_epochs;
    let hung_after_ms = ctx
        .config()
        .hung_after_s_for(&[dev.display_name(), dev.label().unwrap_or_default()])
        as i64
        * 1000;
    // Capture health, from the column that holds it: `state` is presence now,
    // and reading it here would make every boot report claim there was no
    // attestation the moment discovery wrote `discovered`.
    let attested = matches!(
        dev.capture_state.as_deref().unwrap_or(dev.state.as_str()),
        "listening" | "streaming" | "at_prompt"
    );
    let now = ctx.now();
    // Built out here on purpose: `prompts_for` opens the store, and asking for it
    // from inside `with_store` would nest store access.
    let prompts = crate::tools::prompts_for(ctx, dev)?;

    ctx.with_store(dev, |st| {
        let b = match boot {
            Some(id) => st.boot(id)?,
            None => st
                .latest_boot()?
                .ok_or_else(|| ToolError::new(ErrorCode::UnknownBoot, "no epochs recorded yet"))?,
        };

        let stages = st.stages(None, Some(b.id))?;
        let deepest = stages.last().map(|s| s.name.clone());
        let crashes = st.records_in_boot(b.id, Some(RecordKind::Crash), 20)?;
        let garbage = st.records_in_boot(b.id, Some(RecordKind::Garbage), 5)?;
        let last_line = st.last_line_in_boot(b.id)?;
        let silent_ms = last_line.as_ref().map(|l| (now - l.ts_wall).max(0));

        // What the board said it was running, from its own output: the
        // `build=`/`fp=` tokens it printed, and every version banner parsed out
        // of this epoch. `provenance` already reads both to decide whether the
        // running firmware matches the bound image; boot_report never surfaced
        // either, so "what firmware was this?" cost a second call that answered
        // in terms of a *claim* rather than in terms of the epoch (#5).
        let mut build_fps = st.build_fingerprints_in_boot(b.id, 200)?;
        build_fps.sort();
        build_fps.dedup();
        let versions: Value = st
            .versions_in_boot(b.id)?
            .into_iter()
            .collect::<serde_json::Map<_, _>>()
            .into();

        // Epoch chain: how long this fingerprint has held, and where it diverged.
        let history = st.list_boots(loop_min.max(3) * 200)?;
        let mine = b.fingerprint.clone();
        // ONLY EPOCHS THAT ARE BOOTS. `session` attaches, `mark`s and `ingest`s
        // are bookkeeping, and a zero-byte epoch has no behaviour to compare --
        // yet they were counted as boot attempts that differed, which is how a
        // board idling at its prompt earned `history: flapping` with ten
        // "distinct fingerprints". Reported from the bench on an epoch that had
        // exactly one kernel banner and answered a command immediately.
        let chain: Vec<_> = history
            .iter()
            .skip_while(|h| h.seq > b.seq)
            // Exclusion, not allowlist: an `ingest` epoch is a RECORDING of a
            // real boot and belongs in the chain. Listing only power/reset/flash
            // silently disabled loop analysis for every ingested corpus, which
            // the L5 gate caught immediately.
            .filter(|h| !matches!(h.opened_by.as_str(), "session" | "mark") && h.bytes > 0)
            .collect();
        let stable_run = chain
            .iter()
            .take_while(|h| h.fingerprint.is_some() && h.fingerprint == mine)
            .count();
        // The oldest boot still inside the stable run — i.e. when this
        // fingerprint first appeared, walking backwards from `b`.
        let fingerprint_stable_since = stable_run
            .checked_sub(1)
            .and_then(|i| chain.get(i))
            .map(|h| h.seq);
        let first_divergence = chain
            .get(stable_run)
            .map(|h| json!({"boot_seq": h.seq, "boot_id": h.id, "fingerprint": h.fingerprint}));

        // A histogram makes flapping visible: alternating fingerprints are a
        // different problem from a stable loop, and need a different fix.
        let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
        for h in chain.iter().take(loop_min.max(3) * 10) {
            if let Some(fp) = &h.fingerprint {
                *histogram.entry(fp.clone()).or_default() += 1;
            }
        }

        let prompt_stage = stages.iter().rev().find(|s| {
            matches!(
                s.name.as_str(),
                "userspace" | "uboot" | "zephyr" | "android"
            )
        });

        // §L5. Garbage that the boot GREW OUT OF is not the story of the boot.
        //
        // Real UART noise at a strap or power transition is normal on these
        // boards -- the outcome text says so itself ("a baud mismatch after a
        // strap change looks exactly like this") -- and a reset that produces
        // one quarantined span and then marches bl2 -> bl31 -> uefi -> kernel is
        // a good boot with a dirty first inch. Reporting it as `garbage` because
        // a span exists anywhere in the epoch buried five clean stages under a
        // transient.
        //
        // "Grew out of" is checked in stream order, not by clock: a stage banner
        // parsed AFTER the last quarantined span is proof the framer recovered,
        // because that banner had to be read cleanly to be recognised at all.
        // The count still ships in `garbage_spans` either way -- this decides
        // the headline, it does not hide the evidence.
        let last_garbage = st.last_record_in_boot(b.id, RecordKind::Garbage)?;
        let garbage_resolved = last_garbage.as_ref().is_some_and(|g| {
            stages
                .iter()
                .any(|s| s.banner_line_id.is_some_and(|id| id > g.last_line_id))
        });

        // ONLY where it could change the answer: the epoch is open and has been
        // quiet past the hung threshold. Anywhere else the tail describes the
        // console NOW and says nothing about the epoch being reported.
        // DID THIS EPOCH REACH A SHELL? Asked of the console's own output, not
        // of the stage machine.
        //
        // `booted` below requires a STAGE named userspace/uboot/zephyr/android,
        // which a board whose profile nobody has written can never produce. The
        // Uno-Q runs a custom RTOS: it boots, runs its self-tests and settles at
        // `sirocco> `, and no stage banner is ever recognised -- so a complete,
        // healthy boot could not be called booted no matter what it did.
        // Reported from the bench on boot 456, whose 6229 bytes ended at a
        // prompt conminer itself recognises and answers commands at.
        //
        // A prompt IS a terminal state: the board saying it finished and is
        // waiting. Read from the tail, because that is where it lands.
        // ONE ORACLE, TWO QUESTIONS, ASKED ONCE.
        //
        // This used to scan the epoch's stored lines with its own matcher and
        // then, separately, read the live tail behind a `hung_after_ms` silence
        // gate -- so for the first 30 s after a board settled at an unterminated
        // prompt NEITHER fired and a finished boot came back `in_progress`,
        // while `follow` and `console_state` both said it was at a prompt.
        //
        // `prompt_for_boot` answers both: what this epoch RECORDED, and -- only
        // when it is the boot the console is actually in -- what the console is
        // sitting at now.
        let reached_prompt = conminer_core::console::prompt_for_boot(
            st,
            &prompts,
            b.id,
            b.closed_at.is_none(),
        )?
        .map(|(p, _src)| p.raw.clone());

        // Silence no longer decides DETECTION, only wording: an epoch quiet past
        // the hung threshold and sitting at a prompt is described as idle rather
        // than as having just arrived there.
        let idle_prompt = if b.closed_at.is_none() && silent_ms.is_some_and(|ms| ms >= hung_after_ms)
        {
            conminer_core::console::prompt_at_tail(st, &prompts)?.cloned()
        } else {
            None
        };

        let looping = stable_run >= loop_min;
        // FLAPPING PROMISES ALTERNATION, SO IT HAS TO MEASURE ALTERNATION.
        //
        // This counted DISTINCT fingerprints alone, which made every actively
        // worked-on board "flapping" forever: measured on the Uno-Q, boot 473
        // came back `booted` with `history: flapping` and a histogram of 19
        // fingerprints each seen EXACTLY ONCE. Nothing was alternating. The
        // board was simply being reflashed between boots, which is the normal
        // state of a bench, and the word told an agent to go and look for
        // marginal hardware.
        //
        // A shape has to RECUR for the chain to be flapping between shapes.
        // When every recent epoch is unlike every other, that is `varied`, and
        // it is what a board under development looks like.
        // It was wrong in BOTH directions, because `> 2 distinct` is not a test
        // for alternation either way. `A B A B A B` -- a board alternating
        // between exactly two outcomes, which is the most flapping thing a
        // bench can do -- has two distinct fingerprints and was reported
        // `steady`.
        let enough = !looping && chain.len() >= loop_min;
        let recurs = histogram.values().any(|&n| n > 1);
        let flapping = enough && histogram.len() >= 2 && recurs;
        let varied = enough && histogram.len() > 2 && !recurs;

        let (outcome, why) = if b.bytes == 0 {
            if attested {
                (
                    "no_output",
                    "the port was open and zero bytes arrived".to_string(),
                )
            } else {
                (
                    "unknown_capture",
                    "no bytes recorded and no live capture attestation — this is 'I do not know', \
                     not 'nothing happened'"
                        .to_string(),
                )
            }
        } else if !garbage.is_empty() && !garbage_resolved {
            (
                "garbage",
                format!(
                    "{} quarantined span(s); a baud mismatch after a strap change looks exactly \
                     like this",
                    garbage.len()
                ),
            )
        } else if !crashes.is_empty() {
            (
                "crashed",
                format!("{} crash record(s) in this epoch", crashes.len()),
            )
        } else if let Some(p) = prompt_stage {
            ("booted", format!("reached stage {}", p.name))
        } else if let Some(pattern) = &reached_prompt {
            (
                "booted",
                format!(
                    "reached the prompt {pattern:?}; this profile recognises no stage banner for \
                     this board, and a prompt is the board saying it finished and is waiting"
                ),
            )
        } else if let (Some(last), true) = (
            stages.last(),
            b.closed_at.is_none() && silent_ms.is_some_and(|ms| ms < BOOTING_IDLE_MS),
        ) {
            // §K5a. A BOOT IN PROGRESS IS "BOOTING", never a history verdict.
            //
            // The rule, exactly: at least one stage entered, no terminal stage
            // yet, and the console spoke within the last 15s. An epoch that is
            // still producing output has not reached a prompt stage -- that is
            // what `booted` above is for -- and it used to fall through to the
            // epoch-chain verdicts and report `unstable: N distinct
            // fingerprints` for a board three seconds into a perfectly ordinary
            // boot, flipping to `booted` when userspace landed.
            //
            // Requiring a STAGE is what makes this a claim about the boot rather
            // than about the clock: bytes with no stage banner is `in_progress`,
            // which is the honest name for "something is talking and I cannot
            // yet say what it is".
            (
                "booting",
                format!(
                    "stage {} entered {}s ago; boot in progress",
                    last.name,
                    now.saturating_sub(last.entered_ts).max(0) / 1000
                ),
            )
        } else if looping {
            // ONE history verdict survives here, and only this one: `looping`
            // says N CONSECUTIVE epochs were byte-identical, which is a claim
            // about a repeating failure an operator must act on, and it cannot
            // be read off a single epoch.
            //
            // `unstable` is gone from `outcome` entirely (§K5a). It counted
            // DISTINCT fingerprints across recent epochs -- a property of the
            // bench's history, not of this boot -- and it was the fallback that
            // labelled a healthy in-progress boot a failure because earlier
            // boots differed. It still lives in `history` and
            // `distinct_fingerprints_recent` below, where it describes what it
            // actually measures and cannot overwrite what happened here.
            (
                "looping",
                format!("{stable_run} consecutive epochs with an identical fingerprint"),
            )
        } else if let Some(p) = idle_prompt.as_ref() {
            // A CONSOLE WAITING AT A PROMPT IS NOT HUNG, however long it waits.
            //
            // This rung used to be absent, so a quiet epoch fell straight to
            // `hung` on the strength of the clock alone -- and an epoch opened by
            // `start_session` against a board already sitting at `sirocco>` is
            // quiet by definition. Neither `booted` nor `booting` could catch it:
            // both require a STAGE, and joining an already-running board produces
            // no stage banner to parse. So conminer reported `hung` for a console
            // that `console_state`, reading the very same bytes, called
            // `at_prompt` and commandable.
            //
            // Silence is not evidence of failure. It is evidence of silence; the
            // prompt is what says which kind. `hung` still fires below when
            // nothing recognisable is behind the quiet, which is the case it was
            // always meant for -- a board that died mid-boot with a kernel
            // message as its last word.
            let idle_s = silent_ms.unwrap_or(0) / 1000;
            if p.kind == PromptKind::CredentialGate {
                (
                    "login_wait",
                    format!(
                        "waiting at {:?} for {idle_s}s; the board is up, and wants credentials",
                        p.raw
                    ),
                )
            } else {
                (
                    "at_prompt",
                    // Deliberately NOT `booted`: we never witnessed a boot. An
                    // epoch that joined a running board can say what the console
                    // is doing and must not claim what it did.
                    format!(
                        "idle at the {} prompt {:?} for {idle_s}s",
                        p.kind.as_str(),
                        p.raw
                    ),
                )
            }
        } else if silent_ms.is_some_and(|ms| ms >= hung_after_ms) && b.closed_at.is_none() {
            (
                "hung",
                match deepest.clone() {
                    Some(stage) => format!(
                        "silent for {} ms at stage {stage}, with no prompt at the tail",
                        silent_ms.unwrap_or(0)
                    ),
                    // "at stage unknown" read as though a stage had been found
                    // and named `unknown`. No stage was reached at all.
                    None => format!(
                        "silent for {} ms, no stage reached and no prompt at the tail",
                        silent_ms.unwrap_or(0)
                    ),
                },
            )
        } else if b.closed_at.is_some() {
            // A CLOSED EPOCH IS NOT "STILL OPEN". This fallback said exactly
            // that about epochs that had ended hours earlier -- reported from
            // the bench on a closed epoch answering `in_progress: the epoch is
            // still open`. It ended; what it did not do is reach anything this
            // profile recognises, and saying so is the honest answer.
            (
                "ended",
                format!(
                    "the epoch closed after {} bytes without reaching a stage or prompt this \
                     profile recognises",
                    b.bytes
                ),
            )
        } else {
            (
                "in_progress",
                "the epoch is still open and has not reached a terminal state".to_string(),
            )
        };

        // Templates novel to this epoch — the "what's new in this boot?" answer.
        let novel = st.templates_first_seen_in_boot(b.id, 20)?;

        // Against the blessed epoch, when there is one. Deliberately a summary:
        // "12 templates you have never seen in a good boot, and handoff is
        // 380 ms late" is the answer; the enumeration is `diff_boots`, one call
        // away and only worth its tokens once the summary says it matters.
        let vs_baseline = match st.baseline("default")? {
            Some(base) if base.boot_id != b.id => {
                let mine = st.templates_in_boot(b.id)?;
                let theirs = st.templates_in_boot(base.boot_id)?;
                let base_boot = st.boot(base.boot_id)?;
                let stage_at =
                    |boot: &conminer_core::store::BootRow| -> Result<BTreeMap<String, i64>> {
                        let mut m = BTreeMap::new();
                        for s in st.stages(None, Some(boot.id))? {
                            m.entry(s.name).or_insert(s.entered_ts - boot.opened_at);
                        }
                        Ok(m)
                    };
                let (ta, tb) = (stage_at(&base_boot)?, stage_at(&b)?);
                let worst_delta = tb
                    .iter()
                    .filter_map(|(k, v)| ta.get(k).map(|x| (k.clone(), v - x)))
                    .max_by_key(|(_, d)| *d)
                    .map(|(stage, delta_ms)| json!({"stage": stage, "delta_ms": delta_ms}));
                Some(json!({
                    "baseline": base.name,
                    "baseline_boot_seq": base_boot.seq,
                    "new_vs_baseline": mine.difference(&theirs).count(),
                    "missing_vs_baseline": theirs.difference(&mine).count(),
                    "same_fingerprint": base_boot.fingerprint.is_some()
                        && base_boot.fingerprint == b.fingerprint,
                    "slowest_stage_regression": worst_delta,
                    "detail": "call diff_boots for the enumeration",
                }))
            }
            _ => None,
        };

        let time_to_prompt = prompt_stage
            .map(|p| p.entered_ts - b.opened_at)
            .filter(|d| *d >= 0);

        // WHEN THE EPOCH OPENED IS NOT WHEN THE BOOT BEGAN.
        //
        // `boot.opened_at` is stamped when the actuation TOOL finished -- a
        // power hook holds its line for seconds and verification follows -- while
        // the epoch's real boundary is `opened_offset`, the stream position at
        // the moment the button was pressed. A fast board boots and prints in
        // between, so an epoch legitimately contains lines timestamped BEFORE
        // its own `opened_at`.
        //
        // Two different agents read that as corruption and filed it, the second
        // one against a boot whose attribution was by then correct. Saying it
        // outright is cheaper than being re-reported: `began_at` is when this
        // epoch's first line actually arrived.
        let began_at = st.tail_of_boot(b.id, 1)?.last().map(|l| l.ts_wall);
        Ok(json!({
            "boot": b,
            "began_at": began_at,
            "opened_at_is": "when the actuation that opened this epoch COMPLETED; the epoch's \
                             boundary is its stream offset, so a board that booted while the \
                             hook was still running has lines older than this",
            "outcome": outcome,
            "why": why,
            "group_id": b.group_id,
            // History as its OWN field, so churn across recent epochs is still
            // visible without masquerading as this boot's result.
            "history": if looping {
                "looping"
            } else if flapping {
                "flapping"
            } else if varied {
                "varied"
            } else {
                "steady"
            },
            "history_is": "the recent epoch chain, not this boot: looping (N identical in a row), \
                           flapping (a shape recurs, so the board alternates), varied (every \
                           recent epoch differs, which is what a board being reflashed looks \
                           like), or steady",
            "distinct_fingerprints_recent": histogram.len(),
            "deepest_stage": deepest,
            "time_to_prompt_ms": time_to_prompt,
            "silent_ms": silent_ms,
            "stages": stages,
            "crash_records": crashes.iter().map(|r| json!({
                "record_id": r.id,
                "severity": r.severity,
                "profile": r.profile,
                "first_line_id": r.first_line_id,
                "truncated": r.truncated,
            })).collect::<Vec<_>>(),
            "garbage_spans": garbage.len(),
            "novel_templates": novel,
            "vs_baseline": vs_baseline,
            // TWO DIFFERENT THINGS ARE CALLED A FINGERPRINT, so both are here
            // and each says which it is. `fingerprint` is the SHAPE of this
            // epoch -- the hash of its template sequence, the thing that makes
            // "same crash again" a comparison. `build_fingerprints` is what the
            // BOARD said it was running.
            //
            // Report #5 is what conflating them costs: an agent read
            // `fingerprint: null` on a boot whose banner said
            // `build=f0a276f875fba3d6`, on a line `exact boot search` had
            // already found, and filed a defect against firmware detection.
            // Nothing was broken -- it was reading the shape field, which is
            // null until the epoch closes, and the firmware field did not
            // exist.
            "fingerprint": b.fingerprint,
            "fingerprint_is": "the shape of this epoch (its template sequence), not the firmware build",
            // Sealed on close, so an epoch still running has none yet. Saying
            // so beats a bare null next to `outcome: booted`.
            "fingerprint_pending": b.fingerprint.is_none().then_some(
                "this epoch is still open; the shape signature is computed when it closes"
            ),
            "build_fingerprints": build_fps,
            "versions": versions,
            "fingerprint_stable_since": fingerprint_stable_since,
            "fingerprint_histogram": histogram,
            "first_divergence_from_previous": first_divergence,
            "epochs_examined": chain.len(),
        }))
    })
    .and_then(|mut v| {
        // §F1. An epoch opened as part of a board-wide action names its
        // siblings, so an agent that asked the console which stayed silent is
        // pointed at the one that talked instead of concluding "nothing
        // happened". Resolved here rather than in the store closure because it
        // reads OTHER devices' stores.
        let Some(group) = v
            .get("group_id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Ok(v);
        };
        let mut siblings = Vec::new();
        for other in ctx.registry().all_devices()? {
            if other.id == dev.id {
                continue;
            }
            if let Ok(Some(b)) = ctx.with_store(&other, |st| st.boot_in_group(&group)) {
                siblings.push(json!({
                    "device": other.display_name(),
                    "boot_id": b.id,
                    "boot_seq": b.seq,
                    "bytes": b.bytes,
                }));
            }
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("sibling_epochs".into(), json!(siblings));
        }
        Ok(v)
    })
}

/// Templates new in B, gone from B, and count-shifted (§8).
pub fn diff_sessions(
    ctx: &Context,
    dev: &DeviceRow,
    a: i64,
    b: i64,
    limit: usize,
) -> Result<Value> {
    if a == b {
        return Err(ToolError::invalid_arg("a and b must be different sessions"));
    }
    ctx.with_store(dev, |st| {
        // Both sessions must belong to this device, or the diff is meaningless.
        let (sa, sb) = (st.session(a)?, st.session(b)?);

        let load = |sid: i64| -> Result<BTreeMap<i64, (String, i64)>> {
            let rows = st.list_templates(&TemplateQuery {
                session_id: Some(sid),
                limit: 100_000,
                ..Default::default()
            })?;
            Ok(rows
                .into_iter()
                .map(|t| (t.id, (t.text, t.scoped_count.unwrap_or(t.total_count))))
                .collect())
        };
        let ma = load(a)?;
        let mb = load(b)?;

        let ids_a: BTreeSet<i64> = ma.keys().copied().collect();
        let ids_b: BTreeSet<i64> = mb.keys().copied().collect();

        let mut new_in_b: Vec<Value> = ids_b
            .difference(&ids_a)
            .map(|id| json!({"template_id": id, "text": mb[id].0, "count": mb[id].1}))
            .collect();
        let mut gone_from_b: Vec<Value> = ids_a
            .difference(&ids_b)
            .map(|id| json!({"template_id": id, "text": ma[id].0, "count": ma[id].1}))
            .collect();
        let mut shifted: Vec<Value> = ids_a
            .intersection(&ids_b)
            .filter(|id| ma[id].1 != mb[id].1)
            .map(|id| {
                json!({
                    "template_id": id, "text": ma[id].0,
                    "count_a": ma[id].1, "count_b": mb[id].1,
                    "delta": mb[id].1 - ma[id].1,
                })
            })
            .collect();

        let totals = json!({
            "new_in_b": new_in_b.len(),
            "gone_from_b": gone_from_b.len(),
            "count_shifted": shifted.len(),
        });
        let capped = new_in_b.len() > limit || gone_from_b.len() > limit || shifted.len() > limit;
        new_in_b.truncate(limit);
        gone_from_b.truncate(limit);
        shifted.truncate(limit);

        Ok(json!({
            "a": {"session": sa.id, "label": sa.label, "lines": sa.lines},
            "b": {"session": sb.id, "label": sb.label, "lines": sb.lines},
            "totals": totals,
            "new_in_b": new_in_b,
            "gone_from_b": gone_from_b,
            "count_shifted": shifted,
            "capped": capped,
        }))
    })
}

/// Prompt expectations per stage, with provenance (§8.5).
pub fn get_prompts(ctx: &Context, dev: &DeviceRow, stage: Option<&str>) -> Result<Value> {
    let profiles = ctx.profiles().clone();
    let learned = ctx.with_store(dev, |st| st.prompts(stage))?;

    let mut out = Vec::new();
    for p in profiles.all() {
        if let Some(want) = stage {
            if p.stage != want {
                continue;
            }
        }
        for pat in &p.prompts {
            out.push(json!({
                "pattern": pat.raw,
                "kind": pat.kind.as_str(),
                "commandable": pat.kind.is_commandable(),
                "provenance": "profile",
                "stage": p.stage,
                "profile": p.name,
                "observations": 0,
            }));
        }
    }
    for l in &learned {
        out.push(json!({
            "pattern": l.pattern,
            "kind": l.kind,
            "commandable": conminer_core::framer::profile::PromptKind::parse(&l.kind)
                .map(|k| k.is_commandable()).unwrap_or(false),
            "provenance": l.provenance,
            "stage": l.stage,
            "observations": l.observations,
            "last_seen": l.last_seen,
        }));
    }

    Ok(json!({
        "prompts": out,
        // A credential gate is never a shell prompt: the board is up but not
        // commandable, and `prompt:true` must not fire on it (§8.5).
        "credential_gates": out.iter()
            .filter(|p| p["kind"] == "credential_gate")
            .cloned().collect::<Vec<_>>(),
        "stage": stage,
    }))
}

/// Reject a prompt pattern that would match ordinary output.
///
/// LAVA's lesson (§8.3): a "prompt" like `:` matches half of a boot log, and
/// every wait-for-prompt then fires on the first status line. A prompt has to be
/// distinctive or it is worse than none.
pub fn validate_prompt_distinctiveness(pattern: &str) -> Result<()> {
    let stripped: String = pattern
        .chars()
        .filter(|c| !matches!(c, '^' | '$' | '\\' | '*' | '+' | '?' | ' '))
        .collect();
    if stripped.chars().count() < 2 {
        return Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("prompt pattern {pattern:?} is not distinctive enough"),
        )
        .with_hint(
            "a prompt must be more than a single character — `:` matches status output, and \
             every wait-for-prompt would fire early",
        ));
    }
    Ok(())
}

/// Classify an epoch as good or bad for a bisect.
///
/// The predicate is deliberately narrow — a template present, a fingerprint
/// seen, an outcome matched — because a bisect verdict has to be reproducible.
/// Anything it cannot decide comes back `skip`, which steps around the candidate
/// rather than guessing and pinning an innocent build.
pub fn classify_for_bisect(
    st: &mut conminer_core::store::DeviceStore,
    boot_id: i64,
    predicate: &Value,
) -> Result<(String, String)> {
    let boot = st.boot(boot_id)?;

    if let Some(tid) = predicate.get("template_id").and_then(Value::as_i64) {
        let present = st.templates_in_boot(boot_id)?.contains(&tid);
        return Ok(if present {
            ("bad".into(), format!("template {tid} fired in this epoch"))
        } else {
            ("good".into(), format!("template {tid} did not fire"))
        });
    }
    if let Some(fp) = predicate.get("fingerprint").and_then(Value::as_str) {
        return Ok(match boot.fingerprint.as_deref() {
            Some(seen) if seen == fp => ("bad".into(), format!("fingerprint {fp} matched")),
            Some(seen) => ("good".into(), format!("fingerprint is {seen}, not {fp}")),
            None => (
                "skip".into(),
                "this epoch has no fingerprint yet".to_string(),
            ),
        });
    }
    if let Some(want) = predicate.get("outcome").and_then(Value::as_str) {
        return Ok(match boot.outcome.as_deref() {
            Some(o) if o == want => ("bad".into(), format!("outcome is {o}")),
            Some(o) => ("good".into(), format!("outcome is {o}, not {want}")),
            None => (
                "skip".into(),
                "this epoch has no recorded outcome".to_string(),
            ),
        });
    }

    // No predicate: fall back to the coarsest honest signal.
    Ok(match boot.outcome.as_deref() {
        Some("booted") => ("good".into(), "the epoch reached a prompt".to_string()),
        Some(o) => ("bad".into(), format!("outcome is {o}")),
        None => (
            "skip".into(),
            "no predicate and no outcome: cannot classify this epoch".to_string(),
        ),
    })
}

/// The build fingerprints inside a string: long hex runs, lowercased.
///
/// An image is bound by NAME (`sirocco-unoq-appsdk-271e11b419aa852e`) while the
/// board announces a FINGERPRINT (`build=271e11b419aa852e`). Comparing the two
/// with `contains` can only ever fail, so the shared part -- the fingerprint --
/// is what gets compared. Twelve characters minimum: short hex runs turn up
/// inside ordinary version strings and would match by accident.
/// EVERY NAME THE BINDING CARRIES, not just the one `ref` collapsed to.
///
/// `intended_image()` reduces a `set_image` binding to a single `ref` -- name,
/// else git_sha, else image_hash -- and the verdict compared only that. So a
/// board printing `git f1fb57060680`, bound with that exact `git_sha` under a
/// different `name`, was called a mismatch on evidence that agreed (report
/// #24). The claim's identity is the whole row, and a flash hook's `image` is
/// the same kind of claim.
fn binding_identities(flashed: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(f) = flashed else {
        return out;
    };
    let mut take = |v: &Value| {
        if let Some(s) = v.as_str() {
            let s = s.trim();
            if !s.is_empty() && !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
        }
    };
    take(&f["ref"]);
    for k in ["name", "git_sha", "image_hash", "image", "build"] {
        take(&f["detail"][k]);
    }
    out
}

/// Two build identities agree when one is a prefix of the other. A binding may
/// carry a full 40-hex git SHA while the console prints the abbreviated 12, and
/// they are the same commit; requiring equality calls that a mismatch.
fn ids_agree(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn build_fingerprints(s: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let lower = s.to_ascii_lowercase();
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        // Walk each maximal hex run inside the token, so `appsdk-271e...` and
        // `v1.2-deadbeefcafe` both give up their fingerprint.
        let mut run = String::new();
        for ch in tok.chars().chain(std::iter::once('.')) {
            if ch.is_ascii_hexdigit() {
                run.push(ch);
            } else {
                if run.len() >= 12 {
                    out.insert(run.clone());
                }
                run.clear();
            }
        }
    }
    out
}

/// Components that are FIRMWARE, not the operating system.
///
/// A boot that printed only these has not told us what OS is running, and
/// comparing them against a bound OS image can only produce a false mismatch.
const FIRMWARE_COMPONENTS: &[&str] = &[
    "chip", "xbl", "sbl", "pbl", "abl", "uefi", "tz", "bl1", "bl2", "bl31", "bl33", "aop", "cpucp",
    "hyp", "devcfg", "rpm", "qtee", "optee",
];

/// What is running versus what was last flashed (§18.5).
///
/// The failure this exists for is not exotic: a flash that did not take, and an
/// afternoon spent debugging the previous image. Both sides are reported with
/// their provenance, and a mismatch is stated rather than left to be noticed.
pub fn provenance(
    st: &conminer_core::store::DeviceStore,
    boot: &conminer_core::store::BootRow,
) -> Result<Value> {
    // §F2. WHAT IS RUNNING, read from the version banners lifted at mining time.
    //
    // This used to scan the epoch's record FIELDS for anything ending in
    // `_version`, which found nothing on any board this rig has: the data was in
    // the banner TEXT, not in extracted fields, so `running` came back `{}` for
    // four rounds while every store held BL31's fingerprint, OP-TEE's commit,
    // the UEFI string and the kernel's #build.
    let mut components: BTreeMap<String, Value> = BTreeMap::new();
    for (component, detail) in st.versions_in_boot(boot.id)? {
        components.insert(component, detail);
    }
    // §G5. THE CHAIN DOES NOT RESPECT EPOCH BOUNDARIES.
    //
    // An epoch opens when conminer's hook runs; the board's early banners (bl2,
    // bl31, OP-TEE, UEFI) may already have gone past by then. Measured on the
    // rig, one boot's chain arrived split across the boundary-lag pair --
    // kernel and machine in the power epoch, everything beneath them in the
    // epoch before it -- so provenance on either epoch showed half a chain and
    // nothing said why.
    //
    // Sibling epochs of the same action (§F1 `group_id`) are the same boot on
    // other consoles, and the epoch immediately before is the other half of the
    // lag. Both are folded in, and NEITHER OVERWRITES this epoch's own reading:
    // where they disagree, what this console printed in this epoch wins, and
    // every borrowed component says which epoch it came from so the merge can
    // be checked rather than trusted.
    let mut borrowed: Vec<i64> = Vec::new();
    if let Some(g) = boot.group_id.as_deref() {
        for sib in st.boots_in_group(g)? {
            if sib.id == boot.id {
                continue;
            }
            borrowed.push(sib.id);
            for (component, mut detail) in st.versions_in_boot(sib.id)? {
                if let Some(o) = detail.as_object_mut() {
                    o.insert("from_epoch".into(), json!(sib.id));
                    o.insert("from".into(), json!("sibling console, same action"));
                }
                components.entry(component).or_insert(detail);
            }
        }
    }
    // §L4. THE LAG IS NOT ALWAYS ONE EPOCH DEEP.
    //
    // The one-step look-back was fitted to the boundary-lag PAIR, and it is
    // wrong for the same reason the pair exists: epochs are opened by
    // ACTUATIONS, not by boots. Measured on the ADP under selftest churn, a
    // reset and a power press landed 178 ms apart mid-boot and a single boot's
    // banner chain came out across three epochs (535 chip/uefi/xbl, 536
    // kernel/machine, 537 the new boot) -- so provenance on 537 declined the
    // merge while its own `chain_continues_in` pointed straight at 535. Two
    // parts of one answer disagreeing about where the chain is is not a
    // tuning problem; it is one rule expressed twice.
    //
    // So the walk is now the SAME walk the hint does, and the stopping rule is
    // evidence rather than a step count: a reading is this boot's lag if it was
    // printed within CHAIN_WINDOW_MS of this epoch opening. The longest boot on
    // this rig reaches userspace in ~24 s, so three minutes is wide enough to
    // hold any real chain and far too narrow to reach yesterday's firmware.
    // Nearest epoch wins, this epoch's own reading still outranks everything
    // borrowed, and every borrowed component names the epoch and the distance
    // it came from, so the merge can be checked rather than trusted.
    const CHAIN_LOOK_BACK: usize = 6;
    const CHAIN_WINDOW_MS: i64 = 180_000;
    {
        let mut cursor = boot.id;
        for step in 1..=CHAIN_LOOK_BACK {
            let Some(prev) = st.previous_boot(cursor)? else {
                break;
            };
            cursor = prev.id;
            if borrowed.contains(&prev.id) {
                continue;
            }
            let mut used = false;
            for (component, mut detail) in st.versions_in_boot(prev.id)? {
                if components.contains_key(&component) {
                    continue;
                }
                let ts = detail
                    .get("ts")
                    .and_then(Value::as_i64)
                    .unwrap_or(prev.opened_at);
                // Too old to be this boot's lag. Left for the hint below, which
                // will name where it is without claiming it is ours.
                if boot.opened_at - ts > CHAIN_WINDOW_MS {
                    continue;
                }
                if let Some(o) = detail.as_object_mut() {
                    o.insert("from_epoch".into(), json!(prev.id));
                    o.insert("epochs_back".into(), json!(step));
                    o.insert(
                        "from".into(),
                        json!(if step == 1 {
                            "the preceding epoch (boundary lag)".to_string()
                        } else {
                            format!("{step} epochs back, within the same boot's banner window")
                        }),
                    );
                }
                components.insert(component, detail);
                used = true;
            }
            if used {
                borrowed.push(prev.id);
            }
        }
    }
    // §H3. WHEN THE MERGE DECLINES, SAY WHERE THE REST IS.
    //
    // The walk above stops at readings older than the chain window, because
    // reaching past it means attributing some other boot's firmware to this
    // one. Observed on the IQ10 after a mode-clear plus reset: the banners
    // landed outside the window, the merge correctly stopped, and the chain
    // split across two calls with nothing to say so -- the reader has to
    // already suspect it to go looking.
    //
    // So: keep walking WITHOUT merging, and name the nearest epoch that holds
    // components this one does not. A pointer is safe where a merge is not, and
    // it turns "half a chain" into "half a chain, and the other half is in
    // epoch N".
    let mut chain_hint = Value::Null;
    {
        const LOOK_BACK: usize = 12;
        let mut cursor = boot.id;
        for _ in 0..LOOK_BACK {
            let Some(p) = st.previous_boot(cursor)? else {
                break;
            };
            cursor = p.id;
            if borrowed.contains(&p.id) {
                continue;
            }
            let elsewhere: Vec<String> = st
                .versions_in_boot(p.id)?
                .into_iter()
                .map(|(c, _)| c)
                .filter(|c| !components.contains_key(c))
                .collect();
            if !elsewhere.is_empty() {
                chain_hint = json!({
                    "epoch": p.id,
                    "components": elsewhere,
                    "why": "these were printed too long before this epoch opened to be part of \
                            its boot -- merging them here could attribute another boot's firmware \
                            to this one. Call provenance on that epoch to see them.",
                });
                break;
            }
        }
    }
    // The old field-scan is kept as a fallback: a profile that extracts a
    // version into a field still contributes, and nothing that used to work
    // stops working.
    let mut running: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &components {
        if let Some(ver) = v.get("version").and_then(Value::as_str) {
            running.insert(k.clone(), ver.to_string());
        }
    }
    for r in st.records_in_boot(boot.id, None, 400)? {
        let Some(fields) = r.fields.as_object() else {
            continue;
        };
        for (k, v) in fields {
            if k.ends_with("_version") || k == "build" || k == "image" {
                if let Some(s) = v.as_str() {
                    running.entry(k.clone()).or_insert_with(|| s.to_string());
                }
            }
        }
    }

    let bound = match boot.image_id {
        Some(id) => st.image(id).ok(),
        None => None,
    };
    let flashed = st.intended_image()?;

    // A mismatch is only claimable when both sides are known. "I cannot tell" is
    // a third answer and must not read as "they agree".
    let flashed_ref = flashed
        .as_ref()
        .and_then(|f| f["ref"].as_str().map(str::to_string));
    // §G5. A BINDING IS NOT A FLASH, in the PROSE as well as in the fields.
    //
    // The response already separates `last_flashed` from `bound_by_hand`, and
    // then the verdict text said "what was last flashed" for both -- so a build
    // somebody merely asserted with set_image read back as evidence that bytes
    // were pushed to the board. On a rig where "was this reflashed?" is a
    // forensic question, the sentence a human actually reads has to hold the
    // same line the fields do.
    let by_hand = flashed
        .as_ref()
        .is_some_and(|f| f["via"].as_str() == Some("set_image"));
    let (claim_past, claim_noun) = if by_hand {
        ("was bound by hand with set_image", "the binding claims")
    } else {
        ("was last flashed", "the last flash pushed")
    };
    // THE FINGERPRINT THE BOARD PRINTED, gathered before any verdict, because it
    // is evidence in both directions: with parsed components (does the running
    // build agree?) and without them (a board whose banner no profile knows is
    // still telling us what it is). Found on hardware after the first fix
    // shipped: the Uno-Q prints `fp=2413879641ace37b` and parses to no component
    // at all, so the answer was still "this epoch printed no version banner"
    // while the board was naming its build on the very line above.
    let printed_fps = st.build_fingerprints_in_boot(boot.id, 200)?;
    // The claim's identities, and the fingerprints inside them. Gathered from
    // the whole binding rather than its `ref` alone (report #24).
    let want_ids = binding_identities(flashed.as_ref());
    let want_fps: std::collections::BTreeSet<String> = want_ids
        .iter()
        .flat_map(|s| build_fingerprints(s))
        .collect();

    let (verdict, why) = match (&flashed_ref, running.is_empty()) {
        (None, _) => (
            "unknown",
            "nothing has claimed what should be on this board: no flash hook has reported a \
             push and no build has been bound with set_image"
                .to_string(),
        ),
        // No component parsed -- but a printed fingerprint still identifies the
        // build, and it is the same question asked with less punctuation.
        (Some(want), true) if !printed_fps.is_empty() => {
            match want_fps
                .iter()
                .find(|f| printed_fps.iter().any(|p| ids_agree(f, p)))
            {
                Some(f) => (
                    "match",
                    format!(
                        "no component banner parsed, but the board printed build fingerprint \
                         {f}, which is the one in {want} -- what {claim_past}"
                    ),
                ),
                None => (
                    "mismatch",
                    format!(
                        "{claim_noun} {want}, but this epoch printed build fingerprint(s) {:?}. \
                         Everything concluded from this boot is about the other image.",
                        printed_fps
                    ),
                ),
            }
        }
        (Some(_), true) => (
            "unknown",
            "this epoch printed no version banner and no build fingerprint, so what is running \
             cannot be read from the console"
                .to_string(),
        ),
        (Some(want), false) => {
            // Compared *only* against what the console said. The epoch's bound
            // image is a record of intent, written by the same call that claimed
            // the flash, so counting it as agreement would make every check pass
            // by construction: the exact false all-clear this guard exists to
            // prevent. It stays in the response as context, never as evidence.
            // Substring evidence from ANY identity the binding carries, not
            // only `ref`. Short of a fingerprint's 12 hex digits this stays at
            // 7 characters -- an abbreviated git SHA -- because below that a
            // "match" is a coincidence rather than a claim.
            let hit = want_ids
                .iter()
                .filter(|w| w.len() >= 7)
                .any(|w| running.values().any(|v| v.contains(w.as_str())));
            // THE BOARD ANNOUNCES A FINGERPRINT; THE BINDING CARRIES A NAME.
            //
            // `sirocco-unoq-appsdk-271e11b419aa852e` bound, `build=271e11b419aa852e`
            // printed: the same build, and `contains` says otherwise every time.
            let mut seen_fps: std::collections::BTreeSet<String> = running
                .values()
                .flat_map(|v| build_fingerprints(v))
                .collect();
            seen_fps.extend(printed_fps.iter().cloned());
            let fp_hit = want_fps
                .iter()
                .any(|f| seen_fps.iter().any(|s| ids_agree(f, s)));
            // Nothing here is the OS. Saying "mismatch" would claim the board is
            // running something other than what was flashed, on the strength of
            // never having read its OS banner at all.
            // ...but only when there is NO OS evidence at all. A board that
            // printed a build fingerprint has told us what it is running, and if
            // that fingerprint is not the bound one then this is a real
            // mismatch, firmware banners or not.
            let firmware_only = !running.is_empty()
                && seen_fps.is_empty()
                && running
                    .keys()
                    .all(|k| FIRMWARE_COMPONENTS.contains(&k.as_str()));
            if hit || fp_hit {
                (
                    "match",
                    if hit {
                        format!("the running build reports {want}, which is what {claim_past}")
                    } else {
                        format!(
                            "the board printed build fingerprint {}, which is the one in {want} -- \
                             what {claim_past}",
                            want_fps
                                .iter()
                                .find(|f| seen_fps.iter().any(|s| ids_agree(f, s)))
                                .cloned()
                                .unwrap_or_default()
                        )
                    },
                )
            } else if firmware_only {
                (
                    "unknown",
                    format!(
                        "this epoch printed only firmware banners ({}), so what the OS is \
                         cannot be read from the console. {claim_noun} {want}; that is neither \
                         confirmed nor contradicted here.",
                        running.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                )
            } else {
                (
                    "mismatch",
                    format!(
                        "{claim_noun} {want}, but this epoch reports {:?}{}. Everything concluded \
                         from this boot is about the other image.",
                        running.values().cloned().collect::<Vec<_>>(),
                        if seen_fps.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " and printed build fingerprint(s) {:?}",
                                seen_fps.iter().cloned().collect::<Vec<_>>()
                            )
                        }
                    ),
                )
            }
        }
    };

    Ok(json!({
        "boot_id": boot.id,
        "boot_seq": boot.seq,
        // Each component with its version AND what the banner carried alongside
        // (build number, builder, date), plus the line it came from so the claim
        // can be checked against the console rather than trusted.
        "running": components,
        // §H3. Where the rest of the chain is, when it could not be merged.
        "chain_continues_in": chain_hint,
        // The flat name→version view, for callers that only want the strings.
        "running_versions": running,
        "bound_image": bound,
        // A BINDING IS NOT A FLASH, and on a rig where "was this board
        // reflashed?" is a forensic question, one field covering both invites
        // the wrong conclusion. `set_image` records what someone SAYS is on the
        // board; only the flash hook records that bytes were pushed to it.
        // `last_flashed` is now reserved for the latter and is null otherwise.
        "last_flashed": flashed.as_ref().filter(|f| {
            f["via"].as_str().is_some_and(|v| v != "set_image")
        }),
        "bound_by_hand": flashed.as_ref().filter(|f| {
            f["via"].as_str() == Some("set_image")
        }),
        "verdict": verdict,
        "why": why,
        // AN EMPTY EPOCH IS NOT A BOOT, AND SAYING SO BEATS GUESSING BACKWARDS.
        //
        // A `session` marker records nothing, so provenance for it has no
        // evidence of its own and falls back to walking BACKWARDS by epoch id for
        // firmware banners. Once actuation epochs are back-dated to the stream
        // mark, the epoch that actually covers a marker's position can have a
        // HIGHER id and a LOWER offset -- measured on the Uno-Q, where epoch 631
        // is an empty marker at offset 5605020 and the boot covering it is 633 at
        // 5605018, two ids later. An agent asked 631 about a fingerprint the
        // board had printed and was told the epoch showed only Qualcomm firmware
        // strings, because the walk was looking the wrong way.
        //
        // Rather than widen the guess, name the epoch that holds the output.
        "recorded_nothing": (boot.bytes == 0).then_some(true),
        "covered_by": if boot.bytes == 0 {
            st.boot_covering_offset(boot.opened_offset as i64)
                .ok()
                .flatten()
                .filter(|cov| cov.id != boot.id)
                .map(|cov| json!({
                    "boot_id": cov.id,
                    "opened_by": cov.opened_by,
                    "bytes": cov.bytes,
                    "why": "this epoch recorded nothing; that is the epoch whose output \
                            covers this position in the stream. Ask it instead.",
                }))
        } else {
            None
        },
    }))
}

/// Severity ordering helper shared by the reports.
pub fn worst(a: Severity, b: Severity) -> Severity {
    if (a as i64) <= (b as i64) {
        a
    } else {
        b
    }
}

/// Two epochs compared: what fired, and how long each stage took.
///
/// The template diff answers "did the shape change". It cannot answer "did it
/// get slower", which is the other half of a regression and the half a boot
/// fingerprint deliberately ignores: two boots that reach the same stages in the
/// same order have the *same* fingerprint whether handoff took 40 ms or 4 s.
pub fn diff_boots(ctx: &Context, dev: &DeviceRow, a: i64, b: i64, limit: usize) -> Result<Value> {
    if a == b {
        return Err(ToolError::invalid_arg("a and b must be different epochs"));
    }
    ctx.with_store(dev, |st| {
        let (ba, bb) = (st.boot(a)?, st.boot(b)?);

        /// template id -> (text, count, severity): what a diff compares.
        type TemplateSnapshot = BTreeMap<i64, (String, i64, Option<String>)>;
        let load = |boot: i64| -> Result<TemplateSnapshot> {
            let rows = st.list_templates(&TemplateQuery {
                boot_id: Some(boot),
                limit: 100_000,
                ..Default::default()
            })?;
            Ok(rows
                .into_iter()
                .map(|t| (t.id, (t.text, t.scoped_count.unwrap_or(0), t.stage)))
                .collect())
        };
        let ma_all = load(a)?;
        let mb_all = load(b)?;

        // COMPARE ONLY THE STAGES BOTH EPOCHS ACTUALLY COVERED.
        //
        // Epoch boundaries do not all begin in the same place: one opened by a
        // power action contains the firmware stages, one opened by a reset or a
        // session may start at the kernel. Diffing those reported 497 templates
        // "gone from B" -- every firmware line, presented as a regression, when
        // B simply never covered that stage. A diff that cannot tell "this
        // stopped happening" from "I did not look" is worse than no diff,
        // because the phantoms are exactly the shape of a real regression.
        let stages_of = |m: &BTreeMap<i64, (String, i64, Option<String>)>| -> BTreeSet<String> {
            m.values().filter_map(|v| v.2.clone()).collect()
        };
        let sa = stages_of(&ma_all);
        let sb = stages_of(&mb_all);
        let common: BTreeSet<String> = sa.intersection(&sb).cloned().collect();
        let only_a: Vec<String> = sa.difference(&sb).cloned().collect();
        let only_b: Vec<String> = sb.difference(&sa).cloned().collect();

        // A template with no stage attributed cannot be placed, so it is kept:
        // dropping it would hide real changes to silence the phantoms.
        let keep = |m: BTreeMap<i64, (String, i64, Option<String>)>| {
            m.into_iter()
                .filter(|(_, v)| match &v.2 {
                    Some(stage) => common.is_empty() || common.contains(stage),
                    None => true,
                })
                .map(|(k, v)| (k, (v.0, v.1)))
                .collect::<BTreeMap<i64, (String, i64)>>()
        };
        let ma = keep(ma_all);
        let mb = keep(mb_all);
        let ids_a: BTreeSet<i64> = ma.keys().copied().collect();
        let ids_b: BTreeSet<i64> = mb.keys().copied().collect();

        let mut new_in_b: Vec<Value> = ids_b
            .difference(&ids_a)
            .map(|id| json!({"template_id": id, "text": mb[id].0, "count": mb[id].1}))
            .collect();
        let mut gone_from_b: Vec<Value> = ids_a
            .difference(&ids_b)
            .map(|id| json!({"template_id": id, "text": ma[id].0, "count": ma[id].1}))
            .collect();
        let mut shifted: Vec<Value> = ids_a
            .intersection(&ids_b)
            .filter(|id| ma[id].1 != mb[id].1)
            .map(|id| {
                json!({
                    "template_id": id, "text": ma[id].0,
                    "count_a": ma[id].1, "count_b": mb[id].1,
                    "delta": mb[id].1 - ma[id].1,
                })
            })
            .collect();

        // Stage timings, measured from each epoch's own open so two boots at
        // different wall times are comparable.
        let timings = |boot: &conminer_core::store::BootRow| -> Result<BTreeMap<String, i64>> {
            let mut out = BTreeMap::new();
            for s in st.stages(None, Some(boot.id))? {
                // First entry wins: a stage re-entered inside one epoch (a retry
                // loop) must not overwrite when it was first reached.
                out.entry(s.name)
                    .or_insert_with(|| s.entered_ts - boot.opened_at);
            }
            Ok(out)
        };
        let ta = timings(&ba)?;
        let tb = timings(&bb)?;
        let mut stage_deltas: Vec<Value> = Vec::new();
        for name in ta.keys().chain(tb.keys()).collect::<BTreeSet<_>>() {
            let (x, y) = (ta.get(name), tb.get(name));
            stage_deltas.push(json!({
                "stage": name,
                "reached_at_ms_a": x,
                "reached_at_ms_b": y,
                "delta_ms": match (x, y) { (Some(x), Some(y)) => Some(y - x), _ => None },
                "only_in": match (x, y) {
                    (Some(_), None) => Some("a"),
                    (None, Some(_)) => Some("b"),
                    _ => None,
                },
            }));
        }
        // Largest regression first: the answer to "what got slower" should be
        // the first line read, not something to be found by scanning. Stages
        // present in only one epoch have no delta and sort last — they are a
        // different finding ("it never got there"), not a zero-size one.
        stage_deltas.sort_by(|x, y| {
            let d = |v: &Value| v["delta_ms"].as_i64();
            match (d(x), d(y)) {
                (Some(a), Some(b)) => b.cmp(&a),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => x["stage"].as_str().cmp(&y["stage"].as_str()),
            }
        });

        let totals = json!({
            "new_in_b": new_in_b.len(),
            "gone_from_b": gone_from_b.len(),
            "count_shifted": shifted.len(),
        });
        let capped = new_in_b.len() > limit || gone_from_b.len() > limit || shifted.len() > limit;
        new_in_b.truncate(limit);
        gone_from_b.truncate(limit);
        shifted.truncate(limit);

        let summary = |x: &conminer_core::store::BootRow| {
            json!({
                "boot_id": x.id, "seq": x.seq, "bytes": x.bytes,
                "fingerprint": x.fingerprint, "outcome": x.outcome,
            })
        };
        Ok(json!({
            "a": summary(&ba),
            "b": summary(&bb),
            // Say what was NOT compared. A clean diff must not be mistaken for
            // "these boots agree" when one epoch never reached a stage at all.
            // The WINDOWS themselves, because two epochs of very different
            // length are not comparable however the stages line up: an epoch
            // opened by `power` spans firmware-to-userspace, one opened by
            // `mark` may span seconds. Reporting both durations makes the
            // asymmetry visible instead of leaving it to be inferred from a
            // suspiciously large gone_from_b.
            "window_a_ms": ba.closed_at.map(|c| c - ba.opened_at),
            "window_b_ms": bb.closed_at.map(|c| c - bb.opened_at),
            "opened_by_a": ba.opened_by.clone(),
            "opened_by_b": bb.opened_by.clone(),
            // And say it in words when the two windows are not comparable.
            // Reporting the durations was not enough on its own: the phantom
            // `gone_from_b: 497` that started this was read as a regression by
            // someone who had both numbers in front of them.
            "duration_note": duration_note(&ba, &bb),
            "stages_compared": common.iter().cloned().collect::<Vec<_>>(),
            "stages_only_in_a": only_a,
            "stages_only_in_b": only_b,
            "scope_note": if only_a.is_empty() && only_b.is_empty() {
                "both epochs cover the same stages; the diff is complete"
            } else {
                "these epochs do not cover the same stages, so the comparison was restricted \
                 to the stages they share -- lines from a stage only one epoch reached are NOT \
                 reported as changes"
            },
            "same_fingerprint": ba.fingerprint.is_some() && ba.fingerprint == bb.fingerprint,
            "totals": totals,
            "new_in_b": new_in_b,
            "gone_from_b": gone_from_b,
            "count_shifted": shifted,
            "stage_deltas": stage_deltas,
            "capped": capped,
        }))
    })
}

/// Aggregate templates across every epoch of two builds (§15.4).
///
/// Epoch diffs are positional (#443 vs #444); the question people actually ask
/// is "build A versus build B". Binding image identity to epochs is what turns
/// the second question into the first.
pub fn diff_builds(
    ctx: &Context,
    dev: &DeviceRow,
    a: &str,
    b: &str,
    limit: usize,
) -> Result<Value> {
    if a == b {
        return Err(ToolError::invalid_arg("a and b must be different builds"));
    }
    ctx.with_store(dev, |st| {
        let boots_a = st.boots_for_image(a)?;
        let boots_b = st.boots_for_image(b)?;
        for (r, boots) in [(a, &boots_a), (b, &boots_b)] {
            if boots.is_empty() {
                return Err(ToolError::new(
                    ErrorCode::UnknownBoot,
                    format!("no epochs are bound to build {r:?}"),
                )
                .with_hint("bind one with set_image, or flash through the flash hook"));
            }
        }
        let ma = st.template_counts_for_boots(&boots_a)?;
        let mb = st.template_counts_for_boots(&boots_b)?;

        let ids_a: BTreeSet<i64> = ma.keys().copied().collect();
        let ids_b: BTreeSet<i64> = mb.keys().copied().collect();

        let mut new_in_b: Vec<Value> = ids_b
            .difference(&ids_a)
            .map(|id| json!({"template_id": id, "text": mb[id].0, "count": mb[id].1}))
            .collect();
        let mut gone_from_b: Vec<Value> = ids_a
            .difference(&ids_b)
            .map(|id| json!({"template_id": id, "text": ma[id].0, "count": ma[id].1}))
            .collect();
        let mut shifted: Vec<Value> = ids_a
            .intersection(&ids_b)
            .filter(|id| ma[id].1 != mb[id].1)
            .map(|id| {
                json!({
                    "template_id": id, "text": ma[id].0,
                    "count_a": ma[id].1, "count_b": mb[id].1,
                    "delta": mb[id].1 - ma[id].1,
                })
            })
            .collect();

        let totals = json!({
            "new_in_b": new_in_b.len(),
            "gone_from_b": gone_from_b.len(),
            "count_shifted": shifted.len(),
        });
        let capped = new_in_b.len() > limit || gone_from_b.len() > limit || shifted.len() > limit;
        new_in_b.truncate(limit);
        gone_from_b.truncate(limit);
        shifted.truncate(limit);

        Ok(json!({
            "a": {"build": a, "epochs": boots_a.len()},
            "b": {"build": b, "epochs": boots_b.len()},
            "totals": totals,
            "new_in_b": new_in_b,
            "gone_from_b": gone_from_b,

            "count_shifted": shifted,
            "capped": capped,
        }))
    })
}

/// Judge a session or epoch against a declarative policy (§15.11).
///
/// This is the piece that turns the miner from a diagnostic tool into a
/// regression gate: a machine verdict with the evidence that produced it, so a
/// CI failure is actionable rather than a link to a log.
pub fn evaluate_policy(
    ctx: &Context,
    dev: &DeviceRow,
    args: &serde_json::Map<String, Value>,
) -> Result<Value> {
    let session = args.get("session").and_then(Value::as_i64);
    let boot = args.get("boot").and_then(Value::as_i64);
    let novel_only = args
        .get("novel_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let threshold: Severity = args
        .get("fail_at_or_above")
        .and_then(Value::as_str)
        .map(|s| serde_json::from_value(json!(s)).unwrap_or(Severity::Err))
        .unwrap_or(Severity::Err);
    let mut allow_templates: BTreeSet<i64> = args
        .get("allow_templates")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    // Standing verdicts are the allowlist by default. Requiring the caller to
    // pass one every time is what forced the agent to carry its own triage state
    // in its context window; the store already knows.
    let use_verdicts = args
        .get("use_verdicts")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let allow_fingerprints: BTreeSet<String> = args
        .get("allow_fingerprints")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    ctx.with_store(dev, |st| {
        let from_verdicts = if use_verdicts {
            st.templates_with_verdict(conminer_core::store::Verdict::Benign)?
        } else {
            BTreeSet::new()
        };
        allow_templates.extend(&from_verdicts);

        let templates = st.list_templates(&TemplateQuery {
            session_id: session,
            boot_id: boot,
            min_severity: Some(threshold),
            limit: 10_000,
            ..Default::default()
        })?;

        let mut violations = Vec::new();
        let mut waived = Vec::new();
        for t in &templates {
            let novel = session.is_some_and(|s| t.first_seen_session == s)
                || boot.is_some_and(|b| t.first_seen_boot == Some(b));
            let entry = json!({
                "template_id": t.id,
                "text": t.text,
                "severity": t.severity,
                "count": t.scoped_count.unwrap_or(t.total_count),
                "novel": novel,
            });
            if allow_templates.contains(&t.id) {
                waived.push(entry);
            } else if novel_only && !novel {
                // "Fail only on *novel* crash": a known-bad board still passes.
                waived.push(entry);
            } else {
                violations.push(entry);
            }
        }

        // A fingerprint on the allowlist waives its whole epoch: a known-flaky
        // boot signature should not fail every run it appears in.
        let fp = match boot {
            Some(b) => st.boot(b)?.fingerprint,
            None => st.latest_boot()?.and_then(|b| b.fingerprint),
        };
        let fingerprint_waived = fp
            .as_deref()
            .is_some_and(|f| allow_fingerprints.contains(f));
        if fingerprint_waived {
            waived.append(&mut violations);
        }

        let pass = violations.is_empty();
        let summary = if pass {
            format!(
                "PASS — nothing at or above {:?}{}",
                threshold,
                if waived.is_empty() {
                    String::new()
                } else {
                    format!(" ({} waived)", waived.len())
                }
            )
        } else {
            format!(
                "FAIL — {} template(s) at or above {:?}: {}",
                violations.len(),
                threshold,
                violations
                    .iter()
                    .take(3)
                    .filter_map(|v| v["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        };

        Ok(json!({
            "verdict": if pass { "pass" } else { "fail" },
            "summary": summary,
            "threshold": threshold,
            "novel_only": novel_only,
            "waived_by_stored_verdict": from_verdicts.len(),
            "fingerprint": fp,
            "fingerprint_waived": fingerprint_waived,
            "violations": violations,
            "waived": waived,
        }))
    })
}

/// Whether two epochs cover comparable spans of time, in words.
///
/// A ratio, not a difference: two boots of 40 s and 44 s are the same boot; a
/// 40 s epoch against a 3 s `mark` epoch is not a comparison at all, and every
/// template the short one lacks will show up as a regression.
fn duration_note(a: &conminer_core::store::BootRow, b: &conminer_core::store::BootRow) -> String {
    let span = |x: &conminer_core::store::BootRow| x.closed_at.map(|c| c - x.opened_at);
    match (span(a), span(b)) {
        (Some(wa), Some(wb)) if wa > 0 && wb > 0 => {
            let (lo, hi) = if wa <= wb { (wa, wb) } else { (wb, wa) };
            if hi as f64 / lo as f64 >= 2.0 {
                format!(
                    "these epochs are not the same length: {} ms vs {} ms ({:.1}x). Most of the \
                     difference below is coverage, not regression -- the shorter epoch simply did \
                     not run long enough to emit what the longer one did.",
                    wa,
                    wb,
                    hi as f64 / lo as f64
                )
            } else {
                format!("comparable windows ({wa} ms vs {wb} ms)")
            }
        }
        _ => "one of these epochs is still open, so its window is not final and the diff is a \
              snapshot rather than a comparison of two finished boots"
            .to_string(),
    }
}

/// §F3. Stage timings across recent epochs, with the trend.
///
/// "Is boot getting slower?" took hand-assembly across three `boot_report` calls
/// and two searches. Every number needed is already in the stage table; nothing
/// here is new capture, only arithmetic somebody was doing by hand.
pub fn stage_timings(
    ctx: &Context,
    dev: &DeviceRow,
    last: usize,
    stage_filter: Option<&str>,
) -> Result<Value> {
    ctx.with_store(dev, |st| {
        let mut series = Vec::new();
        // Per-stage samples, for the summary underneath.
        let mut samples: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();

        for b in st.list_boots(last.clamp(1, 500))? {
            let stages = st.stages(None, Some(b.id))?;
            if stages.is_empty() {
                // Skipped, never zero-filled: an epoch with no stages did not
                // take zero milliseconds, it produced no evidence at all, and a
                // zero would drag every average it touched.
                continue;
            }
            // Relative to the action that opened the epoch when there was one,
            // else to the first banner: "1.2 s into the boot" means nothing if
            // the clock starts at an arbitrary moment.
            let anchor = b.opened_at.min(stages[0].entered_ts);
            let mut rows = Vec::new();
            for (i, sg) in stages.iter().enumerate() {
                // A stage lasts until the next one starts; the last one lasts
                // until the epoch closed, and is omitted while it is still open
                // rather than reported as ending now.
                let end = stages
                    .get(i + 1)
                    .map(|n| n.entered_ts)
                    .or(sg.exited_ts)
                    .or(b.closed_at);
                let duration = end.map(|e| (e - sg.entered_ts).max(0));
                if let Some(f) = stage_filter {
                    if sg.name != f {
                        continue;
                    }
                }
                if let Some(d) = duration {
                    samples.entry(sg.name.clone()).or_default().push((b.seq, d));
                }
                rows.push(json!({
                    "name": sg.name,
                    "entered_rel_ms": (sg.entered_ts - anchor).max(0),
                    "duration_ms": duration,
                }));
            }
            let to_userspace = stages
                .iter()
                .find(|s| matches!(s.name.as_str(), "userspace" | "android"))
                .map(|s| (s.entered_ts - anchor).max(0));
            series.push(json!({
                "boot_id": b.id,
                "seq": b.seq,
                "label": b.label,
                "opened_by": b.opened_by,
                "stages": rows,
                "total_to_userspace_ms": to_userspace,
            }));
        }

        let per_stage: Vec<Value> = samples
            .iter()
            .map(|(name, pts)| {
                let vals: Vec<i64> = pts.iter().map(|(_, d)| *d).collect();
                let n = vals.len() as f64;
                let mean = vals.iter().sum::<i64>() as f64 / n;
                json!({
                    "name": name,
                    "samples": vals.len(),
                    "min": vals.iter().min(),
                    "max": vals.iter().max(),
                    "mean": mean.round() as i64,
                    // Least squares over epoch sequence: the answer to "is it
                    // getting slower", as a number rather than an impression.
                    "trend_ms_per_boot": trend(pts),
                })
            })
            .collect();

        Ok(json!({
            "series": series,
            "stats": {"per_stage": per_stage},
        }))
    })
}

/// Least-squares slope of value against epoch sequence.
fn trend(points: &[(i64, i64)]) -> Option<f64> {
    if points.len() < 3 {
        // Two points always describe a perfect line, which would report a
        // confident trend from no evidence.
        return None;
    }
    let n = points.len() as f64;
    let (sx, sy): (f64, f64) = points
        .iter()
        .fold((0.0, 0.0), |(x, y), (a, b)| (x + *a as f64, y + *b as f64));
    let (mx, my) = (sx / n, sy / n);
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in points {
        let dx = *x as f64 - mx;
        num += dx * (*y as f64 - my);
        den += dx * dx;
    }
    if den == 0.0 {
        return None;
    }
    Some(((num / den) * 100.0).round() / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_character_prompt_is_rejected() {
        for bad in [":", "$", "^:$", "\\$"] {
            assert!(
                validate_prompt_distinctiveness(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        for good in ["=> ", "uart:~\\$ ", "root@board:.*# ", "grub> "] {
            validate_prompt_distinctiveness(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    #[test]
    fn worst_picks_the_more_severe() {
        assert_eq!(worst(Severity::Warn, Severity::Emerg), Severity::Emerg);
        assert_eq!(worst(Severity::Info, Severity::Unknown), Severity::Info);
    }
}
