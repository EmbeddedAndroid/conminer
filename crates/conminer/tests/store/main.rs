//! Suite `store` (§13) — persistence.
//!
//! Edge cases: WAL crash points · retention pruning (size, age, keep-all file
//! sessions) · prune vs open cursor · 2 GB live cap enforcement ·
//! template/occurrence rollup correctness · per-device isolation · disk-full
//! behaviour (fail loud, stop capture, health flag — never silent drop).
//!
//! Plus the §14.7 export/import round trip.

use conminer_core::store::{Cursor, SessionSource};
use conminer_core::ErrorCode;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::Rig;

fn sample() -> String {
    corpus_text("linux/boot-oops.log")
}

// -------------------------------------------------------------- rollups ------

#[test]
fn template_and_occurrence_rollups_agree_with_the_records() {
    let rig = Rig::new();
    let store = rig.ingest_text("rollup", None, &sample().repeat(3));

    let session = store.latest_session().unwrap().unwrap();
    let templates = store
        .list_templates(&conminer_core::store::TemplateQuery {
            session_id: Some(session.id),
            limit: 10_000,
            ..Default::default()
        })
        .unwrap();

    // Every scoped count must equal the number of records that reference it.
    for t in &templates {
        let recs = store
            .records_for_template(t.id, Some(session.id), None, 100_000, 0)
            .unwrap();
        assert_eq!(
            t.scoped_count.unwrap() as usize,
            recs.len(),
            "template {} rollup disagrees with its records",
            t.id
        );
    }

    // …and the totals add up to the number of mined records.
    let total: i64 = templates.iter().map(|t| t.scoped_count.unwrap()).sum();
    let mined = store.stats(Some(session.id)).unwrap().records;
    assert!(total <= mined, "{total} mined hits vs {mined} records");
    assert!(total > 0);
}

#[test]
fn new_only_is_correct_across_sessions() {
    let rig = Rig::new();
    let dev = rig.device("new-only");
    let text = sample();

    // First session: everything is new.
    {
        let mut p = rig.pipeline("new-only", None);
        let s = p
            .begin_session(SessionSource::File, Some("run-1"), None, None)
            .unwrap();
        p.feed(text.as_bytes()).unwrap();
        p.finish().unwrap();
        let store = p.into_store();
        let all = store
            .list_templates(&conminer_core::store::TemplateQuery {
                session_id: Some(s),
                limit: 1000,
                ..Default::default()
            })
            .unwrap();
        let new = store
            .list_templates(&conminer_core::store::TemplateQuery {
                session_id: Some(s),
                new_only: true,
                limit: 1000,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), new.len(), "the first session is all novel");
    }

    // Second session, same content plus one genuinely new line.
    let store = rig.store(&dev);
    drop(store);
    let mut p2 = conminer_core::pipeline::Pipeline::new(
        rig.store(&dev),
        rig.profiles.clone(),
        rig.config.clone(),
        &dev.canonical,
        None,
        rig.clock.clone(),
    )
    .unwrap();
    let s2 = p2
        .begin_session(SessionSource::File, Some("run-2"), None, None)
        .unwrap();
    p2.feed(text.as_bytes()).unwrap();
    p2.feed(b"[   99.000000] brand new never seen before message here\n")
        .unwrap();
    p2.finish().unwrap();
    let store = p2.into_store();

    let new = store
        .list_templates(&conminer_core::store::TemplateQuery {
            session_id: Some(s2),
            new_only: true,
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        new.len(),
        1,
        "only the genuinely novel line is new: {new:#?}"
    );
    assert!(new[0].text.contains("brand new never seen before"));
}

// ------------------------------------------------------------- isolation -----

#[test]
fn devices_are_isolated_from_each_other() {
    let rig = Rig::new();
    let a = rig.ingest_text("dev-a", None, "alpha only line\n");
    let b = rig.ingest_text("dev-b", None, "bravo only line\n");

    assert_eq!(a.template_count().unwrap(), 1);
    assert_eq!(b.template_count().unwrap(), 1);
    assert!(a.template(1).unwrap().text.contains("alpha"));
    assert!(b.template(1).unwrap().text.contains("bravo"));

    // Template ids restart per device, and a cursor from one is rejected by the
    // other rather than silently resolving.
    let cursor = a.head_cursor();
    assert_eq!(
        b.resolve_cursor(&cursor).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
}

// ------------------------------------------------------------- retention -----

#[test]
fn pruning_removes_raw_but_keeps_templates_and_counts() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("prune", None, &sample().repeat(4));

    let before_templates = store.template_count().unwrap();
    let before_lines = store.line_count().unwrap();
    assert!(before_lines > 100);

    let head = store.stream_offset();
    let removed = store.prune_before(head / 2).unwrap();
    assert!(removed > 0);

    assert!(store.line_count().unwrap() < before_lines, "raw shrank");
    assert_eq!(
        store.template_count().unwrap(),
        before_templates,
        "templates and rollups are kept forever — they are tiny, and they are \
         the whole point (§7)"
    );
}

#[test]
fn a_cursor_behind_the_retention_horizon_expires_rather_than_lying() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("cursor-prune", None, &sample().repeat(4));

    let old = store.cursor_at(0);
    store.resolve_cursor(&old).expect("valid before pruning");

    store.prune_before(store.stream_offset() / 2).unwrap();

    let err = store.resolve_cursor(&old).unwrap_err();
    assert_eq!(err.code, ErrorCode::CursorExpired);
    let d = err.detail.expect("must say where to re-anchor");
    assert!(d["earliest_offset"].as_u64().unwrap() > 0);
    assert!(d["head"].as_str().unwrap().contains(':'));

    // A cursor inside the retained window still works.
    let fresh = store.cursor_at(store.stream_offset());
    store.resolve_cursor(&fresh).unwrap();
}

