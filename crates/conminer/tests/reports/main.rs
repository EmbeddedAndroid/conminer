//! Suite `reports` (§R) — agents filing bugs about conminer itself.
//!
//! The failure this guards against is not a crash: it is a queue that quietly
//! loses things. A report that vanishes, a duplicate that hides a second
//! reporter, a "fixed" that nobody can check, or a regression counted as just
//! another duplicate all end the same way, with an agent hitting a known defect
//! and nobody knowing it came back.

use conminer_core::reports::{self as rep, NewReport, Sighting};
use conminer_core::store::Registry;

fn reg() -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let r = Registry::open(dir.path()).unwrap();
    (dir, r)
}

/// A report whose expected/observed DIFFER per title: the first version of this
/// helper gave every report the same pair, so a search for "hung prompt" matched
/// reports about something else entirely and the gate lied about why.
fn report(title: &str) -> NewReport {
    NewReport {
        title: title.into(),
        expected: Some(format!("expected for: {title}")),
        observed: Some(format!("observed for: {title}")),
        build: Some("build-1".into()),
        reporter: Some("agent-a".into()),
        ..Default::default()
    }
}

#[test]
fn a_report_keeps_the_evidence_the_agent_had() {
    let (_d, mut r) = reg();
    let mut n = report("boot_report says hung at a live prompt");
    n.device = Some("usb-a".into());
    n.boot_id = Some(276);
    n.cursor = Some("abc:0000000000001234".into());
    n.tool = Some("boot_report".into());
    n.args_json = Some(r#"{"device":"usb-a"}"#.into());

    let (filed, outcome) = rep::file(&mut r, &n, 1_000).unwrap();
    assert_eq!(outcome, rep::Filed::New);
    // Every one of these was previously reconstructed by hand from prose.
    assert_eq!(filed.device.as_deref(), Some("usb-a"));
    assert_eq!(filed.boot_id, Some(276));
    assert_eq!(filed.cursor.as_deref(), Some("abc:0000000000001234"));
    assert_eq!(filed.tool.as_deref(), Some("boot_report"));
    assert_eq!(filed.build.as_deref(), Some("build-1"));
    assert_eq!(filed.status, "open");
    assert_eq!(filed.occurrences, 1);
}

/// THE SAME PROBLEM IN DIFFERENT WORDS IS ONE PROBLEM.
///
/// Triage wants eight problems, not forty messages. Punctuation, case and the
/// epoch number an agent happens to quote must not mint a new row.
#[test]
fn the_same_problem_in_different_words_is_one_report() {
    let (_d, mut r) = reg();
    let (first, _) = rep::file(&mut r, &report("Power off hangs"), 1_000).unwrap();
    let (again, outcome) =
        rep::file(&mut r, &report("power off hangs!!  (boot 351)"), 2_000).unwrap();

    assert_eq!(outcome, rep::Filed::Duplicate);
    assert_eq!(again.id, first.id, "one row, not two");
    assert_eq!(again.occurrences, 2);
    assert_eq!(
        rep::list(&r, Some("open"), None, None, 50).unwrap().len(),
        1
    );
}

/// ...but two genuinely different problems stay apart, or the dedupe is just
/// data loss.
#[test]
fn two_different_problems_stay_two_reports() {
    let (_d, mut r) = reg();
    rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();
    rep::file(
        &mut r,
        &report("follow fires on the previous epoch's prompt"),
        2_000,
    )
    .unwrap();
    assert_eq!(
        rep::list(&r, Some("open"), None, None, 50).unwrap().len(),
        2
    );
}

/// WHO hit it, not just how many times. Ten sightings from one retry loop is
/// not ten agents, and priority should not read it as such.
#[test]
fn distinct_reporters_are_counted_separately_from_occurrences() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();

    // The same agent hits it twice more...
    for t in [2_000, 3_000] {
        rep::confirm(
            &mut r,
            filed.id,
            &Sighting {
                reporter: Some("agent-a".into()),
                build: Some("build-1".into()),
                at: t,
                ..blank()
            },
        )
        .unwrap();
    }
    // ...and a second agent hits it once.
    rep::confirm(
        &mut r,
        filed.id,
        &Sighting {
            reporter: Some("agent-b".into()),
            build: Some("build-1".into()),
            at: 4_000,
            ..blank()
        },
    )
    .unwrap();

    let now = rep::get(&r, filed.id).unwrap();
    assert_eq!(now.occurrences, 4, "every sighting counts");
    assert_eq!(
        rep::distinct_reporters(&r, filed.id).unwrap(),
        2,
        "but two agents hit this, not four"
    );
    let seen = rep::sightings(&r, filed.id, 10).unwrap();
    assert_eq!(seen.len(), 4, "each sighting keeps its own evidence");
}

