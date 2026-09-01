//! Suite `follow` (§8.2, §13) — incremental consumption.
//!
//! Edge cases: each predicate type fires correctly · `any` returns the identity
//! of the first match · cursor gap-freeness across rapid increments (property) ·
//! a cursor survives a reconnect · `CURSOR_EXPIRED` after a prune · a timeout
//! returns data rather than an error · concurrent multi-agent follows.

use conminer_core::follow::{clamp_timeout, increment, Predicate, PromptSet};
use conminer_core::store::{Cursor, DeviceStore, SessionSource};
use conminer_core::ErrorCode;
use conminer_testkit::Rig;
use proptest::prelude::*;
use serde_json::json;

fn prompt_set() -> PromptSet {
    use conminer_core::framer::profile::PromptKind;
    use conminer_core::runner::{Prompt, Prompts};
    PromptSet {
        prompts: Prompts(vec![
            Prompt {
                re: regex::Regex::new(r"(^|\n)# $").unwrap(),
                raw: r"(^|\n)# $".into(),
                kind: PromptKind::Shell,
            },
            Prompt {
                re: regex::Regex::new(r"login: *$").unwrap(),
                raw: r"login: *$".into(),
                kind: PromptKind::CredentialGate,
            },
        ]),
    }
}

fn feed(rig: &Rig, name: &str, chunks: &[&str]) -> (DeviceStore, Cursor) {
    let mut p = rig.pipeline(name, None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    for c in chunks {
        p.feed(c.as_bytes()).unwrap();
    }
    p.finish().unwrap();
    (p.into_store(), start)
}

/// `feed`, but leaving the console LIVE at whatever it is sitting on.
///
/// THE REASON THE SUITE MISSED REPORT #4. `feed` ends with `finish()`, which
/// flushes the splitter -- so an unterminated prompt became a stored line
/// before any assertion ran, and every prompt test was really testing the
/// terminated case. A capture loop never calls `finish` mid-stream: it ticks,
/// and a partial that survives a tick unchanged is published as the pending
/// tail. That is the state a board sitting at `sirocco> ` is actually in.
fn feed_live(rig: &Rig, name: &str, chunks: &[&str]) -> (DeviceStore, Cursor) {
    let mut p = rig.pipeline(name, None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    for c in chunks {
        p.feed(c.as_bytes()).unwrap();
    }
    // Twice: the partial is published only once it has stopped changing.
    p.tick().unwrap();
    p.tick().unwrap();
    (p.into_store(), start)
}

fn inc(
    store: &DeviceStore,
    from: &Cursor,
    until: &Predicate,
    now: i64,
) -> conminer_core::follow::Increment {
    increment(store, from, until, 50, &prompt_set(), now).unwrap()
}

// ------------------------------------------------------------- predicates ---

#[test]
fn the_pattern_predicate_fires_on_the_line_that_matched() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f1",
        &["[ 1.0] mmc0: ready\n[ 1.4] Kernel panic - not syncing: VFS\n"],
    );
    let r = inc(&store, &cur, &Predicate::Pattern("Kernel panic".into()), 0);
    assert_eq!(r.matched.as_deref(), Some("pattern:Kernel panic"));
    assert!(r.evidence.unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("Kernel panic"));
}

#[test]
fn the_template_new_predicate_fires_on_a_crash_never_seen_before() {
    let rig = Rig::new();
    let (store, cur) = feed(&rig, "f2", &["[ 1.0] mmc0: ready\n"]);
    let r = inc(&store, &cur, &Predicate::TemplateNew, 0);
    assert_eq!(r.matched.as_deref(), Some("template:new"));
    assert!(!r.new_templates.is_empty());
}

#[test]
fn the_stage_predicate_fires_when_the_boot_reaches_it() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f3",
        &["NOTICE:  BL31: v2.11(release):v2.11\n[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n"],
    );
    let r = inc(&store, &cur, &Predicate::Stage("kernel".into()), 0);
    assert_eq!(r.matched.as_deref(), Some("stage:kernel"));
    assert!(r.stages.iter().any(|s| s["name"] == "kernel"));

    // A stage that never arrived does not fire.
    let none = inc(&store, &cur, &Predicate::Stage("userspace".into()), 0);
    assert!(none.matched.is_none());
}

