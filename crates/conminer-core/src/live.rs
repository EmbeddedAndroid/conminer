//! Live capture (§3, §4): one task per device, reading its ser2net endpoint.
//!
//! Two commitments shape this:
//!
//! * **Capture never stops.** Bytes are persisted before they are interpreted,
//!   the connection reconnects with backoff, and a framer or miner problem can
//!   never cost a byte. The one thing that *does* stop capture is a full disk,
//!   and that fails loudly (§16 `store`).
//! * **Capture health is attested, not assumed.** §8.4 needs "zero bytes arrived
//!   while the port was demonstrably open" to be distinguishable from "I do not
//!   know". So the task publishes a `capture_state` the read side can trust, and
//!   never reports `listening` unless it really is.

use crate::config::Config;
use crate::error::Result;
use crate::framer::ProfileSet;
use crate::pipeline::Pipeline;
use crate::store::{DeviceRow, DeviceStore, Registry, SessionSource};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// What the read side is allowed to believe about a device (§8.4, §8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// The port is not open: discovery lost the device, or ser2net is down.
    /// This is "I do not know", and must never be reported as "no output".
    NotListening,
    /// Port open, read loop alive, nothing arriving.
    Listening,
    /// Bytes arriving.
    Streaming,
    /// Bytes arriving but failing the GARBAGE_BURST threshold — the classic
    /// fresh-power-on baud mismatch, named rather than shown as silence.
    Garbage,
    /// ser2net accepted the connection but could NOT open the serial device,
    /// and is serving its own failure text in place of the board. Distinct from
    /// Listening on purpose: the console is wedged, not quiet, and ser2net never
    /// retries a failed open on its own.
    OpenFailed,
    /// The serial device is GONE BECAUSE THE BOARD IS IN EDL.
    ///
    /// Entering EDL re-enumerates the board's USB: the UART interface
    /// disappears and a 05c6:9008 QDL gadget takes its place, so ser2net cannot
    /// open a tty that no longer exists and serves its failure text. That looks
    /// exactly like a wedged console and is nothing like one -- it is the state
    /// an operator deliberately put the board in, it needs no restart, and the
    /// port comes back on its own when the board leaves EDL. Reported from a
    /// live flashing session, where conminer called a working EDL entry a
    /// capture fault.
    AwayInEdl,
}

impl CaptureState {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureState::NotListening => "not_listening",
            CaptureState::Listening => "listening",
            CaptureState::Streaming => "streaming",
            CaptureState::Garbage => "garbage",
            CaptureState::OpenFailed => "open_failed",
            CaptureState::AwayInEdl => "away_in_edl",
        }
    }
}

/// The USB ports an operator has attributed to this board, if any.
/// Run a CPU-bound, blocking step without freezing the async runtime.
///
/// MINING IS BLOCKING WORK AND IT MUST NOT SIT ON THE REACTOR.
///
/// `pipeline.feed` frames, mines and stores; its cost grows with the device's
/// template set. Measured on the bench: a board whose store had reached 176,315
/// templates took **1954ms to feed a single 15-byte line**, and because that
/// call ran directly inside the capture task it blocked the tokio worker
/// holding the IO/timer driver -- freezing the WHOLE runtime. An independent
/// heartbeat task, which does nothing but sleep 250ms in a loop, stalled 2002ms
/// at the same instant, and the broker task that feeds the web console could
/// not be polled, so an operator saw the console run two seconds behind a board
/// that had answered in 20ms.
///
/// `block_in_place` hands the driver to another worker for the duration, so
/// slow mining costs mining time and nothing else. Capture publishes to the
/// broker BEFORE this runs, so the console is never behind the board regardless
/// of how slow the store gets.
///
/// Falls back to a direct call on a single-threaded runtime, where
/// `block_in_place` would panic -- `#[tokio::test]` uses one by default.
fn off_the_reactor<T>(f: impl FnOnce() -> T) -> T {
    let multi = matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    );
    if multi {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Work handed to the mining thread.
enum MinerMsg {
    /// Raw console bytes, telnet already stripped.
    Bytes(Vec<u8>),
    /// The commit tick: adopt an externally opened epoch and flush.
    Tick,
}

/// How many chunks may be waiting to be mined.
///
/// Deep enough to ride out a slow commit, shallow enough that a miner which has
/// fallen permanently behind is noticed as loss rather than as unbounded memory.
const MINER_QUEUE: usize = 4096;

/// THE READER MUST NEVER WAIT FOR THE MINER.
///
/// ser2net discards for a client that does not read, so every millisecond the
/// capture loop spends framing, mining and writing is console it will never see
/// again. Measured on the bench during one boot: ser2net delivered 709,425
/// bytes while the store recorded 23,341 -- 3.3% -- arriving in 8KB lumps about
/// 2.4s apart, which is exactly the shape of a reader that stops to do work.
/// The web console showed the same 23,341 bytes, because it is fed from the
/// same capture.
///
/// So the socket loop only reads, republishes and enqueues; all of the slow work
/// happens here, on a thread of its own.
fn mine_forever(
    mut pipeline: Pipeline,
    rx: std::sync::mpsc::Receiver<MinerMsg>,
    stats: Arc<Mutex<CaptureStats>>,
    garbage: Arc<std::sync::atomic::AtomicBool>,
) {
    let account = |out: &crate::pipeline::FeedOutcome, p: &Pipeline| {
        if out.bytes == 0 && out.records == 0 {
            return;
        }
        if let Ok(mut s) = stats.lock() {
            s.bytes += out.bytes;
            s.lines += out.lines as u64;
            s.records += out.records as u64;
            s.last_rx_ms = Some(p.last_rx_ms());
        }
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            MinerMsg::Bytes(chunk) => match pipeline.feed(&chunk) {
                Ok(out) => {
                    garbage.store(out.garbage_lines > 0, std::sync::atomic::Ordering::Relaxed);
                    account(&out, &pipeline);
                }
                Err(e) => tracing::warn!(error = %e.message, "mining failed"),
            },
            MinerMsg::Tick => {
                match pipeline.adopt_external_boot() {
                    Ok(true) => {
                        tracing::info!(boot = ?pipeline.boot_id(), "adopted externally opened epoch")
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e.message, "epoch adoption failed"),
                }
                match pipeline.tick() {
                    Ok(out) => account(&out, &pipeline),
                    Err(e) => tracing::warn!(error = %e.message, "commit tick failed"),
                }
            }
        }
    }
    let _ = pipeline.finish();
}