fn blank() -> Sighting {
    Sighting {
        reporter: None,
        node: None,
        build: None,
        device: None,
        boot_id: None,
        cursor: None,
        note: None,
        at: 0,
    }
}

/// A FIX WITHOUT A BUILD IS A CLAIM, and the whole regression mechanism hangs
/// off having one.
#[test]
fn resolving_as_fixed_demands_the_build_that_fixed_it() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();

    let err = rep::resolve(&mut r, filed.id, "fixed", None, None, None, 2_000).unwrap_err();
    assert!(
        err.message.contains("build"),
        "the reason must say what is missing: {}",
        err.message
    );
    // The other outcomes need no build: "we looked and disagreed" is a decision,
    // not a fix, and must not masquerade as one.
    let closed = rep::resolve(
        &mut r,
        filed.id,
        "not_a_bug",
        None,
        None,
        Some("works as designed"),
        2_000,
    )
    .unwrap();
    assert_eq!(closed.status, "not_a_bug");
    assert!(closed.fixed_in_build.is_none());
}

/// THE POINT OF THE WHOLE MECHANISM: the same problem, on the build that
/// claimed to fix it, is a REGRESSION and says so.
///
/// This is what nothing caught when a fixed EDL verdict came back weeks later
/// in different words and was read as a fresh report.
#[test]
fn the_same_problem_on_the_build_that_fixed_it_is_a_regression() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();
    rep::resolve(
        &mut r,
        filed.id,
        "fixed",
        Some("build-2"),
        Some("a_power_off_answers_within_ten_seconds"),
        None,
        2_000,
    )
    .unwrap();

    // An agent still on the old build hits it: expected, not a regression.
    let mut old = report("power off hangs");
    old.build = Some("build-1".into());
    let (_, outcome) = rep::file(&mut r, &old, 3_000).unwrap();
    assert_eq!(
        outcome,
        rep::Filed::Duplicate,
        "an agent on the old build is not evidence the fix failed"
    );

    // An agent on the FIXED build hits it: the fix did not hold.
    let mut new = report("power off hangs");
    new.build = Some("build-2".into());
    let (after, outcome) = rep::file(&mut r, &new, 4_000).unwrap();
    assert_eq!(outcome, rep::Filed::Regression);
    assert_eq!(after.status, "open", "a regression reopens the report");
    assert_eq!(after.regressions, 1);
    assert!(after.resolved_at.is_none());
}

/// The same rule for "me too", because a confirmation from the fixed build is
/// exactly as important as a fresh filing from it.
#[test]
fn confirming_from_the_fixed_build_is_also_a_regression() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();
    rep::resolve(
        &mut r,
        filed.id,
        "fixed",
        Some("build-2"),
        None,
        None,
        2_000,
    )
    .unwrap();

    let (after, outcome) = rep::confirm(
        &mut r,
        filed.id,
        &Sighting {
            reporter: Some("agent-c".into()),
            build: Some("build-2".into()),
            at: 3_000,
            ..blank()
        },
    )
    .unwrap();
    assert_eq!(outcome, rep::Filed::Regression);
    assert_eq!(after.status, "open");
}

/// Closed WITHOUT a fix and it keeps happening: reopen. A decision that keeps
/// costing agents time deserves revisiting rather than silently absorbing
/// sightings.
#[test]
fn a_problem_closed_as_not_a_bug_reopens_when_it_keeps_happening() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();
    rep::resolve(&mut r, filed.id, "not_a_bug", None, None, None, 2_000).unwrap();

    let (after, outcome) = rep::file(&mut r, &report("power off hangs"), 3_000).unwrap();
    assert_eq!(outcome, rep::Filed::Reopened);
    assert_eq!(after.status, "open");
    assert_eq!(after.regressions, 0, "reopening is not a regression");
}