#[test]
fn the_prompt_predicate_is_the_boot_finished_signal() {
    let rig = Rig::new();
    let (store, cur) = feed(&rig, "f4", &["[ 1.0] Run /sbin/init as init process\n# "]);
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert_eq!(r.matched.as_deref(), Some("prompt"));
}

#[test]
fn the_prompt_predicate_never_fires_on_a_credential_gate() {
    let rig = Rig::new();
    let (store, cur) = feed(&rig, "f5", &["[ 1.0] Run /sbin/init\nboard login: "]);
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert!(
        r.matched.is_none(),
        "the board is up but not commandable; firing here would send the \
         agent's next command into a password field"
    );
}

#[test]
fn the_quiet_predicate_is_settle_detection() {
    let rig = Rig::new();
    let (store, cur) = feed(&rig, "f6", &["[ 1.0] last line\n"]);
    let last = store.recent_lines(1).unwrap()[0].ts_wall;

    assert!(inc(&store, &cur, &Predicate::QuietMs(30_000), last + 1_000)
        .matched
        .is_none());
    let r = inc(&store, &cur, &Predicate::QuietMs(30_000), last + 31_000);
    assert_eq!(r.matched.as_deref(), Some("quiet:30000"));
    assert!(r.evidence.unwrap()["idle_ms"].as_i64().unwrap() >= 30_000);
}

#[test]
fn the_reset_predicate_fires_when_an_epoch_opens() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f7",
        &[
            "NOTICE:  BL1: v2.11(release):v2.11\n[ 0.0] Linux version 6.12.9 (b@h) (gcc)\n\
           NOTICE:  BL1: v2.11(release):v2.11\n",
        ],
    );
    let r = inc(&store, &cur, &Predicate::Reset, 0);
    assert_eq!(r.matched.as_deref(), Some("reset"));
    assert!(!r.boots_opened.is_empty());
}

#[test]
fn any_returns_which_predicate_actually_fired() {
    // The one-call dev loop: "tell me when it is up or crashed".
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f8",
        &["[ 1.4] Kernel panic - not syncing: VFS: Unable to mount root fs\n"],
    );
    let any = Predicate::Any(vec![
        Predicate::Prompt,
        Predicate::Pattern("Kernel panic".into()),
        Predicate::QuietMs(999_999),
    ]);
    let r = inc(&store, &cur, &any, 0);
    assert_eq!(
        r.matched.as_deref(),
        Some("pattern:Kernel panic"),
        "the response must say *which* predicate woke it, not just that it woke"
    );
}

#[test]
fn a_timeout_returns_the_data_so_far_rather_than_an_error() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f9",
        &["[ 1.0] mmc0: ready\n[ 1.1] mmc0: still fine\n"],
    );
    let r = inc(&store, &cur, &Predicate::Pattern("never appears".into()), 0);
    assert!(r.matched.is_none(), "nothing fired");
    assert_eq!(r.lines, 2, "…but the data is still there");
    assert!(r.bytes > 0);
    assert!(!r.tail.is_empty());
}

// ---------------------------------------------------------------- cursors ---

#[test]
fn repeated_increments_are_gap_free_and_never_repeat_a_line() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f10", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let mut cursor = p.store().head_cursor();

    let mut seen = Vec::new();
    for round in 0..12 {
        for i in 0..7 {
            p.feed(format!("[ 1.0] round {round} line {i}\n").as_bytes())
                .unwrap();
        }
        let r = increment(
            p.store(),
            &cursor,
            &Predicate::QuietMs(i64::MAX),
            1000,
            &prompt_set(),
            0,
        )
        .unwrap();
        seen.extend(r.tail.iter().map(|l| l["line_id"].as_i64().unwrap()));
        cursor = Cursor::decode(&r.cursor).unwrap();
    }
    assert_eq!(seen.len(), 12 * 7, "every line exactly once");
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "no repeats");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "and in order");
}

#[test]
fn a_cursor_survives_the_store_being_reopened() {
    let rig = Rig::new();
    let dev = rig.device("f11");
    let encoded = {
        let (store, _) = feed(&rig, "f11", &["[ 1.0] before\n"]);
        store.head_cursor().encode()
    };
    // As a reconnecting agent, or a restarted mcpd, would.
    let store = rig.store(&dev);
    let cur = Cursor::decode(&encoded).unwrap();
    store
        .resolve_cursor(&cur)
        .expect("cursors outlive connections");
}

