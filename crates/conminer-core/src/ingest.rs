//! Post-hoc ingestion (§4).
//!
//! `ingest_file` accepts 10 KB to 800 MB — LAVA job logs, field captures, dmesg
//! dumps — as a streaming single pass, and creates a session tagged
//! `source=file` that is queryable through the exact same tools as live data.
//!
//! Throughput target is ≥ 100 MB/s on lab-host hardware, so the worst case in
//! the requirement (800 MB) is roughly 8 seconds. That is achieved by reading in
//! large blocks, batching every store write into one transaction per block, and
//! never materialising the file in memory.

use crate::error::{ErrorCode, Result, ToolError};
use crate::pipeline::{FeedOutcome, Pipeline};
use crate::store::SessionSource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read};
use std::path::Path;

/// Read block size. Large enough that per-call overhead disappears, small enough
/// that an 800 MB ingest never holds more than this in RSS.
const BLOCK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Plain,
    Gzip,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Plain => "plain",
            Codec::Gzip => "gzip",
        }
    }
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub label: Option<String>,
    /// "auto" | "on" | "off", from `ingest.gzip`.
    pub gzip: Option<String>,
    /// Hard cap; exceeding it is `INGEST_TOO_LARGE` with a split hint.
    pub max_bytes: Option<u64>,
    pub source: SessionSource,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            label: None,
            gzip: Some("auto".into()),
            max_bytes: None,
            source: SessionSource::File,
        }
    }
}

impl IngestOptions {
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self {
            label: None,
            gzip: Some(c.ingest.gzip.clone()),
            max_bytes: Some((c.ingest.max_gb * 1024.0 * 1024.0 * 1024.0) as u64),
            source: SessionSource::File,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestReport {
    pub session_id: i64,
    pub bytes: u64,
    pub lines: usize,
    pub records: usize,
    pub templates: usize,
    pub new_templates: usize,
    pub stage_transitions: Vec<String>,
    pub boots: usize,
    pub crash_records: usize,
    pub garbage_lines: usize,
    pub codec: Codec,
    /// The container the console text arrived in, if any (§15.6, §15.7).
    pub wrapper: crate::codec::Wrapper,
    pub content_sha: String,
    /// A session that already holds byte-identical content. The ingest still
    /// happens — re-running a job is a legitimate thing to do — but the agent is
    /// told, so it does not diff a session against itself (§13 `ingest`).
    pub duplicate_of: Option<i64>,
    /// Present when the input was a `conminer export_session` archive: the
    /// metadata of the session it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported_from: Option<serde_json::Value>,
    pub duration_ms: u64,
    pub throughput_mb_s: f64,
    pub compression_ratio: f64,
}

/// Ingest a file into a pipeline that already has its store open.
pub fn ingest_file(pipe: &mut Pipeline, path: &Path, opts: &IngestOptions) -> Result<IngestReport> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ToolError::new(
            ErrorCode::NoSuchPath,
            format!("{} does not exist", path.display()),
        ),
        std::io::ErrorKind::PermissionDenied => ToolError::new(
            ErrorCode::PermissionDenied,
            format!("cannot read {}", path.display()),
        ),
        _ => ToolError::from(e),
    })?;
    if meta.is_dir() {
        return Err(ToolError::new(
            ErrorCode::NoSuchPath,
            format!("{} is a directory", path.display()),
        ));
    }
    if let Some(cap) = opts.max_bytes {
        if meta.len() > cap {
            return Err(ToolError::new(
                ErrorCode::IngestTooLarge,
                format!(
                    "{} is {} bytes, over the {cap}-byte cap",
                    path.display(),
                    meta.len()
                ),
            )
            .with_hint("split the file (e.g. `split -b 1G`) or raise ingest.max_gb")
            .with_detail(serde_json::json!({
                "size": meta.len(),
                "max_bytes": cap,
                "suggested_parts": meta.len().div_ceil(cap.max(1)),
            })));
        }
    }

    let file = std::fs::File::open(path)?;
    let codec = detect_codec(path, opts)?;
    let reader: Box<dyn Read> = match codec {
        Codec::Plain => Box::new(BufReader::with_capacity(BLOCK, file)),
        Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(
            BLOCK, file,
        ))),
    };

    ingest_reader(
        pipe,
        reader,
        codec,
        opts,
        Some(path.to_string_lossy().into_owned()),
    )
}

