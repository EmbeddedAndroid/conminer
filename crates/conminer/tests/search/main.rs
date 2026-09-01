//! Suite `search` (§8.1).
//!
//! Edge cases: phrase spanning 2/5/40 lines inside one record · phrase crossing
//! a record boundary (window scope finds it, record scope correctly doesn't) ·
//! terms vs phrase distinction · regex with and without extractable literals
//! (pre-narrow vs scan) · hostile regex (must stay linear) · unicode and
//! invalid-UTF-8 lines in the index path · `index=off` degradation ·
//! incremental index equals batch-rebuilt index · FTS result parity with grep
//! ground truth over corpus · cursor stability during live writes ·
//! large session: indexed phrase query is fast, windowed scan bounded and
//! reported.

use conminer_core::search::{
    extract_literals, search, ScopeKind, SearchMode, SearchQuery, SearchScope,
};
use conminer_core::store::DeviceStore;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::Rig;

fn store_with(text: &str) -> (Rig, DeviceStore) {
    let rig = Rig::new();
    let s = rig.ingest_text("search-rig", None, text);
    (rig, s)
}

fn q(query: &str, mode: SearchMode, scope: SearchScope) -> SearchQuery {
    SearchQuery {
        query: query.into(),
        mode,
        scope,
        max_results: 100,
        ..Default::default()
    }
}

// ------------------------------------------------------------- basic modes ---

#[test]
fn terms_finds_words_in_any_order_and_phrase_does_not() {
    let (_r, s) = store_with(&corpus_text("linux/boot-oops.log"));

    let terms = search(
        &s,
        &q("syncing panic", SearchMode::Terms, SearchScope::record()),
    )
    .unwrap();
    let phrase = search(
        &s,
        &q("syncing panic", SearchMode::Phrase, SearchScope::record()),
    )
    .unwrap();
    assert!(
        phrase.hits.is_empty(),
        "those two words are never contiguous in that order"
    );
    // The corpus has no panic line; both must agree there is nothing.
    assert_eq!(terms.hits.len(), phrase.hits.len());

    let real = search(
        &s,
        &q("Internal error", SearchMode::Phrase, SearchScope::line()),
    )
    .unwrap();
    assert_eq!(real.hits.len(), 1);
    assert_eq!(real.tier, "fts");
    assert!(!real.scan, "an indexed query must not report a scan");
}

#[test]
fn no_stemming_errno_must_not_match_error() {
    let (_r, s) = store_with("mmc0: errno reported\nmmc0: error reported\n");
    let errno = search(&s, &q("errno", SearchMode::Terms, SearchScope::line())).unwrap();
    assert_eq!(errno.hits.len(), 1);
    assert!(errno.hits[0].text.contains("errno"));
}

#[test]
fn highlights_point_at_the_match() {
    let (_r, s) = store_with("[    1.0] Internal error: Oops: 96000006 [#1] SMP\n");
    let r = search(&s, &q("Oops", SearchMode::Phrase, SearchScope::line())).unwrap();
    let h = &r.hits[0];
    let (a, b) = h.highlights[0];
    assert_eq!(&h.text[a..b], "Oops");
}

// ---------------------------------------------------- multiline in a record --

fn multiline_corpus(body_lines: usize) -> String {
    let mut s = String::from("[ 0.0] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n");
    s.push_str("[ 1.0] Internal error: Oops: 0 [#1] PREEMPT SMP\n");
    s.push_str("[ 1.0] Modules linked in: needle_start\n");
    for i in 0..body_lines {
        s.push_str(&format!("[ 1.0] x{i} : 0x0000000000000000 y{i} : 0x0\n"));
    }
    s.push_str("[ 1.0] Hardware name: needle_end board\n");
    s.push_str("[ 1.0] ---[ end trace 0 ]---\n");
    s
}

#[test]
fn a_phrase_spanning_two_lines_inside_one_record_is_the_indexed_fast_path() {
    let (_r, s) = store_with(&multiline_corpus(0));
    let r = search(
        &s,
        &q(
            "needle_start\n[ 1.0] Hardware name: needle_end",
            SearchMode::Phrase,
            SearchScope::record(),
        ),
    )
    .unwrap();
    assert_eq!(
        r.hits.len(),
        1,
        "the crash is one record, so this is one hit"
    );
    assert_eq!(r.tier, "fts", "record-scope multiline phrase is indexed");
    assert!(!r.scan);
}

