//! `dashd` — the human dashboard (§17).
//!
//! Everything else in conminer answers to an agent. This is the surface for the
//! person standing at the bench, and it has three jobs: show which consoles are
//! actually plugged in *right now*, hand over the connection string for each,
//! and let someone watch a console live and type into it when they need to.
//!
//! ## Connection budget
//!
//! ser2net fans one UART out to several TCP clients, but the budget is small
//! (eight by default, and minerd permanently holds one of them for capture). So
//! the dashboard opens **one** connection per device and fans it out to every
//! browser over WebSocket. Ten people watching one console costs one ser2net
//! client, not ten, and the remaining slots stay free for minicom, labgrid and
//! whatever else the lab attaches.
//!
//! ## Transmit
//!
//! Bytes typed in the browser go straight down that TCP connection to ser2net.
//! They are deliberately *not* routed through the miner, so:
//!
//!   * they cost nothing in latency and work even if minerd is wedged;
//!   * an agent reading the mined stream sees only the console's echo, with no
//!     record of who caused it.
//!
//! Nothing here arbitrates access: two browsers, or a browser and an agent, can
//! interleave keystrokes into the same UART. That is the configured behaviour,
//! not an oversight, so the UI states plainly who else is attached rather than
//! pretending the console is exclusively yours.

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{sse, IntoResponse, Response, Sse};
use axum::routing::get;
use axum::Router;
use conminer_core::config::Config;
use conminer_core::store::Registry;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

/// How long a power reading may be served before it is treated as unknown.
///
/// The sweep runs every 5s and a controller query costs ~1-2s, so a healthy
/// bench refreshes every console well inside this. Anything older means the
/// prober is not running, and the honest report is then "I do not know" -- not
/// the last thing that happened to be true.
const POWER_MAX_AGE_MS: i64 = 30_000;

/// How long the lease taken for one dashboard button press may live.
///
/// A backstop only: the press releases explicitly when it finishes. It has to
/// comfortably outlast the slowest action, because a lease expiring mid-press
/// would strand the action instead of the lease -- measured worst case is a
/// `power on` at 71.9s, and a `cycle` is an off plus an on.
const PRESS_LEASE_TTL_S: u64 = 300;

/// Group consoles by the controller INSTANCE that would answer for them.
///
/// Public and standalone so the rule is testable on its own, because it is the
/// rule that broke: the sweep keyed on `controller`, the controller PROFILE
/// name, and this bench runs two Bantams both named "bantam". All ten consoles
/// across BOTH boards landed in one group, one IQ10 console was probed, and its
/// "off" was published for the NordAU -- which was powered on. A board reported
/// another board's power, and the page showed it as fact.
///
/// The key is therefore the resolved controller tty. Where that cannot be
/// resolved the console groups ALONE, under its own name: probing per console is
/// slower and correct, while grouping under any shared fallback re-creates
/// exactly the bug.
pub fn group_by_controller(
    devices: &[DashDevice],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for d in devices {
        if d.is_controller || d.is_file || !d.has_power_hook {
            continue;
        }
        let key = d
            .controller_port
            .clone()
            .or_else(|| d.target.clone())
            .unwrap_or_else(|| d.canonical.clone());
        groups.entry(key).or_default().push(d.canonical.clone());
    }
    groups
}

/// Is this a thing on the bench, or only a row in the registry?
///
/// The dashboard answers "what is plugged into this rig and what is it doing".
/// The registry legitimately holds entries that are neither: a mined log file, a
/// port config excludes, an internal sibling store. Those belong in
/// `list_devices` -- an agent's tools work identically on all of them -- and not
/// on a page where every row implies something you can walk over to and power.
///
/// Three exclusions, each for its own reason:
///
///  * `is_file` -- `ingest_file` mints a device keyed on the path, so mining a
///    log adds a permanent row for something that was never plugged in.
///  * `#` in the canonical name -- an internal sibling store. `snapshot_dmesg`
///    opens `<port>#dmesg` so it can mine without taking the live console's
///    writer lock; that is an implementation detail, and it rendered as a port.
///  * excluded and not a controller -- config or a controller profile has
///    claimed this port, so it deliberately has no console. Controllers stay:
///    the page draws them as the board's control panel, which is what they are.
fn belongs_on_the_dashboard(d: &DashDevice, now_ms: i64, hook_press_ms: i64) -> bool {
    // §P1. A remote row BELONGS here -- showing the fleet is the feature. It is
    // still filtered by the same rules as a local one, so a peer's controller
    // does not become a console on our page either.
    let bench_hardware =
        !d.is_file && !d.canonical.contains('#') && (!d.ignored || d.is_controller);
    bench_hardware && on_the_bus_now(d, now_ms, hook_press_ms)
}

/// Is this hardware on the bus RIGHT NOW, or on its way back?
///
/// Hardware moves on and off this bench, and a page that accumulates every
/// board it has ever seen stops describing the bench: alpha was drawing 16
/// consoles and two controller panels with ONE cable plugged in. So absence
/// removes the row. The registry still keeps it, with its nickname, port
/// assignment and capture history, and `list_devices` still answers for it --
/// an agent reaching for history must not be told the board never existed.
///
/// The hard part is not hiding things; it is telling "gone" from "gone and
/// coming back", because a board re-enumerates constantly in normal use. Three
/// tests, strongest first, and only the last one is a clock:
///
///  1. THE KERNEL ENUMERATES IT. Real-time truth, from discoveryd's udev +1 Hz
///     scan of `/dev/serial/by-id`. Nothing beats it and nothing else is
///     consulted when it holds.
///
///  2. THE CAPTURE LAYER SAYS IT IS IN A FLASH MODE. During EDL/fastboot/DFU the
///     board is emphatically still plugged in: its UART has re-enumerated as a
///     download gadget, which minerd sees on the board's own USB ports and
///     publishes as `away_in_edl`. NO TIMER APPLIES HERE, so a twenty-minute
///     Firehose flash never makes the board blink out of the rack. minerd is the
///     single authority on recovery mode (it owns the sysfs signature check);
///     dashd reads its answer rather than walking sysfs itself, because a second
///     notion of the same fact is free to disagree with the first.
///
///  3. IT HAS BEEN GONE FOR LESS TIME THAN A HOOK PRESS TAKES. The one case
///     where the physical signal genuinely vanishes is a UART bridge powered BY
///     the board, across a power cycle: the chip loses power, the port empties,
///     and for those seconds it is indistinguishable from a pulled cable. That
///     window is bounded by the power hook's own settle, which is why this
///     reuses `discovery.removal_grace_ms` rather than inventing a second
///     number -- it is already chosen so that "a hook press is shorter", and it
///     is already what the capture supervisor uses to hold a console open
///     across the same event. One definition of transient absence, so the page
///     and the capture loop can never disagree about it.
///
/// Local rows only for (3): a peer's `last_seen` is refreshed by inventory sync
/// whatever the hardware is doing, so the clock there measures our conversation
/// with the owner rather than their cable. Their `state` is the owner's answer
/// and is the only thing worth trusting.
fn on_the_bus_now(d: &DashDevice, now_ms: i64, hook_press_ms: i64) -> bool {
    if d.present {
        return true;
    }
    if d.capture_state.as_deref() == Some(conminer_core::live::CaptureState::AwayInEdl.as_str()) {
        return true;
    }
    d.node.is_none() && now_ms.saturating_sub(d.last_seen) < hook_press_ms
}

/// An absent row is the exception, so absence must be stated explicitly.
///
/// Both `#[serde(default)]` and the hand-written `Default` below have to agree
/// on this: `bool::default()` is `false`, which would have made every
/// deserialized-or-defaulted device read as unplugged.
fn present_by_default() -> bool {
    true
}