/// Ingest from any reader — used by `ingest_file`, by the uploaded-artifact path,
/// and by the replay harness.
pub fn ingest_reader(
    pipe: &mut Pipeline,
    mut reader: impl Read,
    codec: Codec,
    opts: &IngestOptions,
    source_path: Option<String>,
) -> Result<IngestReport> {
    let started = std::time::Instant::now();

    // Content hash first pass is impossible on a stream, so hash as we go and
    // record it on the session at the end. Duplicate detection therefore reports
    // *after* the ingest, which is the honest ordering: we cannot know a file is
    // a duplicate until we have read it.
    let mut hasher = Sha256::new();
    let session_id = pipe.begin_session(
        opts.source,
        opts.label.as_deref(),
        None,
        source_path.as_deref(),
    )?;

    let mut totals = FeedOutcome::default();
    let mut buf = vec![0u8; BLOCK];
    let mut cap_used: u64 = 0;
    let mut header_checked = false;
    let mut exported_from: Option<serde_json::Value> = None;
    // A container is decided once, from the head of the stream, and then applied
    // uniformly — sniffing per block could change its mind halfway through.
    let mut wrapper: Option<crate::codec::Wrapper> = None;

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| ToolError::new(ErrorCode::IngestFailed, format!("read failed: {e}")))?;
        if n == 0 {
            break;
        }
        // A UTF-8 BOM is stored verbatim like every other byte (§6); it is
        // excluded only from the *mining key*, in `Profile::mine_key`.
        let mut block = &buf[..n];

        // A `conminer export_session` archive carries one metadata line ahead of
        // the verbatim capture. Strip and record it so an imported session says
        // where it came from; everything after it round-trips byte for byte.
        if !header_checked {
            header_checked = true;
            if block.starts_with(crate::store::device::EXPORT_MAGIC.as_bytes()) {
                if let Some(nl) = block.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&block[..nl]);
                    let json = line.split_once(' ').map(|(_, j)| j).unwrap_or("{}");
                    exported_from = serde_json::from_str(json).ok();
                    block = &buf[nl + 1..n];
                }
            }
        }

        hasher.update(block);
        cap_used += n as u64;
        let wrapper = *wrapper.get_or_insert_with(|| crate::codec::detect(block));
        if let Some(cap) = opts.max_bytes {
            if cap_used > cap {
                return Err(ToolError::new(
                    ErrorCode::IngestTooLarge,
                    format!("stream exceeded the {cap}-byte cap after decompression"),
                )
                .with_hint("split the input, or raise ingest.max_gb"));
            }
        }
        // Unwrapping happens before the pipeline, so live and post-hoc paths
        // stay a single implementation.
        let unwrapped;
        let payload: &[u8] = if wrapper == crate::codec::Wrapper::None {
            block
        } else {
            unwrapped = crate::codec::unwrap_all(block, wrapper)?;
            &unwrapped
        };
        let out = pipe.feed(payload)?;
        merge(&mut totals, out);
    }

    let out = pipe.finish()?;
    merge(&mut totals, out);

    let content_sha = hex::encode(hasher.finalize());
    let duplicate_of = pipe
        .store()
        .session_with_sha(&content_sha)?
        .map(|s| s.id)
        .filter(|id| *id != session_id);
    pipe.store_mut()
        .set_session_content_sha(session_id, &content_sha)?;

    let stats = pipe.store().stats(Some(session_id))?;
    let elapsed = started.elapsed();
    let secs = elapsed.as_secs_f64().max(1e-9);

    Ok(IngestReport {
        session_id,
        bytes: totals.bytes,
        lines: totals.lines,
        records: totals.records,
        templates: stats.templates as usize,
        new_templates: totals.new_templates.len(),
        stage_transitions: totals.stage_transitions,
        boots: totals.boots_opened.len() + 1,
        crash_records: totals.crash_records.len(),
        garbage_lines: totals.garbage_lines,
        codec,
        wrapper: wrapper.unwrap_or(crate::codec::Wrapper::None),
        content_sha,
        duplicate_of,
        exported_from,
        duration_ms: elapsed.as_millis() as u64,
        throughput_mb_s: (totals.bytes as f64 / (1024.0 * 1024.0)) / secs,
        compression_ratio: stats.compression_ratio,
    })
}

fn merge(a: &mut FeedOutcome, b: FeedOutcome) {
    a.bytes += b.bytes;
    a.lines += b.lines;
    a.records += b.records;
    a.new_templates.extend(b.new_templates);
    a.stage_transitions.extend(b.stage_transitions);
    a.boots_opened.extend(b.boots_opened);
    a.crash_records.extend(b.crash_records);
    a.garbage_lines += b.garbage_lines;
}

fn detect_codec(path: &Path, opts: &IngestOptions) -> Result<Codec> {
    match opts.gzip.as_deref().unwrap_or("auto") {
        "on" => return Ok(Codec::Gzip),
        "off" => return Ok(Codec::Plain),
        "auto" => {}
        other => {
            return Err(ToolError::invalid_arg(format!(
                "ingest.gzip must be auto|on|off, got {other:?}"
            )))
        }
    }
    let mut magic = [0u8; 2];
    let mut f = std::fs::File::open(path)?;
    let n = f.read(&mut magic)?;
    Ok(if n == 2 && magic == [0x1f, 0x8b] {
        Codec::Gzip
    } else {
        Codec::Plain
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_detection_reads_the_magic_not_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("boot.log.gz"); // lying extension
        std::fs::write(&plain, b"not actually compressed\n").unwrap();
        let opts = IngestOptions {
            gzip: Some("auto".into()),
            ..Default::default()
        };
        assert_eq!(detect_codec(&plain, &opts).unwrap(), Codec::Plain);

        let gz = dir.path().join("boot.log"); // lying the other way
        std::fs::write(&gz, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        assert_eq!(detect_codec(&gz, &opts).unwrap(), Codec::Gzip);
    }

    #[test]
    fn codec_override_wins_over_detection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        let off = IngestOptions {
            gzip: Some("off".into()),
            ..Default::default()
        };
        assert_eq!(detect_codec(&p, &off).unwrap(), Codec::Plain);
    }

    #[test]
    fn invalid_gzip_setting_is_a_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, b"x").unwrap();
        let bad = IngestOptions {
            gzip: Some("perhaps".into()),
            ..Default::default()
        };
        assert_eq!(
            detect_codec(&p, &bad).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