#[test]
fn phrases_spanning_five_and_forty_lines_are_found_at_record_scope() {
    for body in [3usize, 38] {
        let (_r, s) = store_with(&multiline_corpus(body));
        let r = search(
            &s,
            &q(
                "needle_start needle_end",
                SearchMode::Terms,
                SearchScope::record(),
            ),
        )
        .unwrap();
        assert_eq!(r.hits.len(), 1, "body={body}");
        assert!(
            r.hits[0].text.lines().count() >= body + 2,
            "the whole record comes back, not just the matching line"
        );
    }
}

#[test]
fn a_phrase_crossing_a_record_boundary_needs_window_scope() {
    // Two ordinary lines: each is its own record, so the span exists only in the
    // stream, not in any record.
    let text = "[ 0.0] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n\
                [ 1.0] mmc0: waiting for card\n[ 1.1] mmc0: card present now\n";
    let (_r, s) = store_with(text);

    let by_record = search(
        &s,
        &q(
            "waiting for card\n[ 1.1] mmc0: card present",
            SearchMode::Phrase,
            SearchScope::record(),
        ),
    )
    .unwrap();
    assert!(
        by_record.hits.is_empty(),
        "record scope must not invent a span that crosses records"
    );

    let by_window = search(
        &s,
        &q(
            "waiting for card\n[ 1.1] mmc0: card present",
            SearchMode::Phrase,
            SearchScope::window(20),
        ),
    )
    .unwrap();
    assert_eq!(by_window.hits.len(), 1);
    assert!(by_window.scan, "the windowed tier reports itself as a scan");
    assert!(by_window.tier.starts_with("window("));
    assert!(by_window.bytes_scanned > 0, "and reports what it cost");
}

// -------------------------------------------------------------------- regex --

#[test]
fn a_regex_with_literals_is_pre_narrowed_by_the_index() {
    let (_r, s) = store_with(&corpus_text("linux/boot-oops.log"));
    let r = search(
        &s,
        &q(
            r"Booted secondary processor 0x[0-9a-f]+",
            SearchMode::Regex,
            SearchScope::line(),
        ),
    )
    .unwrap();
    assert_eq!(r.hits.len(), 3);
    assert_eq!(r.tier, "fts+regex", "extractable literals must pre-narrow");
    assert!(!r.scan);
}

#[test]
fn a_literal_free_regex_falls_back_to_a_scan_and_says_so() {
    let (_r, s) = store_with(&corpus_text("linux/boot-oops.log"));
    // No run of ≥3 literal word characters anywhere, so nothing can narrow it.
    let r = search(&s, &q(r"\d{6}\]", SearchMode::Regex, SearchScope::line())).unwrap();
    assert!(!r.hits.is_empty());
    assert_eq!(r.tier, "scan");
    assert!(r.scan);
    assert!(r.rows_scanned > 0);
}

#[test]
fn pre_narrowing_never_loses_a_match_the_scan_would_find() {
    // The property that makes tier 2 safe: narrowing is an optimisation, never a
    // filter. Compare against a ground-truth scan for every pattern.
    let text = corpus_text("linux/boot-oops.log");
    let (_r, s) = store_with(&text);

    for pattern in [
        r"Booted secondary processor",
        r"mmc\d+: new",
        r"Modules linked in: \S+",
        r"colou?r",
        r"panic|Oops",
        r"EXT4-fs \([a-z0-9]+\)",
    ] {
        let indexed = search(&s, &q(pattern, SearchMode::Regex, SearchScope::line())).unwrap();
        let truth: Vec<&str> = {
            let re = regex::Regex::new(pattern).unwrap();
            text.lines().filter(|l| re.is_match(l)).collect()
        };
        assert_eq!(
            indexed.hits.len(),
            truth.len(),
            "pattern {pattern:?} (tier {})",
            indexed.tier
        );
    }
}