/// A console as the dashboard presents it.
///
/// `Deserialize` so a test can read the API's own output back and hand it to the
/// real grouping function, rather than re-implementing the grouping rule in the
/// assertion and proving only that two copies of it agree.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct DashDevice {
    pub device: String,
    pub canonical: String,
    pub nickname: Option<String>,
    pub target: Option<String>,
    /// The physical USB adapter this console belongs to.
    ///
    /// A multi-port adapter (the IQ10's FT4232 is four UARTs on one chip)
    /// presents as four unrelated-looking by-id names that differ only in their
    /// `-ifNN` suffix. Grouping by what remains is how the page shows one board
    /// as one board.
    pub adapter: Option<String>,
    pub port: Option<u16>,
    /// Boot modes this device accepts, for the dashboard's controls.
    pub boot_modes: Vec<String>,
    pub has_power_hook: bool,
    /// Is this cable actually plugged in right now?
    ///
    /// The page KEEPS an absent board and marks it, rather than dropping it. A
    /// row that silently disappears is its own bug report: an operator looking
    /// for a board they expect cannot tell "unplugged" from "conminer lost it",
    /// and the nickname, port assignment and history are all still here to show.
    ///
    /// `#[serde(default = "yes")]` because the field is younger than the peers
    /// that send us their inventory: a row from an older node arrives without
    /// it, and defaulting to absent would grey out a whole healthy rack.
    #[serde(default = "present_by_default")]
    pub present: bool,
    /// When `power` was last measured (wall ms), so a caller can tell a fresh
    /// reading from one taken before its own action. `None` = never measured.
    pub power_sensed_at: Option<i64>,
    /// "on", "off", or None when this controller cannot measure it.
    ///
    /// None renders as "unknown" and must NEVER render as "off": a
    /// silent-but-running board would then look dead, which is the ambiguity
    /// this whole field exists to remove.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<String>,
    /// Which controller profile drives this console, so the page can say where
    /// its buttons came from.
    pub controller: Option<String>,
    /// The controller INSTANCE this console's hooks resolve to -- the actual tty.
    ///
    /// Distinct from `controller`, which is only the profile NAME, and the
    /// distinction is not academic: this bench has two Bantams, so every console
    /// on both boards carries `controller: "bantam"`. Grouping the power sweep
    /// on that name collapsed both boards into one group, probed a single IQ10
    /// console, and published its answer for the NordAU too -- which is why a
    /// board that was plainly powered ON read "off" on the page. Power is a
    /// property of the CONTROLLER INSTANCE, so that is what the answer may be
    /// shared across, and nothing wider.
    pub controller_port: Option<String>,
    /// The controller ROW's own name and labels, when it is one of ours.
    ///
    /// The controller is not a card on this page (it captures nothing, so it is
    /// filtered out with the other portless rows) -- but it is still a device in
    /// the registry, and an operator naming "the one on the left" means the
    /// CONTROLLER. So its panel carries its own name, edited against its own
    /// selector; binding those controls to the console the panel was drawn from
    /// would put two editors on one name, one of them mislabelled.
    ///
    /// Empty for a peer's board: its controller row lives on the owning node and
    /// is never imported (a row with no endpoint has nothing to export), so
    /// there is no local row a label could attach to.
    // Omitted from the wire when empty, and defaulted when absent: this struct
    // is deserialised too (the sweep reads back its own payload), and a field
    // that only ever serialises turns "nothing to say" into a parse error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_label: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub controller_tags: std::collections::BTreeMap<String, String>,
    /// True when this is a file-ingest pseudo-device rather than hardware.
    ///
    /// `ingest_file` without a device creates one keyed on the path, so a mined
    /// log shows up in `list_devices` alongside real consoles. That is right for
    /// an agent — the tools work identically on both — and wrong for a bench
    /// dashboard, where a row that cannot be plugged in or powered is noise.
    pub is_file: bool,
    /// True when this *is* the controller rather than a console it powers.
    ///
    /// The page renders these as a control panel instead of a dead "ignored"
    /// row: a board controller is not a console that failed to open, it is the
    /// thing that turns the board on.
    pub is_controller: bool,
    /// §P1. The node that OWNS this device, and where it lives. `None` for a
    /// local one.
    ///
    /// Both, because a name alone is not enough on a page whose job is telling
    /// racks apart: "board-b on nodeb" leaves an operator guessing which host
    /// that is, and "192.168.10.10" leaves them guessing which rack.
    pub node: Option<String>,
    pub node_host: Option<String>,
    pub line: String,
    /// PRESENCE, owned by discovery: discovered / gone / ignored / unknown.
    /// Never capture health -- one column held both once, and each writer
    /// erased the other.
    pub state: String,
    /// CAPTURE HEALTH, owned by minerd: is the port open and being read.
    /// `None` when nothing has attested yet, which is "I do not know" and must
    /// never be rendered as "silent".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_state: Option<String>,
    pub ignored: bool,
    /// What the console last showed itself to be (§3.1), so a row is
    /// recognisable as "the IQ10 AP console" and not just a port number.
    pub observed: serde_json::Value,
    pub tags: std::collections::BTreeMap<String, String>,
    pub last_seen: i64,
    /// Browsers currently attached through this server.
    pub viewers: usize,
    /// Whether this server holds a TCP connection to the port.
    pub attached: bool,
}

/// Hand-written, NOT derived, for the sake of one field.
///
/// `bool::default()` is `false`, so a derived `Default` would declare every
/// device it builds unplugged -- and `..Default::default()` is how the tests and
/// one production call site construct these. A device nobody said anything
/// about is a device that is THERE; absence is a claim, and a claim has to be
/// made explicitly.
///
/// Spelled out field by field on purpose. Adding a field to `DashDevice` now
/// fails to compile until someone decides what its default means, which is
/// exactly the check that would have caught this.
impl Default for DashDevice {
    fn default() -> Self {
        Self {
            present: true,
            device: String::new(),
            canonical: String::new(),
            nickname: None,
            target: None,
            adapter: None,
            port: None,
            boot_modes: Vec::new(),
            has_power_hook: false,
            power_sensed_at: None,
            power: None,
            controller: None,
            controller_port: None,
            controller_label: None,
            controller_tags: Default::default(),
            is_file: false,
            is_controller: false,
            node: None,
            node_host: None,
            line: String::new(),
            state: String::new(),
            capture_state: None,
            ignored: false,
            observed: serde_json::Value::Null,
            tags: Default::default(),
            last_seen: 0,
            viewers: 0,
            attached: false,
        }
    }
}

/// The set of consoles, plus a revision that changes whenever the set does.
/// A peer as the page needs it: who, where, and how fresh the belief is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DashPeer {
    pub node: String,
    pub host: Option<String>,
    pub dash_url: Option<String>,
    pub age_s: i64,
    pub ttl_s: u64,
    /// Advertising recently.
    pub live: bool,
    /// Answering calls. A node can do the first and not the second, and the
    /// difference is the difference between a switched-off host and a crashed
    /// mcpd.
    pub answering: bool,
    pub devices: usize,
    /// Which build that node runs, and whether it matches ours. A fleet that
    /// proxies calls between nodes has to make skew visible, not merely
    /// discoverable.
    pub build: Option<String>,
    pub build_matches: Option<bool>,
    /// §P3. It cannot be dialled from here, but it is asking US for work, so its
    /// boards are fully driveable over the reverse channel. Without this a node
    /// that works perfectly renders as one whose mcpd has crashed.
    pub reverse: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// THIS node, so the local bench is a rack unit like any other rather than
    /// an unlabelled pile above the peers. Name, address and build: the same
    /// three facts a peer's header shows, from the same place.
    pub node: String,
    pub node_host: Option<String>,
    pub build: String,
    pub devices: Vec<DashDevice>,
    /// §P1. The peers this node can see, for the rack headers and their
    /// liveness pills.
    pub peers: Vec<DashPeer>,
    pub revision: u64,
    pub server_now: i64,
    pub allow_tx: bool,
    pub max_connections: u16,
}

/// One device's shared connection to ser2net.
struct Attachment {
    /// Console output, fanned out to every attached browser.
    rx: broadcast::Sender<Vec<u8>>,
    /// Browser keystrokes, funnelled into the single TCP writer.
    tx: mpsc::Sender<Vec<u8>>,
    /// Recent output, so a browser that joins mid-boot sees context instead of
    /// an empty pane until the board next prints.
    scrollback: Arc<Mutex<Vec<u8>>>,
    viewers: Arc<Mutex<usize>>,
}

#[derive(Clone)]
pub struct Dash {
    config: Arc<Config>,
    data_dir: std::path::PathBuf,
    snapshot: Arc<Mutex<Snapshot>>,
    changed: broadcast::Sender<u64>,
    attachments: Arc<Mutex<HashMap<String, Arc<Attachment>>>>,
    /// Last known power state per device, refreshed on a slow timer.
    ///
    /// Cached rather than probed per request because asking a controller costs
    /// ~1-2s: an indicator is only useful if the page stays instant, and a
    /// slightly stale light is far better than a slow one.
    power_cache: Arc<Mutex<HashMap<String, Option<String>>>>,
    /// When each cached power reading was taken (wall ms).
    ///
    /// A cached lamp is fine for a human, who sees it settle a moment later. It
    /// is NOT fine for automation, which reads the field the instant after it
    /// acts and cannot tell a fresh "on" from the one left over from before its
    /// own power-off. Publishing the age turns an unknowable race into a
    /// checkable condition: ignore any reading older than the action.
    power_sensed_at: Arc<Mutex<HashMap<String, i64>>>,
    /// Serialises the check-then-dial in `attach`.
    ///
    /// Without it two browsers opening the same console at the same instant both
    /// miss the map, both dial, and the console pays two of its eight ser2net
    /// client slots for one dashboard — the precise thing the fan-out exists to
    /// prevent. Attaching is rare enough that one global lock costs nothing.
    dialing: Arc<tokio::sync::Mutex<()>>,
}