#[test]
fn a_cursor_behind_the_retention_horizon_expires_and_says_where_to_re_anchor() {
    let rig = Rig::new();
    let dev = rig.device("f12");
    let old = {
        let (store, cur) = feed(&rig, "f12", &["[ 1.0] a\n[ 1.1] b\n[ 1.2] c\n"]);
        let _ = store;
        cur
    };
    let mut store = rig.store(&dev);
    store.prune_before(store.stream_offset()).unwrap();

    let err = increment(&store, &old, &Predicate::Prompt, 10, &prompt_set(), 0).unwrap_err();
    assert_eq!(err.code, ErrorCode::CursorExpired);
    let d = err.detail.unwrap();
    assert!(d["head"].as_str().unwrap().contains(':'));
}

#[test]
fn a_cursor_from_another_device_is_refused() {
    let rig = Rig::new();
    let (a, _) = feed(&rig, "f13a", &["[ 1.0] a\n"]);
    let (b, _) = feed(&rig, "f13b", &["[ 1.0] b\n"]);
    let err = increment(
        &b,
        &a.head_cursor(),
        &Predicate::Prompt,
        10,
        &prompt_set(),
        0,
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidCursor);
}

#[test]
fn a_boot_loop_returns_count_deltas_not_forty_times_the_output() {
    // Forty iterations between calls come back as counts, which is what makes
    // each increment cheap.
    let rig = Rig::new();
    let mut p = rig.pipeline("f14", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[ 1.0] mmc0: ready\n").unwrap();
    let cursor = p.store().head_cursor();
    for _ in 0..40 {
        p.feed(b"[ 1.0] mmc0: ready\n").unwrap();
    }
    let r = increment(
        p.store(),
        &cursor,
        &Predicate::QuietMs(i64::MAX),
        5,
        &prompt_set(),
        0,
    )
    .unwrap();
    assert_eq!(r.lines, 40);
    assert!(r.tail.len() <= 5, "the raw tail is capped");
    assert!(
        r.new_templates.is_empty(),
        "nothing novel: this template was already known"
    );
    assert_eq!(r.template_deltas.len(), 1);
    assert_eq!(r.template_deltas[0]["count"], 40);
}

#[test]
fn several_agents_can_follow_the_same_device_at_different_positions() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f15", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[ 1.0] first\n").unwrap();
    let early = p.store().head_cursor();
    p.feed(b"[ 1.1] second\n").unwrap();
    let late = p.store().head_cursor();
    p.feed(b"[ 1.2] third\n").unwrap();

    let a = increment(
        p.store(),
        &early,
        &Predicate::QuietMs(i64::MAX),
        50,
        &prompt_set(),
        0,
    )
    .unwrap();
    let b = increment(
        p.store(),
        &late,
        &Predicate::QuietMs(i64::MAX),
        50,
        &prompt_set(),
        0,
    )
    .unwrap();
    assert_eq!(a.lines, 2);
    assert_eq!(b.lines, 1);
    assert_eq!(a.cursor, b.cursor, "both end at the same head");
}

// ------------------------------------------------------------- validation ---