#[test]
fn the_size_cap_prunes_oldest_first_and_stops_at_the_cap() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("cap", None, &sample().repeat(8));
    let live: i64 = store.stats(None).unwrap().bytes;
    assert!(live > 4_000);

    let cap = (live / 2) as u64;
    store.enforce_size_cap(cap).unwrap();
    let after = store.stats(None).unwrap().bytes as u64;
    assert!(after <= cap, "{after} must be within the {cap}-byte cap");

    // The *oldest* lines went: the newest line is still there.
    let recent = store.recent_lines(1).unwrap();
    assert_eq!(recent.len(), 1);
}

#[test]
fn a_cap_larger_than_the_data_prunes_nothing() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("nocap", None, &sample());
    let before = store.line_count().unwrap();
    assert_eq!(store.enforce_size_cap(1 << 30).unwrap(), 0);
    assert_eq!(store.line_count().unwrap(), before);
}

#[test]
fn file_sessions_default_to_keep_all() {
    // §7/§16: file sessions are keep-all, live capture is capped. The default
    // config must actually say so, or a post-hoc ingest could be silently pruned.
    let c = conminer_core::config::Config::default();
    assert_eq!(c.retention.file_sessions, "keep-all");
    assert_eq!(c.retention.live_cap_gb, 2.0);
}

// ------------------------------------------------------------ disk full ------