impl Dash {
    pub fn new(config: Config, data_dir: std::path::PathBuf) -> Self {
        let (changed, _) = broadcast::channel(64);
        let allow_tx = config.dashboard.allow_tx;
        let max_connections = config.ser2net.max_connections;
        Self {
            config: Arc::new(config),
            data_dir,
            snapshot: Arc::new(Mutex::new(Snapshot {
                node: String::new(),
                node_host: None,
                build: conminer_core::build_id().to_string(),
                devices: Vec::new(),
                peers: Vec::new(),
                revision: 0,
                server_now: 0,
                allow_tx,
                max_connections,
            })),
            changed,
            attachments: Arc::new(Mutex::new(HashMap::new())),
            power_cache: Arc::new(Mutex::new(HashMap::new())),
            power_sensed_at: Arc::new(Mutex::new(HashMap::new())),
            dialing: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The cached power reading, but only while it is still worth believing.
    ///
    /// A reading older than [`POWER_MAX_AGE_MS`] is reported as unknown rather
    /// than served as fact. Without this, the cache has no upper age at all: a
    /// wedged prober, a restarted mcpd or a controller that stopped answering
    /// all leave the last value sitting on the page looking perfectly current,
    /// and the failure is invisible precisely because a stale "on" and a live
    /// "on" render identically.
    pub fn fresh_power(&self, canonical: &str) -> Option<String> {
        let sensed = *self
            .power_sensed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(canonical)?;
        if now_ms().saturating_sub(sensed) > POWER_MAX_AGE_MS {
            return None;
        }
        self.power_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(canonical)
            .cloned()
            .flatten()
    }

    /// Forget what we knew about these consoles' power.
    ///
    /// Called the moment an actuation is dispatched. The alternative is a window
    /// -- up to a whole poll interval -- in which the page confidently shows the
    /// state from BEFORE the button was pressed, which reads exactly like the
    /// button not working and has sent people to check cabling more than once.
    /// Unknown for a second, then the truth.
    pub fn invalidate_power(&self, canonicals: &[String]) {
        let mut cache = self.power_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut at = self
            .power_sensed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for c in canonicals {
            cache.remove(c);
            at.remove(c);
        }
    }

    /// Every console that shares a controller instance with this one.
    ///
    /// The unit of power is the CONTROLLER, so invalidating one console's
    /// reading while leaving its five siblings showing the old value would just
    /// move the stale answer one row over.
    pub fn consoles_sharing_controller(&self, canonical: &str) -> Vec<String> {
        let devices = {
            self.snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .devices
                .clone()
        };
        self.consoles_sharing_controller_in(canonical, &devices)
    }

    /// The same, against a given device list, so the rule can be tested against
    /// the sweep's without standing up a whole snapshot.
    pub fn consoles_sharing_controller_in(
        &self,
        canonical: &str,
        devices: &[DashDevice],
    ) -> Vec<String> {
        let Some(me) = devices.iter().find(|d| d.canonical == canonical) else {
            return vec![canonical.to_string()];
        };
        let Some(port) = me.controller_port.clone() else {
            return vec![canonical.to_string()];
        };
        // THE SAME MEMBERSHIP RULE AS THE SWEEP, which is the point: a
        // controller carries its own tty as its controller_port, so matching on
        // that alone pulled the CONTROLLER ROW into the board's group. The sweep
        // skips controllers, so the value could only ever arrive from a
        // post-action fan-out and then expire -- measured on the rig, the IQ10's
        // Bantam row showed "off" right after a press and was back to unknown
        // 35s later. A row that answers differently depending on how recently
        // somebody pressed a button is not a power indicator.
        devices
            .iter()
            .filter(|d| !d.is_controller && !d.is_file)
            .filter(|d| d.controller_port.as_deref() == Some(port.as_str()))
            .map(|d| d.canonical.clone())
            .collect()
    }

    /// Record a power reading for these consoles, taken at `sensed_at`.
    ///
    /// The time is a parameter rather than read from the clock so that "how old
    /// may a reading be" is a property this type can be tested on, instead of
    /// something only a real bench and a stopwatch could demonstrate.
    pub fn publish_power(&self, canonicals: &[String], state: Option<String>, sensed_at: i64) {
        {
            let mut cache = self.power_cache.lock().unwrap_or_else(|e| e.into_inner());
            for c in canonicals {
                cache.insert(c.clone(), state.clone());
            }
        }
        let mut at = self
            .power_sensed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for c in canonicals {
            at.insert(c.clone(), sensed_at);
        }
    }

    /// Stamp these devices as having been sensed just now.
    pub fn note_power_sensed(&self, canonicals: &[String]) {
        let now = now_ms();
        let mut at = self
            .power_sensed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for c in canonicals {
            at.insert(c.clone(), now);
        }
    }

    /// Re-read the registry. Returns true when the device set actually changed,
    /// which is what makes the browser update on a replug rather than on a
    /// timer.
    /// Ask this board's controller whether it is powered on.
    ///
    /// `None` means the controller cannot answer -- which must render as
    /// "unknown", never as "off". A silent-but-running board shown as dead is
    /// the exact misreading this indicator exists to prevent.
    /// Ask mcpd whether this board is powered on.
    ///
    /// Proxied rather than probed directly, and that is deliberate: dashd has no
    /// /dev access and no controller hook in its image -- only mcpd does. Running
    /// the hook here silently failed for EVERY board, so two Bantams that can
    /// measure their power perfectly well both rendered "cannot measure". It
    /// also keeps hardware access in exactly one service, and keeps two
    /// processes off a single-session controller.
    ///
    /// `None` means unknown, which must render as unknown and never as "off".
    pub async fn probe_power(&self, canonical: &str) -> Option<String> {
        let base = self.config.dashboard.mcp_url.clone();
        let res = call_mcp(&base, "diagnose", json!({"device": canonical}))
            .await
            .ok()?;
        // call_mcp returns the whole JSON-RPC envelope, so the tool's own
        // payload lives under result.structuredContent. Reading `power` off the
        // top level silently yielded None for every board, which rendered as
        // "cannot measure" on two controllers that measure it perfectly well.
        match res
            .get("result")
            .and_then(|r| r.get("structuredContent"))
            .and_then(|c| c.get("power"))
            .and_then(Value::as_str)
        {
            Some("on") => Some("on".into()),
            Some("off") => Some("off".into()),
            _ => None,
        }
    }

    pub fn refresh(&self) -> Result<bool> {
        let reg = Registry::open(&self.data_dir)?;
        let rows = reg.all_devices()?;
        let attachments = self.attachments.lock().unwrap_or_else(|e| e.into_inner());
        // Includes the controllers discovery never opens: resolving a
        // controller's tty needs to see them.
        // Carry by-path too: two identical controllers on one bench can only be
        // told apart by USB topology, and binding the wrong one would put a
        // board's power buttons on another board.
        //
        // AND ONLY WHAT IS PLUGGED IN. This was every row the registry had ever
        // recorded, called `present`. The guard against binding the wrong
        // controller INSTANCE was sound; there was none against binding one that
        // is not there at all.
        let present = conminer_core::store::registry::present_on_this_host(&rows);

        let devices: Vec<DashDevice> = rows
            .iter()
            .map(|d| {
                let att = attachments.get(&d.canonical);
                let controller_port = match &d.node {
                    // A REMOTE ROW NEVER RESOLVES AGAINST LOCAL HARDWARE.
                    //
                    // Only the owner can say which controller drives its board.
                    // Falling back to our own profiles when it does not say
                    // looks harmless and is not: measured on alpha straight
                    // after this shipped, bravo's CMSIS-DAP board was attributed
                    // to a Bantam plugged into ALPHA, and the power sweep then
                    // published that Bantam's "on" as the state of a board on
                    // another host. One board reporting another's power is the
                    // exact failure the per-instance grouping exists to prevent;
                    // across hosts it is worse, because nothing on this bench
                    // can contradict it.
                    Some(owner) => remote_str(d, "controller_port")
                        .flatten()
                        .map(|p| format!("peer:{owner}/{p}")),
                    None => self.config.controller_port_for(
                        &d.canonical,
                        d.by_path.as_deref(),
                        present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                    ),
                };
                // The controller's OWN registry row, for its own name/labels.
                let controller_row = controller_port
                    .as_deref()
                    .filter(|_| d.node.is_none())
                    .and_then(|port| rows.iter().find(|c| c.canonical == port));
                DashDevice {
                    device: d.display_name().to_string(),
                    canonical: d.canonical.clone(),
                    // The cable, not the config. Kept on the page and marked,
                    // rather than dropped: see `DashDevice::present`.
                    present: d.is_present(),
                    node: d.node.clone(),
                    node_host: d.node_host.clone(),
                    nickname: d.nickname.clone(),
                    target: d.target.clone(),
                    adapter: topology_group(d.by_path.as_deref())
                        .or_else(|| adapter_of(&d.canonical)),
                    port: d.ser2net_port,
                    // Only modes that could actually be selected: see
                    // `boot_modes_for_at`. Offering a controller's menu on a
                    // host where that controller is absent is the same lie as
                    // naming the controller itself.
                    // §P2. A REMOTE BOARD'S CONTROLS COME FROM ITS OWNER.
                    //
                    // Resolving them here asks whether the controller is plugged
                    // into THIS host, and for somebody else's board the answer is
                    // always no -- which is why every peer's board rendered with
                    // no controller and no power buttons, on a bench where the
                    // hardware is perfectly driveable from the node that owns it.
                    boot_modes: remote_str_list(d, "boot_modes").unwrap_or_else(|| {
                        self.config.boot_modes_for_at(
                            d.display_name(),
                            &d.canonical,
                            d.by_path.as_deref(),
                            present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                        )
                    }),
                    // Resolved, not merely configured: a console gets controls
                    // because a controller profile claims it, which is what lets
                    // a new board arrive with working buttons and no config.
                    has_power_hook: remote_flag(d, "has_power_hook").unwrap_or_else(|| {
                        self.config
                            .power_hook_for_at(
                                d.display_name(),
                                &d.canonical,
                                d.by_path.as_deref(),
                                present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                            )
                            .is_some()
                    }),
                    // Served from a slow background cache, never probed inline:
                    // a controller query takes ~1-2s and the device list must
                    // stay instant.
                    //
                    // EXPIRED READINGS BECOME UNKNOWN. A cache with no upper age
                    // is the second way this field lies: if the prober wedges or
                    // mcpd goes down, the last value sits there looking current
                    // forever, and somebody reads "on" off a board that was
                    // switched off ten minutes ago. Unknown is a worse-looking
                    // answer and a truthful one; the age is published alongside
                    // so a caller can judge for itself.
                    power: self.fresh_power(&d.canonical),
                    power_sensed_at: self
                        .power_sensed_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&d.canonical)
                        .copied(),
                    // The controller that can actually drive this board, not
                    // merely the profile whose glob claims the name.
                    controller: remote_str(d, "controller").unwrap_or_else(|| {
                        self.config
                            .controller_for_at(
                                &d.canonical,
                                d.by_path.as_deref(),
                                present.iter().map(|(n, p)| (n.as_str(), p.as_deref())),
                            )
                            .map(|c| c.name.clone())
                    }),
                    // §P3. A REMOTE BOARD'S CONTROLLER INSTANCE IS THE OWNER'S,
                    // and namespaced by owner so it can never be confused with a
                    // local controller that happens to sit at the same tty path.
                    // This is what makes the power sweep cost one probe per peer
                    // BOARD rather than one per peer console -- the same
                    // correction that stopped one board reporting another's
                    // power locally, applied across the fleet.
                    controller_port: controller_port.clone(),
                    controller_label: controller_row.and_then(|c| c.nickname.clone()),
                    controller_tags: controller_row.map(|c| c.tags.clone()).unwrap_or_default(),
                    is_controller: self.config.controller_profile_of(&d.canonical).is_some(),
                    is_file: d.canonical.starts_with("file:"),
                    line: d.line.summary(),
                    state: d.state.clone(),
                    capture_state: d.capture_state.clone(),
                    ignored: d.ignored,
                    observed: serde_json::to_value(&d.observed).unwrap_or(json!({})),
                    tags: d.tags.clone(),
                    last_seen: d.last_seen,
                    viewers: att
                        .map(|a| *a.viewers.lock().unwrap_or_else(|e| e.into_inner()))
                        .unwrap_or(0),
                    attached: att.is_some(),
                }
            })
            .collect();
        drop(attachments);

        let mut snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // §P1. The fleet, read from the same registry the rows came from. No
        // network call here: peerd keeps that table fresh, so a browser refresh
        // costs one query however many nodes are out there. The alternative --
        // each page asking each node directly -- multiplies by the number of
        // people watching, which is the mistake this design set out to avoid.
        let peers: Vec<DashPeer> = {
            let now = now_ms();
            let ttl = self.config.peers.ttl_s;
            conminer_core::peers::registry::all(&reg)
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    let (age_s, live) = (p.age_ms(now) / 1000, p.is_live(now, ttl));
                    DashPeer {
                        devices: rows
                            .iter()
                            .filter(|d| d.node.as_deref() == Some(p.name.as_str()))
                            .count(),
                        node: p.name,
                        host: p.host,
                        dash_url: p.dash_url,
                        age_s,
                        ttl_s: ttl,
                        live,
                        answering: p.ok,
                        reverse: p
                            .last_poll
                            .is_some_and(|t| now - t < conminer_core::peers::relay::POLLER_TTL_MS),
                        build: p.version.clone(),
                        build_matches: p
                            .version
                            .as_deref()
                            .filter(|v| !v.is_empty())
                            .map(|v| v == conminer_core::build_id()),
                    }
                })
                .collect()
        };

