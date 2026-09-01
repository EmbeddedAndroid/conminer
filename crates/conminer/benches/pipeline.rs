//! Performance benches (§12.5), run nightly with CI regression gates.
//!
//! Deliberately plain `#[bench]`-free timing rather than a criterion harness, so
//! the numbers can be read from CI output without a report artifact and the
//! floors below are the ones the spec states.
//!
//! Run with `./cm bench`. The floors are asserted only when
//! `CONMINER_BENCH_GATE=1`, so a developer machine reports numbers while CI
//! enforces them.

use conminer_core::store::SessionSource;
use conminer_testkit::Rig;
use std::time::Instant;

fn gated() -> bool {
    std::env::var("CONMINER_BENCH_GATE").as_deref() == Ok("1")
}

fn report(name: &str, bytes: u64, elapsed: std::time::Duration, floor_mb_s: f64) {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let rate = mb / elapsed.as_secs_f64().max(1e-9);
    println!("{name}: {mb:.1} MB in {elapsed:?} = {rate:.1} MB/s (floor {floor_mb_s})");
    if gated() {
        assert!(
            rate >= floor_mb_s,
            "{name} regressed below the {floor_mb_s} MB/s floor: {rate:.1}"
        );
    }
}

fn corpus(target: usize) -> String {
    let unit = conminer_testkit::corpus::corpus_text("linux/boot-oops.log");
    let mut s = String::with_capacity(target + unit.len());
    while s.len() < target {
        s.push_str(&unit);
    }
    s
}

#[test]
#[ignore = "§12.5 nightly: ingest throughput"]
fn bench_ingest_throughput() {
    let text = corpus(64 * 1024 * 1024);
    let rig = Rig::new();
    let path = rig.write_file("bench.log", text.as_bytes());
    let mut p = rig.pipeline("bench", None);
    let started = Instant::now();
    let r = conminer_core::ingest::ingest_file(
        &mut p,
        &path,
        &conminer_core::ingest::IngestOptions::default(),
    )
    .unwrap();
    report("ingest", r.bytes, started.elapsed(), 100.0);
}

#[test]
#[ignore = "§12.5 nightly: smallest-case fixed cost"]
fn bench_smallest_case() {
    let rig = Rig::new();
    let path = rig.write_file("tiny.log", corpus(10 * 1024).as_bytes());
    let mut p = rig.pipeline("tiny", None);
    let started = Instant::now();
    conminer_core::ingest::ingest_file(
        &mut p,
        &path,
        &conminer_core::ingest::IngestOptions::default(),
    )
    .unwrap();
    let e = started.elapsed();
    println!("smallest case (10 KB): {e:?}");
    if gated() {
        assert!(e.as_millis() < 500, "fixed cost regressed: {e:?}");
    }
}

#[test]
#[ignore = "§12.5 nightly: per-line live-path latency"]
fn bench_live_line_latency() {
    let rig = Rig::new();
    let mut p = rig.pipeline("live", None);
    p.begin_session(SessionSource::Live, None, None, None)
        .unwrap();
    let line = b"[    1.234567] mmc0: new HS200 MMC card at address 0001\n";

    let started = Instant::now();
    let n = 20_000;
    for _ in 0..n {
        p.feed(line).unwrap();
    }
    let per = started.elapsed() / n;
    println!("live path: {per:?} per line");
    if gated() {
        assert!(per.as_micros() < 500, "per-line latency regressed: {per:?}");
    }
}

#[test]
#[ignore = "§12.5 nightly: list_templates on a large session"]
fn bench_list_templates() {
    let rig = Rig::new();
    let store = rig.ingest_text("big", None, &corpus(16 * 1024 * 1024));
    let started = Instant::now();
    let rows = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    let e = started.elapsed();
    println!(
        "list_templates over {} lines -> {} rows in {e:?}",
        store.line_count().unwrap(),
        rows.len()
    );
    if gated() {
        assert!(e.as_millis() < 200, "template lookup regressed: {e:?}");
    }
}

#[test]
#[ignore = "§12.5 nightly: template-count growth as the fragmentation health metric"]
fn bench_fragmentation_stays_bounded() {
    let rig = Rig::new();
    let store = rig.ingest_text("frag", None, &corpus(16 * 1024 * 1024));
    let s = store.stats(None).unwrap();
    println!(
        "{} lines -> {} templates, fragmentation {:.2}x, compression {:.0}x",
        s.lines, s.templates, s.fragmentation_ratio, s.compression_ratio
    );
    if gated() {
        assert!(
            s.fragmentation_ratio < 3.0,
            "fragmentation regressed to {:.2}x",
            s.fragmentation_ratio
        );
    }
}