#[test]
fn timeouts_are_clamped_to_the_configured_ceiling() {
    assert_eq!(clamp_timeout(Some(1_000_000), 30, 600).unwrap(), 600);
    assert_eq!(clamp_timeout(None, 30, 600).unwrap(), 30);
    assert_eq!(
        clamp_timeout(Some(-1), 30, 600).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn predicates_round_trip_from_their_wire_form() {
    let p = Predicate::parse(&json!({
        "any": [{"prompt": true}, {"template": "new"}, {"quiet": 30000}]
    }))
    .unwrap();
    match p {
        Predicate::Any(v) => assert_eq!(v.len(), 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        Predicate::parse(&json!({"nonsense": 1})).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

proptest! {
    /// Gap-freeness across arbitrary increment sizes: whatever the chunking,
    /// following from a cursor sees every line exactly once.
    #[test]
    fn prop_increments_are_gap_free(chunks in proptest::collection::vec(1usize..6, 1..12)) {
        let rig = Rig::new();
        let mut p = rig.pipeline("prop", None);
        p.begin_session(SessionSource::Live, None, None, None).unwrap();
        let mut cursor = p.store().head_cursor();

        let mut seen: Vec<i64> = Vec::new();
        let mut written = 0usize;
        for (round, n) in chunks.iter().enumerate() {
            for i in 0..*n {
                p.feed(format!("[ 1.0] r{round} l{i}\n").as_bytes()).unwrap();
                written += 1;
            }
            let r = increment(
                p.store(), &cursor, &Predicate::QuietMs(i64::MAX), 1000, &prompt_set(), 0,
            ).unwrap();
            seen.extend(r.tail.iter().map(|l| l["line_id"].as_i64().unwrap()));
            cursor = Cursor::decode(&r.cursor).unwrap();
        }
        prop_assert_eq!(seen.len(), written);
        let mut d = seen.clone();
        d.sort();
        d.dedup();
        prop_assert_eq!(d.len(), seen.len());
    }
}

/// A PROMPT FROM THE PREVIOUS EPOCH IS NOT THIS EPOCH REACHING A PROMPT.
///
/// `follow` with no cursor starts a grace window BEFORE the epoch boundary, so
/// the first seconds of a boot are not missed -- they are attributed to the
/// epoch that is closing, because the boundary is placed when the tool ran and
/// not when the board acted. That same grace contains the PREVIOUS epoch's
/// prompt.
///
/// Measured on the Uno-Q: a reset opened epoch 276 and `until:{prompt:true}`
/// returned immediately with `sirocco> ` from line 66730, while epoch 276 had
/// got as far as `APP admit` / `CONSOLE`. The agent then drove commands at a
/// board that was still booting.
#[test]
fn the_prompt_predicate_ignores_a_prompt_from_before_this_epoch() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f-grace",
        &[
            // The old epoch, sitting at its prompt...
            "NOTICE:  BL1: v2.11(release):v2.11\nstarting up\n# \n",
            // ...a REPEATED boot banner is what opens a new epoch...
            "NOTICE:  BL1: v2.11(release):v2.11\n",
            // ...which has NOT reached a prompt.
            "APP admit\nCONSOLE\n",
        ],
    );
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    // NON-VACUITY: two epochs must really exist, and the prompt must really be
    // in the window. A fixture with one epoch would pass this test for the
    // wrong reason -- it did, on the first attempt.
    assert!(
        store.list_boots(10).unwrap().len() >= 2,
        "the fixture must actually open a second epoch, or this proves nothing"
    );
    assert_eq!(
        r.matched, None,
        "the only prompt in the window belongs to the previous epoch: {:?}",
        r.evidence
    );
}

/// ...and when THIS epoch reaches its prompt, it fires, naming the epoch it
/// belongs to. Without this the fix above would just be a broken predicate.
#[test]
fn the_prompt_predicate_still_fires_for_the_current_epoch() {
    let rig = Rig::new();
    let (store, cur) = feed(
        &rig,
        "f-grace-ok",
        &[
            "NOTICE:  BL1: v2.11(release):v2.11\nstarting up\n# \n",
            "NOTICE:  BL1: v2.11(release):v2.11\n",
            "APP admit\nCONSOLE\n# \n",
        ],
    );
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert_eq!(r.matched.as_deref(), Some("prompt"), "{:?}", r.evidence);
    let ev = r.evidence.expect("evidence");
    let latest = store.latest_boot().unwrap().expect("an epoch").id;
    assert_eq!(
        ev["boot_id"], latest,
        "the prompt must be attributed to the epoch being waited on: {ev}"
    );
}

// ------------------------------------------- an unterminated prompt (#4) ---

/// A PROMPT WITH NO NEWLINE IS STILL A PROMPT.
///
/// Report #4, boot 473 on the Uno-Q: the board printed `CONSOLE` and then
/// `\r\nsirocco> ` and stopped. `console_state` read the partial and said
/// `at_prompt`, `commandable: true`; `follow {until:{prompt:true}}` scanned
/// stored lines only, found nothing, and timed out after 30 s telling the agent
/// the console was hung. Forcing a newline with `run_command` made the same
/// prompt appear instantly. The board was fine; the reader was not.
#[test]
fn the_prompt_predicate_fires_on_a_prompt_that_never_ended_its_line() {
    let rig = Rig::new();
    let (store, cur) = feed_live(&rig, "f-partial", &["[ 1.0] APP admit\nCONSOLE\n# "]);
    // NON-VACUITY: the prompt must really be unterminated. If `# ` reached the
    // stored lines, this passes through the OLD path and proves nothing.
    assert!(
        store.pending_tail().unwrap().is_some(),
        "the fixture must leave a live partial, or this tests the terminated case"
    );
    assert!(
        !store
            .recent_lines(50)
            .unwrap()
            .iter()
            .any(|l| l.lossy().trim_end() == "#"),
        "the prompt must NOT be a stored line, or this tests the terminated case"
    );
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert_eq!(r.matched.as_deref(), Some("prompt"), "{:?}", r.evidence);
    let ev = r.evidence.expect("evidence");
    assert_eq!(
        ev["unterminated"], true,
        "the evidence must say the prompt has no newline yet: {ev}"
    );
}

/// ...and a credential gate is still not a prompt when it is unterminated.
/// `login: ` sits in the partial buffer exactly like a shell prompt does, so
/// the new path had to carry the commandable rule with it (§8.5).
#[test]
fn an_unterminated_credential_gate_is_still_not_a_prompt() {
    let rig = Rig::new();
    let (store, cur) = feed_live(&rig, "f-partial-gate", &["[ 1.0] Run /sbin/init\nlogin: "]);
    assert!(
        store.pending_tail().unwrap().is_some(),
        "the fixture must leave a live partial"
    );
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert!(
        r.matched.is_none(),
        "firing here would send the agent's next command into a password \
         field: {:?}",
        r.evidence
    );
}

/// ...and a partial that is not a prompt does not fire. A console mid-boot has
/// an unterminated tail too; only a tail that CLASSIFIES as a commandable
/// prompt counts.
#[test]
fn a_partial_that_is_not_a_prompt_does_not_fire() {
    let rig = Rig::new();
    let (store, cur) = feed_live(
        &rig,
        "f-partial-grace",
        &[
            // The old epoch, sitting at its prompt...
            "NOTICE:  BL1: v2.11(release):v2.11\nstarting up\n# \n",
            // ...a repeated boot banner opens a new epoch...
            "NOTICE:  BL1: v2.11(release):v2.11\n",
            // ...which has NOT reached a prompt. It is mid-boot, unterminated.
            "APP admit\nCONSOLE\nprobing ",
        ],
    );
    assert!(
        store.list_boots(10).unwrap().len() >= 2,
        "the fixture must actually open a second epoch, or this proves nothing"
    );
    let r = inc(&store, &cur, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched, None,
        "the only prompt in the window belongs to the previous epoch: {:?}",
        r.evidence
    );
}

/// ...and the fix must not undo the boundary-grace rule through the back door.
///
/// THE DECISIVE SCOPING GATE. The live path reads the console TAIL, and the
/// tail is the newest bytes in the store regardless of which epoch they belong
/// to. An epoch opened by actuation is the sharp case: `power` opens it and it
/// has no output of its own yet, so the newest stored line is the previous
/// boot's prompt. Unscoped, this answers "the board is already at its prompt"
/// with the prompt of the boot the power cycle just killed -- #59 all over
/// again, reached through the new path instead of the old one.
#[test]
fn a_prompt_from_the_epoch_a_power_cycle_ended_is_not_this_boot_reaching_one() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-partial-power", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\nstarting up\n# \n")
        .unwrap();
    p.tick().unwrap();
    // Power-cycle it: a new epoch, with nothing in it yet.
    p.open_boot("power", None).unwrap();
    let store = p.into_store();

    // NON-VACUITY: the previous epoch's prompt must really be the newest stored
    // line, or an unscoped read would not reach it either and this proves
    // nothing.
    let newest = store.recent_lines(1).unwrap();
    assert_eq!(
        newest[0].lossy().trim_end(),
        "#",
        "the fixture must leave the OLD prompt as the newest line in the store"
    );
    let latest = store.latest_boot().unwrap().expect("an epoch");
    assert_ne!(
        newest[0].boot_id,
        Some(latest.id),
        "the fixture must leave that prompt in a DIFFERENT epoch to the current one"
    );

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched, None,
        "the board was just power-cycled; the only prompt in the store belongs \
         to the epoch that ended: {:?}",
        r.evidence
    );
}