        let changed = snap.devices != devices || snap.peers.len() != peers.len();
        snap.devices = devices;
        snap.peers = peers;
        // THIS node's own identity, from the same source peerd advertises: the
        // local bench is a rack unit like any other and its header shows the
        // same three facts a peer's does.
        if let Ok(id) = conminer_core::peers::Identity::load_or_create(
            &self.data_dir,
            &self.config.peers.name,
            now_ms(),
        ) {
            snap.node = id.name;
        }
        snap.node_host = Some(self.config.peers.advertise_host.clone())
            .filter(|h| !h.is_empty())
            .or_else(|| Some("this host".to_string()));
        snap.build = conminer_core::build_id().to_string();
        snap.server_now = now_ms();
        if changed {
            snap.revision += 1;
            let rev = snap.revision;
            drop(snap);
            // A send with no receivers is not an error: nobody is watching yet.
            let _ = self.changed.send(rev);
        }
        Ok(changed)
    }

    fn existing(&self, canonical: &str) -> Option<Arc<Attachment>> {
        self.attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(canonical)
            .cloned()
    }

    fn snapshot(&self) -> Snapshot {
        let mut s = self
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        s.server_now = now_ms();
        // The BENCH view: only things a person can stand in front of. Everything
        // else the registry knows about is real and useful to an agent through
        // `list_devices`, and pure noise on a page whose whole job is "what is
        // plugged into this rig".
        let now = s.server_now;
        let hook_press = self.config.discovery.removal_grace_ms as i64;
        s.devices
            .retain(|d| belongs_on_the_dashboard(d, now, hook_press));
        s
    }

    /// Resolve a device the way every other surface does.
    ///
    /// THE NAME A BOARD IS KNOWN BY IS THE NAME IT ANSWERS TO. This matched
    /// only the by-id path, so `/ws/console/uno-q` -- the nickname printed by
    /// `list_devices`, accepted by every MCP tool, and shown on this very page
    /// -- returned 404, while the percent-encoded `/dev/serial/by-id/...` path
    /// worked. One console, two names, one of them a dead end.
    ///
    /// Exact matches first, so a nickname can never shadow a real device path.
    fn find(&self, selector: &str) -> Option<DashDevice> {
        let snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snap.devices
            .iter()
            .find(|d| d.device == selector || d.canonical == selector)
            .or_else(|| {
                snap.devices
                    .iter()
                    .find(|d| d.nickname.as_deref() == Some(selector))
            })
            .cloned()
    }

    /// The shared connection for a device, opening it if this is the first
    /// viewer.
    async fn attach(&self, dev: &DashDevice) -> Result<Arc<Attachment>> {
        // Fast path: already connected.
        if let Some(a) = self.existing(&dev.canonical) {
            return Ok(a);
        }
        let _dialing = self.dialing.lock().await;
        // Re-check under the lock: another task may have dialled while this one
        // waited, and connecting twice would burn a second ser2net client.
        if let Some(a) = self.existing(&dev.canonical) {
            return Ok(a);
        }

        let port = dev
            .port
            .ok_or_else(|| anyhow::anyhow!("{} has no ser2net endpoint", dev.device))?;
        let host = connect_host(&self.config);
        let broker_sock = conminer_core::broker::socket_path(&self.config.paths.run_dir);
        let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        stream.set_nodelay(true)?;
        let (tcp_reader, writer0) = stream.into_split();
        let dead0 = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // READ FROM THE BROKER when minerd is publishing, so this console has
        // exactly ONE reader of the ser2net connection instead of one per
        // consumer. TX still goes straight to ser2net by design: a broker
        // outage must never be able to swallow a keystroke headed for a board.
        //
        // Falls back to reading the ser2net socket directly, so the dashboard
        // keeps working when minerd is down -- which is exactly when someone is
        // most likely to be staring at a console trying to find out why.
        // THE BROKER CARRIES ONLY WHAT THIS NODE CAPTURES. A remote (peer)
        // device is captured by its OWNER and re-exported here as a ser2net
        // relay; this node's minerd never captures it, so its canonical is
        // never published and a broker subscription for it is an empty channel
        // that exists only because subscribe() makes topics on demand. Reading
        // it -- and draining the relay socket that actually carries the owner's
        // bytes -- is why every peer console rendered BLANK in the web UI while
        // every proxied tool worked. So the broker is for local consoles only;
        // a remote one reads its relay socket directly.
        let mut reader: ConsoleReader = if console_reads_broker(dev) {
            match conminer_core::broker::connect(&broker_sock, &dev.canonical).await {
                Ok(sub) => {
                    tracing::info!(device = %dev.canonical, "console via broker");
                    // NOBODY ELSE IS READING THIS SOCKET NOW. Reading from the
                    // broker leaves the ser2net connection -- the one that
                    // carries keystrokes -- with an unread receive buffer. It
                    // fills, our window closes, ser2net gives up on the client,
                    // and TX dies in silence while RX keeps flowing from the
                    // broker, so nothing ever looks wrong. Drain and discard.
                    tokio::spawn(drain(tcp_reader, dead0.clone()));
                    Box::pin(sub)
                }
                Err(e) => {
                    tracing::info!(
                        device = %dev.canonical,
                        error = %e,
                        "broker unavailable; reading ser2net directly"
                    );
                    Box::pin(tcp_reader)
                }
            }
        } else {
            tracing::info!(
                device = %dev.canonical,
                "remote console: reading the ser2net relay directly (its owner captures it, so \
                 this node's broker never publishes it)"
            );
            Box::pin(tcp_reader)
        };
        // The write half lives in a slot so a reconnect can swap in a fresh one
        // without the browser noticing.
        let writer_slot = Arc::new(tokio::sync::Mutex::new(Some(Writer {
            w: writer0,
            dead: dead0,
        })));

        let (rx_tx, _) = broadcast::channel::<Vec<u8>>(1024);
        let (tx_tx, mut tx_rx) = mpsc::channel::<Vec<u8>>(64);
        let scrollback = Arc::new(Mutex::new(Vec::<u8>::new()));
        let att = Arc::new(Attachment {
            rx: rx_tx.clone(),
            tx: tx_tx,
            scrollback: scrollback.clone(),
            viewers: Arc::new(Mutex::new(0)),
        });
        self.attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dev.canonical.clone(), att.clone());

        // Taken before the reader task moves the originals.
        let tx_notify = rx_tx.clone();
        let tx_host = host.clone();

        // Reader: console → every browser.
        let cap = self.config.dashboard.scrollback_lines * 120;
        let broker_sock_rc = broker_sock.clone();
        let reads_broker = console_reads_broker(dev);
        let attachments = self.attachments.clone();
        let canonical = dev.canonical.clone();
        let changed = self.changed.clone();
        let writer_for_rx = writer_slot.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                // Inner loop: pump this socket until it dies.
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Strip telnet IAC negotiation before it reaches a
                            // browser. ser2net's accepter is telnet (that is what
                            // makes 4.x share one connector across clients), so the
                            // first bytes of every session are option bytes that a
                            // terminal renders as garbage.
                            let chunk = strip_telnet(&buf[..n]);
                            if chunk.is_empty() {
                                continue;
                            }
                            {
                                let mut sb = scrollback.lock().unwrap_or_else(|e| e.into_inner());
                                sb.extend_from_slice(&chunk);
                                if sb.len() > cap {
                                    let drop_to = sb.len() - cap;
                                    sb.drain(..drop_to);
                                }
                            }
                            // No receivers simply means every browser has gone; the
                            // connection stays up so scrollback keeps filling.
                            let _ = rx_tx.send(chunk);
                        }
                    }
                }

                // The console dropped. Do NOT tear the attachment down: a power
                // action detaches the FTDI, so the port vanishes for a few
                // seconds on every power-off, cycle and EDL entry. Dropping the
                // attachment closed every browser's socket and made the operator
                // reconnect by hand each time. Hold the viewers, keep the
                // scrollback, and re-dial underneath them.
                let note =
                    format!("\r\n[conminer] console dropped (port {port}); reconnecting…\r\n");
                {
                    let mut sb = scrollback.lock().unwrap_or_else(|e| e.into_inner());
                    sb.extend_from_slice(note.as_bytes());
                }
                let _ = rx_tx.send(note.into_bytes());

                let mut backoff = std::time::Duration::from_millis(250);
                loop {
                    tokio::time::sleep(backoff).await;
                    match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                        Ok(sock) => {
                            let _ = sock.set_nodelay(true);
                            let (r, w) = sock.into_split();
                            let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            reader = if reads_broker {
                                match conminer_core::broker::connect(&broker_sock_rc, &canonical)
                                    .await
                                {
                                    Ok(sub) => {
                                        // Same trap as the first attach: read
                                        // from the broker and this socket has
                                        // no reader at all.
                                        tokio::spawn(drain(r, dead.clone()));
                                        Box::pin(sub) as ConsoleReader
                                    }
                                    Err(_) => Box::pin(r) as ConsoleReader,
                                }
                            } else {
                                // Remote console: the relay socket is the only
                                // source, exactly as on the first attach.
                                Box::pin(r) as ConsoleReader
                            };
                            *writer_for_rx.lock().await = Some(Writer { w, dead });
                            let back = "[conminer] console back\r\n".to_string();
                            {
                                let mut sb = scrollback.lock().unwrap_or_else(|e| e.into_inner());
                                sb.extend_from_slice(back.as_bytes());
                            }
                            let _ = rx_tx.send(back.into_bytes());
                            let _ = changed.send(0);
                            break;
                        }
                        Err(_) => {
                            // THE BOARD COMING BACK RESETS THE CLOCK, the same
                            // rule capture follows. A power cycle takes the tty
                            // away, so these re-dials fail and the delay
                            // doubles -- and the board then prints its whole
                            // firmware banner while this loop is still asleep,
                            // which is what an operator watching the console
                            // sees as "I missed the boot". The tty existing
                            // again means waiting longer buys nothing.
                            backoff = if std::path::Path::new(canonical.as_str()).exists() {
                                std::time::Duration::from_millis(250)
                            } else {
                                (backoff * 2).min(std::time::Duration::from_secs(5))
                            };
                        }
                    }
                    if attachments
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&canonical)
                        .is_none()
                    {
                        return; // the device really went away
                    }
                }
            }
        });

        // Writer: browsers → console. Owns its connection's whole life, because
        // NOTHING ELSE CAN. The old version leaned on the reader to re-dial and
        // refill the slot -- true only when the reader IS this socket. Read from
        // the broker and the reader never touches it, so a dead write half was
        // never noticed and never replaced: the slot emptied on the first failed
        // write and every keystroke after it was dropped in silence, forever,
        // while output kept scrolling. So: dial on demand, retry once on a fresh
        // socket, and say so out loud when the byte truly cannot be delivered.
        let writer_for_tx = writer_slot.clone();
        tokio::spawn(async move {
            let mut complained = false;
            while let Some(bytes) = tx_rx.recv().await {
                let mut guard = writer_for_tx.lock().await;
                // Retire a socket already known to be dead BEFORE writing to it.
                if guard
                    .as_ref()
                    .is_some_and(|c| c.dead.load(std::sync::atomic::Ordering::SeqCst))
                {
                    *guard = None;
                }
                let mut delivered = false;
                // Two attempts: the socket we hold, then a freshly dialled one.
                for _ in 0..2 {
                    if guard.is_none() {
                        match tokio::net::TcpStream::connect((tx_host.as_str(), port)).await {
                            Ok(sock) => {
                                let _ = sock.set_nodelay(true);
                                let (r, w) = sock.into_split();
                                let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                tokio::spawn(drain(r, dead.clone()));
                                *guard = Some(Writer { w, dead });
                            }
                            Err(_) => break,
                        }
                    }
                    if let Some(c) = guard.as_mut() {
                        if c.w.write_all(&bytes).await.is_ok()
                            && c.w.flush().await.is_ok()
                            && !c.dead.load(std::sync::atomic::Ordering::SeqCst)
                        {
                            delivered = true;
                            break;
                        }
                        *guard = None; // dead; the next attempt dials a new one
                    }
                }
                if delivered {
                    complained = false;
                } else if !complained {
                    // Once per outage, not once per keystroke: an operator
                    // holding a key down must not be shown a wall of these.
                    complained = true;
                    let note = format!(
                        "\r\n[conminer] keystroke not delivered: cannot reach the console (port {port})\r\n"
                    );
                    let _ = tx_notify.send(note.into_bytes());
                }
            }
        });

        Ok(att)
    }
}