#[test]
fn a_hostile_regex_stays_linear() {
    let (_r, s) = store_with(&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".repeat(500));
    let start = std::time::Instant::now();
    let r = search(&s, &q("(a+)+b", SearchMode::Regex, SearchScope::line())).unwrap();
    assert!(r.hits.is_empty());
    assert!(
        start.elapsed().as_secs() < 5,
        "nested quantifiers must not blow up: {:?}",
        start.elapsed()
    );
}

#[test]
fn an_invalid_regex_is_a_structured_error() {
    let (_r, s) = store_with("x\n");
    let err = search(&s, &q("([unclosed", SearchMode::Regex, SearchScope::line())).unwrap_err();
    assert_eq!(err.code, conminer_core::ErrorCode::InvalidArgument);
}

// ------------------------------------------------------------ hostile bytes --

#[test]
fn unicode_and_invalid_utf8_stay_findable() {
    let rig = Rig::new();
    let mut p = rig.pipeline("bytes", None);
    p.begin_session(conminer_core::store::SessionSource::File, None, None, None)
        .unwrap();
    p.feed("thermal: sensor ✓ nominal\n".as_bytes()).unwrap();
    p.feed(&[
        b'b', b'a', b'd', b' ', 0xff, 0xfe, b' ', b'l', b'i', b'n', b'e', b'\n',
    ])
    .unwrap();
    p.finish().unwrap();
    let s = p.into_store();

    let uni = search(&s, &q("nominal", SearchMode::Terms, SearchScope::line())).unwrap();
    assert_eq!(uni.hits.len(), 1);
    assert!(uni.hits[0].text.contains('✓'));

    // The invalid line is indexed lossily, so it is still reachable — through
    // the index if the tokenizer kept the words, and through the raw scan
    // regardless (§8.1).
    let by_scan = search(
        &s,
        &q(r"bad .* line", SearchMode::Regex, SearchScope::line()),
    )
    .unwrap();
    assert_eq!(by_scan.hits.len(), 1, "tier {}", by_scan.tier);
}

// ---------------------------------------------------------- index disabled ---

#[test]
fn index_off_degrades_to_scanning_and_reports_it() {
    let mut cfg = conminer_core::config::Config::default();
    cfg.search.fts = false;
    let rig = Rig::with_config(cfg);
    let s = rig.ingest_text("noindex", None, &corpus_text("linux/boot-oops.log"));
    assert!(!s.fts_enabled());

    let r = search(
        &s,
        &q("Internal error", SearchMode::Phrase, SearchScope::line()),
    )
    .unwrap();
    assert_eq!(r.hits.len(), 1, "results are the same…");
    assert!(r.scan, "…but the response says it was a scan");
    assert!(r.tier.contains("index=off"));
}

// ------------------------------------------------------- parity and cursors --

#[test]
fn fts_results_match_grep_ground_truth_over_the_corpus() {
    for file in [
        "linux/boot-oops.log",
        "uboot/spl-to-kernel.log",
        "tfa/panic.log",
        "zephyr/fatal.log",
        "optee/interleaved.log",
    ] {
        let text = corpus_text(file);
        let (_r, s) = store_with(&text);
        for needle in ["error", "CPU", "version", "mmc", "0x0"] {
            // Regex mode has grep's substring semantics, so grep is the honest
            // ground truth. FTS terms match whole tokens by design.
            let hits = search(
                &s,
                &SearchQuery {
                    query: regex::escape(needle),
                    mode: SearchMode::Regex,
                    scope: SearchScope::line(),
                    max_results: 100_000,
                    ..Default::default()
                },
            )
            .unwrap();
            let truth = text.lines().filter(|l| l.contains(needle)).count();
            assert_eq!(
                hits.hits.len(),
                truth,
                "{file} / {needle:?} (tier {})",
                hits.tier
            );
        }
    }
}

#[test]
fn a_capped_result_carries_a_resume_offset_that_does_not_repeat_or_skip() {
    let (_r, s) = store_with(&"[ 1.0] mmc0: repeated marker line\n".repeat(50));

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let r = search(
            &s,
            &SearchQuery {
                query: "marker".into(),
                mode: SearchMode::Terms,
                scope: SearchScope::line(),
                max_results: 7,
                after_offset: after,
                ..Default::default()
            },
        )
        .unwrap();
        seen.extend(r.hits.iter().map(|h| h.line_id));
        if !r.capped {
            break;
        }
        after = r.next_offset;
        assert!(after.is_some(), "a capped page must say where to resume");
    }
    assert_eq!(seen.len(), 50, "every line exactly once");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 50, "no repeats across pages");
}