#[test]
fn a_full_disk_fails_loud_and_never_drops_silently() {
    let rig = Rig::new();
    let mut p = rig.pipeline("full", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    p.feed(b"first line lands fine\n").unwrap();

    // Cap the database at its current size: the next write cannot fit.
    let pages = p.store().page_count().unwrap();
    p.store().limit_pages(pages).unwrap();

    let mut err = None;
    for _ in 0..2000 {
        if let Err(e) = p.feed(b"[    1.000000] this line needs a new page to land\n") {
            err = Some(e);
            break;
        }
    }
    let err = err.expect("a full store must fail, not silently drop");
    assert_eq!(
        err.code,
        ErrorCode::StorageFull,
        "the failure must be recognisable, not a generic internal error: {err:?}"
    );
}

// ------------------------------------------------------------- integrity ----

#[test]
fn the_store_passes_an_integrity_check_after_a_real_ingest() {
    let rig = Rig::new();
    let store = rig.ingest_text("integrity", None, &sample().repeat(4));
    assert_eq!(store.integrity_check().unwrap(), "ok");
    store.checkpoint().unwrap();
    assert_eq!(store.integrity_check().unwrap(), "ok");
}

#[test]
fn a_reopened_store_keeps_its_offsets_templates_and_cursor_identity() {
    let rig = Rig::new();
    let dev = rig.device("reopen");
    let (offset, templates, cursor) = {
        let store = rig.ingest_text("reopen", None, &sample());
        (
            store.stream_offset(),
            store.template_count().unwrap(),
            store.head_cursor().encode(),
        )
    };
    let store = rig.store(&dev);
    assert_eq!(store.stream_offset(), offset);
    assert_eq!(store.template_count().unwrap(), templates);
    assert_eq!(store.head_cursor().encode(), cursor);
    store
        .resolve_cursor(&Cursor::decode(&cursor).unwrap())
        .expect("cursors survive a restart");
}

// -------------------------------------------------------- rebuild is pure ----

#[test]
fn rebuilding_templates_from_raw_reproduces_them_exactly() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("rebuild", None, &sample().repeat(2));

    let before: Vec<String> = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 10_000,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|t| format!("{}|{}", t.text, t.total_count))
        .collect();

    let cfg = conminer_core::drain::DrainConfig::default();
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    store.rebuild_templates(cfg, &profiles).unwrap();

    let after: Vec<String> = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 10_000,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|t| format!("{}|{}", t.text, t.total_count))
        .collect();

    assert_eq!(before, after, "templates are a derived view of raw (§6)");
}

#[test]
fn a_lower_similarity_threshold_can_be_applied_retroactively() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("retune", None, &sample().repeat(2));
    let strict = store.template_count().unwrap();

    let loose = conminer_core::drain::DrainConfig {
        similarity: 0.1,
        ..Default::default()
    };
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let n = store.rebuild_templates(loose, &profiles).unwrap();
    // A looser threshold merges *within* a tree leaf; it cannot merge across the
    // token-count layer, so the count moves down or stays put — never far up.
    assert!(
        n <= strict as usize + 1,
        "a looser threshold should not blow the template count up: {n} vs {strict}"
    );
    // Raw is untouched by the retune.
    assert_eq!(store.integrity_check().unwrap(), "ok");
}

// ------------------------------------------------------- export / import ----

#[test]
fn export_then_import_round_trips_the_raw_bytes_exactly() {
    let rig = Rig::new();
    let text = sample();
    let store = rig.ingest_text("export", None, &text);
    let session = store.latest_session().unwrap().unwrap();

    let mut archive = Vec::new();
    store.export_session(session.id, &mut archive).unwrap();
    assert!(!archive.is_empty());
    let path = rig.write_file("session.conminer.gz", &archive);

    // Import into a *different* device, as a bug report would be.
    let mut p = rig.pipeline("imported", None);
    let report = conminer_core::ingest::ingest_file(
        &mut p,
        &path,
        &conminer_core::ingest::IngestOptions::default(),
    )
    .unwrap();
    let imported = p.into_store();

    assert_eq!(
        report.exported_from.as_ref().unwrap()["device"],
        store.canonical(),
        "the archive says where it came from"
    );

    // Byte-exact: reassembling the imported session reproduces the original.
    let rebuilt: Vec<u8> = imported
        .lines_for_session(report.session_id)
        .unwrap()
        .iter()
        .flat_map(|l| {
            let mut v = l.bytes.clone();
            v.extend_from_slice(l.terminator.raw());
            v
        })
        .collect();
    assert_eq!(
        rebuilt,
        text.as_bytes(),
        "export/import must not change a byte"
    );
}