/// The physical board a device belongs to, derived from USB topology.
///
/// A board's harness is usually several USB devices behind one hub: on the IQ10
/// the FT4232 carrying four UARTs sits at `3.2.2` while the Bantam controller
/// that powers the same board sits at `3.2.4` -- siblings under hub `3.2`.
/// Grouping by by-id name puts them in separate boxes, which is backwards: they
/// are one board, and the page should show them as one.
///
/// The rule is "share a DOWNSTREAM hub". Stripping the last component blindly
/// would take `3.3` (a device plugged straight into a root-level port) up to
/// `3`, the root itself, and merge every board on the host into one group. So a
/// parent only counts when it is itself below the root.
pub fn topology_group(by_path: Option<&str>) -> Option<String> {
    // `pci-0000:00:14.0-usb-0:3.2.2:1.1-port0` -> `3.2.2`
    let p = by_path?;
    let after = p
        .split("-usb-0:")
        .nth(1)
        .or_else(|| p.split("-usbv2-0:").nth(1))?;
    let chain = after.split(':').next()?;
    if chain.is_empty() {
        return None;
    }
    match chain.rsplit_once('.') {
        // Parent is a real hub below the root: group siblings together.
        Some((parent, _)) if parent.contains('.') => Some(parent.to_string()),
        // Parent would be the root hub; this device is its own board.
        _ => Some(chain.to_string()),
    }
}

/// The physical adapter a by-id name belongs to.
///
/// `usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0` → `usb-FTDI_IQ10_UART-SPI_AR40BYP4AU`.
/// Purely textual, because the by-id name is the identity conminer already
/// trusts everywhere else; walking sysfs would introduce a second notion of the
/// same thing that could disagree with it.
/// What the OWNER said about a remote board, when it said anything.
///
/// `None` means "this is ours, work it out locally" -- so the local path is
/// untouched and only a row held on somebody else's behalf defers to them.
fn remote_controls<'a>(
    d: &'a conminer_core::store::DeviceRow,
    key: &str,
) -> Option<&'a serde_json::Value> {
    d.remote_controls.as_ref()?.get(key)
}

fn remote_str(d: &conminer_core::store::DeviceRow, key: &str) -> Option<Option<String>> {
    let v = remote_controls(d, key)?;
    Some(v.as_str().map(str::to_string))
}

fn remote_flag(d: &conminer_core::store::DeviceRow, key: &str) -> Option<bool> {
    remote_controls(d, key)?.as_bool()
}

fn remote_str_list(d: &conminer_core::store::DeviceRow, key: &str) -> Option<Vec<String>> {
    Some(
        remote_controls(d, key)?
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
    )
}