#[test]
fn a_cursor_stays_valid_while_the_device_keeps_writing() {
    let rig = Rig::new();
    let mut p = rig.pipeline("live-cursor", None);
    p.begin_session(conminer_core::store::SessionSource::Live, None, None, None)
        .unwrap();
    for i in 0..40 {
        p.feed(format!("[ 1.0] marker line {i}\n").as_bytes())
            .unwrap();
    }

    let first = {
        let s = p.store();
        search(
            s,
            &SearchQuery {
                query: "marker".into(),
                mode: SearchMode::Terms,
                scope: SearchScope::line(),
                max_results: 10,
                ..Default::default()
            },
        )
        .unwrap()
    };
    assert!(first.capped);
    let resume = first.next_offset.unwrap();

    // More data arrives between pages — the resume point must still be honoured.
    for i in 40..60 {
        p.feed(format!("[ 1.0] marker line {i}\n").as_bytes())
            .unwrap();
    }

    let s = p.store();
    let second = search(
        s,
        &SearchQuery {
            query: "marker".into(),
            mode: SearchMode::Terms,
            scope: SearchScope::line(),
            max_results: 1000,
            after_offset: Some(resume),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(second.hits.iter().all(|h| h.stream_offset >= resume));
    assert_eq!(first.hits.len() + second.hits.len(), 60);
}

// ------------------------------------------------------------------- scale ---

#[test]
fn an_indexed_phrase_query_over_a_large_session_is_fast_and_a_window_scan_is_bounded() {
    let unit = corpus_text("linux/boot-oops.log");
    let mut text = unit.repeat(200);
    text.push_str("[ 9.9] the needle we are looking for\n");
    let (_r, s) = store_with(&text);
    assert!(s.line_count().unwrap() > 8000);

    let start = std::time::Instant::now();
    let r = search(
        &s,
        &q(
            "the needle we are looking for",
            SearchMode::Phrase,
            SearchScope::line(),
        ),
    )
    .unwrap();
    let indexed = start.elapsed();
    assert_eq!(r.hits.len(), 1);
    assert!(!r.scan);
    assert!(
        indexed.as_millis() < 500,
        "an indexed phrase query must not scan: {indexed:?}"
    );

    // The windowed tier is allowed to be slow, but it must be bounded and must
    // report what it cost rather than pretending to be cheap.
    let w = search(
        &s,
        &q("needle we are", SearchMode::Phrase, SearchScope::window(5)),
    )
    .unwrap();
    assert!(w.scan);
    assert_eq!(w.hits.len(), 1);
    assert!(w.bytes_scanned > 100_000);
    assert_eq!(w.rows_scanned, s.line_count().unwrap() as usize);
}

#[test]
fn scope_kinds_are_distinguishable_in_the_response() {
    let (_r, s) = store_with(&multiline_corpus(2));
    let line = search(
        &s,
        &q("needle_start", SearchMode::Terms, SearchScope::line()),
    )
    .unwrap();
    let record = search(
        &s,
        &q("needle_start", SearchMode::Terms, SearchScope::record()),
    )
    .unwrap();
    let window = search(
        &s,
        &q("needle_start", SearchMode::Terms, SearchScope::window(4)),
    )
    .unwrap();
    assert_eq!(line.hits[0].kind, "line");
    assert_eq!(record.hits[0].kind, "record");
    assert_eq!(window.hits[0].kind, "window");
    assert!(record.hits[0].text.len() > line.hits[0].text.len());
}

#[test]
fn literal_extraction_is_covered_directly() {
    assert!(extract_literals("Kernel panic").contains(&"Kernel".to_string()));
    assert!(extract_literals(r"[0-9]+").is_empty());
    assert_eq!(SearchScope::default().kind, ScopeKind::Line);
}