/// AN EPOCH IS NOT A BOOT, and this is what confusing them cost.
///
/// THE ACTUAL CAUSE OF REPORT #4. A `session` epoch opens every time capture
/// reconnects -- so every deploy adds one -- without the board restarting.
/// `until:{prompt:true}` compared each line's `boot_id` against the NEWEST
/// epoch, so once a session epoch sat on top of the power-on that reached the
/// prompt, every line of that boot was skipped and the follow timed out
/// against a board that had been at its shell for twenty minutes.
///
/// Measured on the Uno-Q: epoch 473 `opened_by=power`, 23,399 bytes, reached
/// `sirocco> `; epochs 474-477 `opened_by=session`, 0 bytes each. `run_command`
/// appeared to fix it only because the newline it forced landed in the current
/// epoch.
#[test]
fn a_prompt_reached_before_a_session_epoch_is_still_this_boot_at_its_prompt() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-session-epochs", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    // The board is powered on and boots to its prompt.
    p.open_boot("power", None).unwrap();
    p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\nAPP admit\nCONSOLE\n# \n")
        .unwrap();
    // Then capture reconnects, repeatedly. The board does not move.
    for _ in 0..4 {
        p.open_boot("session", None).unwrap();
    }
    p.tick().unwrap();
    let store = p.into_store();

    // NON-VACUITY: the fixture must really put the prompt in an OLDER epoch
    // than the current one, and the newer ones must really be empty sessions.
    let boots = store.list_boots(6).unwrap();
    assert_eq!(
        boots[0].opened_by, "session",
        "the newest epoch must be a session marker: {boots:?}"
    );
    assert_eq!(boots[0].bytes, 0, "and it must be empty: {boots:?}");
    let prompt_line = store
        .recent_lines(1)
        .unwrap()
        .first()
        .expect("a stored prompt line")
        .boot_id;
    assert_ne!(
        prompt_line,
        Some(boots[0].id),
        "the prompt must live in an EARLIER epoch, or this proves nothing"
    );

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched.as_deref(),
        Some("prompt"),
        "the board is sitting at the prompt this boot reached; empty session \
         epochs stacked on top do not un-reach it: {:?}",
        r.evidence
    );
}