fn adapter_of(canonical: &str) -> Option<String> {
    let idx = canonical.find("-if")?;
    // Only when what follows really is an interface number.
    let rest = &canonical[idx + 3..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    Some(canonical[..idx].to_string())
}

/// Where to reach ser2net.
///
/// Same rule as minerd's: an explicit `connect_host` wins, because under
/// compose ser2net is a sibling container and the address derived from its
/// *bind* (127.0.0.1) resolves to this container instead — which the browser
/// sees as `Connection refused (os error 111)` on every console.
/// Remove telnet IAC sequences from a console byte stream.
///
/// Console output is otherwise passed through untouched: this drops only the
/// protocol bytes ser2net's telnet accepter inserts, never anything the board
/// printed. IAC is 0xFF; a doubled 0xFF is a literal 0xFF and is preserved.
pub use conminer_core::runner::strip_telnet;

/// The console's write half, plus the flag whoever reads that socket sets when
/// it dies.
///
/// A TCP write to a socket the far end has already closed SUCCEEDS: the bytes
/// land in the local send buffer and the RST arrives afterwards. So "write_all
/// returned Ok" is not evidence a keystroke was delivered, and treating it as
/// evidence is how a dropped connection ate the first keystroke after every
/// power cycle. The reader is the only party that learns the truth, so it is
/// the one that records it.
struct Writer {
    w: tokio::net::tcp::OwnedWriteHalf,
    dead: Arc<std::sync::atomic::AtomicBool>,
}

/// Read and discard, so a socket nobody consumes cannot stall.
///
/// TCP flow control is not advisory: leave a receive buffer unread and the
/// window closes, the far end blocks, and ser2net eventually drops the client.
/// Whenever the writer's socket is not also the reader's socket, this is the
/// only thing keeping the keystroke path alive.
async fn drain(mut r: tokio::net::tcp::OwnedReadHalf, dead: Arc<std::sync::atomic::AtomicBool>) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // This socket is finished. Say so, so the next keystroke dials a new one
    // instead of being written confidently into nothing.
    dead.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn connect_host(cfg: &Config) -> String {
    cfg.ser2net_host()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------------------ routes ---

/// Ask mcpd to perform a hardware action.
///
/// Deliberately a call *into mcpd* rather than dashd running the hook itself.
/// mcpd owns hardware actuation, so a button press gets the same treatment an
/// agent's call does: the epoch opens with `opened_by: power`, the event lands
/// on the timeline, and there is exactly one code path that touches the board.
/// Running the hook here instead would be less code and a second source of
/// truth about what happened to the hardware.
async fn call_mcp(base: &str, tool: &str, args: Value) -> Result<Value> {
    let url = base.trim_end_matches('/').to_string();
    let (host, port, path) = split_url(&url)?;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
    .to_string();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed reply from mcpd"))?;
    // The MCP endpoint may answer as SSE; take the last JSON object either way.
    let payload = body
        .lines()
        .map(|l| l.trim_start_matches("data:").trim())
        .rfind(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("no JSON in mcpd reply: {body}"))?;
    Ok(serde_json::from_str(payload)?)
}

fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(8090)),
        None => (hostport.to_string(), 8090),
    };
    Ok((host, port, path.to_string()))
}

/// Run a hardware action for the browser: take the lease, act, report.
/// Re-read one board's power right after conminer changed it.
///
/// The lamp should move when the user presses the button, not up to 30s later.
/// Polling every board faster would cost a serial round-trip per board per tick
/// for a value that only changes when someone acts; refreshing the ONE board
/// that just changed costs one query and feels instant.
///
/// Delayed slightly because a PMIC does not collapse or come up the moment the
/// hook returns -- reading too early reports the state we just left.
fn refresh_power_soon(d: &Dash, canonical: &str, settle: std::time::Duration) {
    let dash = d.clone();
    let dev = canonical.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(settle).await;
        let state = dash.probe_power(&dev).await;
        // Publish to EVERY console on that controller, not just the one probed.
        // Power belongs to the board: updating a single row left the NordAU's
        // other five showing the state from before the button was pressed, so
        // one board reported two different power states at once until the next
        // sweep caught up.
        let group = dash.consoles_sharing_controller(&dev);
        dash.publish_power(&group, state, now_ms());
        let _ = dash.refresh();
    });
}

async fn hardware_action(d: &Dash, selector: &str, tool: &str, args: Value) -> Response {
    if !d.config.dashboard.allow_power {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "dashboard.allow_power is false"})),
        )
            .into_response();
    }
    let base = d.config.dashboard.mcp_url.clone();
    // Mutating tools need the lease; the dashboard takes it for the call and
    // steals, because a human at the bench pressing a button outranks an agent
    // holding a reservation.
    //
    // BOUNDED, because this lease is for ONE BUTTON PRESS and nothing more.
    // Measured on the rig: a press left `dashboard` holding the console for the
    // full default TTL -- twelve minutes still on the clock afterwards -- and
    // `release` refuses without the token, so the next agent to touch that board
    // got LEASE_HELD by a browser nobody was sitting at. The press is over in
    // ~72s at worst; the lease has no business outliving it by ten minutes.
    if let Err(e) = call_mcp(
        &base,
        "acquire",
        json!({"device": selector, "steal": true, "holder": "dashboard",
               "ttl_s": PRESS_LEASE_TTL_S}),
    )
    .await
    {
        return (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": format!("could not reach mcpd: {e}")})),
        )
            .into_response();
    }
    // DROP THE OLD READING BEFORE DISPATCHING, not after.
    //
    // MEASURED ON HARDWARE: a `power on` against the IQ10 took 71.9 SECONDS to
    // return (the hook powers, settles and verifies). Invalidating only on the
    // response meant the page served the pre-action "off" for that entire
    // minute-plus, while the board was coming up -- the exact window this is
    // supposed to close, and the reading most likely to be acted on, because
    // pressing the button is when somebody watches the lamp.
    //
    // Aimed with the LOCAL selector, which may not be the device the hook
    // finally hits; the authoritative invalidation still happens below, keyed on
    // the canonical the tool reports. Over-invalidating is safe by construction:
    // its only effect is a brief "unknown", never a wrong value. Under-
    // invalidating is what states a stale fact.
    if tool == "power" {
        let group = match d.find(selector) {
            Some(dev) => d.consoles_sharing_controller(&dev.canonical),
            None => vec![selector.to_string()],
        };
        d.invalidate_power(&group);
        let _ = d.refresh();
    }
    let outcome = call_mcp(&base, tool, args).await;
    // HAND THE CONSOLE BACK. The press is done, whether it worked or not.
    //
    // `force` because the lease belongs to "dashboard" and a plain release wants
    // the holder's token, which lives in the mcpd call that took it. The race it
    // opens -- somebody stealing the lease during the action and losing it here
    // -- is narrow and self-inflicted by the steal; leaving every press to
    // strand a console until its TTL is neither narrow nor rare.
    let _ = call_mcp(&base, "release", json!({"device": selector, "force": true})).await;
    match outcome {
        Ok(v) => {
            let r = &v["result"];
            let is_err = r["isError"].as_bool().unwrap_or(false);
            let content = r["structuredContent"].clone();
            // The board's power just changed, so refresh THAT board's lamp now
            // rather than waiting for the slow sweep. The canonical name comes
            // back in the tool result, so this follows whatever device the
            // action actually hit -- not the selector we hoped it hit, which is
            // the distinction that let a button actuate the wrong board.
            if !is_err && tool == "power" {
                if let Some(dev) = content.get("device").and_then(Value::as_str) {
                    // DROP THE OLD READING FIRST. Between dispatching the action
                    // and the settle-time re-probe below, the cache still holds
                    // the pre-action value, and serving it is the page stating
                    // as fact the very thing the button just changed. Unknown is
                    // correct for those few seconds: we genuinely do not know
                    // yet.
                    d.invalidate_power(&d.consoles_sharing_controller(dev));
                    let _ = d.refresh();
                    // A rail takes a moment to collapse or come up; reading
                    // immediately would report the state we just left.
                    refresh_power_soon(d, dev, std::time::Duration::from_secs(4));
                    // And once more after the board has had time to settle, so a
                    // slow PMIC does not leave a stale lamp behind.
                    refresh_power_soon(d, dev, std::time::Duration::from_secs(12));
                }
            }
            (
                if is_err {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::OK
                },
                axum::Json(json!({"ok": !is_err, "result": content})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn power(
    State(d): State<Dash>,
    Path((selector, action)): Path<(String, String)>,
) -> Response {
    hardware_action(
        &d,
        &selector,
        "power",
        json!({"device": selector, "action": action}),
    )
    .await
}

/// Rename a device, or add and drop its labels.
///
/// A SEPARATE PATH FROM ACTUATION, deliberately. `hardware_action` takes the
/// device's lease (stealing it), drops the cached power reading and waits out a
/// hook, because it is about to move a board. Naming one touches no hardware, so
/// it must not bump whoever holds the lease: an operator tidying labels would
/// otherwise evict an agent mid-boot.
async fn meta_action(d: &Dash, tool: &str, args: Value) -> Response {
    if !d.config.dashboard.allow_power {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "dashboard.allow_power is false"})),
        )
            .into_response();
    }
    match call_mcp(&d.config.dashboard.mcp_url, tool, args).await {
        Ok(v) => {
            let result = v.get("result").cloned().unwrap_or(v);
            let failed = result.get("isError").and_then(Value::as_bool) == Some(true);
            let body = result
                .get("structuredContent")
                .cloned()
                .unwrap_or(Value::Null);
            let _ = d.refresh();
            (
                if failed {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::OK
                },
                axum::Json(json!({"ok": !failed, "result": body})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": format!("could not reach mcpd: {e}")})),
        )
            .into_response(),
    }
}

/// `POST /api/label/:selector` with the new name as the body.
async fn set_label(State(d): State<Dash>, Path(selector): Path<String>, body: String) -> Response {
    let nickname = body.trim().to_string();
    meta_action(
        &d,
        "name_device",
        json!({"device": selector, "nickname": nickname}),
    )
    .await
}

/// `POST /api/tags/:selector` with `{"tags": {...}, "remove": [...]}`.
async fn set_tags(State(d): State<Dash>, Path(selector): Path<String>, body: String) -> Response {
    let mut args: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    if let Some(o) = args.as_object_mut() {
        o.insert("device".into(), json!(selector));
    }
    meta_action(&d, "tag_device", args).await
}

async fn boot_mode(
    State(d): State<Dash>,
    Path((selector, mode)): Path<(String, String)>,
) -> Response {
    hardware_action(
        &d,
        &selector,
        "boot_mode",
        json!({"device": selector, "mode": mode}),
    )
    .await
}

/// A console's read side: the broker when minerd is publishing, the raw ser2net
/// socket when it is not. Boxed because those are different types and the
/// reconnect path swaps between them.
type ConsoleReader = std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>;

/// Whether a console viewer should read this device's RX from the local broker.
///
/// The broker publishes only what THIS node's minerd captures -- its local
/// consoles. A remote (peer) device is owned and captured elsewhere and
/// re-exported here as a ser2net relay, so its bytes arrive on the relay socket,
/// never the broker. `broker::subscribe` makes a topic on demand, so a broker
/// read for a peer canonical SUCCEEDS and then delivers nothing forever while
/// the relay socket that carries the real bytes is drained and discarded -- the
/// peer-console-is-blank bug. Local only.
fn console_reads_broker(dev: &DashDevice) -> bool {
    dev.node.is_none()
}

/// Keep the power indicator fresh without ever blocking a page load.
///
/// Every controller query costs ~1-2s, so this walks the devices on a slow timer
/// and caches. A light that is a few seconds stale is useful; a device list that
/// takes ten seconds to render is not.
pub fn spawn_power_poller(dash: Dash, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                // 5s, not 30. The event-driven refresh only fires for actions
                // taken THROUGH dashd -- an agent powering a board over MCP
                // notifies nobody, so the sweep was the only path and the lamp
                // sat stale. Measured: four consoles still reporting "on" 25s
                // after an off, clearing somewhere between 30 and 45s.
                //
                // The cost is bounded: only boards whose controller can actually
                // sense power are probed (see the skip below), so on this rig
                // that is two queries per tick, not one per console.
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
            let devices = {
                dash.snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .devices
                    .clone()
            };
            // ONE PROBE PER CONTROLLER INSTANCE, not per console. Every console
            // on a board shares one controller and therefore one power state, so
            // the NordAU's six consoles were costing six identical ~2s queries
            // and the walk alone took longer than the poll interval: measured
            // 15s for a board to go dark on the page after it was already off.
            // Probing each controller once and fanning the answer out to its
            // consoles makes the sweep proportional to BOARDS, not ports.
            //
            // The KEY IS THE INSTANCE -- the resolved controller tty -- and that
            // correction is the whole point. Keying on `controller` (the profile
            // NAME) is what broke it: this bench runs two Bantams, both named
            // "bantam", so all ten consoles across BOTH BOARDS landed in one
            // group, a single IQ10 console was probed, and its "off" was
            // published for the NordAU as well. Measured on hardware: the NordAU
            // was powered ON and every one of its six rows read "off".
            //
            // An answer may be shared only between consoles that would ask the
            // SAME CONTROLLER. Anything wider is one board reporting another
            // board's power, which is indistinguishable from a lie.
            for (_, consoles) in group_by_controller(&devices) {
                let Some(first) = consoles.first() else {
                    continue;
                };
                let state = dash.probe_power(first).await;
                dash.publish_power(&consoles, state, now_ms());
            }
            let _ = dash.refresh();
        }
    });
}

