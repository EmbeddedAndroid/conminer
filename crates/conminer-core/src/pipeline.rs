//! The pipeline: `bytes → lines → FRAMER → DRAIN → SQLite`.
//!
//! Live capture and post-hoc file ingestion feed the **identical** pipeline
//! (§3), which is what makes `ingest_file` results queryable through exactly the
//! same tools as a live console — and what keeps the two paths from drifting
//! into two different sets of bugs.
//!
//! Ordering inside `feed` matters and is deliberate:
//!
//! 1. split bytes into lines
//! 2. **persist the raw lines first** — capture never waits on interpretation,
//!    so a framer bug or a slow miner can never cost bytes
//! 3. frame the persisted lines into records
//! 4. mine each record's key line
//! 5. write records, templates, occurrences, stages and epochs

use crate::clock::SharedClock;
use crate::config::Config;
use crate::drain::{Drain, DrainConfig};
use crate::error::Result;
use crate::framer::generic::GarbageDetector;
use crate::framer::{Framer, FramerEvent, FramerInput, ProfileFramer, ProfileSet};
use crate::linesplit::LineSplitter;
use crate::store::{DeviceStore, PendingLine, PendingRecord, RecordKind, SessionSource};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What one `feed` call produced. Deliberately small: counts and ids, never the
/// lines themselves, so a caller that ingests 800 MB does not accumulate it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedOutcome {
    pub bytes: u64,
    pub lines: usize,
    pub records: usize,
    /// Templates seen for the very first time — the "what's new?" signal, and
    /// what `follow(until={template:new})` fires on.
    pub new_templates: Vec<i64>,
    pub stage_transitions: Vec<String>,
    /// Epoch ids opened by a detected reset during this feed.
    pub boots_opened: Vec<i64>,
    pub crash_records: Vec<i64>,
    pub garbage_lines: usize,
}

impl FeedOutcome {
    fn merge(&mut self, other: FeedOutcome) {
        self.bytes += other.bytes;
        self.lines += other.lines;
        self.records += other.records;
        self.new_templates.extend(other.new_templates);
        self.stage_transitions.extend(other.stage_transitions);
        self.boots_opened.extend(other.boots_opened);
        self.crash_records.extend(other.crash_records);
        self.garbage_lines += other.garbage_lines;
    }
}

/// One device's mining pipeline.
#[derive(Debug)]
pub struct Pipeline {
    store: DeviceStore,
    profiles: Arc<ProfileSet>,
    clock: SharedClock,
    splitter: LineSplitter,
    /// Last pending tail written to the store, so an idle console does not
    /// rewrite the same prompt several times a second.
    published_tail: String,
    /// The partial as it looked on the PREVIOUS tick. A partial that survives a
    /// tick unchanged is a console that has stopped; one that changes every tick
    /// is a console mid-output, and writing that down helps nobody.
    seen_tail: String,
    /// Every profile's version banners, flattened (§F2).
    ///
    /// Flattened rather than looked up per record, because a component is
    /// identified by ITS OWN banner and not by whichever profile happened to
    /// frame the line: the UEFI version string arrives inside a record the
    /// framer attributed to another stage, and a per-profile lookup silently
    /// missed it.
    version_banners: Vec<crate::framer::profile::VersionBanner>,
    /// One DFA pass to decide whether a line is worth extracting from at all.
    /// Almost no line is, and this runs on every line of every boot.
    version_set: regex::RegexSet,
    /// Whether this process has consumed any console bytes yet. A restart must
    /// not be mistaken for a console that fell silent.
    saw_bytes: bool,
    /// Wall time of the last byte this process actually received (§L7). The
    /// published partial is stamped with THIS, never with "now": the difference
    /// is what made a board silent for six minutes read as `streaming`.
    last_byte_at: i64,
    framer: Box<dyn Framer>,
    drain: Drain,
    garbage: GarbageDetector,
    session_id: i64,
    boot_id: Option<i64>,
    /// The most recent epoch this device has, open or not.
    ///
    /// §G5. Version banners are attributed to it when no epoch is open, so a
    /// board that identifies itself after a reset conminer did not trigger is
    /// still recorded instead of having its whole chain dropped.
    last_boot: Option<i64>,
    stage_id: Option<i64>,
    /// True while an exclusive binary claim is held (§15.2): bytes still land,
    /// interpretation is suspended.
    binary: bool,
    last_rx_ms: i64,
    /// §14.3 backpressure. A 4 Mbaud tracing console can outrun SQLite. When the
    /// queue of unmined lines exceeds this, raw bytes keep landing but mining
    /// degrades to sampling — and the counter below makes that visible instead
    /// of letting it look like a quiet console.
    sample_above: usize,
    sampled_out: u64,
    degraded: bool,
    /// Held for the pipeline's lifetime: one writer per device (§3).
    _lock: Option<crate::store::DeviceLock>,
}