/// ...and a power cycle still ends the boot. The floor moves to the actuation,
/// so the prompt of the boot that was just killed is out of scope again.
#[test]
fn a_power_cycle_still_ends_the_boot_that_reached_a_prompt() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-session-then-power", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    p.open_boot("power", None).unwrap();
    p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\nAPP admit\nCONSOLE\n# \n")
        .unwrap();
    p.open_boot("session", None).unwrap();
    // Now actually power-cycle it: the previous prompt is history.
    p.open_boot("power", None).unwrap();
    p.tick().unwrap();
    let store = p.into_store();

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched, None,
        "the board was power-cycled after reaching that prompt: {:?}",
        r.evidence
    );
}

/// THE SEQUENCE THE BOARD ACTUALLY RAN (report #4, regression on boot 483).
///
/// A `power` epoch opened by mcpd, adopted by the capture loop, the board boots
/// and stops at an UNTERMINATED `sirocco> `. `follow {until:{prompt:true}}`
/// timed out at 45 s with `console=hung` and `boot_report=hung`, and the next
/// `run_command` found the prompt sitting in the preamble.
///
/// The earlier gate for this used a fixture with no actuation epoch at all,
/// which is why it passed while the board did not.
#[test]
fn a_power_epoch_that_ends_at_an_unterminated_prompt_is_at_that_prompt() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-power-partial", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    // mcpd actuates: the epoch is opened by `power`.
    p.open_boot("power", None).unwrap();
    // The board boots and stops at its prompt, with no trailing newline.
    p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\r\nAPP admit\r\nCONSOLE\r\n# ")
        .unwrap();
    p.tick().unwrap();
    p.tick().unwrap();
    let store = p.into_store();

    // NON-VACUITY: the prompt must really be unterminated, and the epoch must
    // really be the power-on.
    assert_eq!(
        store.list_boots(1).unwrap()[0].opened_by,
        "power",
        "the fixture must reproduce an actuated epoch"
    );
    assert!(
        !store
            .recent_lines(50)
            .unwrap()
            .iter()
            .any(|l| l.lossy().trim_end() == "#"),
        "the prompt must NOT be a stored line, or this tests the terminated case"
    );

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched.as_deref(),
        Some("prompt"),
        "the board is sitting at the prompt this power-on reached: {:?}",
        r.evidence
    );
}