pub fn router(dash: Dash) -> Router {
    Router::new()
        .route("/api/power/:selector/:action", axum::routing::post(power))
        .route(
            "/api/boot_mode/:selector/:mode",
            axum::routing::post(boot_mode),
        )
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/label/:selector", axum::routing::post(set_label))
        .route("/api/tags/:selector", axum::routing::post(set_tags))
        .route("/api/devices", get(devices))
        .route("/api/reports", get(reports))
        .route("/api/events", get(events))
        .route("/ws/console/:selector", get(console_ws))
        .with_state(dash)
}

async fn index() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The page carries all of its own JS inline, so a cached copy is a
            // cached *client*. Without this header the response has no caching
            // directives at all and browsers fall back to heuristic caching,
            // which was measured serving a stale dashboard: freshly-fetched
            // clients streamed console data fine while a long-lived browser tab
            // kept running old JS and reporting "connection closed". A console
            // view is live state, never worth re-serving from disk.
            (header::CACHE_CONTROL, "no-store, must-revalidate"),
        ],
        include_str!("dashboard.html"),
    )
        .into_response()
}

async fn healthz(State(d): State<Dash>) -> Response {
    let snap = d.snapshot();
    axum::Json(json!({"status": "ok", "devices": snap.devices.len()})).into_response()
}

async fn devices(State(d): State<Dash>) -> Response {
    axum::Json(d.snapshot()).into_response()
}

/// Server-sent events: one message per actual change to the device set.
///
/// SSE rather than polling so a replug reaches the page in about the time
/// discoveryd takes to notice it, and rather than a WebSocket because this
/// direction is strictly server-to-browser and SSE reconnects by itself.
async fn events(
    State(d): State<Dash>,
) -> Sse<impl futures::Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    let mut rx = d.changed.subscribe();
    let dash = d.clone();
    let stream = async_stream::stream! {
        // The current state first, so a page that connects mid-session is
        // correct immediately instead of correct at the next hotplug.
        yield Ok(sse::Event::default()
            .event("devices")
            .data(serde_json::to_string(&dash.snapshot()).unwrap_or_default()));
        loop {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(_) => yield Ok(sse::Event::default()
                        .event("devices")
                        .data(serde_json::to_string(&dash.snapshot()).unwrap_or_default())),
                    // Lagged: the set changed more than the buffer held, so send
                    // the current truth rather than a stale increment.
                    Err(broadcast::error::RecvError::Lagged(_)) => yield Ok(sse::Event::default()
                        .event("devices")
                        .data(serde_json::to_string(&dash.snapshot()).unwrap_or_default())),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = tokio::time::sleep(Duration::from_secs(15)) => {
                    // Keeps proxies from reaping an idle lab console.
                    yield Ok(sse::Event::default().comment("keepalive"));
                }
            }
        }
    };
    Sse::new(stream).keep_alive(sse::KeepAlive::default())
}

/// §R. The triage queue, as JSON.
///
/// Deliberately a plain feed rather than a page: the habit that works on the
/// other bench is a watcher polling `reports.json` every run, and a queue nobody
/// reads is worse than pasting messages by hand, because it feels like progress.
async fn reports(State(d): State<Dash>) -> Response {
    use conminer_core::reports as rep;
    let Ok(reg) = Registry::open(&d.data_dir) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": "registry unavailable"})),
        )
            .into_response();
    };
    let open = rep::list(&reg, Some("open"), None, None, 200).unwrap_or_default();
    let rows: Vec<Value> = open
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "distinct_reporters".into(),
                    json!(rep::distinct_reporters(&reg, r.id).unwrap_or(0)),
                );
            }
            v
        })
        .collect();
    // THE NODE'S REAL NAME, from the same source everything else uses. The raw
    // config field is empty whenever the name lives in the persisted identity,
    // which is exactly the case on this fleet -- so the feed a watcher polls
    // across three nodes was labelling every one of them "".
    let node =
        conminer_core::peers::Identity::load_or_create(&d.data_dir, &d.config.peers.name, now_ms())
            .map(|i| i.name)
            .unwrap_or_else(|_| d.config.peers.name.clone());
    axum::Json(json!({
        "node": node,
        "build": conminer_core::build_id(),
        "open": rows.len(),
        // Regressions first: a report that came back on the build that claimed
        // to fix it is the one worth reading before any of the others.
        "regressions": open.iter().filter(|r| r.regressions > 0).count(),
        "reports": rows,
    }))
    .into_response()
}

async fn console_ws(
    State(d): State<Dash>,
    Path(selector): Path<String>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(dev) = d.find(&selector) else {
        return (
            StatusCode::NOT_FOUND,
            format!("unknown console {selector:?}"),
        )
            .into_response();
    };
    // `since` is where this browser got to last time. It arrives on a RECONNECT
    // -- after a power cycle took the UART away, after a network blip, after
    // this page was backgrounded -- and it is what turns a gap into a replay.
    let since = q.as_deref().and_then(|q| query_param(q, "since"));
    ws.on_upgrade(move |socket| console_session(d, dev, socket, since))
}