impl Pipeline {
    /// Open a pipeline on an existing store, rehydrating the miner so template
    /// ids stay stable across restarts.
    pub fn new(
        mut store: DeviceStore,
        profiles: Arc<ProfileSet>,
        cfg: Config,
        selector: &str,
        pinned_profile: Option<&str>,
        clock: SharedClock,
    ) -> Result<Self> {
        // Take the writer lock *before* reading the miner state, so a second
        // pipeline cannot rehydrate a stale template set and then collide on ids.
        let lock = store.lock_for_writing()?;
        let capture = cfg.capture_for(selector);
        let drain_cfg = DrainConfig::from(&cfg.mine);
        let drain = store.load_drain(drain_cfg, Default::default())?;
        let framer = Box::new(ProfileFramer::new(
            profiles.clone(),
            &cfg.framer,
            pinned_profile,
        )?);
        Ok(Self {
            splitter: LineSplitter::with_config(&capture),
            published_tail: String::new(),
            seen_tail: String::new(),
            version_set: regex::RegexSet::new(
                profiles
                    .all()
                    .iter()
                    .flat_map(|p| p.version_banners.iter())
                    .map(|v| v.re.as_str()),
            )
            .unwrap_or_else(|_| regex::RegexSet::empty()),
            version_banners: profiles
                .all()
                .iter()
                .flat_map(|p| p.version_banners.iter().cloned())
                .collect(),
            saw_bytes: false,
            last_byte_at: 0,
            garbage: GarbageDetector::from_config(&cfg.framer),
            store,
            profiles,
            clock,
            framer,
            drain,
            session_id: 0,
            boot_id: None,
            last_boot: None,
            stage_id: None,
            binary: false,
            last_rx_ms: 0,
            // Ten commit windows' worth of lines: comfortably above any healthy
            // console, low enough to bound memory on a firehose.
            sample_above: 50_000,
            sampled_out: 0,
            degraded: false,
            _lock: lock,
        })
    }

    pub fn store(&self) -> &DeviceStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut DeviceStore {
        &mut self.store
    }

    pub fn into_store(self) -> DeviceStore {
        self.store
    }

    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    pub fn boot_id(&self) -> Option<i64> {
        self.boot_id
    }

    pub fn stage(&self) -> Option<&str> {
        self.framer.stage()
    }

    pub fn profile(&self) -> &str {
        self.framer.name()
    }

    pub fn last_rx_ms(&self) -> i64 {
        self.last_rx_ms
    }

