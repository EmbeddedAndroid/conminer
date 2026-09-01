//! Suite `epochs` (§8.4, §13) — boot epochs, freshness and fingerprints.
//!
//! Edge cases: mark-then-power ordering · auto-epoch on a detected reset with no
//! mark · an out-of-band double power-cycle creates two epochs · a query with a
//! stale `boot_id` returns *that* epoch and says so · freshness envelope
//! monotonicity · `no_output` requires listening attestation · garbage
//! classification on a baud-mismatch corpus · `boot_report` classification per
//! outcome class including `looping` across many epochs · epoch tagging
//! consistency between records, templates and stages · a failing power hook
//! surfaces as a structured error and does *not* open an epoch · fingerprint
//! equality across byte-identical replayed epochs · fingerprint divergence on a
//! single injected template · fingerprint invariance to wildcarded fields.

use conminer_core::store::SessionSource;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::Rig;

fn loop_text(iterations: usize, tail: &str) -> String {
    let mut s = String::new();
    for i in 0..iterations {
        s.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        s.push_str("U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n");
        s.push_str(&format!(
            "[    {i}.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n"
        ));
        s.push_str(tail);
    }
    s
}

// ------------------------------------------------------------ epoch opening --

#[test]
fn a_pipeline_adopts_an_epoch_another_process_opened() {
    // mcpd's `power`/`flash`/`mark` open the epoch in the database while minerd
    // holds the writer lock and tracks the current boot in memory. Without
    // adoption the two disagree permanently: the agent's epoch stays empty at
    // 0 bytes and the console keeps filling the epoch minerd started with, so
    // fingerprints, boot_report and absence learning all describe a boot that
    // never happened.
    let rig = Rig::new();
    let mut p = rig.pipeline("adopt", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[    1.0] before\n").unwrap();
    let first = p.boot_id().expect("a session epoch");

    // Another process opens an epoch behind the pipeline's back.
    let opened = p
        .store_mut()
        .open_boot("power", Some("cycle"), 5_000, None)
        .unwrap();
    assert_ne!(opened.id, first);
    assert_eq!(p.boot_id(), Some(first), "not adopted until asked");

    assert!(p.adopt_external_boot().unwrap(), "a newer epoch is adopted");
    assert_eq!(p.boot_id(), Some(opened.id));
    assert!(
        !p.adopt_external_boot().unwrap(),
        "adopting twice is a no-op, not a churn of stage state"
    );

    // Output after the power command belongs to the epoch it opened.
    p.feed(b"[    2.0] after\n").unwrap();
    p.finish().unwrap();
    let store = p.into_store();
    assert!(
        store.boot(opened.id).unwrap().bytes > 0,
        "the agent's epoch must not stay empty"
    );
}

#[test]
fn adopting_an_epoch_fingerprints_the_one_it_leaves() {
    // Fingerprints are computed on close. Without this, an epoch opened by
    // another process never closed the previous one, so every epoch that
    // captured bytes stayed open with no fingerprint while the empty ones had
    // them — exactly backwards, and it made fingerprint comparison meaningless.
    let rig = Rig::new();
    let mut p = rig.pipeline("fp-adopt", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[    1.0] Linux version 6.12.9 (build@lab)\n")
        .unwrap();
    let first = p.boot_id().expect("a session epoch");

    let opened = p.store_mut().open_boot("power", None, 5_000, None).unwrap();
    assert!(p.adopt_external_boot().unwrap());

    let store = p.into_store();
    assert!(
        store.boot(first).unwrap().fingerprint.is_some(),
        "the epoch we left must be fingerprinted on the way out"
    );
    assert_eq!(
        store.boot(opened.id).unwrap().fingerprint,
        None,
        "the epoch we just entered is still open"
    );
}

#[test]
fn adoption_never_moves_an_epoch_backwards() {
    // A stale read must not re-attribute live output to a boot that has already
    // been reported on.
    let rig = Rig::new();
    let mut p = rig.pipeline("back", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let newer = p.open_boot("mark", None).unwrap();
    assert_eq!(p.boot_id(), Some(newer));
    assert!(
        !p.adopt_external_boot().unwrap(),
        "latest is already current"
    );
    assert_eq!(p.boot_id(), Some(newer));
}

#[test]
fn a_session_opens_an_epoch_and_a_mark_opens_another() {
    let rig = Rig::new();
    let mut p = rig.pipeline("e1", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let first = p.boot_id().unwrap();

    // The recommended dev-loop idiom: mark *before* flipping power.
    let second = p.open_boot("mark", Some("before power")).unwrap();
    assert_ne!(first, second);

    let store = p.into_store();
    let boots = store.list_boots(10).unwrap();
    assert_eq!(boots.len(), 2);
    assert_eq!(boots[0].opened_by, "mark");
    assert_eq!(boots[0].label.as_deref(), Some("before power"));
    assert!(boots[0].seq > boots[1].seq);
}

#[test]
fn a_detected_reset_opens_an_epoch_with_no_mark_at_all() {
    // An out-of-band power flip — a PDU, labgrid, a finger — still gets its own
    // epoch, just without an agent-chosen label.
    let rig = Rig::new();
    let store = rig.ingest_text("e2", None, &loop_text(3, "[    9.0] mmc0: ready\n"));
    let boots = store.list_boots(10).unwrap();
    assert_eq!(boots.len(), 3);
    assert_eq!(boots[0].opened_by, "reset");
    assert_eq!(boots.last().unwrap().opened_by, "ingest");
}

#[test]
fn an_out_of_band_double_power_cycle_creates_two_epochs() {
    let rig = Rig::new();
    let store = rig.ingest_text("e3", None, &loop_text(3, ""));
    assert_eq!(
        store.list_boots(10).unwrap().len(),
        3,
        "one plus two resets"
    );
}

#[test]
fn lines_records_stages_and_templates_all_agree_on_their_epoch() {
    let rig = Rig::new();
    let store = rig.ingest_text("e4", None, &loop_text(4, "[    9.0] mmc0: ready\n"));
    for b in store.list_boots(10).unwrap() {
        for s in store.stages(None, Some(b.id)).unwrap() {
            assert_eq!(s.boot_id, Some(b.id));
        }
        for r in store.records_in_boot(b.id, None, 500).unwrap() {
            assert_eq!(r.boot_id, Some(b.id));
            let line = store.line(r.first_line_id).unwrap();
            assert_eq!(
                line.boot_id,
                Some(b.id),
                "a record and its first line must belong to the same epoch"
            );
        }
        assert!(b.bytes > 0, "epoch {} recorded no bytes", b.seq);
    }
}

#[test]
fn a_stale_boot_id_returns_that_epoch_rather_than_the_latest() {
    let rig = Rig::new();
    let store = rig.ingest_text("e5", None, &loop_text(3, "[    9.0] mmc0: ready\n"));
    let boots = store.list_boots(10).unwrap();
    let oldest = boots.last().unwrap();
    let newest = boots.first().unwrap();
    assert_ne!(oldest.id, newest.id);

    let old_stages = store.stages(None, Some(oldest.id)).unwrap();
    let new_stages = store.stages(None, Some(newest.id)).unwrap();
    assert!(!old_stages.is_empty() && !new_stages.is_empty());
    assert!(old_stages.iter().all(|s| s.boot_id == Some(oldest.id)));
}

// ------------------------------------------------------------ fingerprints ---

#[test]
fn byte_identical_replayed_epochs_produce_equal_fingerprints() {
    let rig = Rig::new();
    let store = rig.ingest_text("e6", None, &loop_text(4, "[    9.0] mmc0: ready\n"));
    let fps: Vec<String> = store
        .list_boots(10)
        .unwrap()
        .into_iter()
        .filter_map(|b| b.fingerprint)
        .collect();
    assert!(fps.len() >= 3);
    assert!(
        fps.windows(2).all(|w| w[0] == w[1]),
        "identical behaviour must hash identically: {fps:?}"
    );
}

#[test]
fn fingerprints_are_invariant_to_the_fields_drain_wildcards() {
    // Raw bytes of two loop iterations never match — timestamps differ every
    // pass — but the template sequence does, which is the whole point of making
    // the fingerprint semantic rather than a raw hash.
    let rig = Rig::new();
    let mut text = String::new();
    for i in 0..4 {
        text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
        text.push_str(&format!(
            "[ {:6}.{:06}] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n",
            i * 37,
            i * 991
        ));
        text.push_str(&format!(
            "[ {:6}.{:06}] mmc0: new HS200 MMC card at address {:04x}\n",
            i * 37 + 1,
            i * 13,
            i * 7
        ));
    }
    let store = rig.ingest_text("e7", None, &text);
    let fps: Vec<String> = store
        .list_boots(10)
        .unwrap()
        .into_iter()
        .filter_map(|b| b.fingerprint)
        .collect();
    assert!(fps.len() >= 3);
    assert!(
        fps.windows(2).all(|w| w[0] == w[1]),
        "timestamps and addresses are wildcarded, so they must not move the \
         fingerprint: {fps:?}"
    );
}

#[test]
fn one_injected_template_diverges_the_fingerprint_mid_loop() {
    let rig = Rig::new();
    let mut text = loop_text(3, "[    9.0] mmc0: ready\n");
    // A fourth iteration that does one new thing.
    text.push_str("NOTICE:  BL1: v2.11(release):v2.11\n");
    text.push_str("U-Boot 2026.01 (Jan 04 2026 - 12:00:11 +0000)\n");
    text.push_str("[    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n");
    text.push_str("[    9.0] mmc0: ready\n");
    text.push_str("[    9.5] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n");
    text.push_str("[    9.5] ---[ end trace 0000000000000000 ]---\n");

    let store = rig.ingest_text("e8", None, &text);
    let boots = store.list_boots(10).unwrap();
    let latest = boots[0].fingerprint.clone().unwrap();
    let previous = boots[1].fingerprint.clone().unwrap();
    assert_ne!(
        latest, previous,
        "'same crash again' vs 'different crash now' is a hash comparison"
    );
    // …and the earlier epochs are still stable among themselves.
    assert_eq!(boots[1].fingerprint, boots[2].fingerprint);
}

#[test]
fn distinct_epochs_stay_distinct_objects_even_when_byte_identical() {
    let rig = Rig::new();
    let store = rig.ingest_text("e9", None, &loop_text(3, "[    9.0] mmc0: ready\n"));
    let boots = store.list_boots(10).unwrap();
    let ids: std::collections::BTreeSet<i64> = boots.iter().map(|b| b.id).collect();
    let offsets: std::collections::BTreeSet<u64> = boots.iter().map(|b| b.opened_offset).collect();
    assert_eq!(ids.len(), boots.len(), "distinct ids");
    assert_eq!(
        offsets.len(),
        boots.len(),
        "distinct offsets: storage never dedupes, only the template view collapses"
    );
    assert!(
        boots.iter().all(|b| b.fingerprint == boots[0].fingerprint),
        "identical *behaviour*, distinct *objects*"
    );
}

// -------------------------------------------------------------- freshness ----

#[test]
fn the_freshness_envelope_is_monotonic_across_reads() {
    let rig = Rig::new();
    let mut p = rig.pipeline("e10", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();

    let mut offsets = Vec::new();
    for i in 0..20 {
        p.feed(format!("[ 1.0] line {i}\n").as_bytes()).unwrap();
        offsets.push(p.store().stream_offset());
    }
    assert!(
        offsets.windows(2).all(|w| w[0] < w[1]),
        "the stream offset only ever advances"
    );

    let store = p.into_store();
    let head = store.head_cursor();
    assert!(store.resolve_cursor(&head).is_ok());
    assert_eq!(head.offset, *offsets.last().unwrap());
}

#[test]
fn an_epoch_with_no_bytes_is_not_confused_with_one_that_captured_plenty() {
    let rig = Rig::new();
    let mut p = rig.pipeline("e11", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"[ 1.0] plenty of output here\n").unwrap();
    let noisy = p.boot_id().unwrap();
    let quiet = p.open_boot("mark", Some("after power")).unwrap();
    p.finish().unwrap();

    let store = p.into_store();
    assert!(store.boot(noisy).unwrap().bytes > 0);
    assert_eq!(
        store.boot(quiet).unwrap().bytes,
        0,
        "the epoch opened after the mark captured nothing, and says so"
    );
}

// ----------------------------------------------------------- classification --

#[test]
fn garbage_is_classified_from_a_real_baud_mismatch_capture() {
    let rig = Rig::new();
    let bytes = conminer_testkit::corpus::corpus_file("hostile/baud-mismatch.log");
    let mut p = rig.pipeline("e12", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let out = p.feed(&bytes).unwrap();
    p.finish().unwrap();

    assert!(
        out.garbage_lines > 0,
        "a wrong baud rate must be named, not shown as noise lines"
    );
    let store = p.into_store();
    let boots = store.list_boots(5).unwrap();
    let garbage = store
        .records_in_boot(
            boots[0].id,
            Some(conminer_core::store::RecordKind::Garbage),
            10,
        )
        .unwrap();
    assert!(!garbage.is_empty());
    // Quarantined, not mined: a baud mismatch cannot pollute the template store.
    for r in &garbage {
        assert!(r.template_id.is_none());
    }
}

#[test]
fn a_looping_board_is_summarised_rather_than_replayed() {
    let rig = Rig::new();
    let store = rig.ingest_text("e13", None, &loop_text(200, "[    9.0] mmc0: ready\n"));
    let boots = store.list_boots(500).unwrap();
    assert!(
        boots.len() >= 190,
        "one epoch per iteration: {}",
        boots.len()
    );

    // 200 iterations collapse to a handful of templates — the entire point.
    assert!(
        store.template_count().unwrap() < 20,
        "{} templates for 200 boots",
        store.template_count().unwrap()
    );
    let fps: std::collections::BTreeSet<Option<String>> =
        boots.iter().map(|b| b.fingerprint.clone()).collect();
    assert_eq!(fps.len(), 1, "one behaviour, one fingerprint");
}

#[test]
fn every_closed_epoch_is_fingerprinted() {
    let rig = Rig::new();
    let store = rig.ingest_text("e14", None, &corpus_text("mixed/boot-loop.log"));
    let boots = store.list_boots(50).unwrap();
    assert!(!boots.is_empty());
    assert!(
        boots.iter().all(|b| b.fingerprint.is_some()),
        "an epoch without a fingerprint cannot be compared to anything"
    );
}

/// AN ACTUATION MUST CLAIM ITS BOOT BACK FROM EVERY EPOCH THAT OPENED MEANWHILE.
///
/// A `power` call records the stream head BEFORE its hook runs, so the epoch can
/// begin where the board was when the button was pressed. That claim used to
/// move lines only from `prev`, the single newest epoch, assuming nothing else
/// could open while the hook ran. A capture reconnect opens a `session` epoch,
/// and a Bughopper power hook holds its line for seconds with verification after
/// it -- over a minute end to end.
///
/// Measured on the Uno-Q, epochs 542-547: 546 (`power`) marked the head at
/// 5027650; while its hook ran, session epochs 544 (17,355 bytes, the boot 546
/// had just caused) and 545 opened. When 546 opened, `prev` was the empty 545,
/// so nothing matched and 546 recorded 0 bytes -- and the boot fell to 547, the
/// next power call, whose provenance then reported a fingerprint from a boot it
/// never caused.
#[test]
fn a_power_epoch_claims_its_boot_from_every_epoch_opened_during_the_hook() {
    let rig = Rig::new();
    let mut p = rig.pipeline("claim-across-epochs", None);
    let sid = p
        .begin_session(SessionSource::Live, None, None, None)
        .unwrap();

    // Output from BEFORE the button was pressed: must not move.
    p.feed(b"old chatter before the press\n").unwrap();
    let mark = p.store().head_cursor().offset;

    // The hook is running. A capture reconnect opens a session epoch, and the
    // board -- already reset -- boots into it.
    p.open_boot("session", None).unwrap();
    p.feed(b"NOTICE:  BL1: v2.11(release):v2.11\nBUILD fp=deadbeefcafe\nCONSOLE\n")
        .unwrap();
    // A second reconnect, leaving an empty epoch as the newest one.
    p.open_boot("session", None).unwrap();
    p.tick().unwrap();

    // NON-VACUITY: the boot's lines must really be sitting in an epoch that is
    // NOT the newest, or `prev` alone would have found them and this proves
    // nothing.
    let newest = p.store().latest_boot().unwrap().unwrap();
    assert_eq!(newest.opened_by, "session");
    assert_eq!(newest.bytes, 0, "the newest epoch must be the empty one");
    let holder = p
        .store()
        .recent_lines(50)
        .unwrap()
        .iter()
        .find(|l| l.lossy().contains("BUILD fp="))
        .and_then(|l| l.boot_id)
        .expect("the boot output must be attributed somewhere");
    assert_ne!(
        holder, newest.id,
        "the fixture must leave the boot in an epoch BEHIND the newest"
    );

    // Now the hook returns and the power epoch opens, back-dated to the mark.
    let now = p.store().latest_boot().unwrap().unwrap().opened_at + 1;
    let boot = p
        .store_mut()
        .open_boot_at("power", None, now, Some(sid), Some(mark))
        .unwrap();
    let store = p.into_store();

    let claimed = store.boot(boot.id).unwrap();
    assert!(
        claimed.bytes > 0,
        "the power epoch must record the boot it caused, not 0 bytes: {claimed:?}"
    );
    let fp_line_owner = store
        .recent_lines(50)
        .unwrap()
        .iter()
        .find(|l| l.lossy().contains("BUILD fp="))
        .and_then(|l| l.boot_id);
    assert_eq!(
        fp_line_owner,
        Some(boot.id),
        "the fingerprint the board printed after the press belongs to this epoch"
    );
    // ...and output from before the press stays where it was.
    let old_owner = store
        .recent_lines(50)
        .unwrap()
        .iter()
        .find(|l| l.lossy().contains("old chatter"))
        .and_then(|l| l.boot_id);
    assert_ne!(
        old_owner,
        Some(boot.id),
        "output from before the button was pressed must NOT be claimed"
    );
}

/// THE EPOCH CHAINS THE FLEET IS ACTUALLY PRODUCING, CHECKED EVERY FULL RUN.
///
/// `./cm test` refreshes `corpus/shapes/rolling/*.tsv` from every reachable node
/// before running the suite. Those captures feed INVARIANTS, never exact
/// assertions: a fixture that silently updates itself when the board changes
/// turns a regression into a passing test, which is how a golden-file suite
/// stops testing anything.
///
/// MONOTONIC OFFSETS ARE NOT THE INVARIANT, and writing that rule first is what
/// this corpus caught. An actuation epoch marks the stream head BEFORE its hook
/// runs, so it legitimately starts behind a `session` epoch that opened while
/// the hook was still going -- measured on bravo as epochs 555, 565 and 584, all
/// `power`, all starting behind their predecessor, all correctly owning their
/// boot (20,772 / 13,043 / 17,971 bytes). Pinning monotonicity would have
/// enshrined a rule the design deliberately breaks.
///
/// THE REAL SIGNATURE is epoch 546: an actuation that back-dated into a range an
/// earlier epoch already owned and came away with NOTHING, while the boot it had
/// just caused sat in that earlier epoch and the next actuation claimed it. That
/// is what made provenance report a fingerprint from a boot the epoch never
/// caused.
///
/// Skipped, loudly, when no capture exists: an absent bench must not read as a
/// pass.
#[test]
fn no_actuation_on_the_fleet_lost_its_boot_to_an_earlier_epoch() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../corpus/shapes/rolling");
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("no rolling capture at {dir}; run `./cm snapshot` with a node reachable");
        return;
    };

    #[derive(Clone)]
    struct Epoch {
        id: i64,
        by: String,
        off: i64,
        bytes: i64,
    }

    let mut checked = 0usize;
    let mut lost: Vec<String> = Vec::new();

    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("tsv") {
            continue;
        }
        let node = path.file_stem().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut db = String::new();
        let mut per_db: std::collections::HashMap<String, Vec<Epoch>> =
            std::collections::HashMap::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("#db ") {
                db = rest.trim().to_string();
                continue;
            }
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 6 {
                continue;
            }
            let (Ok(id), Ok(off), Ok(bytes)) = (
                f[0].parse::<i64>(),
                f[4].parse::<i64>(),
                f[5].parse::<i64>(),
            ) else {
                continue;
            };
            if off < 0 {
                continue;
            }
            per_db.entry(db.clone()).or_default().push(Epoch {
                id,
                by: f[2].to_string(),
                off,
                bytes,
            });
        }

        for (db, epochs) in per_db {
            for a in &epochs {
                let actuation = matches!(a.by.as_str(), "power" | "reset" | "boot_mode" | "flash");
                if !actuation || a.bytes != 0 {
                    continue;
                }
                checked += 1;
                // An EARLIER epoch holding output at or after where this one
                // starts: the actuation back-dated into ground already taken.
                if let Some(thief) = epochs
                    .iter()
                    .find(|b| b.id < a.id && b.off >= a.off && b.bytes > 0)
                {
                    lost.push(format!(
                        "{node}: {} epoch {} starts at {} with 0 bytes, while earlier epoch {} \
                         at {} holds {} bytes ({db})",
                        a.by, a.id, a.off, thief.id, thief.off, thief.bytes
                    ));
                }
            }
        }
    }

    assert!(
        lost.is_empty(),
        "{} actuation(s) lost their boot, out of {checked} empty actuation epochs examined:\n  {}",
        lost.len(),
        lost.join("\n  ")
    );
}