/// One value out of a raw query string, percent-decoded.
///
/// Hand-rolled rather than pulling in axum's `query` feature: this is one
/// parameter on one route, and a cursor is `token:hex` -- the only character
/// that ever needs decoding is the colon the browser escapes.
fn query_param(raw: &str, key: &str) -> Option<String> {
    raw.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&v[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What minerd captured while this browser was away.
///
/// The dashboard's own scrollback dies with the attachment, and the attachment
/// dies exactly when the interesting thing happens: a Bughopper's power-off
/// takes the tty, ser2net drops the port, and everything the dash was holding
/// goes with it. minerd's store does not: it reattaches and keeps capturing, so
/// it is the witness for the seconds between the board coming back and the
/// browser reconnecting.
///
/// Bounded on purpose. A page left open over a weekend must not be handed a
/// weekend of boot logs; when the bound bites, the caller is told.
const REPLAY_MAX_LINES: usize = 500;
const REPLAY_MAX_BYTES: usize = 128 * 1024;

struct Replay {
    bytes: Vec<u8>,
    lines: usize,
    truncated: bool,
    cursor: String,
}

/// The store's current head for this device, so a fresh viewer starts tracking.
fn head_cursor_of(d: &Dash, dev: &DashDevice) -> Option<String> {
    let reg = Registry::open(&d.data_dir).ok()?;
    let row = reg.resolve(&dev.canonical).ok()?;
    let store = conminer_core::store::DeviceStore::open(
        &d.data_dir.join(&row.db_file),
        &row.canonical,
        d.config.search.fts,
    )
    .ok()?;
    Some(store.head_cursor().encode())
}

fn replay_since(d: &Dash, dev: &DashDevice, since: &str) -> Option<Replay> {
    let reg = Registry::open(&d.data_dir).ok()?;
    let row = reg.resolve(&dev.canonical).ok()?;
    let store = conminer_core::store::DeviceStore::open(
        &d.data_dir.join(&row.db_file),
        &row.canonical,
        d.config.search.fts,
    )
    .ok()?;
    let cursor = conminer_core::store::Cursor::decode(since).ok()?;
    // A cursor from a previous life of this store (retention trimmed it, or the
    // device was re-created) is not an error worth failing a reconnect over:
    // the operator gets the live stream, exactly as before this existed.
    let offset = store.resolve_cursor(&cursor).ok()?;
    let rows = store.lines_after(offset, REPLAY_MAX_LINES + 1).ok()?;
    let truncated_by_count = rows.len() > REPLAY_MAX_LINES;
    let mut bytes = Vec::new();
    let mut lines = 0usize;
    let mut last = offset;
    for r in rows.iter().take(REPLAY_MAX_LINES) {
        if bytes.len() + r.bytes.len() > REPLAY_MAX_BYTES {
            break;
        }
        bytes.extend_from_slice(&r.bytes);
        bytes.extend_from_slice(b"\r\n");
        lines += 1;
        last = r.stream_offset + r.bytes.len() as u64;
    }
    Some(Replay {
        truncated: truncated_by_count || lines < rows.len().min(REPLAY_MAX_LINES),
        bytes,
        lines,
        cursor: store.cursor_at(last).encode(),
    })
}

async fn console_session(d: Dash, dev: DashDevice, socket: WebSocket, since: Option<String>) {
    use futures::{SinkExt, StreamExt};

    let att = match d.attach(&dev).await {
        Ok(a) => a,
        Err(e) => {
            let mut s = socket;
            let _ = s
                .send(Message::Text(
                    json!({"type": "error", "message": e.to_string()}).to_string(),
                ))
                .await;
            return;
        }
    };

    {
        let mut v = att.viewers.lock().unwrap_or_else(|e| e.into_inner());
        *v += 1;
    }
    // OFF THE RUNTIME. `refresh` opens the registry and reads every device --
    // synchronous SQLite on an async worker. One viewer never noticed; several
    // opening at once blocked every worker the runtime had, and sessions that
    // had already upgraded simply never got to run. Measured with five viewers
    // on one console: two attached, three stayed queued, and the count sat at 2
    // for a full sixty seconds while the server looked idle.
    //
    // Fire-and-forget on purpose: the snapshot's viewer count is worth an
    // eventual update, never a viewer waiting on a database read.
    refresh_off_thread(&d);

    let mut rx = att.rx.subscribe();
    let (mut sink, mut stream) = socket.split();

    // Hello + scrollback, so the pane is never blank on a quiet console.
    let history = att
        .scrollback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let _ = sink
        .send(Message::Text(
            json!({
                "type": "hello",
                "device": dev.device,
                "port": dev.port,
                "line": dev.line,
                "allow_tx": d.config.dashboard.allow_tx,
                "viewers": *att.viewers.lock().unwrap_or_else(|e| e.into_inner()),
            })
            .to_string(),
        ))
        .await;

    // HELLO FIRST, STORE SECOND, and the store on a blocking thread.
    //
    // Opening a device store is file I/O -- it can create and migrate a database
    // -- and doing it before the greeting put that latency in front of every
    // viewer, including the ones with nothing to replay. A live console must not
    // wait on SQLite to say hello. (Caught by the late-joiner gate, which timed
    // out waiting for the first live frame.)
    let replay = {
        let d2 = d.clone();
        let dev2 = dev.clone();
        let since2 = since.clone();
        tokio::task::spawn_blocking(move || match since2 {
            Some(c) => replay_since(&d2, &dev2, &c),
            None => head_cursor_of(&d2, &dev2).map(|cursor| Replay {
                bytes: Vec::new(),
                lines: 0,
                truncated: false,
                cursor,
            }),
        })
        .await
        .ok()
        .flatten()
    };
    match (replay, since.is_some()) {
        // REPLAY REPLACES THE SCROLLBACK, never doubles it. The store is the
        // continuous record; the attachment's buffer is whatever survived, and
        // sending both would show the same boot twice.
        (Some(r), true) => {
            let _ = sink
                .send(Message::Text(
                    json!({
                        "type": "replay",
                        "lines": r.lines,
                        "truncated": r.truncated,
                        "cursor": r.cursor,
                    })
                    .to_string(),
                ))
                .await;
            if !r.bytes.is_empty() {
                let _ = sink.send(Message::Binary(r.bytes)).await;
            }
        }
        // A FIRST viewer gets the scrollback it always got, plus the anchor it
        // will need if this connection ever drops.
        (r, false) => {
            if !history.is_empty() {
                let _ = sink.send(Message::Binary(history)).await;
            }
            if let Some(r) = r {
                let _ = sink
                    .send(Message::Text(
                        json!({"type": "cursor", "cursor": r.cursor}).to_string(),
                    ))
                    .await;
            }
        }
        // Asked for a replay and the store could not answer (cursor expired, or
        // the device has no store yet): the live stream is still correct.
        (None, true) => {}
    }

    let allow_tx = d.config.dashboard.allow_tx;
    let tx = att.tx.clone();

    // Console → browser.
    let mut to_browser = tokio::spawn(async move {
        while let Ok(chunk) = rx.recv().await {
            if sink.send(Message::Binary(chunk)).await.is_err() {
                break;
            }
        }
    });

    // Browser → console. Binary frames are raw bytes for the UART; text frames
    // are control messages, so a keystroke can never be confused with a command.
    let mut from_browser = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(bytes) => {
                    if allow_tx && tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut to_browser => from_browser.abort(),
        _ = &mut from_browser => to_browser.abort(),
    }

    {
        let mut v = att.viewers.lock().unwrap_or_else(|e| e.into_inner());
        *v = v.saturating_sub(1);
    }
    refresh_off_thread(&d);
}

/// Republish the snapshot without blocking an async worker on SQLite.
fn refresh_off_thread(d: &Dash) {
    let d = d.clone();
    tokio::task::spawn_blocking(move || {
        let _ = d.refresh();
    });
}

/// Serve until `shutdown` flips.
pub async fn serve(
    dash: Dash,
    bind: &str,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    serve_on(dash, listener, shutdown).await
}

/// Serve on a listener the caller already holds.
///
/// CLOSES A RACE THE TEST HARNESS CANNOT CLOSE ITSELF. Picking a free port by
/// bind-then-drop leaves a window in which another process -- in practice
/// another test rig, when the whole workspace runs at once -- binds the same
/// port first. The rig then talks to somebody else's server on "its" port and
/// sees somebody else's devices: measured once in a full-workspace run, where a
/// dashboard test asserting a Bantam has no adapter was answered by a different
/// rig's board and failed on an assertion that is otherwise deterministic.
///
/// Handing the bound listener straight to the server means the port is never
/// released, so there is no window at all.
pub async fn serve_on(
    dash: Dash,
    listener: tokio::net::TcpListener,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "dashd listening");

    // Keep the power lamps fresh on their own slow timer, off the request path.
    spawn_power_poller(dash.clone(), shutdown.clone());

    // Keep the device list fresh; every change wakes every connected browser.
    let poller = dash.clone();
    let interval = Duration::from_millis(poller.config.dashboard.refresh_ms.max(100));
    let mut stop = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = poller.refresh() {
                        tracing::warn!(error = %e, "device refresh failed");
                    }
                }
                _ = stop.changed() => break,
            }
        }
    });

    axum::serve(listener, router(dash))
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod selector_tests {
    use super::*;

    fn dev(canonical: &str, nickname: Option<&str>) -> DashDevice {
        DashDevice {
            device: canonical.to_string(),
            canonical: canonical.to_string(),
            nickname: nickname.map(str::to_string),
            ..Default::default()
        }
    }

    fn dash_with(devices: Vec<DashDevice>) -> Dash {
        let d = Dash::new(Config::default(), std::path::PathBuf::from("/tmp"));
        d.snapshot.lock().unwrap().devices = devices;
        d
    }

    /// The console route took only the by-id path, so `/ws/console/uno-q` --
    /// the nickname this page itself displays and every MCP tool accepts --
    /// answered 404. A board's name has to work on every surface.
    #[test]
    fn a_console_answers_to_its_nickname_as_well_as_its_path() {
        let d = dash_with(vec![dev(
            "/dev/serial/by-id/usb-Bughopper-if00-port0",
            Some("uno-q"),
        )]);
        assert!(
            d.find("uno-q").is_some(),
            "the nickname must resolve, or the web console is unreachable by the name people use"
        );
        assert!(
            d.find("/dev/serial/by-id/usb-Bughopper-if00-port0")
                .is_some(),
            "the by-id path must keep working"
        );
        assert!(d.find("no-such-board").is_none());
    }

    /// A nickname must never shadow a real device path: exact paths win.
    #[test]
    fn a_device_path_outranks_someone_elses_nickname() {
        let d = dash_with(vec![
            dev("/dev/serial/by-id/usb-A-if00-port0", Some("spare")),
            // A mischievous nickname that happens to look like the other's path.
            dev(
                "/dev/serial/by-id/usb-B-if00-port0",
                Some("/dev/serial/by-id/usb-A-if00-port0"),
            ),
        ]);
        let hit = d
            .find("/dev/serial/by-id/usb-A-if00-port0")
            .expect("resolves");
        assert_eq!(
            hit.canonical, "/dev/serial/by-id/usb-A-if00-port0",
            "the real path must win over a nickname imitating it"
        );
    }
}
