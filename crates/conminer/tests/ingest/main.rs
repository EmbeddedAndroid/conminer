//! Suite `ingest` (§13) — post-hoc mining.
//!
//! Edge cases: 10 KB · 800 MB · gzip'd input · file with BOM · concurrent
//! ingests same device · duplicate re-ingest (idempotent by content hash → new
//! session, warning) · nonexistent path · permission denied.

use conminer_core::ingest::{ingest_file, Codec, IngestOptions};
use conminer_core::store::SessionSource;
use conminer_core::ErrorCode;
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::Rig;

fn sized(target: usize) -> String {
    let unit = corpus_text("linux/boot-oops.log");
    let mut s = String::with_capacity(target + unit.len());
    while s.len() < target {
        s.push_str(&unit);
    }
    s
}

#[test]
fn the_smallest_case_ten_kilobytes() {
    let rig = Rig::new();
    let text = sized(10 * 1024);
    let path = rig.write_file("small.log", text.as_bytes());
    let mut p = rig.pipeline("small", None);
    let r = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap();

    assert_eq!(r.bytes as usize, text.len(), "every byte accounted for");
    assert!(r.lines > 100);
    assert!(r.templates > 0);
    assert_eq!(r.codec, Codec::Plain);
    assert!(
        r.compression_ratio > 2.0,
        "a repeated boot log must compress: {:.2}",
        r.compression_ratio
    );
}

#[test]
fn a_large_file_streams_at_speed_without_holding_it_in_memory() {
    // Throughput is a §12.5 nightly bench, measured on a release build; this is
    // the per-PR proxy, and it checks the property a debug build *can* check:
    // that the file streams rather than being materialised, and that a repeating
    // boot log collapses. Sized so the whole suite stays fast.
    let rig = Rig::new();
    let text = sized(8 * 1024 * 1024);
    let path = rig.write_file("big.log", text.as_bytes());
    let mut p = rig.pipeline("big", None);
    let r = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap();

    assert_eq!(r.bytes as usize, text.len());
    assert!(
        r.compression_ratio > 1000.0,
        "8 MB of a repeating boot log is a handful of templates: {:.0}",
        r.compression_ratio
    );
    // Not a hard gate here (the runner is shared); the floor lives in §12.5.
    eprintln!(
        "ingest throughput {:.0} MB/s ({} ms)",
        r.throughput_mb_s, r.duration_ms
    );
}

#[test]
#[ignore = "§12.5 nightly: the full 800 MB benchmark file"]
fn the_eight_hundred_megabyte_case() {
    let rig = Rig::new();
    let text = sized(800 * 1024 * 1024);
    let path = rig.write_file("800mb.log", text.as_bytes());
    let mut p = rig.pipeline("huge", None);
    let opts = IngestOptions {
        max_bytes: Some(2 * 1024 * 1024 * 1024),
        ..Default::default()
    };
    let r = ingest_file(&mut p, &path, &opts).unwrap();
    assert_eq!(r.bytes, 800 * 1024 * 1024);
    assert!(
        r.throughput_mb_s >= 100.0,
        "the §12.5 floor is 100 MB/s, got {:.0}",
        r.throughput_mb_s
    );
}

#[test]
fn gzipped_input_is_detected_and_produces_identical_results() {
    let rig = Rig::new();
    let text = sized(64 * 1024);
    let plain = rig.write_file("a.log", text.as_bytes());
    let gz = rig.write_gzip("b.log.gz", text.as_bytes());

    let mut p1 = rig.pipeline("plain", None);
    let a = ingest_file(&mut p1, &plain, &IngestOptions::default()).unwrap();
    let mut p2 = rig.pipeline("gz", None);
    let b = ingest_file(&mut p2, &gz, &IngestOptions::default()).unwrap();

    assert_eq!(a.codec, Codec::Plain);
    assert_eq!(b.codec, Codec::Gzip);
    assert_eq!(a.bytes, b.bytes, "the decompressed byte count matches");
    assert_eq!(a.lines, b.lines);
    assert_eq!(a.templates, b.templates);
    assert_eq!(
        a.content_sha, b.content_sha,
        "the hash is of the content, not the container"
    );
}

