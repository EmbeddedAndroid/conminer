//! Shared server state: the registry, per-device stores, and the freshness
//! envelope every read response carries.

use conminer_core::clock::SharedClock;
use conminer_core::config::Config;
use conminer_core::error::{ErrorCode, Result, ToolError};
use conminer_core::framer::ProfileSet;
use conminer_core::store::{DeviceRow, DeviceStore, Registry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// Everything a tool call needs. Cloneable and cheap; the interior mutability is
/// where the SQLite connections live, because `rusqlite::Connection` is `Send`
/// but not `Sync`.
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    profiles: Arc<ProfileSet>,
    data_dir: PathBuf,
    clock: SharedClock,
    registry: Mutex<Registry>,
    /// Read handles, opened once per device and reused.
    stores: Mutex<HashMap<i64, Arc<Mutex<DeviceStore>>>>,
    /// Identity of the agent making calls, for lease ownership (§15.1).
    holder: Mutex<String>,
    /// §P3. Calls parked for a node that can reach us but that we cannot dial.
    ///
    /// Per-context, not a process-wide static: the fleet tests run two nodes in
    /// one process, and a shared queue would have each answering the other's
    /// work by accident -- a green test proving nothing.
    relay: Arc<conminer_core::peers::RelayQueue>,
    /// Actuations in flight, by console id.
    ///
    /// A power or boot-mode call is one workflow from hook to verified effect,
    /// escalation included, and it can run well past a minute on a board whose
    /// controller holds a 6 s press behind a ~35 s USB claim. Nothing else
    /// serialised a second call on the same board while that ran: the lease
    /// does not, because the same agent holds it. Measured on the Uno Q, an
    /// `on` accepted mid-escalation booted the board and the escalation's
    /// final `off` press then reset that kernel 28 s later, recorded as a
    /// power epoch nobody had asked for.
    actuations: Mutex<HashMap<i64, InFlight>>,
    /// The last actuation that FINISHED on each console: what it was, when,
    /// and its final effect. This is where an escalation that outlived its
    /// caller's patience leaves its answer, and what `actuation_status` reads.
    outcomes: Mutex<HashMap<i64, Value>>,
    /// The last time each CONTROLLER's boot-mode overrides were read, and what
    /// it said. Keyed on the controller instance, because the overrides belong
    /// to the board and every console of it shares them.
    ///
    /// A read costs seconds of a single-session controller, and every dashboard
    /// on the fleet asks the owner for it on a timer, so an asker may accept a
    /// reading up to an age it names. It is never served without that age, and
    /// every conminer action that changes an override replaces it with the
    /// post-action readback, so a reading can only be stale with respect to a
    /// change made OUTSIDE conminer.
    overrides: Mutex<HashMap<String, OverridesReading>>,
}

/// One read of a controller's boot-mode overrides, with when and how it went.
#[derive(Clone, Debug)]
pub struct OverridesReading {
    pub overrides: conminer_core::overrides::BootOverrides,
    pub read_at_ms: i64,
    /// Why the read produced nothing, when it did. A failed read is kept, so an
    /// asker inside the age window is told "unknown, and here is why" instead of
    /// hammering a controller that is not answering.
    pub error: Option<String>,
    pub controller: Option<String>,
    pub controller_port: Option<String>,
}

/// One actuation still running on a console.
#[derive(Clone, Debug)]
pub struct InFlight {
    pub tool: &'static str,
    pub action: String,
    pub holder: String,
    pub since_ms: i64,
    /// Where the workflow is: `hook`, `verify`, or an escalation step. Named so
    /// a caller that got ACTUATION_IN_FLIGHT can tell "still pressing" from
    /// "settling", and budget accordingly.
    pub phase: String,
}

impl InFlight {
    pub fn to_json(&self, now_ms: i64) -> Value {
        json!({
            "tool": self.tool,
            "action": self.action,
            "holder": self.holder,
            "phase": self.phase,
            "since_ms": self.since_ms,
            "running_for_ms": now_ms.saturating_sub(self.since_ms),
        })
    }
}

/// Holds the consoles of one actuation until it drops.
///
/// Released on EVERY exit path -- an error, a panic in the hook runner, a
/// timeout -- by being a guard rather than a pair of calls: a slot that outlives
/// its actuation would refuse the board forever, which is the failure this
/// exists to prevent, inverted.
pub struct ActuationGuard {
    ctx: Context,
    ids: Vec<i64>,
}

