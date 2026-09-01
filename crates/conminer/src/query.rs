//! CLI query and ingest commands (§8, §10 Phase 1).
//!
//! The whole point of the tool surface is that an agent — or a human — reads a
//! *table of contents* instead of paging a multi-megabyte log, and drills into
//! verbatim raw lines only on demand. These commands are the local expression of
//! that: `templates` is the contents, `records`/`context` are the drill-down.

use crate::app::App;
use crate::output::{ellipsize, escape_bytes, human_bytes, Writer};
use anyhow::{Context as _, Result};
use conminer_core::config::Config;
use conminer_core::drain::DrainConfig;
use conminer_core::ingest::{ingest_file, IngestOptions};
use conminer_core::store::{DeviceStore, TemplateQuery};
use serde_json::json;
use std::path::Path;

fn open(app: &App, selector: Option<&str>) -> Result<DeviceStore> {
    let reg = app.registry()?;
    let dev = match selector {
        Some(s) => reg.resolve(s)?,
        None => {
            let all = reg.all_devices()?;
            match all.len() {
                1 => all.into_iter().next().unwrap(),
                0 => anyhow::bail!("no devices known yet; run `conminer ingest <file>` first"),
                _ => anyhow::bail!(
                    "{} devices known; pass --device (one of: {})",
                    all.len(),
                    all.iter()
                        .map(|d| d.display_name().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    app.open_store(&reg, &dev)
}

// -------------------------------------------------------------------- ingest --

pub fn ingest(
    app: &App,
    out: &Writer,
    path: &Path,
    device: Option<&str>,
    profile: Option<&str>,
    label: Option<String>,
) -> Result<()> {
    let mut reg = app.registry()?;
    let dev = app.resolve_or_create(&mut reg, device, Some(path))?;
    let mut pipe = app.open_pipeline(&reg, &dev, profile)?;

    let mut opts = IngestOptions::from_config(&app.config);
    opts.label = label;
    let report = ingest_file(&mut pipe, path, &opts)?;

    out.emit(&report, || {
        println!(
            "session {}  device {}",
            report.session_id,
            dev.display_name()
        );
        println!(
            "  {} in {} lines, {} records, {} templates ({} new)",
            human_bytes(report.bytes),
            report.lines,
            report.records,
            report.templates,
            report.new_templates
        );
        println!(
            "  compression {:.1}x  ·  {:.0} MB/s  ·  {} ms  ·  codec {}",
            report.compression_ratio,
            report.throughput_mb_s,
            report.duration_ms,
            report.codec.as_str()
        );
        if !report.stage_transitions.is_empty() {
            println!("  stages: {}", report.stage_transitions.join(" → "));
        }
        if report.crash_records > 0 {
            println!("  crash records: {}", report.crash_records);
        }
        if report.garbage_lines > 0 {
            println!(
                "  {} lines quarantined as garbage (baud mismatch?)",
                report.garbage_lines
            );
        }
        if let Some(prev) = report.duplicate_of {
            println!("  note: byte-identical content was already ingested as session {prev}");
        }
        Ok(())
    })
}

// ------------------------------------------------------------------ queries --

pub fn templates(app: &App, out: &Writer, device: Option<&str>, q: TemplateQuery) -> Result<()> {
    let store = open(app, device)?;
    let rows = store.list_templates(&q)?;
    out.emit(&rows, || {
        println!("{:>8}  {:>9}  {:<9}  template", "id", "count", "severity");
        for t in &rows {
            println!(
                "{:>8}  {:>9}  {:<9}  {}",
                t.id,
                t.scoped_count.unwrap_or(t.total_count),
                format!("{:?}", t.severity).to_lowercase(),
                ellipsize(&t.text, 90)
            );
        }
        println!("\n{} templates", rows.len());
        Ok(())
    })
}

pub fn records(
    app: &App,
    out: &Writer,
    device: Option<&str>,
    template: i64,
    session: Option<i64>,
    limit: usize,
) -> Result<()> {
    let store = open(app, device)?;
    let t = store.template(template)?;
    let recs = store.records_for_template(template, session, None, limit, 0)?;

    let payload: Vec<_> = recs
        .iter()
        .map(|r| {
            let lines = store.record_lines(r.id).unwrap_or_default();
            json!({
                "record_id": r.id,
                "session_id": r.session_id,
                "boot_id": r.boot_id,
                "kind": r.kind,
                "severity": r.severity,
                "profile": r.profile,
                "truncated": r.truncated,
                "fields": r.fields,
                "lines": lines.iter().map(|l| json!({
                    "line_id": l.id,
                    "offset": l.stream_offset,
                    "ts_wall": l.ts_wall,
                    "raw": escape_bytes(&l.bytes),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    out.emit(&json!({"template": t, "records": payload}), || {
        println!("template {}: {}", t.id, t.text);
        println!("  seen {} times overall\n", t.total_count);
        for r in &recs {
            println!("--- record {} (session {})", r.id, r.session_id);
            for l in store.record_lines(r.id)? {
                println!("  [{}] {}", l.id, escape_bytes(&l.bytes));
            }
        }
        Ok(())
    })
}

pub fn context(
    app: &App,
    out: &Writer,
    device: Option<&str>,
    line: i64,
    before: usize,
    after: usize,
) -> Result<()> {
    let store = open(app, device)?;
    let lines = store.context(line, before, after)?;
    let payload: Vec<_> = lines
        .iter()
        .map(|l| {
            json!({
                "line_id": l.id,
                "offset": l.stream_offset,
                "ts_wall": l.ts_wall,
                "anchor": l.id == line,
                "raw": escape_bytes(&l.bytes),
            })
        })
        .collect();
    out.emit(&payload, || {
        for l in &lines {
            let marker = if l.id == line { ">>" } else { "  " };
            println!("{marker} [{}] {}", l.id, escape_bytes(&l.bytes));
        }
        Ok(())
    })
}

pub fn search(
    app: &App,
    out: &Writer,
    device: Option<&str>,
    pattern: &str,
    session: Option<i64>,
    limit: usize,
) -> Result<()> {
    let store = open(app, device)?;
    let re = regex::Regex::new(pattern).with_context(|| format!("invalid regex {pattern:?}"))?;
    let (hits, scanned) = store.search_regex(&re, session, limit)?;
    let capped = hits.len() >= limit;
    let payload = json!({
        "matches": hits.iter().map(|l| json!({
            "line_id": l.id,
            "session_id": l.session_id,
            "offset": l.stream_offset,
            "raw": escape_bytes(&l.bytes),
        })).collect::<Vec<_>>(),
        "scanned": scanned,
        "scan": true,
        "capped": capped,
    });
    out.emit(&payload, || {
        for l in &hits {
            println!("[{}] {}", l.id, escape_bytes(&l.bytes));
        }
        println!(
            "\n{} matches ({} lines scanned){}",
            hits.len(),
            scanned,
            if capped { ", capped" } else { "" }
        );
        Ok(())
    })
}

pub fn recent(app: &App, out: &Writer, device: Option<&str>, lines: usize) -> Result<()> {
    let store = open(app, device)?;
    let rows = store.recent_lines(lines)?;
    let payload: Vec<_> = rows
        .iter()
        .map(|l| json!({"line_id": l.id, "offset": l.stream_offset, "raw": escape_bytes(&l.bytes)}))
        .collect();
    out.emit(&payload, || {
        for l in &rows {
            println!("{}", escape_bytes(&l.bytes));
        }
        Ok(())
    })
}

pub fn stats(app: &App, out: &Writer, device: Option<&str>, session: Option<i64>) -> Result<()> {
    let store = open(app, device)?;
    let s = store.stats(session)?;
    out.emit(&s, || {
        println!("sessions   {}", s.sessions);
        println!("lines      {}", s.lines);
        println!("records    {}", s.records);
        println!("templates  {}", s.templates);
        println!("raw bytes  {}", human_bytes(s.bytes as u64));
        println!("db bytes   {}", human_bytes(s.db_bytes as u64));
        println!(
            "compression {:.1}x  (lines per template)",
            s.compression_ratio
        );
        println!(
            "fragmentation {:.2}x  (templates per message family)",
            s.fragmentation_ratio
        );
        println!("fts index  {}", if s.fts_enabled { "on" } else { "off" });
        Ok(())
    })
}

pub fn sessions(app: &App, out: &Writer, device: Option<&str>, limit: usize) -> Result<()> {
    let store = open(app, device)?;
    let rows = store.list_sessions(limit)?;
    out.emit(&rows, || {
        println!(
            "{:>5}  {:<6}  {:>9}  {:>8}  label",
            "id", "source", "lines", "records"
        );
        for s in &rows {
            println!(
                "{:>5}  {:<6}  {:>9}  {:>8}  {}",
                s.id,
                format!("{:?}", s.source).to_lowercase(),
                s.lines,
                s.records,
                s.label.as_deref().unwrap_or("-")
            );
        }
        Ok(())
    })
}

pub fn boots(app: &App, out: &Writer, device: Option<&str>, limit: usize) -> Result<()> {
    let store = open(app, device)?;
    let rows = store.list_boots(limit)?;
    out.emit(&rows, || {
        println!(
            "{:>5}  {:<8}  {:>10}  {:<16}  outcome",
            "seq", "opened", "bytes", "fingerprint"
        );
        for b in &rows {
            println!(
                "{:>5}  {:<8}  {:>10}  {:<16}  {}",
                b.seq,
                b.opened_by,
                human_bytes(b.bytes as u64),
                b.fingerprint.as_deref().unwrap_or("-"),
                b.outcome.as_deref().unwrap_or("-")
            );
        }
        Ok(())
    })
}

pub fn stages(
    app: &App,
    out: &Writer,
    device: Option<&str>,
    session: Option<i64>,
    boot: Option<i64>,
) -> Result<()> {
    let store = open(app, device)?;
    let rows = store.stages(session, boot)?;
    out.emit(&rows, || {
        for s in &rows {
            println!(
                "{:>6}  {:<16}  profile {:<10}  banner line {}",
                s.entered_ts,
                s.name,
                s.profile,
                s.banner_line_id
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
        Ok(())
    })
}

pub fn devices(app: &App, out: &Writer) -> Result<()> {
    let reg = app.registry()?;
    let rows = reg.all_devices()?;
    out.emit(&rows, || {
        if rows.is_empty() {
            println!("no devices");
            return Ok(());
        }
        for d in &rows {
            println!(
                "{:<24}  {:<10}  port {:<6}  {}",
                d.display_name(),
                d.identity.as_str(),
                d.ser2net_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into()),
                d.line.summary()
            );
            if !d.tags.is_empty() {
                let tags: Vec<String> = d.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
                println!("    tags: {}", tags.join(" "));
            }
            if let Some(o) = d.observed.as_object() {
                if !o.is_empty() {
                    let obs: Vec<String> = o.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    println!("    last booted: {}", obs.join(" "));
                }
            }
        }
        Ok(())
    })
}

/// §K1. Run the acceptance gauntlet against a real board.
///
/// Exit code is the point for CI: 0 pass, 1 fail, 2 pass_with_skips, and 3 when
/// cleanup could not VERIFY the board is off -- which is louder than a failing
/// check, because it means hardware was left in an unknown state.
#[allow(clippy::too_many_arguments)]
pub fn selftest(
    app: &App,
    out: &Writer,
    target: Option<String>,
    device: Option<String>,
    suites: Vec<String>,
    steal: bool,
    keep_on: bool,
) -> Result<()> {
    let ctx =
        conminer_mcp::Context::open(app.config.clone(), app.profiles.clone(), app.clock.clone())?;
    let report = conminer_mcp::selftest::run(
        &ctx,
        conminer_mcp::selftest::Opts {
            target,
            device,
            suites,
            steal,
            keep_on,
        },
    )?;
    let verdict = report["verdict"].as_str().unwrap_or("fail").to_string();
    let off_ok = report["cleanup"]["board_off_verified"]
        .as_bool()
        .unwrap_or(false)
        || keep_on;

    out.emit(&report, || {
        println!(
            "selftest {}: {} in {}s",
            report["target"].as_str().unwrap_or("(device)"),
            verdict,
            report["duration_s"]
        );
        for c in report["checks"].as_array().into_iter().flatten() {
            println!(
                "  {:4} {:34} {}",
                c["result"].as_str().unwrap_or("?"),
                c["id"].as_str().unwrap_or("?"),
                c["evidence"].as_str().unwrap_or("")
            );
        }
        println!(
            "  cleanup: board_off_verified={} straps_cleared={} ({})",
            report["cleanup"]["board_off_verified"],
            report["cleanup"]["straps_cleared"],
            report["cleanup"]["evidence"].as_str().unwrap_or("")
        );
        Ok(())
    })?;

    // A board left in an unknown power state outranks every other outcome.
    if !off_ok {
        std::process::exit(3);
    }
    match verdict.as_str() {
        "pass" => Ok(()),
        "pass_with_skips" => std::process::exit(2),
        _ => std::process::exit(1),
    }
}

/// §K5b. Fill in historical firmware versions without a template rebuild.
pub fn backfill_versions(app: &App, out: &Writer, device: Option<&str>) -> Result<()> {
    let mut store = open(app, device)?;
    let banners: Vec<conminer_core::framer::profile::VersionBanner> = app
        .profiles
        .all()
        .iter()
        .flat_map(|p| p.version_banners.iter().cloned())
        .collect();
    let report = store.backfill_versions(&banners)?;
    out.emit(&report, || {
        println!(
            "scanned {} lines: {} version extractions across {} epoch(s){}",
            report["lines_scanned"],
            report["versions_written"],
            report["epochs_filled"],
            match report["skipped_pruned"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0)
            {
                0 => String::new(),
                n => format!("; {n} epoch(s) skipped, their raw is pruned"),
            }
        );
        println!("no template was created, altered or renumbered");
        Ok(())
    })
}

pub fn rebuild_templates(
    app: &App,
    out: &Writer,
    device: Option<&str>,
    similarity: Option<f64>,
) -> Result<()> {
    let mut store = open(app, device)?;
    let mut cfg = DrainConfig::from(&app.config.mine);
    if let Some(s) = similarity {
        anyhow::ensure!((0.0..=1.0).contains(&s), "similarity must be in 0.0..=1.0");
        cfg.similarity = s;
    }
    let before = store.template_count()?;
    let after = store.rebuild_templates(cfg, &app.profiles)?;
    out.emit(
        &json!({"before": before, "after": after, "similarity": cfg.similarity}),
        || {
            println!(
                "rebuilt from raw: {before} → {after} templates at similarity {:.2}",
                cfg.similarity
            );
            Ok(())
        },
    )
}

// ---------------------------------------------------------- operator tools --

pub fn check_config(app: &App, out: &Writer, path: Option<&Path>) -> Result<()> {
    let text = match path {
        Some(p) => std::fs::read_to_string(p)?,
        None => match std::env::var("CONMINER_CONFIG") {
            Ok(p) if Path::new(&p).exists() => std::fs::read_to_string(p)?,
            _ => String::new(),
        },
    };
    let problems = Config::check(&text)?;
    let ok = problems.is_empty();
    out.emit(&json!({"ok": ok, "problems": problems}), || {
        if ok {
            println!("config ok ({} profiles loaded)", app.profiles.names().len());
        } else {
            for p in &problems {
                println!("{}: {}", p.key, p.problem);
            }
        }
        Ok(())
    })?;
    if ok {
        Ok(())
    } else {
        // Fail fast so the compose healthcheck catches it (§14.5).
        std::process::exit(1)
    }
}

pub fn profiles(app: &App, out: &Writer) -> Result<()> {
    let rows: Vec<_> = app
        .profiles
        .all()
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "stage": p.stage,
                "stage_rank": p.stage_rank,
                "overlay": p.overlay,
                "banners": p.banners.len(),
                "triggers": p.triggers.len(),
                "prompts": p.prompts.len(),
            })
        })
        .collect();
    out.emit(&rows, || {
        println!(
            "{:<10}  {:<14}  {:>4}  {:>8}  {:>8}",
            "name", "stage", "rank", "banners", "triggers"
        );
        for p in app.profiles.all() {
            println!(
                "{:<10}  {:<14}  {:>4}  {:>8}  {:>8}{}",
                p.name,
                p.stage,
                p.stage_rank,
                p.banners.len(),
                p.triggers.len(),
                if p.overlay { "  (overlay)" } else { "" }
            );
        }
        Ok(())
    })
}

/// `conminer profile test <profile> <corpus-file>` (§14.11) — develop a profile
/// against your own logs without touching Rust.
pub fn profile_test(
    app: &App,
    out: &Writer,
    profile: &str,
    corpus: &Path,
    verbose: bool,
) -> Result<()> {
    let p = app.profiles.require(profile)?;
    let store = DeviceStore::open_memory(&format!("profile-test:{}", p.name))?;
    let mut pipe = conminer_core::pipeline::Pipeline::new(
        store,
        app.profiles.clone(),
        app.config.clone(),
        "profile-test",
        Some(profile),
        app.clock.clone(),
    )?;
    let opts = IngestOptions {
        label: Some(format!("profile test {profile}")),
        ..Default::default()
    };
    let report = ingest_file(&mut pipe, corpus, &opts)?;

    let store = pipe.into_store();
    let templates = store.list_templates(&TemplateQuery {
        limit: if verbose { 1000 } else { 25 },
        ..Default::default()
    })?;
    let stages = store.stages(None, None)?;

    let payload = json!({
        "profile": p.name,
        "corpus": corpus.display().to_string(),
        "lines": report.lines,
        "records": report.records,
        "templates": report.templates,
        "crash_records": report.crash_records,
        "compression_ratio": report.compression_ratio,
        "stages": stages.iter().map(|s| &s.name).collect::<Vec<_>>(),
        "top_templates": templates.iter().map(|t| json!({
            "id": t.id, "count": t.total_count,
            "severity": t.severity, "text": t.text,
        })).collect::<Vec<_>>(),
    });

    out.emit(&payload, || {
        println!("profile {} against {}", p.name, corpus.display());
        println!(
            "  {} lines → {} records → {} templates ({:.1}x)",
            report.lines, report.records, report.templates, report.compression_ratio
        );
        if !stages.is_empty() {
            println!(
                "  stages: {}",
                stages
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → ")
            );
        }
        println!("  crash records: {}", report.crash_records);
        println!();
        for t in &templates {
            println!(
                "  {:>6}  {:<9}  {}",
                t.total_count,
                format!("{:?}", t.severity).to_lowercase(),
                ellipsize(&t.text, 96)
            );
        }
        Ok(())
    })
}