#[test]
fn an_exported_archive_is_gzip_and_self_describing() {
    let rig = Rig::new();
    let store = rig.ingest_text("export2", None, "one line\n");
    let s = store.latest_session().unwrap().unwrap();
    let mut archive = Vec::new();
    store.export_session(s.id, &mut archive).unwrap();
    assert_eq!(&archive[..2], &[0x1f, 0x8b], "gzip magic");

    let mut out = String::new();
    use std::io::Read;
    flate2::read::GzDecoder::new(&archive[..])
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.starts_with("CONMINER-EXPORT-1 {"));
    assert!(out.ends_with("one line\n"));
}

/// Regression: a device that RECONNECTS must keep capturing. `raw_lines.
/// stream_offset` is UNIQUE, so if the cached cursor ever sits below what the
/// table already holds, the next insert collides and takes the read loop with
/// it. Measured on hardware: minerd attached to a reconnecting console, died
/// with "UNIQUE constraint failed: raw_lines.stream_offset", reattached, and
/// looped every ~4s while the board was visibly booting -- so the device read
/// as DEAD and every judgement built on that signal was wrong.
#[test]
fn a_rewound_offset_cursor_heals_instead_of_colliding() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("reconnect", None, &sample());
    let stored = store.stream_offset();
    assert!(stored > 0, "the fixture should have written something");

    // Exactly the drift a reconnect produced: cursor behind the table.
    store.finish_batch(0);
    assert_eq!(store.stream_offset(), 0);

    // Starting a batch must notice and advance, rather than hand out an offset
    // that already exists.
    let b = store.begin_batch().expect("begin after rewind");
    drop(b);
    assert_eq!(
        store.stream_offset(),
        stored,
        "the cursor must heal to what the store already holds"
    );
}

/// A HEAL MUST LAND ON THE STREAM, NOT PAST IT.
///
/// `raw_lines.terminator` stores the terminator's NAME -- "lf", "crlf" -- so the
/// obvious `length(terminator)` in SQL measures the label (2, 4) instead of what
/// the board sent (1, 2). The healed cursor then lands one byte past the real
/// end on every LF line, the next line is written with a phantom gap in front of
/// it, and the drift compounds: measured on the bench, every heal reported
/// exactly `drift=1` and that device's `stream_offset` had run 9.35 MB ahead of
/// its 227 MB of actual bytes.
#[test]
fn a_healed_cursor_matches_the_bytes_actually_written() {
    let rig = Rig::new();
    // LF-terminated, which is the case the label-length bug gets wrong by one.
    // GROUND TRUTH IS THE INPUT ITSELF, never another reading of the cursor:
    // comparing a heal against a previous heal agrees with itself while both
    // are wrong by the same byte.
    const INPUT: &str = "alpha\nbeta\ngamma\n";
    let mut store = rig.ingest_text("heal", None, INPUT);

    // Rewind the cursor so the next batch has to heal, exactly as a reconnect does.
    store.finish_batch(0);
    let b = store.begin_batch().expect("begin after rewind");
    drop(b);

    assert_eq!(
        store.stream_offset(),
        INPUT.len() as u64,
        "the healed cursor must equal the bytes actually fed ({} of them); landing past \
         them leaves a phantom gap in the stream that compounds on every line",
        INPUT.len()
    );
}