impl ActuationGuard {
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }
    pub fn phase(&self, phase: &str) {
        self.ctx.set_actuation_phase(&self.ids, phase);
    }
}

impl Drop for ActuationGuard {
    fn drop(&mut self) {
        let mut map = self.ctx.inner.actuations.lock().expect("actuations");
        for id in &self.ids {
            map.remove(id);
        }
    }
}

impl Context {
    pub fn open(config: Config, profiles: Arc<ProfileSet>, clock: SharedClock) -> Result<Self> {
        let data_dir = config.paths.data_dir.clone();
        let registry = Registry::open(&data_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                profiles,
                data_dir,
                clock,
                registry: Mutex::new(registry),
                stores: Mutex::new(HashMap::new()),
                holder: Mutex::new("mcp".to_string()),
                relay: Arc::new(conminer_core::peers::RelayQueue::new()),
                actuations: Mutex::new(HashMap::new()),
                outcomes: Mutex::new(HashMap::new()),
                overrides: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Claim every console of an actuation for its whole duration, or refuse.
    ///
    /// All-or-nothing: a target's consoles are claimed together, so two calls
    /// racing for overlapping console sets cannot each win half. The refusal
    /// names what is running and since when, so the caller can decide whether
    /// to wait or to look at the outcome that is about to land.
    pub fn begin_actuation(
        &self,
        tool: &'static str,
        action: &str,
        consoles: &[DeviceRow],
    ) -> Result<ActuationGuard> {
        let mut map = self.inner.actuations.lock().expect("actuations");
        for c in consoles {
            if let Some(f) = map.get(&c.id) {
                let mut detail = f.to_json(self.now());
                if let Some(o) = detail.as_object_mut() {
                    o.insert("device".into(), json!(c.display_name()));
                }
                return Err(ToolError::new(
                    ErrorCode::ActuationInFlight,
                    format!(
                        "{} is mid-{} {} (phase: {}) since {} ms ago; a second actuation on the \
                         same board while that runs is a race with the hardware",
                        c.display_name(),
                        f.tool,
                        f.action,
                        f.phase,
                        self.now().saturating_sub(f.since_ms)
                    ),
                )
                .with_hint(
                    "poll actuation_status(device) until in_flight is null; its `last` then \
                     carries the outcome, and the board is free",
                )
                .with_detail(detail));
            }
        }
        let f = InFlight {
            tool,
            action: action.to_string(),
            holder: self.holder(),
            since_ms: self.now(),
            phase: "hook".into(),
        };
        for c in consoles {
            map.insert(c.id, f.clone());
        }
        Ok(ActuationGuard {
            ctx: self.clone(),
            ids: consoles.iter().map(|c| c.id).collect(),
        })
    }

    /// The last overrides reading for this controller, if there is one.
    pub fn cached_overrides(&self, key: &str) -> Option<OverridesReading> {
        self.inner
            .overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub fn store_overrides(&self, key: &str, reading: OverridesReading) {
        self.inner
            .overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), reading);
    }

    /// Refuse a console-touching call while an actuation runs on the console.
    ///
    /// `run_command` and `send` put characters on a line whose board is being
    /// reset or powered off; the answer can only mislead (report #16: NO_PROMPT,
    /// zero bytes, filed as a console defect while the off press was held).
    pub fn require_no_actuation(&self, dev: &DeviceRow) -> Result<()> {
        let Some(f) = self.actuation_in_flight(dev.id) else {
            return Ok(());
        };
        let mut detail = f.to_json(self.now());
        if let Some(o) = detail.as_object_mut() {
            o.insert("device".into(), json!(dev.display_name()));
        }
        Err(ToolError::new(
            ErrorCode::ActuationInFlight,
            format!(
                "{} is mid-{} {} (phase: {}); the board is being actuated, so a command \
                 now cannot get a truthful answer",
                dev.display_name(),
                f.tool,
                f.action,
                f.phase
            ),
        )
        .with_hint(
            "poll actuation_status(device) until in_flight is null, then check console_state \
             before commanding",
        )
        .with_detail(detail))
    }

    /// Refuse a TRANSMITTING call while the board is in a flash/recovery mode.
    ///
    /// Once the capture layer authoritatively reports `away_in_edl` (the board's
    /// normal console re-enumerated away for EDL/fastboot/DFU/Firehose), there
    /// is no OS console to drive -- and pushing a newline or command bytes into
    /// a board being flashed is worse than useless. `run_command` and `send`
    /// check this before they transmit (report #23), so the answer is a clean
    /// "it is in recovery mode" instead of a misleading NO_PROMPT after a
    /// one-second UART probe.
    pub fn require_console_deliverable(&self, dev: &DeviceRow) -> Result<()> {
        if self.capture_health(dev) == conminer_core::live::CaptureState::AwayInEdl.as_str() {
            return Err(ToolError::new(
                ErrorCode::AwayInEdl,
                format!(
                    "{} is in a flash/recovery mode (capture_state=away_in_edl): its normal \
                     console has re-enumerated away, so there is nothing to command and no bytes \
                     will be transmitted",
                    dev.display_name()
                ),
            )
            .with_hint("poll console_state until capture_state leaves away_in_edl, then retry"));
        }
        Ok(())
    }

    /// Move an actuation to its next phase, for whoever asks meanwhile.
    pub fn set_actuation_phase(&self, ids: &[i64], phase: &str) {
        let mut map = self.inner.actuations.lock().expect("actuations");
        for id in ids {
            if let Some(f) = map.get_mut(id) {
                f.phase = phase.to_string();
            }
        }
    }

    /// Leave the finished actuation's answer where `actuation_status` reads it.
    pub fn record_actuation_outcome(&self, ids: &[i64], outcome: Value) {
        let mut map = self.inner.outcomes.lock().expect("outcomes");
        for id in ids {
            map.insert(*id, outcome.clone());
        }
    }

    /// The last actuation that finished on this console, if any this process.
    pub fn last_actuation_outcome(&self, id: i64) -> Option<Value> {
        self.inner
            .outcomes
            .lock()
            .expect("outcomes")
            .get(&id)
            .cloned()
    }

    /// What is actuating on this console right now, if anything.
    pub fn actuation_in_flight(&self, id: i64) -> Option<InFlight> {
        self.inner
            .actuations
            .lock()
            .expect("actuations")
            .get(&id)
            .cloned()
    }

    /// §P3. The reverse-call rendezvous for this node.
    pub fn relay(&self) -> &Arc<conminer_core::peers::RelayQueue> {
        &self.inner.relay
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn profiles(&self) -> &Arc<ProfileSet> {
        &self.inner.profiles
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.inner.data_dir
    }

    pub fn now(&self) -> i64 {
        self.inner.clock.now_wall_ms()
    }

    pub fn clock(&self) -> &SharedClock {
        &self.inner.clock
    }

    /// This node's name in the fleet (§P1).
    ///
    /// Sent as the origin of every proxied call, and prefixed onto lease holders
    /// so a `LEASE_HELD` names the node as well as the agent: "alpha/claude-x"
    /// is actionable, "claude-x" on a two-node fleet is a guessing game.
    pub fn node_name(&self) -> String {
        let cfg = self.config();
        if !cfg.peers.name.is_empty() {
            return cfg.peers.name.clone();
        }
        conminer_core::peers::Identity::load_or_create(&cfg.paths.data_dir, "", self.now())
            .map(|i| i.name)
            .unwrap_or_else(|_| "unnamed".into())
    }

    /// Who holds leases taken through this connection.
    ///
    /// §P1. A PROXIED CALL BRINGS ITS OWN IDENTITY. When one arrives, the holder
    /// is the ORIGIN's `<node>/<agent>`, not this process's default -- otherwise
    /// every agent in the fleet leases as "mcp", `LEASE_HELD` names nobody worth
    /// asking, and a steal cannot be attributed to anyone. The origin is
    /// per-call, so a proxied request never leaves its identity behind for the
    /// next local one.
    pub fn holder(&self) -> String {
        let origin = crate::tools::call_opts().origin;
        if !origin.is_empty() {
            return origin;
        }
        self.inner.holder.lock().expect("holder").clone()
    }

    pub fn set_holder(&self, who: impl Into<String>) {
        *self.inner.holder.lock().expect("holder") = who.into();
    }

    pub fn registry(&self) -> MutexGuard<'_, Registry> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Capture health for the response envelope, read LIVE, never from the
    /// snapshot row the call was resolved with (§W4).
    ///
    /// The envelope rides on every response and must reflect what an actuation
    /// in THIS call just published: report #7 shipped `commandable: true` in
    /// the very `boot_mode edl` response that re-enumerated the console away,
    /// because the envelope quoted `dev.capture_state` from the row resolved at
    /// entry. Handlers publish to the registry and this reads it back, so the
    /// two local row-patches that used to paper over it are gone.
    ///
    /// `try_lock`, deliberately: this is called from inside the envelope
    /// builder, and a handler may already hold the registry guard when it
    /// returns. A blocking lock would deadlock the one thread; the honest
    /// fallback when the registry is momentarily busy is the row we were handed,
    /// which is never staler than the old behaviour. Contention here is a
    /// microsecond registry write, so the fast path is taken virtually always.
    pub fn capture_health(&self, dev: &DeviceRow) -> String {
        let live = match self.inner.registry.try_lock() {
            Ok(reg) => reg.capture_state(dev.id).ok().flatten(),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(e)) => {
                e.into_inner().capture_state(dev.id).ok().flatten()
            }
        };
        live.or_else(|| dev.capture_state.clone())
            .unwrap_or_else(|| dev.state.clone())
    }

    /// Resolve a selector to exactly one device (§3.1).
    pub fn device(&self, selector: &str) -> Result<DeviceRow> {
        self.registry().resolve(selector)
    }

    /// Resolve a selector that may legitimately match several devices.
    pub fn device_group(&self, selector: &str) -> Result<Vec<DeviceRow>> {
        self.registry().resolve_group(selector)
    }

    /// The one device, when only one exists — so a single-console lab does not
    /// have to name it on every call.
    pub fn device_or_only(&self, selector: Option<&str>) -> Result<DeviceRow> {
        match selector {
            Some(s) => self.device(s),
            None => {
                let all = self.registry().all_devices()?;
                match all.len() {
                    1 => Ok(all.into_iter().next().unwrap()),
                    0 => Err(
                        ToolError::new(ErrorCode::UnknownDevice, "no devices are known yet")
                            .with_hint("plug one in, or call ingest_file with a path"),
                    ),
                    n => Err(ToolError::new(
                        ErrorCode::AmbiguousDevice,
                        format!("{n} devices exist; pass `device`"),
                    )
                    .with_detail(json!({
                        "candidates": all.iter().map(|d| d.display_name()).collect::<Vec<_>>()
                    }))),
                }
            }
        }
    }

    pub fn store(&self, dev: &DeviceRow) -> Result<Arc<Mutex<DeviceStore>>> {
        let mut cache = self.inner.stores.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = cache.get(&dev.id) {
            return Ok(s.clone());
        }
        let path = self.inner.data_dir.join(&dev.db_file);
        let fts = self.inner.config.fts_for(dev.display_name());
        let store = Arc::new(Mutex::new(DeviceStore::open(&path, &dev.canonical, fts)?));
        cache.insert(dev.id, store.clone());
        Ok(store)
    }

    /// Run a closure with the device's store locked.
    /// Run `f` against this device's store.
    ///
    /// §P1. A REMOTE DEVICE HAS NO STORE HERE, and asking for one is a bug
    /// worth failing loudly on rather than papering over. The owner runs the
    /// only miner for its boards; opening a local store for one would create an
    /// empty database, take its writer lock, and answer every later question
    /// about that board with silence -- which is exactly what happened on the
    /// first two-host bring-up, where mcpd stopped serving within seconds
    /// because it had opened stores for a peer's thirteen consoles.
    ///
    /// Every read for a remote device is federated before it reaches here, so
    /// this is a guard on a path that should already be impossible.
    pub fn with_store<T>(
        &self,
        dev: &DeviceRow,
        f: impl FnOnce(&mut DeviceStore) -> Result<T>,
    ) -> Result<T> {
        if dev.kind.is_remote() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!(
                    "{} is owned by node {:?}: there is no local store to read",
                    dev.display_name(),
                    dev.node.clone().unwrap_or_default()
                ),
            )
            .with_hint("this call should have been federated to the owning node"));
        }
        let s = self.store(dev)?;
        let mut g = s.lock().unwrap_or_else(|e| e.into_inner());
        // THE WRITER IS ANOTHER PROCESS. minerd appends to this database
        // continuously; this handle is cached for the life of mcpd and its
        // append-only counters were last read when it was opened. So "where is
        // the head" answered from memory drifts further from the truth the
        // longer the console talks -- measured on the bench: a watch created
        // `from: "now"` was stamped at offset 203777 while capture had already
        // reached 221403, and it duly replayed an old banner as if it were new.
        //
        // One indexed meta read per call, against tools that open sockets and
        // wait on hardware. Correctness is worth more than the microsecond.
        g.reload_counters()?;
        f(&mut g)
    }

    /// Drop a cached handle, e.g. after a device is forgotten.
    pub fn forget_store(&self, device_id: i64) {
        self.inner
            .stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&device_id);
    }

    /// The §8.4 freshness envelope.
    ///
    /// Every read response carries it so that an agent which just power-cycled a
    /// board and sees an unchanged `boot_id`, or a `last_rx_ts` predating its
    /// `mark`, *knows* it is looking at stale data. The answer describes its own
    /// currency instead of relying on the agent to reason about it.
    pub fn freshness(&self, dev: &DeviceRow) -> Result<Value> {
        let now = self.now();
        // Resolved live, and BEFORE the store lock below, so the envelope never
        // quotes the capture health the row was resolved with (§W4).
        let capture_health = self.capture_health(dev);
        self.with_store(dev, |store| {
            let boot = store.latest_boot()?;
            let last = store.recent_lines(1)?.into_iter().next();
            let last_rx = last.as_ref().map(|l| l.ts_wall);
            let bytes_this_boot = boot.as_ref().map(|b| b.bytes).unwrap_or(0);
            // TRIMMED, because this envelope rides on EVERY response.
            //
            // Measured at 250-400 bytes on calls whose own answer was smaller
            // than the envelope wrapping it. `server_now` and `last_rx_ts` are
            // two timestamps whose only use was computing `idle_ms`, which is
            // already here; `boot_seq` tracks `boot_id`; `line` changes about
            // once a year and is one `console_state` away. What remains is what
            // an agent actually branches on: how stale, which epoch, who opened
            // it, is capture recording, and where to resume.
            Ok(json!({
                "idle_ms": last_rx.map(|t| (now - t).max(0)),
                "boot_id": boot.as_ref().map(|b| b.id),
                "boot_opened_by": boot.as_ref().map(|b| b.opened_by.clone()),
                "bytes_this_boot": bytes_this_boot,
                // Live attestation is minerd's to provide (§8.4). Without it the
                // honest answer is "unknown", never "listening".
                // The column minerd owns. Falls back to `state` only for a row
                // upgraded before capture health had a column of its own and
                // never re-observed since.
                "capture_state": capture_health,
                "cursor": store.head_cursor().encode(),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conminer_core::store::IdentityKind;

    fn ctx() -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let c = Context::open(
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        (dir, c)
    }

    #[test]
    fn device_or_only_needs_no_selector_with_one_device() {
        let (_d, c) = ctx();
        assert_eq!(
            c.device_or_only(None).unwrap_err().code,
            ErrorCode::UnknownDevice
        );
        c.registry()
            .upsert_device("usb-a", None, IdentityKind::ById, None, 1)
            .unwrap();
        assert_eq!(c.device_or_only(None).unwrap().canonical, "usb-a");

        c.registry()
            .upsert_device("usb-b", None, IdentityKind::ById, None, 1)
            .unwrap();
        let err = c.device_or_only(None).unwrap_err();
        assert_eq!(err.code, ErrorCode::AmbiguousDevice);
        assert_eq!(
            err.detail.unwrap()["candidates"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn store_handles_are_cached_per_device() {
        let (_d, c) = ctx();
        let dev = c
            .registry()
            .upsert_device("usb-a", None, IdentityKind::ById, None, 1)
            .unwrap();
        let a = c.store(&dev).unwrap();
        let b = c.store(&dev).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn the_freshness_envelope_is_present_even_for_a_silent_device() {
        let (_d, c) = ctx();
        let dev = c
            .registry()
            .upsert_device("usb-a", None, IdentityKind::ById, None, 1)
            .unwrap();
        let f = c.freshness(&dev).unwrap();
        // `server_now`/`last_rx_ts` were removed: two timestamps whose only use
        // was computing `idle_ms`, on an envelope that rides every response.
        // A device that has never spoken has no idle measurement at all, which
        // is the honest answer and distinct from "idle for 0 ms".
        assert!(f["idle_ms"].is_null(), "no bytes have ever arrived");
        assert_eq!(f["bytes_this_boot"], 0);
        assert_eq!(
            f["capture_state"], "unknown",
            "without live attestation the honest answer is 'unknown', not 'listening'"
        );
        assert!(f["cursor"].as_str().unwrap().contains(':'));
    }
}