fn declared_usb_ports(d: &DeviceRow) -> Vec<String> {
    d.tags
        .get("usb_ports")
        .map(|t| {
            t.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Publish capture health for a device.
///
/// ITS OWN COLUMN, NOT `state`. Presence belongs to discovery (discovered /
/// gone / ignored) and capture health belongs to minerd (listening / streaming /
/// open_failed / …); they shared one column and two processes wrote it, so each
/// erased the other -- measured on the bench as `capture_state: not_listening`
/// on a console that was capturing a boot at the time.
pub fn publish_capture_state(
    reg: &mut crate::store::Registry,
    device_id: i64,
    state: CaptureState,
) -> crate::error::Result<()> {
    reg.set_capture_state(device_id, state.as_str())
}

/// Has this capture been asserting a failure long enough to go and check?
///
/// A console in `open_failed` or `away_in_edl` produces no more bytes, so
/// nothing arrives to correct it and the registry keeps publishing a fault after
/// the port recovered -- measured on the bench, where a probe read cleanly while
/// `capture_state` still said `open_failed`. Only a fresh connection can tell,
/// so those two states are given a deadline. A healthy console has none: it is
/// never dropped on a timer.
pub fn should_revalidate(
    state: CaptureState,
    failed_for: std::time::Duration,
    interval: std::time::Duration,
) -> bool {
    should_revalidate_with(state, failed_for, interval, false)
}

/// As [`should_revalidate`], plus the one piece of evidence worth more than the
/// clock: the console's device node is back.
///
/// THE TIMER IS FOR "IS THIS STILL TRUE?", NOT FOR "IT IS OBVIOUSLY UNTRUE".
///
/// A power cycle takes the tty away, and ser2net keeps ACCEPTING the connection
/// while answering "Device open failure" -- so the TCP connect succeeds and the
/// dial backoff never applies. Capture lands in `open_failed` and then waits out
/// `revalidate_failed_after_ms` (15s by default) before dialling again, while
/// the board powers on and prints its whole bootloader into a console nobody is
/// reading. Reported from the bench as "I got zero bootloader messages"; #31/#34
/// are the same window seen from the store side.
///
/// The node existing again is proof the wait buys nothing, so it re-dials on the
/// next tick instead: a quarter second, not fifteen.
pub fn should_revalidate_with(
    state: CaptureState,
    failed_for: std::time::Duration,
    interval: std::time::Duration,
    device_present: bool,
) -> bool {
    match state {
        // A node that has RETURNED is proof that waiting longer buys nothing.
        //
        // `device_present` here means "absent when this failure began, present
        // now" -- a transition, never a level. Read as a level it fires on every
        // tick for a console that is present and still un-openable, which is a
        // 250ms re-dial loop: measured at 28 re-attaches in two minutes, exactly
        // 270ms apart, and felt as a laggy web console because every re-attach
        // restarts the byte stream its viewers are reading.
        CaptureState::OpenFailed => device_present || failed_for >= interval,
        // NOT for recovery mode. `away_in_edl` does not mean "we cannot read";
        // it means "what we are reading is a flasher, not the OS" -- the bytes
        // are still recorded, only the STATE is suppressed. During a flash the
        // console tty is usually present AND streaming DevProg output, so
        // short-circuiting on presence re-dialled on every commit tick and
        // threw that output away: capture reported away_in_edl with
        // bytes_this_boot=0 while a probe on the same endpoint read 324 bytes
        // of firmware text (report #36). Only the timer belongs here, to notice
        // when the board leaves EDL.
        CaptureState::AwayInEdl => failed_for >= interval,
        _ => false,
    }
}

/// Does EDL explain why this console's serial device cannot be opened?
///
/// Entering EDL re-enumerates the board's USB: the UART interface disappears and
/// a QDL gadget takes its place. ser2net then serves a device-open failure,
/// which is indistinguishable from a wedge unless somebody looks at the bus.
///
/// SCOPED TO THIS BOARD'S OWN PORTS, never bus-wide. Another board in download
/// mode says nothing about this one, and letting it speak would mean one flash
/// silences every other console's recovery on the host. With no ports declared
/// the answer is no: the old behaviour is the safe one, and attributing the
/// board's ports is what buys the better answer.
pub fn edl_took_the_uart(ports: &[String], usb: &[crate::usb::UsbDevice]) -> bool {
    if ports.is_empty() {
        return false;
    }
    crate::usb::in_edl_on_ports(usb, ports)
}

/// Counters a device task exposes for `/metrics` and for the freshness envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureStats {
    pub bytes: u64,
    pub lines: u64,
    pub records: u64,
    pub reconnects: u64,
    pub read_errors: u64,
    /// Bytes the pipeline could not accept. Must stay zero; it exists so that
    /// loss is a *counter*, never a silence (§14.3).
    pub dropped_bytes: u64,
    pub last_rx_ms: Option<i64>,
    pub state: Option<String>,
}

/// A device capture task.
pub struct Capture {
    device: DeviceRow,
    cfg: Config,
    registry: Arc<Mutex<Registry>>,
    /// Taken by `run` and moved onto the mining thread.
    pipeline: Option<Pipeline>,
    stats: Arc<Mutex<CaptureStats>>,
    /// Sends work to the mining thread. `None` outside `run`.
    miner: Option<std::sync::mpsc::SyncSender<MinerMsg>>,
    /// Set by the miner when the last chunk framed as garbage.
    garbage: Arc<std::sync::atomic::AtomicBool>,
    state: CaptureState,
    /// Until the first publish, the registry still says `unknown` — which is a
    /// different claim from `not_listening` and must not be left standing.
    published: bool,
    /// Fan-out for every other consumer.
    ///
    /// This capture task is the SINGLE reader of the device's ser2net
    /// connection; dashd and mcpd subscribe to what it republishes instead of
    /// each opening their own. Optional so an ad-hoc capture (tests, `conminer
    /// ingest`) needs no broker at all.
    hub: Option<Arc<crate::broker::Hub>>,
    /// Strips ser2net's telnet negotiation out of the captured stream.
    ///
    /// Lives on the capture rather than in `pump` so it is rebuilt on every
    /// reconnect -- a fresh connection means a fresh negotiation, and a filter
    /// left mid-sequence from a dropped socket would swallow the first byte of
    /// the new one.
    telnet: crate::runner::TelnetFilter,
    /// The board's own hub ports and the recovery-gadget signatures to watch on
    /// them, cached once: neither changes for the life of the task.
    board_ports: Vec<String>,
    recovery_sigs: Vec<crate::usb::GadgetSig>,
    /// Have we already said this console is wedged? Released the moment it
    /// delivers real bytes again, so the error marks an outage rather than
    /// counting re-dials.
    reported_wedge: bool,
    /// Is the board in a flash/recovery mode right now? A recovery gadget on the
    /// board's ports means its normal console is gone even if a tty is open and
    /// streaming (a flasher's DevProg/progress serial), so this makes every
    /// reader agree the console is not at a prompt -- the single authoritative
    /// signal that replaces each tool discovering EDL for itself (reports
    /// #7/#22). Refreshed on a throttle, so the sysfs walk is not per-byte.
    in_recovery: bool,
    recovery_checks_left: u32,
}

impl Capture {
    pub fn open(
        device: DeviceRow,
        cfg: Config,
        profiles: Arc<ProfileSet>,
        registry: Arc<Mutex<Registry>>,
        clock: crate::clock::SharedClock,
        data_dir: &std::path::Path,
    ) -> Result<Self> {
        let store = DeviceStore::open(
            &data_dir.join(&device.db_file),
            &device.canonical,
            cfg.fts_for(device.display_name()),
        )?;
        // Takes the device writer lock for the task's lifetime: one writer per
        // device, so an ad-hoc `conminer ingest` cannot race live capture.
        let pipeline = Pipeline::new(
            store,
            profiles,
            cfg.clone(),
            device.display_name(),
            device.pinned_profile.clone().as_deref(),
            clock,
        )?;
        let board_ports = declared_usb_ports(&device);
        let recovery_sigs = cfg.capture.recovery_signatures();
        Ok(Self {
            device,
            cfg,
            registry,
            pipeline: Some(pipeline),
            stats: Arc::new(Mutex::new(CaptureStats::default())),
            miner: None,
            reported_wedge: false,
            garbage: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            state: CaptureState::NotListening,
            published: false,
            hub: None,
            telnet: crate::runner::TelnetFilter::default(),
            board_ports,
            recovery_sigs,
            in_recovery: false,
            recovery_checks_left: 0,
        })
    }

    /// Republish this device's bytes to broker subscribers.
    pub fn with_hub(mut self, hub: Arc<crate::broker::Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    pub fn stats_handle(&self) -> Arc<Mutex<CaptureStats>> {
        self.stats.clone()
    }

    pub fn device(&self) -> &DeviceRow {
        &self.device
    }

    /// Is this board in EDL right now?
    ///
    /// Scoped to the board's OWN hub ports, never bus-wide: another board in
    /// download mode on the same host says nothing about this one, and treating
    /// it as if it did is how one board's flash would silence another's
    /// recovery. The ports come from the same `usb_ports` tag the diagnostics
    /// use, so both sides agree about which gadget belongs to whom.
    /// Refresh `in_recovery` from a CHEAP sysfs presence check, throttled so the
    /// walk runs about once a second rather than on every commit tick. `force`
    /// bypasses the throttle -- used right after a reconnect so the first bytes
    /// are classified correctly.
    fn refresh_recovery(&mut self, ticks_per_check: u32, force: bool) {
        if !force && self.recovery_checks_left > 0 {
            self.recovery_checks_left -= 1;
            return;
        }
        self.recovery_checks_left = ticks_per_check;
        self.in_recovery =
            crate::usb::recovery_gadget_on_ports(&self.board_ports, &self.recovery_sigs);
    }

    fn set_state(&mut self, s: CaptureState) {
        if self.state == s && self.published {
            return;
        }
        self.state = s;
        if let Ok(mut st) = self.stats.lock() {
            st.state = Some(s.as_str().to_string());
        }
        // The registry is where the read side (a separate mcpd container) learns
        // capture health, which is what makes the §8.4 attestation cross-process.
        //
        // PUBLISHED MEANS THE REGISTRY TOOK IT. This used to set the flag first
        // and then drop the write's error on the floor, so one busy registry --
        // and four processes share it here -- left capture health frozen
        // FOREVER: the guard above saw `state == s && published` and short
        // circuited every retry after it. Measured as an intermittent
        // `capture_state: not_listening` on a console that was demonstrably
        // capturing, which is the exact ambiguity §8.4 exists to remove.
        //
        // Leaving the flag false is what makes the next tick try again.
        self.published = match self.registry.lock() {
            Ok(mut reg) => publish_capture_state(&mut reg, self.device.id, s).is_ok(),
            Err(_) => false,
        };
    }

    /// The endpoint this device's ser2net connection listens on.
    pub fn endpoint(&self) -> Option<String> {
        self.device
            .ser2net_port
            .map(|p| format!("{}:{p}", endpoint_host(&self.cfg)))
    }

    /// Run until cancelled, reconnecting with backoff.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let Some(endpoint) = self.endpoint() else {
            self.set_state(CaptureState::NotListening);
            tracing::warn!(device = %self.device.display_name(), "no ser2net port assigned");
            return Ok(());
        };

        let mut pipeline = self
            .pipeline
            .take()
            .expect("capture pipeline is taken exactly once, by run");
        pipeline.begin_session(SessionSource::Live, None, None, Some(&endpoint))?;
        // The slow half, on its own thread. The socket loop below never waits
        // for it.
        let (miner_tx, miner_rx) = std::sync::mpsc::sync_channel::<MinerMsg>(MINER_QUEUE);
        let (mstats, mgarbage) = (self.stats.clone(), self.garbage.clone());
        let miner = std::thread::Builder::new()
            .name("conminer-mine".into())
            .spawn(move || mine_forever(pipeline, miner_rx, mstats, mgarbage))
            .map_err(|e| {
                crate::error::ToolError::new(
                    crate::error::ErrorCode::Internal,
                    format!("cannot start the miner: {e}"),
                )
            })?;
        self.miner = Some(miner_tx);

        let mut backoff = Duration::from_millis(self.cfg.attach.reconnect_backoff_ms);
        // Repetition of the SAME error is the signal that this is stuck rather
        // than merely reconnecting.
        let mut same_error: u32 = 0;
        let mut last_error: Option<String> = None;
        let max_backoff = Duration::from_millis(self.cfg.attach.reconnect_backoff_max_ms);
        // WHAT THIS CEILING BUYS IS TIME-NOT-ATTACHED, and that is the number
        // an operator feels.
        //
        // ser2net keeps buffering the console while nothing is reading it, so
        // every second capture spends backed off is a second of boot that later
        // arrives in one lump: measured from the browser as 21KB delivered in a
        // single millisecond, in 8KB reads, after a six-second silence. A long
        // ceiling is therefore not "patient", it is the chunkiness. Equally, a
        // floor-pinned retry re-dials four times a second at a console ser2net
        // cannot open, which is the spin.
        //
        // SER2NET OPENS THE TTY WHEN A CLIENT CONNECTS, which is why this
        // ceiling is short. While nothing is connected the tty stays closed and
        // the kernel buffers whatever the board prints, so the delay here is
        // not politeness -- it is exactly the lump that lands in the console
        // afterwards. A 1.5s ceiling still showed 8KB frames and 2s gaps in the
        // browser during a boot.
        //
        // The reason the ceiling existed -- a floor-pinned retry making ser2net
        // log open failures and the supervisor restart it, dropping every
        // console -- is now handled where it belongs: the supervisor attributes
        // a failure to its device and does not restart for one that is absent.
        // So this can be short again.
        let recover_cap = max_backoff.min(Duration::from_millis(350));

        // How often to look for the console coming back. Cheap (one stat), and
        // the whole point is to be dialling within a tick of its return.
        let present_poll = Duration::from_millis(100);
        // Whether this console's node has EVER been seen on this host.
        //
        // Only a device that was really there can be really gone. A relayed
        // peer console, a file-backed device and the e2e harness all name a
        // path that never exists locally while ser2net serves the bytes
        // perfectly well; refusing to dial those would mean capturing nothing
        // at all.
        let mut node_seen = false;
        loop {
            if *shutdown.borrow() {
                break;
            }
            // DO NOT DIAL AT A CONSOLE THAT IS NOT THERE.
            //
            // The capture task is deliberately held through a power cycle so it
            // can re-attach the instant the tty returns. But dialling an ABSENT
            // device makes ser2net try to open it, fail, and log the failure --
            // and the supervisor restarts ser2net to recover a wedged open,
            // which drops EVERY console on the host. Held task plus a 270ms dial
            // loop turned that into a restart storm: measured 27 re-attaches in
            // three minutes on one board while every other console was attached
            // twice, and an operator watching the web console saw the output
            // arrive in laggy chunks (this is report #13's failure mode, from
            // the other side).
            //
            // So: wait for the node, THEN dial. Nothing is lost by waiting --
            // there is nothing to read from a console that does not exist.
            let mut refused = false;
            let node_here = !self.device.canonical.is_empty()
                && std::path::Path::new(&self.device.canonical).exists();
            node_seen |= node_here;
            if node_seen && !node_here {
                self.set_state(CaptureState::NotListening);
                tokio::select! {
                    _ = tokio::time::sleep(present_poll) => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            }
            match TcpStream::connect(&endpoint).await {
                Ok(sock) => {
                    let _ = sock.set_nodelay(true);
                    // Fresh connection, fresh negotiation: a filter left
                    // mid-sequence by a socket that dropped between an IAC and
                    // its command byte would eat the first byte of this one.
                    self.telnet = crate::runner::TelnetFilter::default();
                    self.set_state(CaptureState::Listening);
                    // What this session actually delivered decides whether the
                    // backoff resets -- see below.
                    let bytes_before = self.stats.lock().map(|s| s.bytes).unwrap_or(0);
                    tracing::info!(device = %self.device.display_name(), %endpoint, "attached");
                    if let Err(e) = self.pump(sock, &mut shutdown).await {
                        // A loop that cannot make progress must say so. Measured:
                        // a store error (UNIQUE constraint on raw_lines.stream_offset)
                        // killed the read loop every ~4s for hours while the device
                        // still advertised itself as attached, and capture recorded
                        // NOTHING. A WARN per cycle is indistinguishable from an
                        // occasional reconnect, so repetition of the SAME error
                        // escalates.
                        if last_error.as_deref() == Some(e.message.as_str()) {
                            same_error += 1;
                        } else {
                            same_error = 1;
                            last_error = Some(e.message.clone());
                        }
                        // Stop advertising a device as attached when capture
                        // cannot record. Reporting "listening" while every write
                        // fails is the same lie a hook tells when it returns 0
                        // for an action the board ignored.
                        if same_error >= 3 {
                            self.set_state(CaptureState::NotListening);
                            tracing::error!(
                                device = %self.device.display_name(),
                                error = %e.message,
                                occurrences = same_error,
                                "read loop is failing repeatedly with the same error; \
                                 capture is NOT recording despite the device being attached"
                            );
                        } else {
                            tracing::warn!(device = %self.device.display_name(), error = %e.message, "read loop ended");
                        }
                        if let Ok(mut s) = self.stats.lock() {
                            s.read_errors += 1;
                        }
                    } else {
                        same_error = 0;
                        last_error = None;
                    }
                    // A DROP IS NOT AN OUTAGE until the reconnect fails.
                    //
                    // This used to flip to not_listening the instant a
                    // connection ended, including the routine re-dials that
                    // succeed milliseconds later -- so a sweep caught `stats`
                    // reporting not_listening mid-boot while adjacent calls on
                    // the same device said streaming and the byte count kept
                    // climbing. Nothing was wrong, but a monitoring loop keying
                    // on that field would have paged someone.
                    //
                    // The connect-failure branch below sets it when a re-dial
                    // actually fails, which is the honest signal: capture is not
                    // recording. Silence here means "reconnecting", not "fine".
                    if let Ok(mut s) = self.stats.lock() {
                        s.reconnects += 1;
                    }
                    // A CONNECTION IS NOT A CONSOLE.
                    //
                    // The backoff used to reset the moment TCP connected, but
                    // ser2net accepts a client for a device it cannot open,
                    // serves "Device open failure", and closes. That is a
                    // successful connect and a useless session, so the delay
                    // returned to its floor every time and capture re-dialled
                    // four times a second for as long as the device stayed
                    // un-openable -- measured on the bench at 28 re-attaches in
                    // two minutes, 270ms apart. Every re-attach restarts the
                    // byte stream the web console is reading, which is what an
                    // operator feels as lag.
                    //
                    // So only a session that carried real console bytes earns
                    // the reset. One that carried nothing lets the backoff grow,
                    // which is exactly what backoff is for.
                    let delivered = self.stats.lock().map(|s| s.bytes).unwrap_or(0) > bytes_before;
                    if delivered {
                        backoff = Duration::from_millis(self.cfg.attach.reconnect_backoff_ms);
                    }
                }
                Err(e) => {
                    self.set_state(CaptureState::NotListening);
                    refused = true;
                    tracing::debug!(device = %self.device.display_name(), %endpoint, error = %e, "connect failed");
                }
            }
            // NOTE: the backoff is NOT reset here on the console merely being
            // present. It was, and that reset fired on every pass for a device
            // whose node exists but which ser2net cannot open -- pinning the
            // delay at its floor and re-dialling four times a second forever.
            // Absence is now handled by not dialling at all (above), and a
            // session that delivered real bytes resets the backoff itself.
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.changed() => {}
            }
            let _ = refused;
            backoff = (backoff * 2).min(recover_cap);
        }

        // Dropping the sender ends the miner loop, which finishes the session.
        self.miner = None;
        let _ = miner.join();
        self.set_state(CaptureState::NotListening);
        Ok(())
    }

    async fn pump(
        &mut self,
        mut sock: TcpStream,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        let commit = Duration::from_millis(self.cfg.capture.commit_interval_ms.max(1));
        let mut ticker = tokio::time::interval(commit);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // ~1 s between recovery-presence walks at the default 250 ms commit tick;
        // `max(1)` keeps it sane if the commit interval is configured very large.
        let recovery_every = (1_000 / commit.as_millis().max(1) as u32).max(1);
        // A fresh connection: classify its first bytes against the current bus,
        // not a stale flag from before a reconnect.
        self.refresh_recovery(recovery_every, true);
        // A FAILURE STATE MUST NOT OUTLIVE THE FAILURE.
        //
        // `open_failed` and `away_in_edl` are both reached by reading something
        // and then, by their nature, receiving nothing more -- so no byte ever
        // arrives to correct them, and the registry goes on asserting a fault
        // after ser2net has been restarted or the board has left EDL. Measured:
        // a probe connected and read cleanly while `capture_state` still said
        // `open_failed`. Nothing but a fresh connection can tell the difference,
        // so take one, periodically, and only while in one of those states.
        let revalidate =
            Duration::from_millis(self.cfg.attach.revalidate_failed_after_ms.max(1_000));
        let mut failed_since: Option<std::time::Instant> = None;
        // Was the console's node missing when the current failure began?
        let mut absent_when_failed = false;

        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => return Ok(()),
                n = sock.read(&mut buf) => {
                    match n {
                        Ok(0) => return Ok(()),          // peer closed
                        Ok(n) => self.absorb(&buf[..n])?,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                _ = ticker.tick() => {
                    // Pick up an epoch another process opened (mcpd's `power`,
                    // `flash`, `mark`). Done on the tick rather than per byte so
                    // it costs one cheap query per interval, and before the
                    // tick's own work so a power cycle's first output already
                    // belongs to the new epoch.
                    // Both of these are the miner's work now: adopting an
                    // epoch another process opened, and the DEAD_AIR/commit
                    // tick. Sent, never awaited -- if the miner is busy the
                    // reader carries on reading.
                    if let Some(tx) = &self.miner {
                        let _ = tx.try_send(MinerMsg::Tick);
                    }
                    // The single authoritative recovery-mode check: if a flash
                    // gadget is on the board's ports, every reader must see
                    // `away_in_edl` -- even while a tty streams DevProg output
                    // (reports #7/#22). Cheap and throttled.
                    self.refresh_recovery(recovery_every, false);
                    if self.in_recovery {
                        self.set_state(CaptureState::AwayInEdl);
                    } else if self.state == CaptureState::Streaming {
                        self.set_state(CaptureState::Listening);
                    }
                    match self.state {
                        CaptureState::OpenFailed | CaptureState::AwayInEdl => {
                            let first = failed_since.is_none();
                            let since = *failed_since.get_or_insert_with(std::time::Instant::now);
                            // Cheap: one stat() per tick, and only while this
                            // console is already known to be in a failure state.
                            let present = !self.device.canonical.is_empty()
                                && std::path::Path::new(&self.device.canonical).exists();
                            if first {
                                absent_when_failed = !present;
                            }
                            // The console CAME BACK: absent when this failure
                            // began, here now. That is worth an immediate
                            // re-dial; being merely present is not, or a console
                            // ser2net cannot open spins the loop.
                            let returned = absent_when_failed && present;
                            if should_revalidate_with(self.state, since.elapsed(), revalidate, returned)
                            {
                                tracing::info!(
                                    device = %self.device.display_name(),
                                    state = self.state.as_str(),
                                    "re-dialling to find out whether this is still true"
                                );
                                // Returning re-enters the attach loop, which
                                // connects again and republishes the state it
                                // actually observes.
                                return Ok(());
                            }
                        }
                        _ => failed_since = None,
                    }
                }
            }
        }
    }

    fn absorb(&mut self, raw: &[u8]) -> Result<()> {
        // ser2net's accepter is `telnet(rfc2217=false)`, so a connection opens
        // with a negotiation burst and can carry IAC at any point after. None of
        // it came from the board, so none of it is console data: strip it here,
        // at the boundary, before anything counts bytes, frames lines, mines
        // templates or republishes to the broker.
        let filtered = self.telnet.push(raw);
        let bytes: &[u8] = &filtered;
        if bytes.is_empty() {
            return Ok(());
        }
        // ser2net answers a failed device open with its own failure text and
        // keeps the accepter bound, so a wedged console looks like a talkative
        // one. Storing that text as board output is how ~81KB of "console
        // output" on five of six RIDE consoles turned out to be pure error
        // message -- and, worse, how a power action gets "verified" by bytes the
        // board never sent. Refuse it at the boundary: it is not console data.
        if crate::runner::is_open_failure_banner(bytes) {
            // A BOARD IN EDL TOOK ITS UART WITH IT.
            //
            // EDL re-enumerates the USB device: the tty node vanishes and a QDL
            // gadget appears in its place, so of course ser2net cannot open it.
            // Restarting ser2net cannot conjure the node back -- only leaving
            // EDL can -- and asking for one every few seconds churns every other
            // console on the host during a flash. So this is named for what it
            // is and left alone.
            // ONE RECOVERY AUTHORITY, AND IT IS THE CHEAP ONE.
            //
            // This asked `board_is_in_edl()`, which runs `usb::scan()` -- the
            // per-device liveness probe that opens every device on the host
            // (~6.6s on this bench; report #20). On every re-dial of an
            // un-openable console, which is the one situation where it fires
            // repeatedly. `in_recovery` is the same fact from the sysfs
            // presence check the capture layer already maintains, and it is the
            // signal `diagnose` reports -- so consulting it is both cheaper and
            // the reason the two stop contradicting each other.
            self.refresh_recovery(0, true);
            if self.in_recovery {
                if self.state != CaptureState::AwayInEdl {
                    tracing::info!(
                        "this board is in a flash/recovery mode, so its UART has re-enumerated \
                         away and ser2net cannot open it; that is expected and needs no recovery"
                    );
                }
                self.set_state(CaptureState::AwayInEdl);
                return Ok(());
            }
            // ONCE PER OUTAGE, NOT ONCE PER RE-DIAL.
            //
            // Every re-dial sets `Listening` before the failure banner arrives,
            // so a `state != OpenFailed` guard re-fires on each attempt: an
            // operator watching a board sit in recovery saw this error stream
            // continuously while `diagnose` was calling the same state "not a
            // fault". Latched here and released when the console actually
            // delivers bytes again.
            if !self.reported_wedge {
                self.reported_wedge = true;
                tracing::error!(
                    "ser2net could not open this device and is serving its failure text; \
                     treating the console as wedged rather than capturing the error as output"
                );
            }
            // Tell the supervisor, which lives in another process and cannot see
            // this: ser2net serves the failure to the CLIENT and does not
            // reliably log it, so the only witness is whoever holds the
            // connection. Repeats collapse to one pending request per device.
            // ...and only when the console actually EXISTS. A device-open
            // failure for a tty that is not on the bus is not a wedged ser2net,
            // it is a board mid-reset, and restarting ser2net over it drops
            // every other console for nothing.
            if self.device.canonical.is_empty()
                || std::path::Path::new(&self.device.canonical).exists()
            {
                crate::recovery::request_reopen(
                    &self.cfg.paths.run_dir,
                    self.device.display_name(),
                    "ser2net served a device-open failure instead of console data",
                );
            }
            self.set_state(CaptureState::OpenFailed);
            return Ok(());
        }
        // Real console bytes: whatever outage we reported is over.
        self.reported_wedge = false;
        // Republish BEFORE framing: subscribers want the raw stream exactly as
        // the board sent it, and they must not wait on the store.
        if let Some(hub) = &self.hub {
            hub.publish(self.device.display_name(), bytes);
        }
        // HAND OFF, NEVER MINE HERE. Framing, mining and the store write happen
        // on the miner thread; this loop's only job is to keep the socket
        // drained so ser2net has no reason to discard.
        //
        // A full queue means the miner has fallen behind for longer than
        // `MINER_QUEUE` chunks. Losing the chunk is the honest outcome -- the
        // alternative is blocking the reader, which loses MORE, silently, at
        // ser2net instead. It is counted either way.
        let garbage = if let Some(tx) = &self.miner {
            if tx.try_send(MinerMsg::Bytes(bytes.to_vec())).is_err() {
                if let Ok(mut st) = self.stats.lock() {
                    st.dropped_bytes += bytes.len() as u64;
                }
                tracing::warn!(
                    device = %self.device.display_name(),
                    bytes = bytes.len(),
                    "the miner is behind; console bytes were dropped rather than stall the reader"
                );
            }
            self.garbage.load(std::sync::atomic::Ordering::Relaxed)
        } else if let Some(p) = self.pipeline.as_mut() {
            // No miner thread: an ad-hoc capture (`conminer ingest`, tests).
            // Nothing is racing the socket here, so mine inline -- but still off
            // the reactor, so a slow store cannot freeze whatever else shares
            // this runtime.
            let out = off_the_reactor(|| p.feed(bytes))?;
            if out.bytes != 0 || out.records != 0 {
                if let Ok(mut st) = self.stats.lock() {
                    st.bytes += out.bytes;
                    st.lines += out.lines as u64;
                    st.records += out.records as u64;
                    st.last_rx_ms = Some(p.last_rx_ms());
                }
            }
            out.garbage_lines > 0
        } else {
            false
        };
        // A flasher's DevProg/progress serial IS bytes, but the board's normal
        // console is gone -- so recovery mode outranks "streaming" (report #22).
        // The bytes are still stored above; only the STATE every reader sees is
        // suppressed to `away_in_edl`, which the prompt oracle already treats as
        // "no commandable console".
        self.set_state(if self.in_recovery {
            CaptureState::AwayInEdl
        } else if garbage {
            CaptureState::Garbage
        } else {
            CaptureState::Streaming
        });
        Ok(())
    }
}

/// Where to reach ser2net.
///
/// An explicit `connect_host` wins: under compose ser2net is a separate
/// container, so the address derived from `bind` (127.0.0.1) resolves to *this*
/// container and the attach silently never connects.
fn endpoint_host(cfg: &Config) -> String {
    cfg.ser2net_host()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix #1 (reports #7/#22): a flash/recovery gadget on the board's ports
    /// means its normal console is GONE even while a tty streams the flasher's
    /// DevProg/progress serial. The capture layer -- the one process watching the
    /// board -- must publish `away_in_edl` in that case, so every reader agrees
    /// the console is not at a prompt instead of each tool discovering it for
    /// itself. Here the recovery flag stands in for the cheap gadget check
    /// (`usb::recovery_gadget_on_ports`, tested in `usb::tests`); this asserts it
    /// OUTRANKS streaming in the capture state machine.
    #[test]
    fn recovery_mode_outranks_streaming_so_every_reader_sees_away_in_edl() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = reg
            .lock()
            .unwrap()
            .upsert_device("usb-flash", None, crate::store::IdentityKind::ById, None, 1)
            .unwrap();
        let mut cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg,
            Arc::new(crate::clock::StepClock::default()),
            dir.path(),
        )
        .unwrap();
        let p = cap.pipeline.as_mut().unwrap();
        p.begin_session(crate::store::SessionSource::Live, None, None, None)
            .unwrap();
        p.open_boot("power", None).unwrap();

        // Baseline: ordinary output means streaming.
        cap.absorb(b"hello world\n").unwrap();
        assert_eq!(
            cap.state,
            CaptureState::Streaming,
            "bytes normally mean streaming"
        );

        // A recovery gadget is present: the SAME kind of streaming bytes (a
        // flasher's progress serial) must now read as away_in_edl.
        cap.in_recovery = true;
        cap.absorb(b"DevProg: flashing lun0\n").unwrap();
        assert_eq!(
            cap.state,
            CaptureState::AwayInEdl,
            "recovery mode must outrank streaming so every reader suppresses the prompt"
        );

        // And it clears once the gadget leaves and the real console returns.
        cap.in_recovery = false;
        cap.absorb(b"# \n").unwrap();
        assert_eq!(
            cap.state,
            CaptureState::Streaming,
            "recovery cleared -> streaming again"
        );
    }

    #[test]
    fn capture_states_name_the_distinction_that_matters() {
        // "I do not know" and "nothing arrived" must never be the same string.
        assert_eq!(CaptureState::NotListening.as_str(), "not_listening");
        assert_eq!(CaptureState::Listening.as_str(), "listening");
        assert_ne!(
            CaptureState::NotListening.as_str(),
            CaptureState::Listening.as_str()
        );
    }

    #[test]
    fn a_wildcard_bind_resolves_to_a_connectable_host() {
        let mut cfg = Config::default();
        cfg.ser2net.bind = "0.0.0.0".into();
        assert_eq!(endpoint_host(&cfg), "127.0.0.1");
        cfg.ser2net.bind = "10.0.0.4".into();
        assert_eq!(endpoint_host(&cfg), "10.0.0.4");
    }

    /// A CAPTURE STATE THAT FAILED TO PUBLISH MUST BE PUBLISHED AGAIN.
    ///
    /// `published` used to be set before the write, and the write's error was
    /// dropped on the floor. Four processes share this registry, so a busy
    /// moment is ordinary -- and after one, the guard at the top of `set_state`
    /// saw `state == s && published` and short circuited every retry, freezing
    /// capture health for the life of the process.
    ///
    /// The symptom was an intermittent `capture_state: not_listening` on a
    /// console that was demonstrably capturing: the exact ambiguity between "no
    /// output" and "I do not know" that §8.4 exists to remove.
    ///
    /// The failure is injected rather than waited for: `query_only` makes the
    /// registry refuse writes, which is what a busy or read-only registry looks
    /// like from here.
    #[tokio::test]
    async fn a_capture_state_the_registry_refused_is_retried_not_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = reg
            .lock()
            .unwrap()
            .upsert_device("usb-busy", None, crate::store::IdentityKind::ById, None, 1)
            .unwrap();
        let mut cap = Capture::open(
            dev.clone(),
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            Arc::new(crate::clock::StepClock::default()),
            dir.path(),
        )
        .unwrap();

        // The registry refuses writes, as a busy one does.
        reg.lock()
            .unwrap()
            .conn()
            .pragma_update(None, "query_only", true)
            .unwrap();
        cap.set_state(CaptureState::Listening);
        assert!(
            !cap.published,
            "a write the registry refused is not a publication"
        );
        assert!(
            reg.lock()
                .unwrap()
                .device(dev.id)
                .unwrap()
                .capture_state
                .is_none(),
            "fixture premise: nothing was actually written"
        );

        // The registry recovers, and the very next attempt at the SAME state
        // must go through -- that is the retry the old code made impossible.
        reg.lock()
            .unwrap()
            .conn()
            .pragma_update(None, "query_only", false)
            .unwrap();
        cap.set_state(CaptureState::Listening);
        assert!(cap.published, "and once it lands, it is published");
        assert_eq!(
            reg.lock()
                .unwrap()
                .device(dev.id)
                .unwrap()
                .capture_state
                .as_deref(),
            Some("listening"),
            "the registry now holds what capture actually sees"
        );
    }

    #[tokio::test]
    async fn a_device_with_no_endpoint_reports_not_listening_rather_than_pretending() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = reg
            .lock()
            .unwrap()
            .upsert_device(
                "usb-no-port",
                None,
                crate::store::IdentityKind::ById,
                None,
                1,
            )
            .unwrap();

        let cap = Capture::open(
            dev.clone(),
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            Arc::new(crate::clock::StepClock::default()),
            dir.path(),
        )
        .unwrap();
        assert!(cap.endpoint().is_none());

        let (_tx, rx) = tokio::sync::watch::channel(false);
        cap.run(rx).await.unwrap();
        // CAPTURE HEALTH, in the column that now holds it. A device with no
        // endpoint reports `not_listening` -- "I do not know", never "no
        // output" -- and it must not claim anything about PRESENCE, which is
        // discovery's to say.
        let row = reg.lock().unwrap().device(dev.id).unwrap();
        assert_eq!(row.capture_state.as_deref(), Some("not_listening"));
        assert_ne!(
            row.state, "not_listening",
            "capture health must not be written into the presence column"
        );
    }

    /// VERBATIM from the bravo node's first attach, captured off the wire:
    /// ser2net's `telnet(rfc2217=false)` accepter opens with this, and before
    /// the fix it became the first thing ever recorded on that board's console.
    const SER2NET_NEGOTIATION: &[u8] = &[
        0xFF, 0xFB, 0x03, // IAC WILL SUPPRESS-GO-AHEAD
        0xFF, 0xFD, 0x03, // IAC DO   SUPPRESS-GO-AHEAD
        0xFF, 0xFB, 0x01, // IAC WILL ECHO
        0xFF, 0xFD, 0x01, // IAC DO   ECHO
        0xFF, 0xFB, 0x00, // IAC WILL BINARY
        0xFF, 0xFD, 0x00, // IAC DO   BINARY
    ];

    #[tokio::test]
    async fn telnet_negotiation_is_not_captured_as_console_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        cfg.ser2net.bind = "127.0.0.1".into();

        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    "usb-telnet",
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            // The negotiation, then a sequence SPLIT mid-command across two
            // writes, then real console output. The split is the case a
            // buffer-at-a-time stripper gets wrong.
            sock.write_all(SER2NET_NEGOTIATION).await.unwrap();
            sock.write_all(b"[    0.000000] Linux version 6.12.9 (build@lab)\n\xff")
                .await
                .unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            sock.write_all(b"\xfd\x03[    1.000000] mmc0: new HS200 MMC card\n")
                .await
                .unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
        });

        let cap = Capture::open(
            dev.clone(),
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let stats = cap.stats_handle();

        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let store =
            DeviceStore::open(&dir.path().join(&dev.db_file), &dev.canonical, true).unwrap();
        assert_eq!(
            store.line_count().unwrap(),
            2,
            "the negotiation must not have framed a line of its own"
        );
        for t in store
            .list_templates(&crate::store::TemplateQuery {
                limit: 50,
                ..Default::default()
            })
            .unwrap()
        {
            assert!(
                !t.text.contains('\u{fffd}') && !t.text.as_bytes().contains(&0xFF),
                "telnet bytes reached a mined template: {:?}",
                t.text
            );
        }
        // Byte accounting must not count them either: 18 negotiation bytes plus
        // the 3 of the split sequence are not console traffic.
        let s = stats.lock().unwrap().clone();
        assert_eq!(
            s.bytes, 88,
            "only the two console lines (48 + 40 bytes) should have been counted, got {s:?}"
        );
        assert_eq!(s.dropped_bytes, 0);
    }

    /// THE READER MUST KEEP DRAINING WHILE MINING IS SLOW.
    ///
    /// ser2net discards for a client that stops reading, so a capture loop that
    /// stops to mine loses console it can never get back. Measured on the bench
    /// during one boot: ser2net delivered 709,425 bytes, the store recorded
    /// 23,341 -- 3.3% -- in 8KB lumps ~2.4s apart, and the web console showed
    /// exactly those same 23,341 bytes.
    ///
    /// This serves a fast, continuous stream and requires capture to take
    /// essentially all of it. Mining happens on its own thread; the socket is
    /// drained regardless of how far behind mining is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fast_console_is_drained_even_while_mining_is_slow() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;
        cfg.ser2net.bind = "127.0.0.1".into();

        const LINES: usize = 4000;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sent_c = sent.clone();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                for i in 0..LINES {
                    let line = format!("[{i:>8}.000000] driver-core: probe {i} state=4 ok\r\n");
                    if sock.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    sent_c.fetch_add(line.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });

        let tty = dir.path().join("usb-fast-if00-port0");
        std::fs::write(&tty, b"").unwrap();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    tty.to_str().unwrap(),
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };
        let cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let stats = cap.stats_handle();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));
        tokio::time::sleep(Duration::from_secs(4)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(10), task).await;

        let s = stats.lock().unwrap().clone();
        let offered = sent.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            offered > 100_000,
            "the fixture must actually be fast: {offered}"
        );
        let kept = s.bytes + s.dropped_bytes;
        assert!(
            kept * 100 >= offered * 95,
            "capture accounted for {kept} of {offered} bytes ({}%): a reader that stops to \
             mine loses console at ser2net, and it is gone for good",
            kept * 100 / offered.max(1)
        );
    }

    /// LOSS MUST BE A COUNTER, NEVER A SILENCE.
    ///
    /// `dropped_bytes` exists so that a gap in the console is visible rather
    /// than mistaken for an idle board. It read zero through an episode that
    /// lost 97% of a boot, which is the worst possible failure for it.
    #[test]
    fn bytes_the_miner_cannot_take_are_counted_not_silently_lost() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device("usb-drop", None, crate::store::IdentityKind::ById, None, 1)
                .unwrap();
            r.device(d.id).unwrap()
        };
        let mut cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();

        // A miner that never drains: the queue fills, and every chunk after
        // that is a chunk the console lost.
        let (tx, _held) = std::sync::mpsc::sync_channel::<MinerMsg>(1);
        cap.miner = Some(tx);
        for _ in 0..(MINER_QUEUE + 8) {
            let _ = cap.absorb(b"[    0.000000] chatter\r\n");
        }
        let dropped = cap.stats.lock().unwrap().dropped_bytes;
        assert!(
            dropped > 0,
            "console bytes were discarded and dropped_bytes stayed zero: loss must be a \
             counter, never a silence"
        );
    }

    /// A SLOW STORE MUST NOT FREEZE THE RUNTIME.
    ///
    /// `pipeline.feed` is blocking and its cost grows with the template set.
    /// Measured on the bench: 1954ms to feed a single 15-byte line on a board
    /// whose store had reached 176,315 templates. Called directly from the
    /// capture task it blocked the worker holding the IO/timer driver, so the
    /// entire runtime froze -- a bare heartbeat task stalled 2002ms and the
    /// broker feeding the web console could not be polled, putting the console
    /// two seconds behind a board that answered in 20ms.
    ///
    /// This drives the real helper on a real multi-threaded runtime with a
    /// deliberately slow closure, and requires a concurrent timer to keep
    /// ticking through it.
    #[test]
    fn slow_blocking_work_does_not_freeze_other_tasks() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            // ONE worker, deliberately: with spare workers another thread picks
            // up the timer and the freeze hides. Production had sixteen and
            // still froze, because the blocked worker was holding the IO/timer
            // driver -- which is exactly what one worker reproduces every time
            // instead of occasionally.
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let drift = rt.block_on(async {
            let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let worst = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let (t, w) = (ticks.clone(), worst.clone());
            let beat = tokio::spawn(async move {
                let mut last = std::time::Instant::now();
                for _ in 0..40 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let d = last.elapsed().as_millis() as u64;
                    w.fetch_max(d, std::sync::atomic::Ordering::Relaxed);
                    t.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    last = std::time::Instant::now();
                }
            });
            // Let the heartbeat settle, then do 800ms of blocking work FROM A
            // SPAWNED TASK -- on a worker thread, exactly where capture does it.
            // Blocking the `block_on` thread instead would prove nothing: that
            // is not a worker, and the heartbeat would tick through it happily.
            tokio::time::sleep(Duration::from_millis(150)).await;
            tokio::spawn(async {
                off_the_reactor(|| std::thread::sleep(Duration::from_millis(800)));
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            beat.abort();
            worst.load(std::sync::atomic::Ordering::Relaxed)
        });
        assert!(
            drift < 400,
            "a concurrent timer stalled {drift}ms while blocking work ran: mining on the \
             reactor is what puts the web console seconds behind the board"
        );
    }

    /// TIME-NOT-ATTACHED IS THE NUMBER AN OPERATOR FEELS.
    ///
    /// ser2net buffers the console while nothing reads it, so every second
    /// capture spends backed off is a second of boot that arrives later in one
    /// lump. Measured from the browser during a real boot: 21KB delivered in a
    /// single millisecond, in 8KB reads, after six seconds of silence -- the
    /// console had not stopped, capture had.
    ///
    /// This console refuses to open for the first second and then works, as one
    /// does while a board re-enumerates. Capture must be reading again quickly
    /// after it becomes openable, not sitting out a long ceiling.
    #[tokio::test]
    async fn capture_does_not_sit_out_a_console_that_becomes_openable() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;
        cfg.ser2net.bind = "127.0.0.1".into();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Five seconds of refusing, which is what a board re-enumerating looks
        // like -- long enough for a doubling backoff to have escalated well past
        // it.
        let healthy_at = tokio::time::Instant::now() + Duration::from_secs(5);
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    use tokio::io::AsyncWriteExt;
                    if tokio::time::Instant::now() < healthy_at {
                        // Un-openable, exactly as ser2net answers.
                        let _ = sock
                            .write_all(b"Device open failure: Value or file not found\r\n")
                            .await;
                        let _ = sock.flush().await;
                    } else {
                        // The board is talking now.
                        for _ in 0..40 {
                            if sock.write_all(b"[    0.000000] booting\r\n").await.is_err() {
                                break;
                            }
                            let _ = sock.flush().await;
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        });

        let tty = dir.path().join("usb-slowopen-if00-port0");
        std::fs::write(&tty, b"").unwrap();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    tty.to_str().unwrap(),
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        let cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let stats = cap.stats_handle();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));

        // Five seconds of refusing plus at most ~1.5s of ceiling lands inside
        // this window; an escalated ceiling does not, and the boot arrives in a
        // lump afterwards instead.
        tokio::time::sleep(Duration::from_millis(7_500)).await;
        let seen = stats.lock().unwrap().bytes;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        assert!(
            seen > 0,
            "capture read nothing in 7.5s from a console that became openable after 5s: \
             every second it sits out is a second of boot that arrives later in one lump"
        );
    }

    /// THE REAL PUMP: the backoff must actually ENGAGE on a console that cannot
    /// be opened -- but only as far as the recovery ceiling.
    ///
    /// This gate used to count dials and forbid more than five in three
    /// seconds. That instrument is now the wrong one: the harm it was aimed at
    /// -- a floor-pinned retry making ser2net log open failures until the
    /// supervisor restarted it and dropped every console -- is handled at the
    /// supervisor, which attributes a failure to its device. Meanwhile the
    /// ceiling has to stay SHORT, because ser2net only opens the tty while a
    /// client is connected and every backed-off second is buffered console that
    /// lands in the browser as a lump. A count cannot tell a 250ms floor from a
    /// 350ms ceiling without being flaky, so this asserts the property itself:
    /// the delay grows past the floor, and stops at the ceiling.
    #[tokio::test]
    async fn an_unopenable_console_backs_off_to_the_ceiling_and_no_further() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;
        cfg.ser2net.bind = "127.0.0.1".into();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dials: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = dials.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    if let Ok(mut v) = seen.lock() {
                        v.push(std::time::Instant::now());
                    }
                    use tokio::io::AsyncWriteExt;
                    let _ = sock
                        .write_all(b"Device open failure: Value or file not found\r\n")
                        .await;
                    let _ = sock.flush().await;
                }
            }
        });

        let tty = dir.path().join("usb-present-unopenable-if00-port0");
        std::fs::write(&tty, b"").unwrap();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    tty.to_str().unwrap(),
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        let cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));
        tokio::time::sleep(Duration::from_secs(4)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let times = dials.lock().unwrap().clone();
        assert!(
            times.len() >= 4,
            "expected repeated dials, got {}",
            times.len()
        );
        // Steady state: the last few gaps must have grown past the 250ms floor
        // (the backoff engaged) and must not exceed the ceiling by much (it is
        // still quick to re-attach).
        let gaps: Vec<u128> = times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_millis())
            .collect();
        let steady = &gaps[gaps.len().saturating_sub(3)..];
        assert!(
            steady.iter().all(|g| *g >= 300),
            "the backoff never engaged; dialling stayed pinned at the floor: {gaps:?}"
        );
        assert!(
            steady.iter().all(|g| *g <= 900),
            "the backoff went past the recovery ceiling: every backed-off second is \
             buffered console that reaches the browser as a lump: {gaps:?}"
        );
    }

    /// A console that is PRESENT and still cannot be opened must not spin.
    ///
    /// ser2net answers an un-openable device by serving its failure text and
    /// closing, so capture lands in `open_failed` immediately. Treating "the
    /// node exists" as a reason to re-dial then loops at the backoff floor --
    /// measured at 28 re-attaches in two minutes, 270ms apart -- and every
    /// re-attach restarts the byte stream the web console is reading, which is
    /// what an operator feels as lag. Only a node that CAME BACK earns an
    /// instant retry.
    #[test]
    fn a_present_but_unopenable_console_waits_instead_of_spinning() {
        let tick = Duration::from_millis(250);
        let interval = Duration::from_secs(15);
        // Present all along, still failing: the timer governs, not presence.
        assert!(
            !should_revalidate_with(CaptureState::OpenFailed, tick, interval, false),
            "a console that never went away must wait out the interval"
        );
        // It came back after being absent: retry at once.
        assert!(should_revalidate_with(
            CaptureState::OpenFailed,
            tick,
            interval,
            true
        ));
        // And the interval still applies when nothing changed.
        assert!(should_revalidate_with(
            CaptureState::OpenFailed,
            interval,
            interval,
            false
        ));
    }

    /// A WEDGE IS AN OUTAGE, NOT A COUNT OF RE-DIALS.
    ///
    /// Every re-dial sets `Listening` before ser2net's failure banner arrives,
    /// so guarding the error on `state != OpenFailed` re-fires it on every
    /// attempt. An operator watching a board sit in recovery saw this error
    /// stream continuously while `diagnose` called the very same state "not a
    /// fault" -- two layers describing one console differently.
    #[test]
    fn a_wedged_console_is_reported_once_not_once_per_redial() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device("usb-wedge", None, crate::store::IdentityKind::ById, None, 1)
                .unwrap();
            r.device(d.id).unwrap()
        };
        let mut cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();

        {
            let p = cap.pipeline.as_mut().unwrap();
            p.begin_session(crate::store::SessionSource::Live, None, None, None)
                .unwrap();
            p.open_boot("power", None).unwrap();
        }

        const BANNER: &[u8] = b"Device open failure: Value or file not found\r\n";
        // Five re-dials: each one attaches (Listening) and then reads the
        // failure banner, exactly as the live loop does.
        for _ in 0..5 {
            cap.set_state(CaptureState::Listening);
            cap.absorb(BANNER).unwrap();
        }
        assert!(
            cap.reported_wedge,
            "the outage must be latched after the first report"
        );

        // ...and the latch releases when the console actually comes back, so a
        // LATER outage is reported again rather than swallowed.
        cap.absorb(b"[    0.000000] the board is talking again\r\n")
            .unwrap();
        assert!(
            !cap.reported_wedge,
            "real console bytes must end the outage, or a second wedge goes unreported"
        );
    }

    /// Report #36: a board being flashed still produces bytes, and they must be
    /// recorded.
    ///
    /// `away_in_edl` suppresses the STATE, never the recording. Treating a
    /// present device node as a reason to re-dial applied to recovery mode too,
    /// so during a flash -- when the tty is present and streaming the flasher's
    /// own output -- capture re-dialled every commit tick and stored nothing.
    #[test]
    fn recovery_mode_does_not_re_dial_on_every_tick_and_throw_the_flash_away() {
        let quick = Duration::from_millis(1);
        let long = Duration::from_secs(15);
        assert!(
            !should_revalidate_with(CaptureState::AwayInEdl, quick, long, true),
            "a present tty in recovery mode is the flasher talking, not a reason to re-dial"
        );
        // The timer still applies, so leaving EDL is noticed.
        assert!(should_revalidate_with(
            CaptureState::AwayInEdl,
            long,
            long,
            true
        ));
        // ...while an open failure still short-circuits on the node returning.
        assert!(should_revalidate_with(
            CaptureState::OpenFailed,
            quick,
            long,
            true
        ));
        assert!(!should_revalidate_with(
            CaptureState::OpenFailed,
            quick,
            long,
            false
        ));
        // A healthy console is never re-dialled for either reason.
        assert!(!should_revalidate_with(
            CaptureState::Streaming,
            long,
            long,
            true
        ));
    }

    /// The other half of holding capture through a power cycle: while the
    /// console is ABSENT it must not dial ser2net at all.
    ///
    /// Dialling an absent device makes ser2net try to open it and log the
    /// failure, and the supervisor restarts ser2net to clear what looks like a
    /// wedged open -- dropping every console on the host. Measured as 27
    /// re-attaches in three minutes on one board while every other console
    /// attached twice, felt by an operator as a laggy, chunky web console.
    #[tokio::test]
    async fn an_absent_console_is_not_dialled_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;
        cfg.ser2net.bind = "127.0.0.1".into();

        // A listener that COUNTS dials, standing in for ser2net.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = dials.clone();
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_ok() {
                    seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        // The node exists first -- this is a real console on this host -- and
        // is then taken away, which is what a power cycle does.
        let missing = dir.path().join("usb-absent-if00-port0");
        std::fs::write(&missing, b"").unwrap();
        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    missing.to_str().unwrap(),
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        let cap = Capture::open(
            dev,
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));
        // Let it attach to the console that IS there...
        tokio::time::sleep(Duration::from_millis(600)).await;
        let attached = dials.load(std::sync::atomic::Ordering::Relaxed);
        assert!(attached > 0, "a present console must be dialled");
        // ...then the board resets and takes its tty with it.
        std::fs::remove_file(&missing).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        assert_eq!(
            dials.load(std::sync::atomic::Ordering::Relaxed) - attached,
            0,
            "an absent console must not be dialled: every dial makes ser2net fail an open, \
             and the supervisor restarts ser2net over it, dropping every other console"
        );
    }

    /// Reports #31/#32/#34: a power cycle takes the tty away, the re-dials
    /// fail, the backoff doubles to its 15s ceiling, and the board comes back
    /// and prints its whole firmware banner while capture is still asleep.
    ///
    /// The tty being present again is the evidence that waiting longer is
    /// pointless. This drives the real reconnect loop: let the backoff escalate
    /// against a dead endpoint, bring the endpoint back, and require capture to
    /// be reading again quickly. With the escalation unbounded the next attempt
    /// would be ~8s away and this window closes empty.
    #[tokio::test]
    async fn a_console_that_comes_back_is_re_attached_without_waiting_out_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;
        cfg.ser2net.bind = "127.0.0.1".into();

        // A port nothing is listening on yet: every dial fails and the backoff
        // grows, exactly as it does while a board is off.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // The device's tty EXISTS -- the board is present, its console is just
        // not being served yet. That is the state a power cycle ends in.
        let tty = dir.path().join("usb-present-if00-port0");
        std::fs::write(&tty, b"").unwrap();

        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device(
                    tty.to_str().unwrap(),
                    None,
                    crate::store::IdentityKind::ById,
                    None,
                    1,
                )
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        let cap = Capture::open(
            dev.clone(),
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let stats = cap.stats_handle();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));

        // Let the failures compound well past the floor.
        tokio::time::sleep(Duration::from_secs(8)).await;

        // The console comes back, as it does when the board finishes resetting.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("rebind the console port");
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = sock
                    .write_all(b"[    0.000000] Linux version 6.12.9 (build@lab) #1 SMP\n")
                    .await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let s = stats.lock().unwrap().clone();
        assert!(
            s.bytes > 0,
            "capture must re-attach as soon as the console returns, not wait out \
             an escalated backoff: {s:?}"
        );
    }

    #[tokio::test]
    async fn bytes_from_a_live_socket_are_captured_framed_and_mined() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        cfg.capture.commit_interval_ms = 20;

        // Stand in for ser2net: a listener that plays a boot log.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        cfg.ser2net.bind = "127.0.0.1".into();

        let reg = Arc::new(Mutex::new(Registry::open(dir.path()).unwrap()));
        let dev = {
            let mut r = reg.lock().unwrap();
            let d = r
                .upsert_device("usb-live", None, crate::store::IdentityKind::ById, None, 1)
                .unwrap();
            r.assign_port(d.id, port).unwrap();
            r.device(d.id).unwrap()
        };

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            sock.write_all(
                b"[    0.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n\
                  [    1.000000] mmc0: new HS200 MMC card at address 0001\n\
                  [    2.000000] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n\
                  [    2.000000] Modules linked in: foo\n\
                  [    2.000000] ---[ end trace 0000000000000000 ]---\n",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
        });

        let cap = Capture::open(
            dev.clone(),
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            reg.clone(),
            crate::clock::system(),
            dir.path(),
        )
        .unwrap();
        let stats = cap.stats_handle();

        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(cap.run(rx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let s = stats.lock().unwrap().clone();
        assert!(s.bytes > 200, "{s:?}");
        assert_eq!(s.lines, 5);
        assert_eq!(s.dropped_bytes, 0, "capture must never drop silently");

        // …and the mined result is queryable through the ordinary store API.
        let store =
            DeviceStore::open(&dir.path().join(&dev.db_file), &dev.canonical, true).unwrap();
        assert_eq!(store.line_count().unwrap(), 5);
        let stages = store.stages(None, None).unwrap();
        assert_eq!(stages[0].name, "kernel");
        let crashes = store
            .list_templates(&crate::store::TemplateQuery {
                min_severity: Some(crate::store::Severity::Crit),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(
            crashes.iter().any(|t| t.text.contains("Internal error")),
            "the oops must have been framed and mined live"
        );
    }
}

#[cfg(test)]
mod open_failed_state_tests {
    use super::*;

    /// The wedge must be NAMED, not folded into "listening". A wedged console
    /// and a quiet one look identical from outside -- that ambiguity is exactly
    /// what hid five dead RIDE consoles while they reported ~81KB of "output"
    /// that was ser2net's error text repeated.
    #[test]
    fn open_failed_is_distinct_from_quiet_and_from_streaming() {
        assert_eq!(CaptureState::OpenFailed.as_str(), "open_failed");
        for other in [
            CaptureState::NotListening,
            CaptureState::Listening,
            CaptureState::Streaming,
            CaptureState::Garbage,
        ] {
            assert_ne!(
                other.as_str(),
                CaptureState::OpenFailed.as_str(),
                "the wedge must not be reported as {}",
                other.as_str()
            );
        }
    }
}

#[cfg(test)]
mod reconnect_state_tests {

    /// S8: a routine re-dial must not look like an outage.
    ///
    /// The state was flipped to not_listening the moment a connection ended,
    /// including reconnects that succeed immediately -- so `stats` reported
    /// not_listening mid-boot while capture was demonstrably recording. Anything
    /// monitoring that field would false-alarm on healthy behaviour.
    #[test]
    fn a_drop_alone_does_not_report_an_outage() {
        let src = include_str!("live.rs");
        assert!(
            src.contains("A DROP IS NOT AN OUTAGE until the reconnect fails"),
            "the reasoning must stay with the code"
        );
        // The state is only asserted where a re-dial actually failed.
        let drop_site = src
            .find("s.reconnects += 1")
            .expect("the reconnect counter");
        let window = &src[drop_site.saturating_sub(400)..drop_site];
        assert!(
            !window.contains("set_state(CaptureState::NotListening)"),
            "a drop must not assert not_listening; only a failed re-dial may"
        );
    }
}

#[cfg(test)]
mod edl_not_a_wedge_tests {
    use super::*;

    fn qdl(port: &str) -> crate::usb::UsbDevice {
        crate::usb::UsbDevice {
            vendor_id: 0x05c6,
            product_id: 0x9008,
            bus: 3,
            address: 7,
            port_path: Some(port.to_string()),
            liveness: crate::usb::Liveness::Alive,
        }
    }

    /// A CONSOLE THAT WENT AWAY WITH ITS BOARD IS NOT WEDGED.
    ///
    /// Reported from a live flashing session: EDL entry worked, and conminer
    /// called it a capture fault -- because ser2net cannot open a tty that
    /// re-enumerated away and answers with its failure banner. Asking the
    /// supervisor to restart ser2net cannot bring the node back (only leaving
    /// EDL can) and churns every other console on the host mid-flash.
    #[test]
    fn edl_on_this_boards_ports_explains_the_open_failure() {
        let mine = vec!["3-1.2".to_string()];
        assert!(
            edl_took_the_uart(&mine, &[qdl("3-1.2")]),
            "this board's own gadget must explain its missing UART"
        );
    }

    /// ...but only THIS board's. One board's flash must not silence another
    /// board's recovery: that would hide a real wedge behind somebody else's
    /// download mode, which is the same cross-board confusion that once had one
    /// board reporting another's power.
    #[test]
    fn a_neighbours_edl_explains_nothing() {
        let mine = vec!["3-1.2".to_string()];
        assert!(
            !edl_took_the_uart(&mine, &[qdl("3-4.1")]),
            "a gadget on another hub port is not this board"
        );
        assert!(
            !edl_took_the_uart(&mine, &[]),
            "and with no gadget at all there is nothing to explain"
        );
    }

    /// With no ports attributed, the answer is NO -- the conservative one. A
    /// bus-wide yes would let any board's EDL suppress this one's recovery.
    #[test]
    fn an_unattributed_board_keeps_the_old_behaviour() {
        assert!(!edl_took_the_uart(&[], &[qdl("3-1.2")]));
    }

    /// A FAILURE STATE IS RE-CHECKED, NOT ASSERTED FOREVER.
    ///
    /// `open_failed` and `away_in_edl` are reached by reading something and then
    /// receiving nothing more, so no byte ever arrives to correct them. Reported
    /// from the bench: a probe connected and read cleanly while `capture_state`
    /// still said `open_failed`. Only a fresh connection can tell the
    /// difference, so the pump gives up on those states after an interval and
    /// lets the attach loop republish what it actually observes.
    #[test]
    fn a_failed_capture_re_dials_instead_of_asserting_the_failure_forever() {
        use std::time::Duration;
        let interval = Duration::from_secs(15);

        // The two states that stop producing bytes: re-checked once the interval
        // has passed, left alone before it.
        for state in [CaptureState::OpenFailed, CaptureState::AwayInEdl] {
            assert!(
                should_revalidate(state, Duration::from_secs(16), interval),
                "{} must be re-checked rather than asserted forever",
                state.as_str()
            );
            assert!(
                !should_revalidate(state, Duration::from_secs(2), interval),
                "{}: a moment of failure is not yet a stale claim",
                state.as_str()
            );
        }

        // A HEALTHY CONSOLE IS NEVER DROPPED ON A TIMER. Re-dialling a streaming
        // board would interrupt the capture this whole system exists to keep.
        for state in [
            CaptureState::Streaming,
            CaptureState::Listening,
            CaptureState::Garbage,
            CaptureState::NotListening,
        ] {
            assert!(
                !should_revalidate(state, Duration::from_secs(3600), interval),
                "{} must never be re-dialled on a timer",
                state.as_str()
            );
        }

        // And the shipped default is a real interval, not zero.
        assert!(
            crate::config::Config::default()
                .attach
                .revalidate_failed_after_ms
                >= 1_000
        );
    }

    /// The state has a name of its own, so nothing reads it as a fault.
    #[test]
    fn away_in_edl_is_not_open_failed() {
        assert_eq!(CaptureState::AwayInEdl.as_str(), "away_in_edl");
        for other in [
            CaptureState::OpenFailed,
            CaptureState::NotListening,
            CaptureState::Listening,
            CaptureState::Streaming,
            CaptureState::Garbage,
        ] {
            assert_ne!(other.as_str(), CaptureState::AwayInEdl.as_str());
        }
    }
}

#[cfg(test)]
mod capture_state_column_tests {
    use super::*;
    use crate::store::{IdentityKind, Registry};

    /// MINERD'S WRITER MUST TARGET THE CAPTURE COLUMN.
    ///
    /// Asserting the store can hold two facts is not the same as asserting the
    /// capture path writes the right one -- a test that wrote through the store
    /// directly passed with minerd still clobbering `state`, which is exactly
    /// the bug. So this calls what minerd calls.
    #[test]
    fn publishing_capture_health_leaves_presence_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::open(dir.path()).unwrap();
        let row = reg
            .upsert_device("usb-A-if00-port0", None, IdentityKind::ById, None, 1_000)
            .unwrap();
        reg.set_state(row.id, "discovered").unwrap();

        publish_capture_state(&mut reg, row.id, CaptureState::Streaming).unwrap();

        let after = reg.device(row.id).unwrap();
        assert_eq!(
            after.capture_state.as_deref(),
            Some("streaming"),
            "capture health must land in its own column"
        );
        assert_eq!(
            after.state, "discovered",
            "...and must not overwrite what discovery said about presence"
        );
    }
}