/// SEARCH BEFORE YOU FILE has to actually work, or every agent files a copy.
#[test]
fn an_agent_can_find_an_existing_report_before_filing_another() {
    let (_d, mut r) = reg();
    rep::file(
        &mut r,
        &report("boot_report says hung at a live prompt"),
        1_000,
    )
    .unwrap();
    rep::file(
        &mut r,
        &report("follow fires on the previous epoch prompt"),
        1_100,
    )
    .unwrap();

    // Matched on normalised text: different case, punctuation and an epoch
    // number the searcher happens to include.
    let hits = rep::list(&r, Some("open"), Some("Hung PROMPT!"), None, 20).unwrap();
    assert_eq!(hits.len(), 1, "search must find it: {hits:?}");
    assert!(hits[0].title.contains("hung"));

    assert!(
        rep::list(&r, Some("open"), Some("flashing"), None, 20)
            .unwrap()
            .is_empty(),
        "and must not match everything"
    );
}

/// A resolved report leaves the triage queue but is never destroyed: the
/// history is what makes a regression recognisable later.
#[test]
fn resolving_clears_the_queue_without_losing_the_report() {
    let (_d, mut r) = reg();
    let (filed, _) = rep::file(&mut r, &report("power off hangs"), 1_000).unwrap();
    rep::resolve(
        &mut r,
        filed.id,
        "fixed",
        Some("build-2"),
        Some("a_gate_name"),
        Some("root cause was the OFF verification path"),
        2_000,
    )
    .unwrap();

    assert!(
        rep::list(&r, Some("open"), None, None, 50)
            .unwrap()
            .is_empty(),
        "the queue is clear"
    );
    let kept = rep::get(&r, filed.id).unwrap();
    assert_eq!(kept.status, "fixed");
    assert_eq!(kept.fixed_in_build.as_deref(), Some("build-2"));
    assert_eq!(
        kept.gate.as_deref(),
        Some("a_gate_name"),
        "checkable by name"
    );
    assert!(kept.resolved_at.is_some());
    assert_eq!(
        rep::list(&r, None, None, None, 50).unwrap().len(),
        1,
        "still there when you ask for everything"
    );
}

#[test]
fn a_report_without_a_title_is_refused() {
    let (_d, mut r) = reg();
    let mut n = report("x");
    n.title = "   ".into();
    assert!(rep::file(&mut r, &n, 1_000).is_err());
}

/// TWO AGENTS, ONE BUG, ONE ROW.
///
/// Verbatim from the bench: `sirocco-codex` and `codex-sirocco-ext4release`
/// reported the same `list_reports` defect minutes apart, in slightly different
/// words. They scored 0.58 against a 0.6 bar and the queue grew two rows for one
/// bug -- the exact duplication this mechanism exists to prevent. Naming the
/// same tool on both sides is strong evidence, and it now buys a lower bar.
#[test]
fn two_agents_describing_one_defect_in_the_same_tool_file_one_report() {
    let (_d, mut r) = reg();
    let first = NewReport {
        title: "list_reports status all loses previously filed reports after upgrade".into(),
        tool: Some("list_reports".into()),
        reporter: Some("sirocco-codex".into()),
        ..Default::default()
    };
    let second = NewReport {
        title: "list_reports still loses previously filed Uno Q reports after upgrade".into(),
        tool: Some("list_reports".into()),
        reporter: Some("codex-sirocco-ext4release".into()),
        ..Default::default()
    };

    let (a, _) = rep::file(&mut r, &first, 1_000).unwrap();
    let (b, outcome) = rep::file(&mut r, &second, 2_000).unwrap();
    assert_eq!(outcome, rep::Filed::Duplicate, "one bug, one row");
    assert_eq!(b.id, a.id);
    assert_eq!(
        rep::distinct_reporters(&r, a.id).unwrap(),
        2,
        "and BOTH agents are visible on it, which is what drives priority"
    );
}