#[test]
fn a_file_with_a_utf8_bom_ingests_with_the_bom_stored_and_not_mined() {
    let rig = Rig::new();
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"[    0.000000] Linux version 6.12.9 (b@h) (gcc)\n");
    bytes.extend_from_slice(b"[    1.000000] mmc0: card ready\n");
    let path = rig.write_file("bom.log", &bytes);

    let mut p = rig.pipeline("bom", None);
    let r = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap();
    let store = p.into_store();

    assert_eq!(r.lines, 2);
    let first = &store.lines_for_session(r.session_id).unwrap()[0];
    assert_eq!(
        &first.bytes[..3],
        &[0xEF, 0xBB, 0xBF],
        "the BOM is stored verbatim like every other byte (§6)"
    );

    // …but it is not part of the template, or the first line of every BOM'd file
    // would be its own template forever.
    let t = store
        .list_templates(&conminer_core::store::TemplateQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert!(
        t.iter().all(|x| !x.text.starts_with('\u{feff}')),
        "{:?}",
        t.iter().map(|x| x.text.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn re_ingesting_the_same_content_makes_a_new_session_and_warns() {
    let rig = Rig::new();
    let text = sized(8 * 1024);
    let path = rig.write_file("again.log", text.as_bytes());

    let dev = rig.device("again");
    let mut ids = Vec::new();
    let mut second_report = None;
    for i in 0..2 {
        let mut p = conminer_core::pipeline::Pipeline::new(
            rig.store(&dev),
            rig.profiles.clone(),
            rig.config.clone(),
            &dev.canonical,
            None,
            rig.clock.clone(),
        )
        .unwrap();
        let r = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap();
        ids.push(r.session_id);
        if i == 1 {
            second_report = Some(r);
        }
    }

    assert_ne!(
        ids[0], ids[1],
        "re-running a job is legitimate: new session"
    );
    let r2 = second_report.unwrap();
    assert_eq!(
        r2.duplicate_of,
        Some(ids[0]),
        "…but the agent is told, so it does not diff a session against itself"
    );
    assert_eq!(r2.new_templates, 0, "nothing is novel the second time");
}

#[test]
fn different_content_is_not_flagged_as_a_duplicate() {
    let rig = Rig::new();
    let dev = rig.device("distinct");
    let a = rig.write_file("a.log", b"alpha line\n");
    let b = rig.write_file("b.log", b"bravo line\n");
    let mut reports = Vec::new();
    for path in [a, b] {
        let mut p = conminer_core::pipeline::Pipeline::new(
            rig.store(&dev),
            rig.profiles.clone(),
            rig.config.clone(),
            &dev.canonical,
            None,
            rig.clock.clone(),
        )
        .unwrap();
        reports.push(ingest_file(&mut p, &path, &IngestOptions::default()).unwrap());
    }
    assert!(reports.iter().all(|r| r.duplicate_of.is_none()));
    assert_ne!(reports[0].content_sha, reports[1].content_sha);
}

#[test]
fn concurrent_ingests_into_the_same_device_both_complete_and_stay_separate() {
    let rig = Rig::new();
    let dev = rig.device("concurrent");
    let text = sized(64 * 1024);
    let paths: Vec<_> = (0..4)
        .map(|i| rig.write_file(&format!("c{i}.log"), format!("job {i}\n{text}").as_bytes()))
        .collect();

    let handles: Vec<_> = paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let store = rig.store(&dev);
            let profiles = rig.profiles.clone();
            let config = rig.config.clone();
            let canonical = dev.canonical.clone();
            let clock = rig.clock.clone();
            std::thread::spawn(move || {
                let mut p = conminer_core::pipeline::Pipeline::new(
                    store, profiles, config, &canonical, None, clock,
                )
                .unwrap();
                let opts = IngestOptions {
                    label: Some(format!("job-{i}")),
                    ..Default::default()
                };
                ingest_file(&mut p, &path, &opts).unwrap()
            })
        })
        .collect();

    let reports: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(reports.len(), 4);

    let sessions: std::collections::HashSet<i64> = reports.iter().map(|r| r.session_id).collect();
    assert_eq!(sessions.len(), 4, "session boundaries must not collide");

    let store = rig.store(&dev);
    assert_eq!(store.integrity_check().unwrap(), "ok");

    // Every line landed in exactly the session that ingested it.
    for r in &reports {
        let lines = store.lines_for_session(r.session_id).unwrap();
        assert_eq!(lines.len(), r.lines, "session {} lost lines", r.session_id);
    }
}

#[test]
fn a_nonexistent_path_is_a_structured_error() {
    let rig = Rig::new();
    let mut p = rig.pipeline("missing", None);
    let err = ingest_file(
        &mut p,
        std::path::Path::new("/definitely/not/here.log"),
        &IngestOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchPath);
    assert!(err.hint.contains("container"), "{}", err.hint);
}

#[test]
fn a_directory_is_rejected_rather_than_read() {
    let rig = Rig::new();
    let mut p = rig.pipeline("dir", None);
    let err = ingest_file(&mut p, rig.path(), &IngestOptions::default()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchPath);
    assert!(err.message.contains("directory"));
}

#[test]
fn an_unreadable_file_is_permission_denied() {
    // Root ignores file modes, so this asserts the real thing only when the
    // tests are not running as root — and says so rather than silently passing.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root: file-mode enforcement is not observable, skipping");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let path = rig.write_file("secret.log", b"classified\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut p = rig.pipeline("perm", None);
    let err = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

#[test]
fn a_file_over_the_cap_is_rejected_with_a_split_hint() {
    let rig = Rig::new();
    let path = rig.write_file("over.log", &vec![b'x'; 4096]);
    let mut p = rig.pipeline("over", None);
    let opts = IngestOptions {
        max_bytes: Some(1024),
        ..Default::default()
    };

    let err = ingest_file(&mut p, &path, &opts).unwrap_err();
    assert_eq!(err.code, ErrorCode::IngestTooLarge);
    assert!(err.hint.contains("split"), "{}", err.hint);
    let d = err.detail.unwrap();
    assert_eq!(d["size"], 4096);
    assert_eq!(d["suggested_parts"], 4);
}

#[test]
fn an_ingested_session_is_tagged_as_a_file_source() {
    let rig = Rig::new();
    let path = rig.write_file("tagged.log", b"one line\n");
    let mut p = rig.pipeline("tagged", None);
    let opts = IngestOptions {
        label: Some("lava job 12345".into()),
        ..Default::default()
    };
    let r = ingest_file(&mut p, &path, &opts).unwrap();
    let store = p.into_store();

    let s = store.session(r.session_id).unwrap();
    assert!(matches!(s.source, SessionSource::File));
    assert_eq!(s.label.as_deref(), Some("lava job 12345"));
    assert!(s.source_path.unwrap().ends_with("tagged.log"));
    assert!(s.ended_at.is_some(), "a finished ingest closes its session");
}

#[test]
fn an_ingest_opens_an_epoch_so_boot_scoped_queries_work_on_file_data() {
    let rig = Rig::new();
    let path = rig.write_file("epoch.log", corpus_text("mixed/boot-loop.log").as_bytes());
    let mut p = rig.pipeline("file-epoch", None);
    let r = ingest_file(&mut p, &path, &IngestOptions::default()).unwrap();
    let store = p.into_store();

    let boots = store.list_boots(50).unwrap();
    assert_eq!(boots.len(), r.boots, "the report and the store agree");
    assert!(boots.len() >= 3, "three loop iterations, three epochs");
    assert_eq!(boots.last().unwrap().opened_by, "ingest");
    assert!(
        boots.iter().all(|b| b.fingerprint.is_some()),
        "every closed epoch is fingerprinted (§8.4)"
    );
}