/// A BOARD CAN FINISH BOOTING BEFORE ITS OWN POWER EPOCH IS RECORDED.
///
/// THE MEASURED CAUSE OF THE #4 REGRESSION (boot 483 on the Uno-Q). The partial
/// buffer held `sirocco> ` stamped 1786896518281 -- the same instant as the
/// `CONSOLE` line -- while epoch 483 (`opened_by=power`) was stamped
/// 1786896528401, 10.1 SECONDS LATER. `pending_partial` invalidates the partial
/// whenever an actuation epoch opened after it was seen, on the assumption that
/// such an epoch means a reboot the partial predates. On a board that boots in
/// well under a second, while `power` spends up to 8 s in its off phase before
/// the epoch is recorded, that assumption is inverted: the board's own output is
/// stamped BEFORE the epoch that contains it.
///
/// One discarded partial explains all three symptoms the agent reported at
/// once: `follow` matched nothing, `console_state` said hung, `boot_report`
/// said hung.
#[test]
fn a_board_that_booted_before_its_power_epoch_was_recorded_is_still_at_its_prompt() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-late-power-epoch", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();

    // mcpd records the power epoch with the time its hook FINISHED, 10 s after
    // the board had already booted and gone quiet.
    let late = rig.clock.now_wall_ms() + 10_120;
    let boot = p
        .store_mut()
        .open_boot("power", None, late, Some(sid))
        .unwrap();
    p.adopt_external_boot().unwrap();

    // The board boots fast and stops at an unterminated prompt.
    p.feed(b"SMP PSCI CPU_ON SGI PASS\r\nAPP admit\r\nCONSOLE\r\n# ")
        .unwrap();
    p.tick().unwrap();
    p.tick().unwrap();
    let store = p.into_store();

    // NON-VACUITY: reproduce the measured ordering, or this proves nothing.
    let (partial, seen_at) = store
        .pending_tail()
        .unwrap()
        .expect("the console must be sitting on an unterminated prompt");
    assert_eq!(partial.trim_end(), "#", "the partial must be the prompt");
    let row = store.boot(boot.id).unwrap();
    assert_eq!(row.opened_by, "power", "the epoch must be an actuation");
    assert!(
        row.opened_at > seen_at,
        "the epoch must be stamped AFTER the partial ({} vs {}), which is the \
         whole point of this fixture",
        row.opened_at,
        seen_at
    );
    assert!(row.bytes > 0, "and the boot's output must be inside it");

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched.as_deref(),
        Some("prompt"),
        "the board is sitting at the prompt this power-on reached: {:?}",
        r.evidence
    );
}

/// ...and the case the invalidation exists for must still work. A power cycle
/// that leaves the board SILENT means the prompt in the partial buffer belongs
/// to the boot that was just killed, and must not be reported as this one.
/// The distinguisher is output inside the actuated epoch, not the clock.
#[test]
fn a_prompt_in_the_partial_is_dropped_when_a_power_cycle_leaves_the_board_silent() {
    let rig = Rig::new();
    let mut p = rig.pipeline("f-dark-after-power", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let start = p.store().head_cursor();
    // A board sitting at its prompt, unterminated...
    p.feed(b"APP admit\r\nCONSOLE\r\n# ").unwrap();
    p.tick().unwrap();
    p.tick().unwrap();
    // ...is power-cycled, and says NOTHING afterwards.
    let later = rig.clock.now_wall_ms() + 5_000;
    let boot = p
        .store_mut()
        .open_boot("power", None, later, Some(sid))
        .unwrap();
    p.adopt_external_boot().unwrap();
    p.tick().unwrap();
    let store = p.into_store();

    // NON-VACUITY: the actuated epoch must really be empty and really newer.
    let row = store.boot(boot.id).unwrap();
    assert_eq!(row.bytes, 0, "the board must have said nothing since");
    assert_eq!(row.opened_by, "power");

    let r = inc(&store, &start, &Predicate::Prompt, 0);
    assert_eq!(
        r.matched, None,
        "that prompt belongs to the boot the power cycle killed: {:?}",
        r.evidence
    );
}