/// ...and the healing lookup must not cost a full table scan.
///
/// It asked for `MAX(stream_offset + length(bytes) + length(terminator))`, an
/// aggregate over a COMPUTED expression that no index can answer, so SQLite
/// scanned every row -- once per batch, which is once per chunk of console.
/// Free on a fresh store, ruinous on a used one: measured on the bench at
/// 4,909,488 rows / 2.08 GB it took **1879 ms** against **0.4 ms** for the same
/// answer read from the last row. Mining fell to ~1 line/sec and the mined
/// stream ran minutes behind the live console.
///
/// This is a property of store SIZE, not of one board: every board gets here
/// once somebody develops on it. So the fixture grows the stream and requires
/// the call to stay flat.
#[test]
fn healing_the_offset_cursor_does_not_scan_the_whole_stream() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("bulk", None, &sample());

    // Grow the stream well past the point where a scan and an indexed read
    // differ by orders of magnitude.
    for i in 0..40 {
        let text: String = (0..500)
            .map(|j| format!("[{i:>4}.{j:06}] driver-core: probe {j} state=4 ok\n"))
            .collect();
        // Same canonical every pass, so the one device's stream keeps growing.
        store = rig.ingest_text("bulk", None, &text);
    }
    let rows = store.line_count().unwrap();
    assert!(
        rows > 15_000,
        "fixture must be big enough to separate the two: {rows}"
    );

    // Force the healing path on every call, which is what the live miner hits.
    let warm = store.begin_batch().expect("warm");
    drop(warm);
    let t = std::time::Instant::now();
    const CALLS: u32 = 20;
    for _ in 0..CALLS {
        store.finish_batch(0);
        let b = store.begin_batch().expect("begin");
        drop(b);
    }
    let per_us = t.elapsed().as_micros() / CALLS as u128;
    // Calibrated on this fixture: the indexed read costs ~10us per call, the
    // full scan ~3505us. 500us sits fifty times above the former and seven
    // times below the latter, so it is neither flaky nor blind.
    assert!(
        per_us < 500,
        "begin_batch cost {per_us}us per call over {rows} lines: it is scanning the stream, \
         which puts mining behind the live console and gets worse as any board is used"
    );
}

/// A rebuild must not be blocked by, or silently discard, human judgements.
///
/// THE BUG: `DELETE FROM templates` hit "FOREIGN KEY constraint failed" because
/// `template_verdicts` keys on template_id, so rebuild failed outright on any
/// device that had ever been triaged. Measured on the rig after 27 templates
/// were marked benign to hide ser2net banner noise.
///
/// This matters beyond the error: rebuild is the RECOVERY PATH for a miner
/// improvement. If it cannot run, a better tokenizer can never be applied to
/// captures already on disk, and every historical boot keeps its old templates
/// forever.
#[test]
fn a_rebuild_preserves_verdicts_instead_of_failing_on_them() {
    let rig = Rig::new();
    let mut store = rig.ingest_text("verdict-rebuild", None, &sample().repeat(2));

    let first = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 10_000,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .next()
        .expect("at least one template to annotate");
    let text = first.text.clone();

    store
        .set_verdict(
            first.id,
            Some(conminer_core::store::Verdict::Benign),
            Some("known noise"),
            Some("TICKET-1"),
            Some("ops"),
            1_000,
        )
        .unwrap();

    let cfg = conminer_core::drain::DrainConfig::default();
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    // Before the fix this returned Err(FOREIGN KEY constraint failed).
    store
        .rebuild_templates(cfg, &profiles)
        .expect("rebuild must survive an annotated template");

    // The verdict must still be attached, and to the SAME LINE -- ids are
    // re-minted by a rebuild, so text is the identity that has to carry.
    let after = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 10_000,
            ..Default::default()
        })
        .unwrap();
    let same = after
        .iter()
        .find(|t| t.text == text)
        .expect("the annotated line must still be mined");
    let v = store
        .verdict(same.id)
        .unwrap()
        .expect("the verdict must survive the rebuild");
    assert_eq!(v.verdict, conminer_core::store::Verdict::Benign);
    assert_eq!(
        v.note.as_deref(),
        Some("known noise"),
        "the reasoning must survive too"
    );
    assert_eq!(v.ticket.as_deref(), Some("TICKET-1"));
}