    /// Lines whose *mining* was skipped under backpressure. Raw bytes are always
    /// stored; this counter exists so degradation is reported, never silent.
    pub fn sampled_out(&self) -> u64 {
        self.sampled_out
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Override the backpressure threshold (tests, and very fast consoles).
    pub fn set_sample_threshold(&mut self, lines: usize) {
        self.sample_above = lines;
    }

    pub fn drain(&self) -> &Drain {
        &self.drain
    }

    pub fn profiles(&self) -> &Arc<ProfileSet> {
        &self.profiles
    }

    /// Suspend/resume line interpretation without ever suspending capture.
    pub fn set_binary(&mut self, on: bool) {
        self.binary = on;
    }

    pub fn begin_session(
        &mut self,
        source: SessionSource,
        label: Option<&str>,
        content_sha: Option<&str>,
        source_path: Option<&str>,
    ) -> Result<i64> {
        let now = self.clock.now_wall_ms();
        self.session_id = self
            .store
            .begin_session(source, now, label, content_sha, source_path)?;
        let opened_by = match source {
            SessionSource::Live => "session",
            _ => "ingest",
        };
        let boot = self
            .store
            .open_boot(opened_by, label, now, Some(self.session_id))?;
        self.boot_id = Some(boot.id);
        self.last_boot = Some(boot.id);
        self.stage_id = None;
        Ok(self.session_id)
    }

    /// Explicitly open a new epoch — the `mark(device)` idiom of §8.4, called
    /// *before* flipping power so the answer to "did it boot?" cannot come from
    /// the previous boot's output.
    pub fn open_boot(&mut self, opened_by: &str, label: Option<&str>) -> Result<i64> {
        self.finalize_boot()?;
        let now = self.clock.now_wall_ms();
        let boot = self
            .store
            .open_boot(opened_by, label, now, Some(self.session_id))?;
        self.boot_id = Some(boot.id);
        self.last_boot = Some(boot.id);
        self.stage_id = None;
        Ok(boot.id)
    }

    /// Feed raw bytes. Returns what they produced, never the bytes back.
    /// Adopt an epoch opened by another process.
    ///
    /// `power`, `flash` and `mark` run in mcpd, which opens the epoch in the
    /// database; minerd holds the writer lock and tracks the current boot in
    /// memory. Without this the two disagree permanently: every epoch an agent
    /// opens stays empty at 0 bytes while the console keeps filling the epoch
    /// minerd started with, so per-boot fingerprints, `boot_report` and
    /// absence learning are all reading a boot that never happened.
    ///
    /// Called before each read so a power cycle's output lands in the epoch the
    /// power command opened.
    pub fn adopt_external_boot(&mut self) -> Result<bool> {
        let Some(latest) = self.store.latest_boot()? else {
            return Ok(false);
        };
        if self.boot_id == Some(latest.id) {
            return Ok(false);
        }
        // Only ever forward: an epoch id lower than the current one means a
        // stale read, and moving backwards would re-attribute live output to a
        // boot that has already been reported on.
        if self.boot_id.is_some_and(|cur| latest.id <= cur) {
            return Ok(false);
        }
        // Close and fingerprint the epoch being left behind.
        //
        // A fingerprint is only computed when an epoch closes, and an epoch
        // opened by another process never closed the previous one — so every
        // epoch that actually captured bytes stayed open and unfingerprinted,
        // while the empty ones carried fingerprints. That inverts the whole
        // point: `boot_report` classified real boots as having no signature and
        // compared empty epochs against each other.
        if self.boot_id.is_some() {
            self.finalize_boot()?;
        }
        self.boot_id = Some(latest.id);
        self.last_boot = Some(latest.id);
        self.stage_id = None;
        Ok(true)
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<FeedOutcome> {
        if !chunk.is_empty() {
            self.saw_bytes = true;
            // WHEN THE BYTES ARRIVED, which is not when we get around to
            // publishing them (§L7).
            self.last_byte_at = self.clock.now_wall_ms();
        }
        let lines = self.splitter.push(chunk);
        self.last_rx_ms = self.clock.now_wall_ms();
        // NOT published here. A busy console calls feed many times a second and
        // each publish is its own transaction; the partial is only interesting
        // when the console STOPS, so the 250 ms tick is both sufficient and
        // bounded. Capture throughput is not worth a prompt arriving 250 ms
        // sooner.
        self.consume(lines)
    }

    /// Make the unterminated line visible to other processes.
    ///
    /// mcpd derives `console_state` from the store, in its own process, so an
    /// idle prompt is invisible to it unless the capture loop writes it down.
    ///
    /// WRITTEN ONLY WHEN THE PARTIAL HAS STOPPED CHANGING, which is the whole
    /// point: a prompt is a partial line that STAYS. A console mid-firehose has
    /// a partial too, but it is different on every tick and nobody is waiting to
    /// classify it -- publishing it would spend a transaction per tick per
    /// device on precisely the devices that can least afford one. Requiring the
    /// text to survive one tick unchanged costs an idle console a single 250 ms
    /// delay and costs a busy console nothing at all.
    fn publish_pending_tail(&mut self) -> Result<()> {
        let text = self.splitter.pending_text();
        let stable = text == self.seen_tail;
        self.seen_tail = text.clone();

        // Clearing is not subject to the stability rule. The moment the partial
        // becomes a real line, a published copy is stale, and a stale prompt is
        // worse than no prompt: it would have console_state reporting one at a
        // console that has moved on.
        //
        // ...but a FRESH PROCESS HAS AN EMPTY BUFFER, which is not the same fact.
        // Measured on the IQ10: redeploying the stack put a board that was
        // sitting at its root prompt back to `unstable, commandable: false`,
        // because the new minerd cleared the stored prompt on its first tick
        // while the board -- silent, unchanged, still at that prompt -- had no
        // reason to say anything again. Restarting conminer must not blind it.
        // Until this process has actually seen a byte, the previous process's
        // observation is the best evidence there is, and `console::derive`
        // decides whether it is still trustworthy.
        if text.is_empty() {
            if !self.published_tail.is_empty() && self.saw_bytes {
                self.store.set_pending_tail("", 0)?;
                self.published_tail.clear();
            }
            return Ok(());
        }
        if !stable || text == self.published_tail {
            return Ok(());
        }
        // §L7. STAMP IT WITH WHEN THE BOARD SPOKE, NOT WITH NOW.
        //
        // `console_state` reads this timestamp as "the console last produced a
        // byte" -- a partial line is output like any other. Stamping it with the
        // publish time made every REPUBLISH look like fresh console activity,
        // and a republish happens whenever this process is new (a restart, a
        // reconnect) while the buffer is not. Measured on the ADP after a
        // verified power-off: last real line 378 s old, pending-tail timestamp
        // 70 s old and advancing, so a dark board reported `streaming` with the
        // explanation "the board is still producing output".
        //
        // Until this process has received a byte of its own it has no timestamp
        // to offer, so it leaves the previous process's observation -- text and
        // time together -- exactly as it found it.
        if self.last_byte_at == 0 {
            return Ok(());
        }
        self.store.set_pending_tail(&text, self.last_byte_at)?;
        self.published_tail = text;
        Ok(())
    }

    /// End of stream: flush the splitter and close any open record.
    pub fn finish(&mut self) -> Result<FeedOutcome> {
        let lines = self.splitter.flush();
        // The partial just became a real line; nothing is pending any more.
        self.store.set_pending_tail("", 0)?;
        self.published_tail.clear();
        self.seen_tail.clear();
        let mut out = self.consume(lines)?;
        let events = self.framer.flush();
        out.merge(self.apply(events)?);
        self.finalize_boot()?;
        let now = self.clock.now_wall_ms();
        if self.session_id != 0 {
            self.store.end_session(self.session_id, now)?;
        }
        Ok(out)
    }

    /// Wall-clock tick: closes records that have gone silent (DEAD_AIR).
    pub fn tick(&mut self) -> Result<FeedOutcome> {
        let now = self.clock.now_wall_ms();
        let events = self.framer.tick(now);
        let out = self.apply(events)?;
        self.publish_pending_tail()?;
        Ok(out)
    }

    fn consume(&mut self, lines: Vec<crate::linesplit::Line>) -> Result<FeedOutcome> {
        if lines.is_empty() {
            return Ok(FeedOutcome::default());
        }
        let mut out = FeedOutcome::default();

        let ts_wall = self.clock.now_wall_ms();
        let ts_mono = self.clock.now_mono_ns();
        let pending: Vec<PendingLine<'_>> = lines
            .iter()
            .map(|l| PendingLine {
                bytes: &l.bytes,
                terminator: l.terminator,
                truncated: l.truncated,
                continuation: l.continuation,
                ts_mono,
                ts_wall,
                stage_id: self.stage_id,
            })
            .collect();

        // 1-2. persist raw first — capture never waits on interpretation.
        let mut batch = self.store.begin_batch()?;
        let refs = batch.append_lines(self.session_id, self.boot_id, &pending)?;
        out.lines = refs.len();
        out.bytes = pending.iter().map(|p| p.consumed()).sum();

        // §14.3: a block far larger than any healthy console means the source is
        // outrunning us. Capture continues regardless; interpretation is what
        // degrades, loudly.
        let over = refs.len() > self.sample_above;
        if over && !self.degraded {
            tracing::warn!(
                lines = refs.len(),
                threshold = self.sample_above,
                "backpressure: mining degraded to sampling; raw capture continues"
            );
        }
        self.degraded = over;

        // 3. frame
        let mut events = Vec::new();
        for (i, (l, r)) in lines.iter().zip(&refs).enumerate() {
            let garbage = self.garbage.push(&l.bytes);
            if garbage {
                out.garbage_lines += 1;
            }
            if over && i % 16 != 0 {
                // Sampled out: the bytes are already durable, only the mining of
                // this line is skipped, and the counter says how often.
                self.sampled_out += 1;
                continue;
            }
            events.extend(self.framer.push(FramerInput {
                line_id: r.id,
                text: String::from_utf8_lossy(&l.bytes).into_owned(),
                raw_len: l.bytes.len(),
                ts_wall,
                ts_mono,
                garbage,
                binary: self.binary,
            }));
        }

        // 4-5. mine and persist, in the same transaction as the raw lines: a
        // block of input is one commit, so a crash can never leave records
        // referring to lines that were never durable.
        out.merge(Self::apply_in(
            &mut batch,
            &mut self.drain,
            &self.profiles,
            &self.version_set,
            &self.version_banners,
            self.session_id,
            &mut self.boot_id,
            &mut self.last_boot,
            &mut self.stage_id,
            &self.clock,
            events,
        )?);
        let offset = batch.commit()?;
        self.store.finish_batch(offset);
        Ok(out)
    }

    /// Mine and persist framer output outside a caller-owned batch (tick/flush).
    fn apply(&mut self, events: Vec<FramerEvent>) -> Result<FeedOutcome> {
        if events.is_empty() {
            return Ok(FeedOutcome::default());
        }
        let mut batch = self.store.begin_batch()?;
        let out = Self::apply_in(
            &mut batch,
            &mut self.drain,
            &self.profiles,
            &self.version_set,
            &self.version_banners,
            self.session_id,
            &mut self.boot_id,
            &mut self.last_boot,
            &mut self.stage_id,
            &self.clock,
            events,
        )?;
        let offset = batch.commit()?;
        self.store.finish_batch(offset);
        Ok(out)
    }

    /// 4-5. mine and persist framer output into an open batch.
    ///
    /// Takes its state as separate borrows rather than `&mut self` because the
    /// batch already holds the store: the fields it touches are disjoint from it.
    #[allow(clippy::too_many_arguments)]
    fn apply_in(
        batch: &mut crate::store::device::Batch<'_>,
        drain: &mut Drain,
        profiles: &Arc<ProfileSet>,
        version_set: &regex::RegexSet,
        version_banners: &[crate::framer::profile::VersionBanner],
        session_id: i64,
        boot_id: &mut Option<i64>,
        last_boot: &mut Option<i64>,
        stage_id: &mut Option<i64>,
        clock: &SharedClock,
        events: Vec<FramerEvent>,
    ) -> Result<FeedOutcome> {
        let mut out = FeedOutcome::default();
        for ev in events {
            match ev {
                FramerEvent::Stage(t) => {
                    if t.is_reset {
                        // A detected reset opens its own epoch, so an
                        // out-of-band power flip still gets one — just without
                        // an agent-chosen label (§8.4).
                        if let Some(prev) = *boot_id {
                            let fp = fingerprint_of(batch, prev)?;
                            batch.set_boot_summary(prev, Some(&fp), None)?;
                        }
                        let previous = *boot_id;
                        // The epoch starts at the banner that announced the
                        // reset, not at the end of the block it arrived in —
                        // otherwise two resets in one block share an offset and
                        // stop being distinct positions.
                        let at_offset = batch.offset_of_line(t.banner_line_id).ok();
                        let id = batch.open_boot_after(
                            "reset",
                            None,
                            t.at,
                            Some(session_id),
                            previous,
                            at_offset,
                            // The pipeline reassigns its own tail below, so it
                            // must not also be claimed here: doing both moved
                            // the same lines twice and emptied the epoch they
                            // came from.
                            false,
                        )?;
                        // Lines from the reset banner onward belong to the new
                        // epoch, not the one they were written under.
                        batch.reassign_tail_to_boot(session_id, t.banner_line_id, previous, id)?;
                        *boot_id = Some(id);
                        *last_boot = Some(id);
                        *stage_id = None;
                        out.boots_opened.push(id);
                    }
                    if t.stage_changed || t.is_reset {
                        let id = batch.append_stage_after(
                            session_id,
                            *boot_id,
                            &t.name,
                            &t.profile,
                            t.at,
                            Some(t.banner_line_id),
                            *stage_id,
                        )?;
                        *stage_id = Some(id);
                        out.stage_transitions.push(t.name.clone());
                    }
                }
                FramerEvent::Record(rec) => {
                    let rec_ts = clock.now_wall_ms();

                    // §F2. WHAT IS RUNNING, lifted as it goes past.
                    //
                    // Every version banner this rig cares about -- BL31's
                    // fingerprint, OP-TEE's commit, the UEFI string, the kernel
                    // and its #build -- was already flowing through here and
                    // being templated like any other line, while `provenance`
                    // reported `running: {}` for four rounds. Extraction belongs
                    // at mining time: the text is in hand, and a query-time log
                    // scan for it would be exactly the thing conminer exists to
                    // avoid.
                    // §G5. NO OPEN EPOCH IS NOT A REASON TO DROP THE ANSWER.
                    //
                    // This used to require an open epoch, so a board that
                    // printed its identity while conminer had no epoch open --
                    // a hand power-cycle, a watchdog reset, anything conminer
                    // did not itself trigger -- had its whole chain thrown away
                    // as the line went past. Measured on the ADP: `UEFI Ver :
                    // 6.0.260212...KODIAKLA-1` and `QC_IMAGE_VERSION_STRING=...`
                    // sat in the store as ordinary text while `provenance`
                    // reported `running: null`.
                    //
                    // The most recent epoch is the honest home for it: these
                    // banners describe the firmware running NOW, and that is the
                    // epoch the console is in even if conminer did not open it.
                    let attach = boot_id.or(*last_boot);
                    if let Some(boot) = attach {
                        for line in rec.text.lines() {
                            for idx in version_set.matches(line).into_iter() {
                                let Some(vb) = version_banners.get(idx) else {
                                    continue;
                                };
                                if let Some((version, detail)) = vb.extract(line) {
                                    batch.note_version(
                                        boot,
                                        &vb.component,
                                        &version,
                                        &detail,
                                        Some(rec.first_line_id),
                                        rec_ts,
                                    )?;
                                }
                            }
                        }
                    }
                    let mut template_id = None;
                    if let Some(key) = rec.mine_key.as_deref() {
                        let rules = profiles
                            .get(&rec.profile)
                            .map(|p| p.tokenizer.clone())
                            .unwrap_or_default();
                        let tokens = rules.tokenize(key);
                        if !tokens.is_empty() {
                            let m = drain.add_tokens(&tokens);
                            let t = drain.template(m.template_id).expect("just mined").clone();
                            batch.note_template(
                                &t,
                                m.created,
                                m.generalized,
                                session_id,
                                *boot_id,
                                rec_ts,
                                rec.stage.as_deref(),
                                &rec.profile,
                                rec.severity,
                            )?;
                            if m.created {
                                out.new_templates.push(t.id as i64);
                            }
                            template_id = Some(t.id as i64);
                        }
                    }

                    let id = batch.append_record(&PendingRecord {
                        session_id,
                        boot_id: *boot_id,
                        first_line_id: rec.first_line_id,
                        last_line_id: rec.last_line_id,
                        line_count: rec.line_count,
                        stage_id: *stage_id,
                        profile: rec.profile.clone(),
                        severity: rec.severity,
                        kind: rec.kind,
                        template_id,
                        truncated: rec.truncated,
                        fields: rec.fields.clone(),
                        text: rec.text.clone(),
                    })?;
                    out.records += 1;
                    if rec.kind == RecordKind::Crash {
                        out.crash_records.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Stamp the closing epoch with its semantic fingerprint (§8.4): a hash of
    /// the ordered template-id sequence and stage timeline. Raw bytes of two
    /// loop iterations never match (timestamps differ every pass) but the
    /// template sequence does, which is what makes "same crash again" a hash
    /// comparison instead of a log read.
    /// Close the current epoch and leave NONE open.
    ///
    /// The state a board is in after a reset conminer did not trigger: bytes
    /// keep arriving with no epoch to file them under. Exposed so §G5's
    /// "identify yourself with nothing open" case is reachable without a rig.
    pub fn close_epoch(&mut self) -> Result<()> {
        self.finalize_boot()?;
        self.boot_id = None;
        Ok(())
    }

    fn finalize_boot(&mut self) -> Result<()> {
        let Some(id) = self.boot_id else {
            return Ok(());
        };
        let fp = self.fingerprint(id)?;
        self.store.set_boot_summary(id, Some(&fp), None)?;
        Ok(())
    }

    pub fn fingerprint(&self, boot_id: i64) -> Result<String> {
        let (templates, stages) = self.store.boot_signature(boot_id)?;
        Ok(fingerprint_from(&templates, &stages))
    }
}

fn fingerprint_of(batch: &crate::store::device::Batch<'_>, boot_id: i64) -> Result<String> {
    let (templates, stages) = batch.boot_signature(boot_id)?;
    Ok(fingerprint_from(&templates, &stages))
}

/// Hash of the ordered template-id sequence and stage timeline (§8.4).
///
/// Deliberately semantic, not a raw hash: the raw bytes of two loop iterations
/// never match because timestamps differ every pass, but the template sequence —
/// with variable fields already wildcarded — matches exactly when the boot
/// behaved identically. "Same crash again" is then a hash comparison, not a log
/// read.
fn fingerprint_from(templates: &[i64], stages: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for t in templates {
        h.update(t.to_le_bytes());
    }
    h.update(b"\x00stages\x00");
    for s in stages {
        h.update(s.as_bytes());
        h.update([0]);
    }
    hex::encode(&h.finalize()[..16])
}