/// ...but a lower bar must not fuse different defects that share a tool name.
#[test]
fn two_different_defects_in_one_tool_stay_apart() {
    let (_d, mut r) = reg();
    for title in [
        "list_reports loses reports when filtered by device",
        "list_reports returns templates instead of the requested limit",
    ] {
        rep::file(
            &mut r,
            &NewReport {
                title: title.into(),
                tool: Some("list_reports".into()),
                ..Default::default()
            },
            1_000,
        )
        .unwrap();
    }
    assert_eq!(
        rep::list(&r, Some("open"), None, None, 50).unwrap().len(),
        2,
        "same tool, different problems: merging them would hide one"
    );
}

/// A REPORT FOUND BY THE NAME AN AGENT ACTUALLY TYPES.
///
/// Reports store the resolved device; agents ask with a nickname. Comparing
/// those directly answered `count: 0` for a board with three reports on it,
/// which reads as data loss -- and was filed as exactly that, twice.
#[test]
fn a_report_is_found_by_a_partial_device_name() {
    let (_d, mut r) = reg();
    let mut n = report("power off hangs");
    n.device = Some("/dev/serial/by-id/usb-Arduino_Bughopper_DK0HEVIC-if00-port0".into());
    rep::file(&mut r, &n, 1_000).unwrap();

    assert_eq!(
        rep::list(&r, None, None, Some("Bughopper_DK0HEVIC"), 20)
            .unwrap()
            .len(),
        1,
        "a recognisable part of the device must find it"
    );
    assert_eq!(
        rep::list(&r, None, None, Some("bughopper_dk0hevic"), 20)
            .unwrap()
            .len(),
        1,
        "and case must not decide whether a report exists"
    );
    assert!(
        rep::list(&r, None, None, Some("some-other-board"), 20)
            .unwrap()
            .is_empty(),
        "while a different board still matches nothing"
    );
}

/// A BUILD ID IS A CONTENT HASH, NOT A VERSION (report #6).
///
/// `confirm` only called a sighting a regression when its build STRING equalled
/// `fixed_in_build`, which is true for exactly one build: the one that shipped
/// the fix. Report #1 was fixed in `79565d1e7302` and reproduced on
/// `9a3e7cff487e`; that is a defect surviving a later build, and it was filed as
/// a duplicate that left the report closed and `regressions` at 0. Hashes cannot
/// be ordered, so the question has to be asked of time instead.
#[test]
fn a_recurrence_on_a_later_build_is_a_regression_not_a_duplicate() {
    let (_d, mut reg) = reg();
    let filed = rep::file(
        &mut reg,
        &report("boot_report leaves a complete boot in_progress"),
        500,
    )
    .unwrap();
    let id = filed.0.id;
    rep::resolve(
        &mut reg,
        id,
        "fixed",
        Some("79565d1e7302"),
        Some("some_gate"),
        None,
        1_000,
    )
    .unwrap();

    // Seen again later, on a DIFFERENT (later) build.
    let mut s = blank();
    s.build = Some("9a3e7cff487e".into());
    s.at = 2_000;
    let (after, outcome) = rep::confirm(&mut reg, id, &s).unwrap();

    assert_eq!(
        outcome,
        rep::Filed::Regression,
        "a defect reproduced after it was closed is a regression, whatever the \
         build string: {after:?}"
    );
    assert_eq!(after.status, "open", "and it must be reopened");
    assert_eq!(after.regressions, 1, "and counted");
}

/// ...while a sighting from BEFORE the fix is still just another occurrence.
/// An agent filing a late observation of the old build must not reopen a report
/// that a later build genuinely fixed.
#[test]
fn a_sighting_from_before_the_fix_does_not_reopen_it() {
    let (_d, mut reg) = reg();
    let filed = rep::file(&mut reg, &report("some defect worth closing"), 500).unwrap();
    let id = filed.0.id;
    rep::resolve(
        &mut reg,
        id,
        "fixed",
        Some("79565d1e7302"),
        Some("some_gate"),
        None,
        5_000,
    )
    .unwrap();

    let mut s = blank();
    s.build = Some("6637e78efd8e".into());
    s.at = 1_000; // observed before the fix landed
    let (after, outcome) = rep::confirm(&mut reg, id, &s).unwrap();

    assert_eq!(outcome, rep::Filed::Duplicate, "{after:?}");
    assert_eq!(after.status, "fixed", "it must stay closed");
    assert_eq!(after.regressions, 0);
}
