//! Configuration (§14.5, §16).
//!
//! One `conminer.toml`, env-overridable, validated by `conminer check-config`.
//! Invalid config fails fast at startup so the compose healthcheck catches it.
//!
//! Two rules drive the design:
//!   * `deny_unknown_fields` everywhere — a typo'd key is a hard error with the
//!     offending line, not a silently-ignored setting.
//!   * every knob in §16 has a `Default` impl carrying its documented default, so
//!     an empty file produces exactly the shipped behaviour.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-device overrides. Every field is optional; `None` means "inherit".
///
/// Keys marked (per-device) in §16 appear here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceOverride {
    pub nickname: Option<String>,
    pub pinned_profile: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    pub line: Option<LineConfig>,
    pub capture: Option<CaptureOverride>,
    pub session: Option<SessionOverride>,
    pub retention: Option<RetentionOverride>,
    pub search: Option<SearchOverride>,
    pub runner: Option<RunnerOverride>,
    pub state: Option<StateOverride>,
    #[serde(default)]
    pub hooks: DeviceHooks,
    /// Extra prompt patterns, and credential-gate patterns, for this device (§8.5).
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub credential_gates: Vec<String>,
    /// Logical target this device belongs to (§15.8 multi-console targets).
    pub target: Option<String>,
    /// This board's memory map, for address decoding (§18.3).
    #[serde(default)]
    pub memory_map: Vec<MemoryRegion>,
    /// Boot modes this board accepts, offered by the dashboard and validated by
    /// the `boot_mode` tool. Empty means the board has none conminer knows of.
    #[serde(default)]
    pub boot_modes: Vec<String>,
    /// §L6. The USB port paths (`2-3.1`, `3-3.4`) that belong to THIS board:
    /// its console cable, its controller, and the port its own USB device mode
    /// enumerates on. A hub path covers everything beneath it.
    ///
    /// Without this, a USB observation can only be attributed to "the bench" --
    /// and a stale gadget left by one board failed another board's cleanup
    /// check, because zombie matching ran bus-wide. Unset is not an error: it
    /// means bus-wide findings are reported with attribution unknown rather
    /// than blamed on whoever asked.
    #[serde(default)]
    pub usb_ports: Vec<String>,
}

/// External command hooks. conminer never flashes or switches power itself; it
/// invokes the lab's tooling and records the event in the epoch machinery.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceHooks {
    /// `power on|off|cycle` — `{action}` is substituted.
    pub power: Option<String>,
    /// `flash <image_ref>` — `{image}` is substituted.
    pub flash: Option<String>,
    /// Invoked by the runner's recovery ladder rung 4 when `recover=power`.
    pub reset: Option<String>,
    /// Select a boot mode: `{mode}` is substituted (EDL, fastboot, UEFI…).
    ///
    /// Separate from `power` because it is a different question. Power is
    /// on/off/reset and means the same thing on every rig; a boot mode is a
    /// choice about *how* the SoC should come up, and the set of valid choices
    /// is a property of the silicon.
    pub boot_mode: Option<String>,
}

// ------------------------------------------------------------- discovery -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Glob list over `/dev/serial/by-id` names.
    pub include: Vec<String>,
    /// The important one: lab hosts carry devices conminer must be keepable off.
    pub exclude: Vec<String>,
    pub hotplug_debounce_ms: u64,
    pub poll_fallback_hz: u32,
    /// How long a console may be ABSENT before its accepter leaves the ser2net
    /// config.
    ///
    /// Additions and returns still land after `hotplug_debounce_ms`; only the
    /// removal waits. A controller that shares its FTDI between console and
    /// power control (the Bughopper) detaches that console for the length of
    /// every press, and rewriting the config on each detach meant a SIGHUP,
    /// which ser2net 4.x answers by dropping accepters, which meant a restart
    /// that severed every console on the node -- 39 times in 30 minutes on
    /// one bench, one of them under a run_command mid-echo. ser2net serves an
    /// absent path as an open failure and opens it fine on the next attach
    /// once it is back (measured, 4.6.4), so the accepter can simply wait.
    #[serde(default = "default_removal_grace_ms")]
    pub removal_grace_ms: u64,
}

fn default_removal_grace_ms() -> u64 {
    45_000
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            include: vec!["*".into()],
            exclude: vec![],
            hotplug_debounce_ms: 500,
            poll_fallback_hz: 1,
            removal_grace_ms: default_removal_grace_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ser2netConfig {
    pub base_port: u16,
    pub bind: String,
    pub config_path: PathBuf,
    /// Host to *connect* to when attaching to a console endpoint.
    ///
    /// Separate from `bind` because they answer different questions. `bind` is
    /// where ser2net listens; this is where everything else finds it, and under
    /// compose those differ: ser2net is its own container, so deriving the
    /// connect address from `bind` lands on 127.0.0.1 — which inside minerd's
    /// container is minerd. Empty means "derive from `bind`", which is right for
    /// a bare-metal install.
    #[serde(default)]
    pub connect_host: String,
    /// Simultaneous clients per console port.
    ///
    /// ser2net refuses a second client unless this is raised, which would make
    /// the whole fan-out premise false: minerd holds one connection for capture,
    /// so without headroom nothing else — the dashboard, minicom, labgrid — can
    /// attach to a console conminer is watching.
    pub max_connections: u16,
}

impl Default for Ser2netConfig {
    fn default() -> Self {
        Self {
            base_port: 5001,
            bind: "0.0.0.0".into(),
            config_path: PathBuf::from("/var/lib/conminer/ser2net.yaml"),
            connect_host: String::new(),
            max_connections: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttachConfig {
    pub reconnect_backoff_ms: u64,
    pub reconnect_backoff_max_ms: u64,
    pub tcp_keepalive_s: u64,
    /// How long a capture may sit in a FAILURE state before re-dialling.
    ///
    /// A console that answered with ser2net's device-open banner, or whose board
    /// went away into EDL, stops producing bytes -- so nothing ever arrives to
    /// move it out of that state, and the registry keeps asserting a fault long
    /// after the port recovered. Reported from the bench: a fresh probe
    /// connected cleanly and read fine while `capture_state` still said
    /// `open_failed`. Re-dialling is the cheapest way to find out, and one
    /// connect per interval is nothing next to being wrong.
    pub revalidate_failed_after_ms: u64,
}

impl Default for AttachConfig {
    fn default() -> Self {
        Self {
            reconnect_backoff_ms: 250,
            reconnect_backoff_max_ms: 15_000,
            tcp_keepalive_s: 10,
            revalidate_failed_after_ms: 15_000,
        }
    }
}

// ------------------------------------------------------------------ line -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowControl {
    None,
    #[serde(rename = "rtscts")]
    RtsCts,
    #[serde(rename = "xonxoff")]
    XonXoff,
}

/// §3.2 — UART line settings. Default 115200 8N1 flow=none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineConfig {
    #[serde(default = "d_baud")]
    pub baud: u32,
    #[serde(default = "d_data_bits")]
    pub data_bits: u8,
    #[serde(default = "d_parity")]
    pub parity: Parity,
    #[serde(default = "d_stop_bits")]
    pub stop_bits: u8,
    #[serde(default = "d_flow")]
    pub flow: FlowControl,
    #[serde(default = "d_tx_line_ending")]
    pub tx_line_ending: String,
    /// §15.9 — perturbs the port, so opt-in only.
    #[serde(default)]
    pub auto_baud: bool,
    #[serde(default = "d_auto_baud_rates")]
    pub auto_baud_rates: Vec<u32>,
}

fn d_baud() -> u32 {
    115200
}
fn d_data_bits() -> u8 {
    8
}
fn d_parity() -> Parity {
    Parity::None
}
fn d_stop_bits() -> u8 {
    1
}
fn d_flow() -> FlowControl {
    FlowControl::None
}
fn d_tx_line_ending() -> String {
    "\n".into()
}
fn d_auto_baud_rates() -> Vec<u32> {
    vec![
        115200, 921600, 1_500_000, 9600, 38400, 57600, 230400, 460800,
    ]
}

impl Default for LineConfig {
    fn default() -> Self {
        Self {
            baud: d_baud(),
            data_bits: d_data_bits(),
            parity: d_parity(),
            stop_bits: d_stop_bits(),
            flow: d_flow(),
            tx_line_ending: d_tx_line_ending(),
            auto_baud: false,
            auto_baud_rates: d_auto_baud_rates(),
        }
    }
}

impl LineConfig {
    /// The `115200 8N1` shorthand used in `list_devices` and `identify`.
    pub fn summary(&self) -> String {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Even => 'E',
            Parity::Odd => 'O',
            Parity::Mark => 'M',
            Parity::Space => 'S',
        };
        format!("{} {}{}{}", self.baud, self.data_bits, p, self.stop_bits)
    }

    /// ser2net's `<baud> <bits>DATABITS <parity> <stop>STOPBIT` option string.
    pub fn ser2net_options(&self) -> String {
        // ser2net 4.x wants the line settings as ONE compact word --
        // `<baud><parity><bits><stop>`, e.g. `115200n81`. The spelled-out
        // `115200 8DATABITS NONE 1STOPBIT` is 3.x syntax, and on 4.x it is not
        // merely ignored: measured on an FT4232 with ser2net 4.6.4, the whole
        // phrase is taken as a single unparsable connector option and every
        // client attach fails with `Device open failure: Object was already in
        // use` -- an error that reads like a busy tty and is not one. `lsof`
        // showed no holder, and the same device opened fine outside ser2net.
        let parity = match self.parity {
            Parity::None => "n",
            Parity::Even => "e",
            Parity::Odd => "o",
            Parity::Mark => "m",
            Parity::Space => "s",
        };
        // Flow control is stated only when it is actually wanted.
        //
        // Emitting `-RTSCTS -XONXOFF LOCAL` for the no-flow case looks harmless
        // and is not: measured on an FT4232, ser2net 3.5.1 given those options
        // opens the port, accepts clients, sends its banner and then forwards
        // **nothing**, while a plain open of the same tty reads the console
        // fine. Omitting them is also simply more honest — no flow control is
        // ser2net's default, so saying nothing says exactly that.
        // Measured on an FT4232 with ser2net 3.5.1: emitting
        // `-RTSCTS -XONXOFF LOCAL` for the no-flow case makes ser2net open the
        // port, accept clients, send its banner and then forward nothing, while
        // a plain open of the same tty reads the console fine. Saying nothing
        // is also what "no flow control" means.
        // Comma separated: this is a ser2net 4.x connector option list, not the
        // space-separated 3.x words. Flow control is named only when wanted —
        // on 3.x, spelling out `-RTSCTS -XONXOFF LOCAL` for the no-flow case was
        // measured to mute the port entirely.
        let flow = match self.flow {
            FlowControl::None => "",
            FlowControl::RtsCts => ",rtscts=on",
            FlowControl::XonXoff => ",xonxoff=on",
        };
        format!(
            "{}{}{}{},local{}",
            self.baud, parity, self.data_bits, self.stop_bits, flow
        )
    }
}

// --------------------------------------------------------------- capture -----

/// How the line splitter decides what terminates a line (§13 `linesplit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEndingMode {
    /// Probe the stream: lock to LF on the first `\n`, to CR if `probe_bytes`
    /// pass with carriage returns and no linefeed at all.
    Auto,
    /// Only `\n` terminates; a bare `\r` is an overwrite control (spinners,
    /// U-Boot countdowns) and is rendered, not split.
    Lf,
    /// As `lf` — CRLF is absorbed as one terminator either way.
    Crlf,
    /// Classic `\r`-only console.
    Cr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub ring_mb: u64,
    /// Durability window: batched SQLite commits, so a host power cut loses at
    /// most this much captured data. 0 = per-line fsync.
    pub commit_interval_ms: u64,
    pub encoding: String,
    pub max_line_bytes: usize,
    #[serde(default = "d_line_ending_mode")]
    pub line_ending_mode: LineEndingMode,
    #[serde(default = "d_probe_bytes")]
    pub line_ending_probe_bytes: usize,
    /// USB signatures that mean "the board is in a flash/recovery mode, not at
    /// its normal console" -- `"vid:pid"` or `"vid:*"` (hex). EDL/QDL is only
    /// one; fastboot, DFU and the rest are added here, not in code. Empty means
    /// use the built-in defaults (`usb::default_recovery_signatures`).
    #[serde(default)]
    pub recovery_gadgets: Vec<String>,
}

fn d_line_ending_mode() -> LineEndingMode {
    LineEndingMode::Auto
}
fn d_probe_bytes() -> usize {
    4096
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            ring_mb: 64,
            commit_interval_ms: 250,
            encoding: "raw".into(),
            max_line_bytes: 1024 * 1024,
            line_ending_mode: LineEndingMode::Auto,
            line_ending_probe_bytes: 4096,
            recovery_gadgets: Vec::new(),
        }
    }
}

impl CaptureConfig {
    /// The recovery-gadget signatures to watch for, parsed from config, falling
    /// back to the built-in defaults when none are configured. A malformed entry
    /// is dropped with a warning rather than silently matching nothing.
    pub fn recovery_signatures(&self) -> Vec<crate::usb::GadgetSig> {
        if self.recovery_gadgets.is_empty() {
            return crate::usb::default_recovery_signatures();
        }
        self.recovery_gadgets
            .iter()
            .filter_map(|s| {
                let sig = crate::usb::GadgetSig::parse(s);
                if sig.is_none() {
                    tracing::warn!(entry = %s, "ignoring malformed recovery_gadgets signature");
                }
                sig
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaptureOverride {
    pub ring_mb: Option<u64>,
    pub commit_interval_ms: Option<u64>,
    pub max_line_bytes: Option<usize>,
    pub line_ending_mode: Option<LineEndingMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub autosplit_quiet_s: u64,
    pub max_hours: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            autosplit_quiet_s: 300,
            max_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionOverride {
    pub autosplit_quiet_s: Option<u64>,
    pub max_hours: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    pub live_cap_gb: f64,
    /// "keep-all" or a duration like "30d".
    pub file_sessions: String,
    /// §F9. Prune verbatim bytes older than this. 0 = no age rule.
    ///
    /// The compressed knowledge IS the product: templates, epochs, stages,
    /// fingerprints, verdicts, metrics and version extractions are kept whatever
    /// happens here, and only the bytes they were derived from age out. The ADP
    /// alone reached 222k lines / 120 MB from one board in a few weeks with
    /// `pruned_before_offset` still 0, because nothing could age out.
    #[serde(default)]
    pub raw_keep_days: u64,
    /// Prune verbatim bytes beyond this many, per device. 0 = no size rule.
    #[serde(default)]
    pub raw_keep_bytes: u64,
    /// Epoch rows kept regardless of raw pruning.
    #[serde(default = "default_keep_epochs")]
    pub keep_epochs: u64,
    /// Never prune raw covered by a baseline epoch or an exported session.
    #[serde(default = "default_true")]
    pub protect_baselines: bool,
}

fn default_keep_epochs() -> u64 {
    500
}
fn default_true() -> bool {
    true
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            live_cap_gb: 2.0,
            file_sessions: "keep-all".into(),
            // OFF by default. A lab host that has been capturing for a month
            // must not lose bytes because it upgraded conminer; turning this on
            // is a decision, not a side effect.
            raw_keep_days: 0,
            raw_keep_bytes: 0,
            keep_epochs: default_keep_epochs(),
            protect_baselines: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetentionOverride {
    pub live_cap_gb: Option<f64>,
    pub file_sessions: Option<String>,
}

// -------------------------------------------------------- framing/mining -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FramerConfig {
    /// Retro-attach depth for inverted record shapes (Zephyr fault lines that
    /// precede the FATAL banner, Python tracebacks).
    pub lookback_lines: usize,
    /// Runaway-record cap; the record closes with a `truncated` flag.
    pub max_record_lines: usize,
    /// An open record with no continuation closes after this (DEAD_AIR).
    pub record_timeout_s: u64,
    /// Non-printable ratio over `garbage_window_bytes` that trips GARBAGE_BURST.
    pub garbage_threshold: f64,
    pub garbage_window_bytes: usize,
}

impl Default for FramerConfig {
    fn default() -> Self {
        Self {
            lookback_lines: 64,
            max_record_lines: 2000,
            record_timeout_s: 10,
            garbage_threshold: 0.30,
            garbage_window_bytes: 512,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MineConfig {
    pub similarity: f64,
    pub depth: usize,
    pub max_children: usize,
    /// Longer lines are mined on the first N tokens; stored whole regardless.
    pub max_line_tokens: usize,
}

impl Default for MineConfig {
    fn default() -> Self {
        Self {
            similarity: 0.4,
            depth: 4,
            max_children: 100,
            max_line_tokens: 128,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub fts: bool,
    pub window_default_lines: usize,
    pub window_max_lines: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            fts: true,
            window_default_lines: 20,
            window_max_lines: 200,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchOverride {
    pub fts: Option<bool>,
}

// ----------------------------------------------------------- interaction -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub char_delay_ms: u64,
    pub echo_timeout_ms: u64,
    pub command_timeout_s: u64,
    pub settle_quiet_ms: u64,
    /// Recovery ladder rungs 2-3, in order.
    pub escape_set: Vec<String>,
    /// §8.3 low-level `send` passthrough escape hatch.
    pub allow_raw_send: bool,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            char_delay_ms: 10,
            echo_timeout_ms: 200,
            command_timeout_s: 30,
            settle_quiet_ms: 500,
            escape_set: vec!["C-c".into(), "C-\\".into(), "C-d".into()],
            allow_raw_send: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunnerOverride {
    pub char_delay_ms: Option<u64>,
    pub echo_timeout_ms: Option<u64>,
    pub command_timeout_s: Option<u64>,
    pub settle_quiet_ms: Option<u64>,
    pub escape_set: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    pub hung_after_s: u64,
    /// Epochs required before `boot_looping` is claimed.
    pub loop_min_epochs: usize,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            hung_after_s: 30,
            loop_min_epochs: 3,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StateOverride {
    pub hung_after_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfig {
    pub ttl_s: u64,
    pub max_s: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            ttl_s: 900,
            max_s: 14_400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    pub power_timeout_s: u64,
    pub flash_timeout_s: u64,
    /// How long to let a board settle before judging what an action did.
    #[serde(default = "default_verify_settle_s")]
    pub verify_settle_s: u64,
    /// How long to watch the console for the effect of an `off`.
    #[serde(default = "default_verify_off_watch_s")]
    pub verify_off_watch_s: u64,
    /// ...and for an action that should bring the board BACK.
    #[serde(default = "default_verify_boot_watch_s")]
    pub verify_boot_watch_s: u64,
    /// How long to watch USB before stating a board is NOT in EDL.
    ///
    /// Measured on the ADP: a warm reset into download mode drops the QDL gadget
    /// and brings it back about six seconds later, so a shorter window can only
    /// produce the false negative round 4 filed as R4.
    #[serde(default = "default_edl_settle_s")]
    pub edl_settle_s: u64,
}

fn default_verify_settle_s() -> u64 {
    3
}
fn default_verify_off_watch_s() -> u64 {
    12
}
fn default_verify_boot_watch_s() -> u64 {
    30
}
fn default_edl_settle_s() -> u64 {
    8
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            power_timeout_s: 30,
            flash_timeout_s: 600,
            verify_settle_s: default_verify_settle_s(),
            verify_off_watch_s: default_verify_off_watch_s(),
            verify_boot_watch_s: default_verify_boot_watch_s(),
            edl_settle_s: default_edl_settle_s(),
        }
    }
}

fn default_max_snapshot_bytes() -> u64 {
    4 * 1024 * 1024
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CredentialsConfig {
    /// Path to a separate 0600 file. Never the registry DB, never an export.
    #[serde(default)]
    pub file: String,
}

// --------------------------------------------------------------- service -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpdConfig {
    pub bind: String,
    pub port: u16,
}

impl Default for McpdConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 8090,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub max_raw_lines: usize,
    /// Cap on a single `snapshot_dmesg` capture (§F4).
    ///
    /// A console runs at ~11 KB/s, so 4 MB is already six minutes of streaming;
    /// past that the caller wants `-l err,warn`, not patience.
    #[serde(default = "default_max_snapshot_bytes")]
    pub max_snapshot_bytes: u64,
    /// Advertise every tool, or just the core console/power set.
    ///
    /// DEFAULTS TO EVERYTHING, and the reasoning changed once a real client
    /// tried it. The core set was chosen to save context: measured, 19 tools
    /// cost ~14KB (~3.6k tokens) against ~52KB (~13k tokens) for all 69, paid at
    /// the start of every session.
    ///
    /// But "callable and discoverable through `help`" turned out to be false in
    /// practice. An MCP client binds the tools that `tools/list` returns and
    /// cannot call anything else, so the other 50 -- `decode`, `template_detail`,
    /// `get_records`, baselines, watches, bisect, evidence, expectations,
    /// `evaluate_policy` -- were unreachable without hand-built HTTP. A tool that
    /// cannot be called might as well not exist, and that costs far more than
    /// its schema.
    ///
    /// Set false on a context-constrained deployment that only needs the console
    /// and power basics.
    pub full_toolset: bool,
    pub max_results: usize,
    pub follow_timeout_max_s: u64,
    pub max_concurrent_follows: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_raw_lines: 200,
            max_snapshot_bytes: default_max_snapshot_bytes(),
            full_toolset: true,
            max_results: 100,
            follow_timeout_max_s: 600,
            max_concurrent_follows: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FollowConfig {
    pub default_timeout_s: u64,
}

impl Default for FollowConfig {
    fn default() -> Self {
        Self {
            default_timeout_s: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    pub coalesce_ms: u64,
    /// §K4. Where a watch is allowed to POST.
    ///
    /// MANDATORY, and empty means "nowhere". A watch payload carries console
    /// content -- kernel logs, command output, whatever the board printed -- and
    /// an unrestricted URL on a LAN-open endpoint is an exfiltration primitive
    /// that anyone who can reach the API can arm. Patterns are globbed against
    /// the URL, so `http://127.0.0.1:*` and `http://192.168.86.*` read the way
    /// an operator expects.
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            coalesce_ms: 5000,
            // Loopback AND the private ranges. A bench sink almost never runs
            // inside the same netns as mcpd -- ours did not, and a
            // loopback-only default made the feature untestable before it made
            // it safe.
            //
            // What the allowlist is actually for is keeping console content off
            // the public internet: a watch payload carries kernel logs and
            // command output, and an unrestricted URL on a LAN-open endpoint is
            // an exfiltration primitive. Private ranges are the same trust
            // boundary this tool already sits inside (the dashboard is LAN-open
            // by deliberate choice), so allowing them by default gives up
            // nothing the deployment had not already decided -- while a routable
            // address still has to be added by hand.
            //
            // §M2. THE PRIVATE RANGES COME FIRST, and loopback is last on
            // purpose. Under compose, `127.0.0.1` in a webhook URL is mcpd's own
            // container -- the single address class that can never reach a
            // receiver on the lab host -- and it was the first entry here, which
            // is precisely the one an operator copies. It stays allowed (a bare
            // metal install, or a sidecar in the same network namespace, is a
            // real deployment) but it stops being the example.
            allow: vec![
                "http://10.*".into(),
                "http://192.168.*".into(),
                "http://172.16.*".into(),
                "http://172.17.*".into(),
                "http://172.18.*".into(),
                "http://172.19.*".into(),
                "http://172.2?.*".into(),
                "http://172.3?.*".into(),
                "http://127.0.0.1:*".into(),
                "http://localhost:*".into(),
            ],
        }
    }
}

impl NotifyConfig {
    /// Is this URL one the operator has allowed?
    pub fn url_allowed(&self, url: &str) -> bool {
        self.allow.iter().any(|p| glob_match(p, url))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub max_gb: f64,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self { max_gb: 4.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IngestConfig {
    pub max_gb: f64,
    /// "auto" | "on" | "off"
    pub gzip: String,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_gb: 2.0,
            gzip: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub bind: String,
}

/// A hook template with everything the caller needs to render it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedHook {
    /// Controller-declared timeout, if any; see ControllerProfile.
    pub power_timeout_s: Option<u64>,
    pub template: String,
    /// The controller's tty, when the hook came from a controller profile.
    pub controller: Option<String>,
    pub off_settle_s: f64,
    /// Which profile supplied it, so a response can say *why* a board has these
    /// controls rather than presenting them as if from nowhere.
    pub source: String,
}

/// A board-controller driver, matched to hardware by name (§18.6).
///
/// Per-device `hooks` require someone to write config for every board before it
/// can be power-cycled, which does not survive a bench where boards come and go.
/// A controller profile is declarative and *auto-binds*: it says which by-id
/// names are controllers of this type, which consoles they power, and how to
/// drive them. Plug in a new board of a known type and its power controls
/// appear with no configuration at all.
///
/// This is the same shape as a framer profile, for the same reason: adding
/// hardware support should not mean touching Rust.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControllerProfile {
    pub name: String,
    /// Glob over `/dev/serial/by-id` names identifying the controller itself.
    #[serde(rename = "match")]
    pub match_glob: String,
    /// Glob over the consoles this controller powers. A console matching it
    /// inherits these hooks automatically.
    pub controls: String,
    /// `{action}`, `{device}`, `{controller}` and `{off_settle}` are substituted.
    pub power: Option<String>,
    /// `{mode}`, `{device}`, `{controller}` are substituted.
    pub boot_mode: Option<String>,
    /// Command that answers "is this board powered on?", printing `on`, `off`
    /// or `unknown`.
    ///
    /// Without this, conminer could only GUESS: `diagnose` said a silent console
    /// meant the board "may be powered off", and a human had to go read a
    /// controller line by hand to find out. The answer was always one query
    /// away. `{controller}` and `{device}` are substituted as for `power`.
    #[serde(default)]
    pub power_state: Option<String>,
    pub flash: Option<String>,
    #[serde(default)]
    pub boot_modes: Vec<String>,
    /// How long this controller's power/boot-mode hooks may take.
    ///
    /// A property of the CONTROLLER, not the deployment: the Bughopper claims a
    /// USB interface, holds PM_RESIN_N for 6s and settles, which measures ~35s
    /// wall -- past the 30s global default, so every `power off`/`cycle` through
    /// MCP returned HOOK_TIMEOUT with the action killed mid-flight or never
    /// actuated at all, while the Bantam hooks (1.7-5.4s) were unaffected.
    /// Falls back to `hooks.power_timeout_s` when unset.
    #[serde(default)]
    pub power_timeout_s: Option<u64>,
    /// Seconds to wait between cutting power and restoring it.
    ///
    /// A PMIC needs the rail to actually collapse before a restart takes, and
    /// the right number is a property of the board, not of the script. Passed to
    /// the hook as `{off_settle}` so a driver cannot silently disagree with it.
    #[serde(default = "d_off_settle")]
    pub off_settle_s: f64,
    /// Whether the controller itself should be kept out of discovery.
    ///
    /// Almost always yes: a controller is a command processor, not a console,
    /// and several of them (the Bantam among them) are single-session, so
    /// letting ser2net hold one would break board control outright.
    #[serde(default = "d_true")]
    pub exclude_from_discovery: bool,
    /// Does setting a boot mode PUT the board in it, or only arm the next boot?
    ///
    /// The two kinds are genuinely different hardware and the difference is not
    /// cosmetic. A Bantam LATCHES a strap: the board enters the mode on its next
    /// boot, so a reset (or power cycle) is required and the strap must later be
    /// cleared. A Bughopper drives FORCED_USB_BOOT_N directly and sequences the
    /// reset ITSELF, holding the strap across the sampling window -- setting the
    /// mode is the whole entry, and resetting afterwards boots the board straight
    /// back OUT of it.
    ///
    /// Measured: `boot_mode EDL` then `power reset` on the ADP left no QDL
    /// gadget, and read as "EDL is broken on this board" when in fact the second
    /// step had undone the first.
    #[serde(default)]
    pub mode_enters_immediately: bool,
}

/// Shared glob test, so controller matching and the discovery include/exclude
/// lists cannot drift into two different notions of "matches".
fn glob_match(pattern: &str, name: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(name))
        .unwrap_or(false)
}

fn d_off_settle() -> f64 {
    6.0
}
fn d_true() -> bool {
    true
}

/// One named region of a device's memory map (§18.3).
///
/// Supplied by config rather than parsed from a device tree on purpose: the map
/// an engineer is reasoning with during bring-up is often *not* what the DT
/// says, and a decode ring that silently disagreed with the person using it
/// would be worse than none.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryRegion {
    pub name: String,
    #[serde(deserialize_with = "de_u64")]
    pub base: u64,
    #[serde(deserialize_with = "de_u64")]
    pub size: u64,
    #[serde(default)]
    pub note: Option<String>,
}

/// Accept `0x88e1000` as a TOML string as well as a bare integer: a memory map
/// written in decimal is unreadable, and TOML has no hex literal.
fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    use serde::de::Error as _;
    match serde_json::Value::deserialize(d).map_err(D::Error::custom)? {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| D::Error::custom("address must be a non-negative integer")),
        serde_json::Value::String(s) => {
            let t = s.trim();
            let r = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => t.parse::<u64>(),
            };
            r.map_err(|e| D::Error::custom(format!("bad address {s:?}: {e}")))
        }
        other => Err(D::Error::custom(format!(
            "address must be an integer or a hex string, got {other}"
        ))),
    }
}

/// §17 — the human dashboard.
///
/// Deliberately separate from `mcpd`: this surface is for people, it streams a
/// console straight through to a browser, and it can transmit. Keeping it its
/// own process and its own port means it can be switched off entirely on a host
/// that should only serve agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    pub bind: String,
    /// Whether a browser may transmit to a console at all. Watching is always
    /// allowed; this is the one switch that makes the dashboard read-only.
    pub allow_tx: bool,
    /// Lines of console history a newly-attached browser is shown.
    pub scrollback_lines: usize,
    /// How often the device list is re-read from the registry. discoveryd polls
    /// hotplug at 1 Hz, so anything faster only burns CPU.
    pub refresh_ms: u64,
    /// Hostname to advertise in the connection strings shown to humans. Empty
    /// means "whatever host the browser used", which is right when the
    /// dashboard and ser2net are reachable at the same address.
    pub advertise_host: String,
    /// Whether the dashboard may actuate hardware (power, boot mode).
    ///
    /// Separate from `allow_tx`: typing into a console and power-cycling a board
    /// are different sizes of mistake, and a rig can reasonably want one without
    /// the other.
    pub allow_power: bool,
    /// Where mcpd is, so the dashboard's hardware buttons go through the same
    /// path an agent uses instead of becoming a second way to touch the board.
    pub mcp_url: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            allow_tx: true,
            scrollback_lines: 2000,
            refresh_ms: 1000,
            advertise_host: String::new(),
            allow_power: true,
            mcp_url: default_mcp_url(in_test_mode()),
        }
    }
}

/// True when this process is a test run (`CONMINER_TEST_MODE=1`, set by the dev
/// container).
fn in_test_mode() -> bool {
    std::env::var("CONMINER_TEST_MODE").is_ok_and(|v| v == "1")
}

/// Where the dashboard sends actuation -- AND A DEAD ADDRESS UNDER TEST.
///
/// The test container shares the `conminer` docker network with the live stack,
/// so the name `mcpd` resolves there to the RUNNING mcpd of this bench, which
/// federates to every peer. Any rig that left this default and posted
/// `/api/power/...` was therefore one resolvable selector away from power-
/// cycling real hardware -- and `allow_power` defaults to true, so nothing else
/// stood in the way. Measured: from inside the dev container,
/// `curl http://mcpd:8090/healthz` answers 200.
///
/// Port 1 is reserved and never listened on, so a test that reaches this far
/// gets a connection refused it can assert about, instead of a board that moves.
pub fn default_mcp_url(test_mode: bool) -> String {
    if test_mode {
        "http://127.0.0.1:1/mcp".into()
    } else {
        "http://mcpd:8090/mcp".into()
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9090".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TimeConfig {
    pub store: String,
}

impl Default for TimeConfig {
    fn default() -> Self {
        Self {
            store: "utc".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    pub data_dir: PathBuf,
    pub profiles_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/conminer"),
            profiles_dir: PathBuf::from("/etc/conminer/profiles.d"),
            run_dir: PathBuf::from("/run/conminer"),
        }
    }
}

// ------------------------------------------------------------------ root -----

/// The whole of `conminer.toml`.
///
/// `Default` is derived, so any field whose default is not its type's `Default`
/// carries `#[serde(default = "…")]` *and* a matching `#[default_with]`-style
/// initialiser below. `controllers` is the one such field: an empty controller
/// list would mean a fresh host recognises no hardware at all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Rig-wide memory map, inherited by any device without its own.
    #[serde(default = "d_memory_map")]
    pub memory_map: Vec<MemoryRegion>,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub ser2net: Ser2netConfig,
    #[serde(default)]
    pub attach: AttachConfig,
    #[serde(default)]
    pub line: LineConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub framer: FramerConfig,
    #[serde(default)]
    pub mine: MineConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub runner: RunnerConfig,
    #[serde(default)]
    pub state: StateConfig,
    #[serde(default)]
    pub lease: LeaseConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub credentials: CredentialsConfig,
    #[serde(default)]
    pub mcpd: McpdConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub follow: FollowConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub export: ExportConfig,
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub time: TimeConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    /// §P1. Fleet peering: this node's identity and how it finds the others.
    #[serde(default)]
    pub peers: PeersConfig,
    /// Per-device overrides, keyed by selector (nickname or canonical id).
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceOverride>,
    /// Board-controller drivers, auto-bound by name (§18.6).
    #[serde(default = "d_controllers")]
    pub controllers: Vec<ControllerProfile>,
}

/// §P1. FLEET PEERING.
///
/// Two conminer instances on a LAN find each other, share what hardware each
/// one owns, and let either drive the other's boards. The node physically
/// attached to a board is its OWNER: it runs the only miner and the only store
/// for that board, and remote nodes proxy to it. That keeps one source of truth
/// for epochs, templates and leases -- the alternative, mirrored mining, means
/// two stores disagreeing about the same console at 3am.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeersConfig {
    /// Whether this node participates in a fleet at all.
    pub enabled: bool,
    /// This node's name in the fleet. Empty means "use the persisted identity",
    /// which is generated once and kept in `<data_dir>/instance.json`.
    ///
    /// The NAME is what a human and an agent type (`power {device: "alpha/3.2"}`);
    /// the instance id underneath it is what the protocol matches on, so renaming
    /// a node never makes it look like a different one.
    pub name: String,
    /// The address other nodes should use to reach this one. Empty means "guess
    /// the primary interface address".
    ///
    /// This is where the container-loopback trap bites hardest (§M2): a node
    /// that advertises 127.0.0.1 is advertising the PEER's own container. The
    /// guess deliberately never returns loopback, and `check-config` rejects it.
    pub advertise_host: String,
    /// §P3. Announce this node's inventory to every peer that answers.
    ///
    /// Inventory is otherwise a PULL, and a node that can open no connections
    /// therefore learns nothing -- however many peers can open one to it. On a
    /// bench where one host sits upstream of a NAT that is the difference
    /// between seeing the fleet and seeing nothing. Cheap on a small fleet: one
    /// extra call per peer per tick, and only to peers that just answered.
    pub announce: bool,
    /// UDP beacon port. Broadcast, so it must be the same on every node.
    pub udp_port: u16,
    pub beacon_interval_s: u64,
    /// How long a peer stays live after its last advert. Rows from an expired
    /// peer are held briefly (see the grace in `inventory`) rather than deleted
    /// on the first missed beat: a blip must not churn every port on the bench.
    pub ttl_s: u64,
    pub inventory_interval_s: u64,
    /// Statically configured peers, as mcpd URLs. These never expire -- they are
    /// how a fleet works across a subnet boundary that broadcast cannot cross
    /// (a NAT-ed host talking to a lab host, for one), and they are the seam
    /// the two-node tests drive.
    pub nodes: Vec<String>,
}

impl Default for PeersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            advertise_host: String::new(),
            announce: true,
            udp_port: 49111,
            beacon_interval_s: 5,
            ttl_s: 30,
            inventory_interval_s: 5,
            nodes: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            memory_map: d_memory_map(),
            peers: Default::default(),
            discovery: Default::default(),
            ser2net: Default::default(),
            attach: Default::default(),
            line: Default::default(),
            capture: Default::default(),
            session: Default::default(),
            retention: Default::default(),
            framer: Default::default(),
            mine: Default::default(),
            search: Default::default(),
            runner: Default::default(),
            state: Default::default(),
            lease: Default::default(),
            hooks: Default::default(),
            credentials: Default::default(),
            mcpd: Default::default(),
            api: Default::default(),
            follow: Default::default(),
            notify: Default::default(),
            export: Default::default(),
            ingest: Default::default(),
            log: Default::default(),
            metrics: Default::default(),
            dashboard: Default::default(),
            time: Default::default(),
            paths: Default::default(),
            devices: Default::default(),
            controllers: d_controllers(),
        }
    }
}

/// Controller profiles that ship with the binary.
///
/// Built in for the same reason the framer profiles are: a lab host should
/// recognise known hardware the moment it is plugged in, without someone first
/// writing config for it. `conminer.toml` restates them so they remain
/// editable, and a test pins the two together.
/// MMIO blocks seen on this rig's Qualcomm boards, from their own boot logs and
/// device trees.
///
/// Sized to the block, not guessed: each is the window the driver maps. These
/// exist so `decode` can answer "what is at 0x0a600000?" the first time someone
/// asks, instead of returning `regions_known: 0` forever because per-device
/// config was never written.
fn d_memory_map() -> Vec<MemoryRegion> {
    let r = |name: &str, base: u64, size: u64, note: &str| MemoryRegion {
        name: name.into(),
        base,
        size,
        note: Some(note.into()),
    };
    vec![
        r(
            "dwc3-usb",
            0x0a60_0000,
            0x10_0000,
            "USB3 DWC3 controller; the ADP's flapping port lives here",
        ),
        r("gmu", 0x03d6_a000, 0x1000, "Adreno GMU; CM3 init timeouts"),
        r(
            "mdss-dpu",
            0x0ae0_1000,
            0x9_0000,
            "MDSS display processing unit",
        ),
        r("dp0", 0x0af5_4000, 0x8000, "DisplayPort 0 (wired output)"),
        r("dp1", 0x0af5_c000, 0x8000, "DisplayPort 1 (wired output)"),
        r(
            "combo-phy0",
            0x088e_1000,
            0x3000,
            "USB/DP combo PHY; XBL's base, not the DTS copy-paste value",
        ),
        r(
            "combo-phy1",
            0x088e_4000,
            0x3000,
            "USB/DP combo PHY (second)",
        ),
        r("gic-its", 0x1704_0000, 0x2_0000, "GICv3 ITS"),
        r(
            "smmu-apps",
            0x15a0_0000,
            0x10_0000,
            "APPS SMMU; SACR.CACHE_LOCK complaints reference this",
        ),
    ]
}

fn d_controllers() -> Vec<ControllerProfile> {
    vec![
        ControllerProfile {
            power_timeout_s: None,
            name: "bantam".into(),
            match_glob: "*Bantam*".into(),
            // Any console on the same board. Which board is decided by USB
            // TOPOLOGY (controller_port_for), not by this glob: a bench can hold
            // several Bantam-driven boards -- an IQ10 and a NordAU RIDE SX were the
            // first pair -- and a name glob cannot tell them apart. Narrowing this
            // to one board name is what left a newly plugged board with no power
            // control at all.
            controls: "*".into(),
            // `--port {controller}` is MANDATORY, not cosmetic.
            //
            // These templates used to omit it. The resolved controller was
            // therefore substituted nowhere and silently discarded, so the hook
            // fell back to its own default port and drove WHICHEVER board sat
            // there. Measured: asking conminer to power off the RIDE powered off
            // the IQ10 (RIDE PS_HOLD stayed 1, IQ10 went 1->0), and pressing
            // power-off on the Bughopper board in the dashboard did the same --
            // both answering ok:true verified:true, because the board that DID
            // get hit read its line back correctly.
            //
            // Naming {controller} here also arms the refusal in
            // power_hook_for_at: a template that mentions {controller} will not
            // be handed back at all when none can be resolved.
            power: Some("bantam-power {action} --settle {off_settle} --port {controller}".into()),
            boot_mode: Some("bantam-power mode {mode} --port {controller}".into()),
            power_state: Some("bantam-power power-state --port {controller}".into()),
            flash: None,
            boot_modes: vec![
                "BOOT_MD_EDL".into(),
                "BOOT_SS_EDL".into(),
                "BOOT_UEFI".into(),
                "MD_FASTBOOT".into(),
                "SS_MD_FASTBOOT".into(),
            ],
            off_settle_s: 6.0,
            exclude_from_discovery: true,
            // A Bantam latches the strap: the board enters the mode on its NEXT
            // boot, so an entry is mode-then-reset and the strap needs clearing
            // afterwards.
            mode_enters_immediately: false,
        },
        ControllerProfile {
            name: "bughopper".into(),
            match_glob: "*Bughopper*".into(),
            // A Bughopper controls the board whose console it also carries: the
            // same FTDI exposes one UART for the AP/Linux console and drives
            // power/reset/EDL on its CBUS pins.
            controls: "*Bughopper*".into(),
            power: Some(
                "conminer bughopper-power {action} --settle {off_settle} --device {device}".into(),
            ),
            boot_mode: Some("conminer bughopper-power mode {mode} --device {device}".into()),
            // Generic surface, honest answer: this controller CAN be queried,
            // and what it reports is "unknown" -- its CBUS pins are outputs
            // driving the power button, with no sense line back from the board,
            // and they park ALL_LOW after every action so a readback cannot tell
            // "off" from "idle after powering on". Wiring it up anyway means the
            // question is asked the same way for every controller and the answer
            // is explicit rather than missing.
            power_state: Some("conminer bughopper-power power-state --device {device}".into()),
            flash: None,
            // CBUS gives one boot-mode line (FORCED_USB_BOOT_N), so EDL is the only
            // mode this controller can select. Claiming more would be a lie.
            boot_modes: vec!["EDL".into()],
            // This controller pulses reset itself and holds the strap across the
            // sampling window: setting the mode IS the entry. A reset afterwards
            // boots the board back out of EDL.
            mode_enters_immediately: true,
            // Claiming the USB interface, holding PM_RESIN_N for 6s and settling
            // measures ~35s wall -- past the 30s global default, which made
            // `power off` and `cycle` unusable through MCP on this board.
            power_timeout_s: Some(60),
            // PMIC long-press.
            off_settle_s: 6.0,
            // NOT excluded, unlike the Bantam. The Bantam is a separate control-only
            // CDC device that ser2net must never hold; the Bughopper's FTDI *is* the
            // console, so excluding it would delete the very port we want.
            exclude_from_discovery: false,
        },
        ControllerProfile {
            name: "tac".into(),
            // A Qualcomm TAC ("Alpaca") debug board: one FT4232H whose channels
            // are split by EEPROM into UARTs and GPIO ports. The by-id name
            // carries the USB product string, which is exactly what the vendor's
            // own catalogue matches on, so a board of a known type arrives with
            // working controls and no configuration -- which is the point.
            //
            // The glob is the product, not the board: `RIDE MICRO 4.0` becomes
            // `RIDE_MICRO_4.0` in a by-id name. Adding a TAC board type means
            // adding its pin map (built in, or a `.tcnf` in /etc/conminer/tac.d)
            // and one more profile here.
            match_glob: "*RIDE_MICRO*".into(),
            // It drives the board whose consoles it also carries, like a
            // Bughopper: the GPIO channels and the UARTs are one chip.
            controls: "*RIDE_MICRO*".into(),
            power: Some(
                "conminer tac-power {action} --settle {off_settle} --device {device}".into(),
            ),
            boot_mode: Some("conminer tac-power mode {mode} --device {device}".into()),
            // Asked like every controller; answers "unknown" and says why. The
            // TAC has one candidate sense line (`md_resout`, the SoC reporting
            // back through the TAC's buffer) and it has not yet been shown to
            // track a real power cycle on this board -- so the readback is
            // reported as evidence, not as a verdict.
            power_state: Some("conminer tac-power power-state --device {device}".into()),
            flash: None,
            // Four real modes, each a distinct strap sequence in the vendor's
            // own config. `SAIL_EDL` is not a synonym for `EDL`: one straps the
            // main domain as well.
            boot_modes: vec![
                "EDL".into(),
                "SAIL_EDL".into(),
                "UEFI".into(),
                "FASTBOOT".into(),
            ],
            // Every mode sequence powers the board down, straps, powers up and
            // releases across the sampling window: setting the mode IS the
            // entry, and a reset afterwards boots the board back out of it.
            mode_enters_immediately: true,
            // The longest sequence (UEFI) holds for 8s after a 1.5s off, and
            // EDL for 5s; the 30s global default leaves no margin for the USB
            // claim on top.
            power_timeout_s: Some(60),
            // The rail collapse the vendor's own sequences wait out. NOT the
            // Bantam's 6s: this is a level on a load switch, not a PMIC
            // long-press.
            off_settle_s: 1.5,
            // Its GPIO channels never appear as ttys (they are claimed away from
            // ftdi_sio), and its UART channels are the consoles we are here to
            // serve. Excluding it would delete them.
            exclude_from_discovery: false,
        },
    ]
}

/// Bad-value report from `check-config`, carrying the offending key.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigProblem {
    pub key: String,
    pub problem: String,
}

/// Deep-merge `over` onto `base`. Tables merge key-wise; every other value is
/// replaced wholesale (so `exclude = []` really means "nothing", not "unchanged").
fn merge_toml(base: &mut toml::Value, over: &toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) => merge_toml(bv, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Attach the offending source line to a deserialization error.
///
/// The merged-defaults parse below loses TOML spans, so we recover the location
/// the way a human would: find the key serde complained about in the original
/// text. A config error that does not point at a line is a config error the
/// operator has to bisect by hand.
fn locate(text: &str, message: String, span: Option<std::ops::Range<usize>>) -> ToolError {
    let mut err = ToolError::new(ErrorCode::InvalidConfig, message.clone());

    let mut located = None;
    if let Some(span) = span {
        let line = text[..span.start.min(text.len())].lines().count().max(1);
        located = Some(line);
    }
    if located.is_none() {
        // e.g. "unknown field `char_delay_msec`, expected one of ..."
        if let Some(start) = message.find('`') {
            if let Some(end) = message[start + 1..].find('`') {
                let key = &message[start + 1..start + 1 + end];
                located = text
                    .lines()
                    .position(|l| {
                        let l = l.trim();
                        l.starts_with(key) && l[key.len()..].trim_start().starts_with('=')
                    })
                    .map(|i| i + 1);
            }
        }
    }

    if let Some(line) = located {
        let t = text.lines().nth(line - 1).unwrap_or("");
        err = err.with_detail(serde_json::json!({ "line": line, "text": t.trim() }));
    }
    err
}

/// The physical board a device belongs to, derived from USB topology.
///
/// Devices sharing a DOWNSTREAM hub are one board's harness: on this bench the
/// IQ10's FT4232 sits at `3.2.2` with its Bantam at `3.2.4`, while a NordAU RIDE
/// SX sits under `3.1` with its own Bantam. Both controllers match `*Bantam*`,
/// so name matching alone cannot say which one drives which board -- and
/// picking the first match would power-cycle the wrong board.
///
/// A parent only counts when it is itself below the root: stripping the last
/// component blindly would take `3.3` up to `3` and merge every board on the
/// host into one group.
pub fn topology_group(by_path: Option<&str>) -> Option<String> {
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
        Some((parent, _)) if parent.contains('.') => Some(parent.to_string()),
        _ => Some(chain.to_string()),
    }
}

impl Config {
    /// Parse without range validation.
    ///
    /// Sections are merged over the compiled defaults, so a file may set a
    /// single key (`[mine] similarity = 0.55`) without restating its siblings —
    /// while `deny_unknown_fields` still rejects typos, because the merged table
    /// carries the unknown key straight into the final deserialize.
    pub fn parse_unvalidated(s: &str) -> Result<Self> {
        let user: toml::Value =
            toml::from_str(s).map_err(|e| locate(s, e.message().to_string(), e.span()))?;
        let mut merged = toml::Value::try_from(Config::default()).map_err(ToolError::internal)?;
        merge_toml(&mut merged, &user);
        merged
            .try_into()
            .map_err(|e: toml::de::Error| locate(s, e.message().to_string(), e.span()))
    }

    pub fn from_toml_str(s: &str) -> Result<Self> {
        let cfg = Self::parse_unvalidated(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// `conminer check-config`: report *every* problem, not just the first, so a
    /// misconfigured lab host is fixed in one pass.
    pub fn check(s: &str) -> Result<Vec<ConfigProblem>> {
        Ok(Self::parse_unvalidated(s)?.problems())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ToolError::new(
                ErrorCode::InvalidConfig,
                format!("cannot read {}: {e}", path.display()),
            )
        })?;
        let mut cfg = Self::from_toml_str(&text)?;
        cfg.apply_env(&std_env_pairs());
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `$CONMINER_CONFIG` if set and present, else defaults.
    pub fn load_or_default() -> Result<Self> {
        match std::env::var("CONMINER_CONFIG") {
            Ok(p) if Path::new(&p).exists() => Self::load(Path::new(&p)),
            _ => {
                let mut cfg = Config::default();
                cfg.apply_env(&std_env_pairs());
                cfg.validate()?;
                Ok(cfg)
            }
        }
    }

    /// Env overrides. `CONMINER_<SECTION>_<KEY>`, e.g. `CONMINER_MCPD_PORT=9000`.
    /// Also honours the container-level `CONMINER_DATA` / `CONMINER_PROFILES`.
    pub fn apply_env(&mut self, env: &[(String, String)]) {
        for (k, v) in env {
            match k.as_str() {
                "CONMINER_DATA" => self.paths.data_dir = PathBuf::from(v),
                "CONMINER_PROFILES" => self.paths.profiles_dir = PathBuf::from(v),
                "CONMINER_RUN" => self.paths.run_dir = PathBuf::from(v),
                "CONMINER_MCPD_BIND" => self.mcpd.bind = v.clone(),
                "CONMINER_MCPD_PORT" => {
                    if let Ok(p) = v.parse() {
                        self.mcpd.port = p;
                    }
                }
                // §P1. Peering, so a fleet can be configured by compose env
                // without a per-node config file.
                "CONMINER_PEERS_ENABLED" => {
                    self.peers.enabled = matches!(v.as_str(), "1" | "true" | "yes" | "on")
                }
                "CONMINER_PEERS_NAME" => self.peers.name = v.clone(),
                "CONMINER_PEERS_ADVERTISE_HOST" => self.peers.advertise_host = v.clone(),
                "CONMINER_PEERS_NODES" => {
                    self.peers.nodes = v
                        .split(',')
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                "CONMINER_LOG_LEVEL" => self.log.level = v.clone(),
                "CONMINER_METRICS_BIND" => self.metrics.bind = v.clone(),
                "CONMINER_LINE_BAUD" => {
                    if let Ok(b) = v.parse() {
                        self.line.baud = b;
                    }
                }
                "CONMINER_SER2NET_HOST" => self.ser2net.connect_host = v.clone(),
                "CONMINER_SER2NET_BASE_PORT" => {
                    if let Ok(p) = v.parse() {
                        self.ser2net.base_port = p;
                    }
                }
                "CONMINER_MINE_SIMILARITY" => {
                    if let Ok(s) = v.parse() {
                        self.mine.similarity = s;
                    }
                }
                "CONMINER_SEARCH_FTS" => {
                    self.search.fts = matches!(v.as_str(), "1" | "true" | "on" | "yes")
                }
                // §H2. A rig arms its own retention here rather than in
                // conminer.toml, which documents the COMPILED DEFAULTS -- and
                // those stay off, because a host that has been capturing for a
                // month must not start discarding bytes as a side effect of an
                // upgrade. Set per deployment, visible in one place, reversible
                // without a rebuild. Nothing is deleted until `prune` is called;
                // these only decide what it would take when called bare.
                "CONMINER_RETENTION_RAW_KEEP_DAYS" => {
                    if let Ok(d) = v.parse() {
                        self.retention.raw_keep_days = d;
                    }
                }
                "CONMINER_RETENTION_RAW_KEEP_BYTES" => {
                    if let Ok(b) = v.parse() {
                        self.retention.raw_keep_bytes = b;
                    }
                }
                _ => {}
            }
        }
    }

    /// Range and consistency checks. Every problem is reported, not just the first.
    pub fn problems(&self) -> Vec<ConfigProblem> {
        let mut p = Vec::new();
        let mut bad = |key: &str, problem: String| {
            p.push(ConfigProblem {
                key: key.into(),
                problem,
            })
        };

        if !(0.0..=1.0).contains(&self.mine.similarity) {
            bad(
                "mine.similarity",
                format!("must be in 0.0..=1.0, got {}", self.mine.similarity),
            );
        }
        if self.mine.depth < 3 {
            bad(
                "mine.depth",
                format!("Drain requires depth >= 3, got {}", self.mine.depth),
            );
        }
        if self.mine.max_children == 0 {
            bad("mine.max_children", "must be > 0".into());
        }
        if self.mine.max_line_tokens == 0 {
            bad("mine.max_line_tokens", "must be > 0".into());
        }
        if !(0.0..=1.0).contains(&self.framer.garbage_threshold) {
            bad(
                "framer.garbage_threshold",
                format!(
                    "must be a ratio in 0.0..=1.0, got {}",
                    self.framer.garbage_threshold
                ),
            );
        }
        if self.framer.garbage_window_bytes == 0 {
            bad("framer.garbage_window_bytes", "must be > 0".into());
        }
        if self.framer.max_record_lines == 0 {
            bad("framer.max_record_lines", "must be > 0".into());
        }
        if self.capture.max_line_bytes < 64 {
            bad(
                "capture.max_line_bytes",
                format!("must be >= 64, got {}", self.capture.max_line_bytes),
            );
        }
        if self.capture.encoding != "raw" {
            bad(
                "capture.encoding",
                format!("only \"raw\" is supported, got {:?}", self.capture.encoding),
            );
        }
        if self.ser2net.base_port < 1024 {
            bad(
                "ser2net.base_port",
                format!("must be >= 1024, got {}", self.ser2net.base_port),
            );
        }
        if self.api.max_raw_lines == 0 || self.api.max_results == 0 {
            bad("api.max_raw_lines", "caps must be > 0".into());
        }
        if self.api.follow_timeout_max_s == 0 {
            bad("api.follow_timeout_max_s", "must be > 0".into());
        }
        if self.follow.default_timeout_s > self.api.follow_timeout_max_s {
            bad(
                "follow.default_timeout_s",
                format!(
                    "exceeds api.follow_timeout_max_s ({})",
                    self.api.follow_timeout_max_s
                ),
            );
        }
        if self.lease.ttl_s > self.lease.max_s {
            bad(
                "lease.ttl_s",
                format!("exceeds lease.max_s ({})", self.lease.max_s),
            );
        }
        if self.retention.file_sessions != "keep-all"
            && parse_duration_days(&self.retention.file_sessions).is_none()
        {
            bad(
                "retention.file_sessions",
                format!(
                    "must be \"keep-all\" or a duration like \"30d\", got {:?}",
                    self.retention.file_sessions
                ),
            );
        }
        if !matches!(self.ingest.gzip.as_str(), "auto" | "on" | "off") {
            bad(
                "ingest.gzip",
                format!("must be auto|on|off, got {:?}", self.ingest.gzip),
            );
        }
        if !matches!(self.time.store.as_str(), "utc") {
            bad("time.store", "only \"utc\" is supported".into());
        }
        if !matches!(
            self.log.level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            bad(
                "log.level",
                format!(
                    "must be trace|debug|info|warn|error, got {:?}",
                    self.log.level
                ),
            );
        }
        if !(1..=2).contains(&self.line.stop_bits) {
            bad("line.stop_bits", "must be 1 or 2".into());
        }
        if !(5..=8).contains(&self.line.data_bits) {
            bad("line.data_bits", "must be 5..=8".into());
        }
        if self.line.baud == 0 {
            bad("line.baud", "must be > 0".into());
        }
        if self.line.tx_line_ending.is_empty() {
            bad("line.tx_line_ending", "must not be empty".into());
        }
        if self.discovery.poll_fallback_hz == 0 {
            bad("discovery.poll_fallback_hz", "must be > 0".into());
        }
        for g in self.discovery.include.iter().chain(&self.discovery.exclude) {
            if globset::Glob::new(g).is_err() {
                bad("discovery.include", format!("invalid glob {g:?}"));
            }
        }
        for (sel, dev) in &self.devices {
            if let Some(line) = &dev.line {
                if line.baud == 0 {
                    bad(&format!("devices.{sel}.line.baud"), "must be > 0".into());
                }
            }
            for pat in dev.prompts.iter().chain(&dev.credential_gates) {
                if let Err(e) = regex::Regex::new(pat) {
                    bad(
                        &format!("devices.{sel}.prompts"),
                        format!("invalid regex {pat:?}: {e}"),
                    );
                }
            }
        }
        p
    }

    pub fn validate(&self) -> Result<()> {
        let problems = self.problems();
        if problems.is_empty() {
            return Ok(());
        }
        let first = &problems[0];
        Err(ToolError::new(
            ErrorCode::InvalidConfig,
            format!("{}: {}", first.key, first.problem),
        )
        .with_detail(serde_json::json!({ "problems": problems })))
    }

    // ---- effective per-device resolution (§3.2 layering) ----

    /// Line settings for a device: built-in default → `[line]` → per-device.
    pub fn line_for(&self, selector: &str) -> LineConfig {
        match self.devices.get(selector).and_then(|d| d.line.clone()) {
            Some(l) => l,
            None => self.line.clone(),
        }
    }

    pub fn runner_for(&self, selector: &str) -> RunnerConfig {
        let mut r = self.runner.clone();
        if let Some(o) = self.devices.get(selector).and_then(|d| d.runner.as_ref()) {
            if let Some(v) = o.char_delay_ms {
                r.char_delay_ms = v;
            }
            if let Some(v) = o.echo_timeout_ms {
                r.echo_timeout_ms = v;
            }
            if let Some(v) = o.command_timeout_s {
                r.command_timeout_s = v;
            }
            if let Some(v) = o.settle_quiet_ms {
                r.settle_quiet_ms = v;
            }
            if let Some(v) = &o.escape_set {
                r.escape_set = v.clone();
            }
        }
        r
    }

    /// The per-device overrides for a console, matched by EITHER its port path
    /// or the label an operator hung on it.
    ///
    /// Both, because a port path is the identity but a label is what a human
    /// types into a config file. Round 5 made the port the identity everywhere,
    /// and a map keyed by `[devices."board-a"]` silently stopped applying --
    /// the decode suite caught it resolving against the rig-wide map instead.
    /// Config should not care which name you used.
    fn device_override(&self, keys: &[&str]) -> Option<&DeviceOverride> {
        keys.iter()
            .filter(|k| !k.is_empty())
            .find_map(|k| self.devices.get(*k))
    }

    pub fn capture_for(&self, selector: &str) -> CaptureConfig {
        let mut c = self.capture.clone();
        if let Some(o) = self
            .device_override(&[selector])
            .and_then(|d| d.capture.as_ref())
        {
            if let Some(v) = o.ring_mb {
                c.ring_mb = v;
            }
            if let Some(v) = o.commit_interval_ms {
                c.commit_interval_ms = v;
            }
            if let Some(v) = o.max_line_bytes {
                c.max_line_bytes = v;
            }
            if let Some(v) = o.line_ending_mode {
                c.line_ending_mode = v;
            }
        }
        c
    }

    pub fn fts_for(&self, selector: &str) -> bool {
        self.devices
            .get(selector)
            .and_then(|d| d.search.as_ref())
            .and_then(|s| s.fts)
            .unwrap_or(self.search.fts)
    }

    /// The USB port paths belonging to a board, by port path or label (§L6).
    pub fn usb_ports_for(&self, keys: &[&str]) -> Vec<String> {
        self.device_override(keys)
            .map(|d| d.usb_ports.clone())
            .unwrap_or_default()
    }

    pub fn hung_after_s_for(&self, keys: &[&str]) -> u64 {
        self.device_override(keys)
            .and_then(|d| d.state.as_ref())
            .and_then(|s| s.hung_after_s)
            .unwrap_or(self.state.hung_after_s)
    }

    /// Where to reach ser2net for a console endpoint.
    ///
    /// The single implementation of this rule. It previously existed as four
    /// near-identical copies — in minerd, dashd, mcpd's tool surface and the
    /// dashboard's console proxy — and was fixed three separate times, each fix
    /// covering only the copy that happened to fail that day. Deriving the
    /// *connect* address from ser2net's *bind* address lands on 127.0.0.1,
    /// which under compose is the calling container itself.
    pub fn ser2net_host(&self) -> String {
        if !self.ser2net.connect_host.is_empty() {
            return self.ser2net.connect_host.clone();
        }
        match self.ser2net.bind.as_str() {
            "0.0.0.0" | "" | "::" => "127.0.0.1".to_string(),
            other => other.to_string(),
        }
    }

    /// This board's memory map, for address decoding (§18.3).
    ///
    /// Per-device only: two boards on one host have different maps, and a
    /// global default would decode an address against the wrong silicon, which
    /// is worse than not decoding it.
    /// This device's memory map, falling back to the rig-wide one.
    ///
    /// `decode` resolved zero regions on every call because the map is
    /// per-device config and nobody had written it -- so address-to-region
    /// resolution, the part that matters most for crash triage, was inert while
    /// the errno/ESR/GIC decoding worked. Most MMIO on a bench is shared across
    /// boards of the same SoC family, so a rig-wide map makes the feature work
    /// out of the box and a per-device map still overrides it entirely.
    pub fn memory_map_for(&self, keys: &[&str]) -> Vec<MemoryRegion> {
        let own = self
            .device_override(keys)
            .map(|d| d.memory_map.clone())
            .unwrap_or_default();
        if own.is_empty() {
            return self.memory_map.clone();
        }
        own
    }

    /// The power hook for a console, and the substitutions it needs.
    ///
    /// A per-device `hooks.power` wins when present — an operator overriding one
    /// board must not be silently outvoted by a profile — and otherwise the
    /// auto-bound controller supplies it. Returns the template plus the
    /// controller-derived values so the caller does not have to re-resolve them.
    /// Topology-aware variant: `present` carries each candidate's by-path so the
    /// controller on the SAME board is chosen. Prefer this wherever by-path is
    /// available; the name-only version cannot tell two identical controllers
    /// apart.
    pub fn power_hook_for_at<'a>(
        &self,
        selector: &str,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Option<ResolvedHook> {
        if let Some(t) = self
            .devices
            .get(selector)
            .and_then(|d| d.hooks.power.clone())
        {
            return Some(ResolvedHook {
                power_timeout_s: None,
                template: t,
                controller: None,
                off_settle_s: d_off_settle(),
                source: "device".into(),
            });
        }
        let c = self.controller_for(canonical)?;
        let template = c.power.clone()?;
        let self_present = present.clone().into_iter().any(|(n, _)| n == canonical);
        let controller = self.controller_port_for(canonical, by_path, present);
        // REFUSE rather than run a hook that has to guess which board it drives.
        //
        // A template containing `{controller}` is saying, in its own words, that
        // it cannot act without knowing WHICH controller. When the topology check
        // correctly declined to bind one (the only Bantams were on other USB
        // branches), this used to hand back the hook anyway with controller:
        // None; the command then ran with no port and the script fell back to its
        // default, /dev/ttyACM0 -- another board entirely. Pressing power-off on
        // the Bughopper board in the dashboard powered off the IQ10, and reported
        // ok:true verified:true, because the IQ10's controller really did read
        // back PWR_OFF=1.
        //
        // No controller resolved plus a template that needs one means NO HOOK.
        if !Self::template_can_run(&template, controller.as_deref(), self_present) {
            return None;
        }
        Some(ResolvedHook {
            power_timeout_s: c.power_timeout_s,
            template,
            controller,
            off_settle_s: c.off_settle_s,
            source: c.name.clone(),
        })
    }

    /// Resolve the command that answers "is this board powered on?".
    ///
    /// Same refusal rule as `power_hook_for_at`: a query aimed at an unknown
    /// controller would report ANOTHER board's power state, which is worse than
    /// reporting nothing.
    pub fn power_state_hook_for_at<'a>(
        &self,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Option<ResolvedHook> {
        let c = self.controller_for(canonical)?;
        let template = c.power_state.clone()?;
        let self_present = present.clone().into_iter().any(|(n, _)| n == canonical);
        let controller = self.controller_port_for(canonical, by_path, present);
        if !Self::template_can_run(&template, controller.as_deref(), self_present) {
            return None;
        }
        Some(ResolvedHook {
            power_timeout_s: c.power_timeout_s,
            template,
            controller,
            off_settle_s: c.off_settle_s,
            source: c.name.clone(),
        })
    }

    /// Same, for boot modes.
    pub fn boot_mode_hook_for_at<'a>(
        &self,
        selector: &str,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Option<ResolvedHook> {
        if let Some(t) = self
            .devices
            .get(selector)
            .and_then(|d| d.hooks.boot_mode.clone())
        {
            return Some(ResolvedHook {
                power_timeout_s: None,
                template: t,
                controller: None,
                off_settle_s: d_off_settle(),
                source: "device".into(),
            });
        }
        let c = self.controller_for(canonical)?;
        let template = c.boot_mode.clone()?;
        let self_present = present.clone().into_iter().any(|(n, _)| n == canonical);
        let controller = self.controller_port_for(canonical, by_path, present);
        // Same rule as power: a boot mode aimed at an unknown controller would
        // strap the wrong board. See power_hook_for_at.
        if !Self::template_can_run(&template, controller.as_deref(), self_present) {
            return None;
        }
        Some(ResolvedHook {
            power_timeout_s: c.power_timeout_s,
            template,
            controller,
            off_settle_s: c.off_settle_s,
            source: c.name.clone(),
        })
    }

    pub fn power_hook_for<'a>(
        &self,
        selector: &str,
        canonical: &str,
        present: impl IntoIterator<Item = &'a str>,
    ) -> Option<ResolvedHook> {
        if let Some(t) = self
            .devices
            .get(selector)
            .and_then(|d| d.hooks.power.clone())
        {
            return Some(ResolvedHook {
                power_timeout_s: None,
                template: t,
                controller: None,
                off_settle_s: d_off_settle(),
                source: "device".into(),
            });
        }
        let c = self.controller_for(canonical)?;
        Some(ResolvedHook {
            power_timeout_s: c.power_timeout_s,
            template: c.power.clone()?,
            controller: self.controller_port(canonical, present),
            off_settle_s: c.off_settle_s,
            source: c.name.clone(),
        })
    }

    /// The boot-mode hook, resolved the same way.
    pub fn boot_mode_hook_for<'a>(
        &self,
        selector: &str,
        canonical: &str,
        present: impl IntoIterator<Item = &'a str>,
    ) -> Option<ResolvedHook> {
        if let Some(t) = self
            .devices
            .get(selector)
            .and_then(|d| d.hooks.boot_mode.clone())
        {
            return Some(ResolvedHook {
                power_timeout_s: None,
                template: t,
                controller: None,
                off_settle_s: d_off_settle(),
                source: "device".into(),
            });
        }
        let c = self.controller_for(canonical)?;
        Some(ResolvedHook {
            power_timeout_s: c.power_timeout_s,
            template: c.boot_mode.clone()?,
            controller: self.controller_port(canonical, present),
            off_settle_s: c.off_settle_s,
            source: c.name.clone(),
        })
    }

    /// Can this hook template actually run for this device, right now?
    ///
    /// A template says what it needs, and both forms have to be honoured or a
    /// surface offers a button that cannot work:
    ///
    ///  * `{controller}` -- it needs a SEPARATE controller device. Refusing when
    ///    none resolved is what stops a hook falling back to its script's own
    ///    default port and driving another board entirely.
    ///
    ///  * `{device}` -- it drives the board THROUGH THIS CONSOLE. A Bughopper's
    ///    FTDI is the board's UART and its CBUS strap lines at once, so it needs
    ///    nothing else plugged in -- but it does need ITSELF plugged in, and
    ///    that was never checked. Measured on alpha and bravo the hour presence
    ///    shipped: an unplugged Bughopper still advertised `has_power_hook:
    ///    true` and an EDL boot mode, and an agent acting on that opens a tty
    ///    that is not there.
    ///
    /// DELIBERATELY ASYMMETRIC. A board whose console vanished because it is
    /// switched OFF must keep the hook that turns it back on -- that hook names
    /// `{controller}` and lives on a separate device which is still present.
    /// Demanding self-presence there would make a powered-off board impossible
    /// to power on, which is the opposite of the point.
    fn template_can_run(template: &str, controller: Option<&str>, self_present: bool) -> bool {
        if template.contains("{controller}") {
            return controller.is_some();
        }
        if template.contains("{device}") {
            return self_present;
        }
        true
    }

    /// The controller profile that can ACTUALLY drive this console right now.
    ///
    /// [`controller_for`] answers a different question -- which profile's glob
    /// claims this name -- and that answer is the right input to hook
    /// resolution, which then refuses when no controller resolved. But it is the
    /// WRONG thing to publish: on a host with no Bantam at all, the bantam
    /// profile's deliberate `controls = "*"` claimed both consoles of an FTDI
    /// TAC board, so the dashboard labelled them `controller: "bantam"` and
    /// offered five Bantam boot modes that could never run. `has_power_hook`
    /// was correctly false beside them, which is the tell: the surface was
    /// contradicting itself.
    ///
    /// Actionable means at least one of the profile's templates can run: either
    /// a matching controller device is present, or the profile drives the board
    /// through the console itself (a Bughopper-class controller uses `{device}`
    /// and needs nothing else plugged in).
    pub fn controller_for_at<'a>(
        &self,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Option<&ControllerProfile> {
        let c = self.controller_for(canonical)?;
        let self_present = present.clone().into_iter().any(|(n, _)| n == canonical);
        if self
            .controller_port_for(canonical, by_path, present)
            .is_some()
        {
            return Some(c);
        }
        [
            c.power.as_deref(),
            c.boot_mode.as_deref(),
            c.power_state.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|t| Self::template_can_run(t, None, self_present))
        .then_some(c)
    }

    /// Boot modes to OFFER for a console: only ones that could actually be
    /// selected.
    ///
    /// Same lesson as [`controller_for_at`]. A menu of modes on a board whose
    /// controller is not present is an invitation to press something that
    /// returns HOOK_NOT_CONFIGURED at best -- and the reason it is worth a
    /// separate resolver rather than a filter at the UI is that mcpd advertises
    /// this list too, to agents that cannot see a greyed-out button.
    pub fn boot_modes_for_at<'a>(
        &self,
        selector: &str,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Vec<String> {
        let own = self
            .devices
            .get(selector)
            .map(|d| d.boot_modes.clone())
            .unwrap_or_default();
        if !own.is_empty() {
            return own;
        }
        let Some(c) = self.controller_for(canonical) else {
            return Vec::new();
        };
        let Some(template) = c.boot_mode.as_deref() else {
            return Vec::new();
        };
        let self_present = present.clone().into_iter().any(|(n, _)| n == canonical);
        let controller = self.controller_port_for(canonical, by_path, present);
        if !Self::template_can_run(template, controller.as_deref(), self_present) {
            return Vec::new();
        }
        c.boot_modes.clone()
    }

    /// Boot modes offered for a console: the device's own list, else its
    /// controller's.
    pub fn boot_modes_for(&self, selector: &str, canonical: &str) -> Vec<String> {
        let own = self
            .devices
            .get(selector)
            .map(|d| d.boot_modes.clone())
            .unwrap_or_default();
        if !own.is_empty() {
            return own;
        }
        self.controller_for(canonical)
            .map(|c| c.boot_modes.clone())
            .unwrap_or_default()
    }

    /// The controller profile that powers this console, if any.
    ///
    /// Matched on the *canonical* by-id name rather than the nickname, because a
    /// board's identity is its hardware and a nickname is a label someone chose
    /// afterwards.
    /// The controller profile that drives this console.
    ///
    /// MOST SPECIFIC `controls` GLOB WINS, and that ordering is load-bearing.
    /// The bantam profile deliberately uses `controls = "*"` so a newly plugged
    /// board of that family gets power with no config, with USB topology
    /// deciding WHICH board. But a plain first-match then let that catch-all
    /// claim consoles belonging to a different controller family entirely: the
    /// Bughopper board -- whose own FTDI drives its power over CBUS -- resolved
    /// to `bantam`, which then had no Bantam to talk to on that branch. The board
    /// showed a power button that could not work.
    ///
    /// Specificity is measured by the length of the glob's literal (non-`*`)
    /// text, so `*Bughopper*` beats `*`, and an exact name beats both.
    pub fn controller_for(&self, canonical: &str) -> Option<&ControllerProfile> {
        self.controllers
            .iter()
            .filter(|c| glob_match(&c.controls, canonical))
            .max_by_key(|c| c.controls.chars().filter(|ch| *ch != '*').count())
    }

    /// Is this by-id name a board controller (of any profile)?
    pub fn controller_profile_of(&self, canonical: &str) -> Option<&ControllerProfile> {
        self.controllers
            .iter()
            .find(|c| glob_match(&c.match_glob, canonical))
    }

    /// Is this by-id name a controller that should be kept out of discovery?
    pub fn is_excluded_controller(&self, canonical: &str) -> bool {
        self.controllers
            .iter()
            .any(|c| c.exclude_from_discovery && glob_match(&c.match_glob, canonical))
    }

    /// The controller device serving this console, resolved against the by-id
    /// names actually present on the host.
    pub fn controller_port<'a>(
        &self,
        canonical: &str,
        present: impl IntoIterator<Item = &'a str>,
    ) -> Option<String> {
        let c = self.controller_for(canonical)?;
        present
            .into_iter()
            .find(|name| glob_match(&c.match_glob, name))
            .map(|s| s.to_string())
    }

    /// The controller that drives this console, chosen by USB TOPOLOGY.
    ///
    /// Name matching cannot answer this once a bench has two boards of the same
    /// family: both Bantams match `*Bantam*`, so picking the first would bind
    /// the IQ10's controller to a NordAU RIDE SX and power-cycle the wrong
    /// board. Devices sharing a downstream hub are one harness, so the
    /// controller in the console's own topology group is the right one.
    ///
    /// Falls back to name matching when topology is unknown (an adapter with no
    /// by-path, or a single-board bench), so this never makes things worse than
    /// they were.
    pub fn controller_port_for<'a>(
        &self,
        canonical: &str,
        by_path: Option<&str>,
        present: impl IntoIterator<Item = (&'a str, Option<&'a str>)> + Clone,
    ) -> Option<String> {
        let c = self.controller_for(canonical)?;
        let mine = topology_group(by_path);

        if mine.is_some() {
            // Same board first.
            if let Some((name, _)) = present
                .clone()
                .into_iter()
                .find(|(name, bp)| glob_match(&c.match_glob, name) && topology_group(*bp) == mine)
            {
                return Some(name.to_string());
            }
            // A controller matching by name but sitting on ANOTHER board is not
            // a fallback -- it is the wrong board. Say so rather than guess.
            if present
                .clone()
                .into_iter()
                .any(|(name, bp)| glob_match(&c.match_glob, name) && topology_group(bp).is_some())
            {
                tracing::warn!(
                    console = %canonical,
                    "a matching controller exists but on a different USB branch; \
                     refusing to bind it -- that would actuate another board"
                );
                return None;
            }
        }

        present
            .into_iter()
            .find(|(name, _)| glob_match(&c.match_glob, name))
            .map(|(name, _)| name.to_string())
    }

    /// Is this by-id name one conminer is allowed to open? (§16 exclude list.)
    pub fn device_included(&self, by_id_name: &str) -> bool {
        let matches_any = |pats: &[String]| {
            pats.iter().any(|p| {
                globset::Glob::new(p)
                    .map(|g| g.compile_matcher().is_match(by_id_name))
                    .unwrap_or(false)
            })
        };
        // A controller profile excludes its own controller automatically, so a
        // new board type does not need someone to remember to add it to the
        // exclude list before conminer grabs a port that must stay free.
        matches_any(&self.discovery.include)
            && !matches_any(&self.discovery.exclude)
            && !self.is_excluded_controller(by_id_name)
            // A TAC's GPIO channels appear in /dev/serial/by-id like any other
            // ftdi_sio port on a freshly booted host, and they are the board's
            // power and strap lines. Opening a tty asserts DTR and RTS, so
            // serving one to ser2net is a WRITE to those pins.
            && crate::tac::gpio_channel(by_id_name).is_none()
    }
}

fn parse_duration_days(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('h') {
        // Hours are accepted for symmetry, but retention is a whole-day knob:
        // anything under 24h floors to 0 (= prune immediately), which is what
        // "keep it for 6h" honestly means at day granularity.
        return Some(n.parse::<u64>().ok()? / 24);
    }
    let (num, mult) = match s.strip_suffix('d') {
        Some(n) => (n, 1),
        None => (s.strip_suffix('w')?, 7),
    };
    Some(num.parse::<u64>().ok()? * mult)
}

fn std_env_pairs() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("CONMINER_"))
        .collect()
}

#[cfg(test)]
mod tests {

    /// A TEST RUN MUST NOT BE ABLE TO REACH THE BENCH.
    ///
    /// The dev container is on the same docker network as the live stack, so
    /// `mcpd` resolves there to the mcpd that drives real boards -- and dashd's
    /// power route posts to whatever `mcp_url` says. A default that names the
    /// live service turns "run the suite" into "one resolvable selector from a
    /// power cycle". Under test the address must be dead, and in production it
    /// must still be the service.
    #[test]
    fn the_dashboards_mcp_url_is_a_dead_address_under_test_mode() {
        assert_eq!(super::default_mcp_url(false), "http://mcpd:8090/mcp");
        let under_test = super::default_mcp_url(true);
        assert!(
            !under_test.contains("mcpd"),
            "a test must not be pointed at the live service: {under_test}"
        );
        assert!(
            under_test.contains("127.0.0.1:1/"),
            "...and the address it gets must refuse rather than answer: {under_test}"
        );
        // And the default a test actually builds is the dead one: this suite
        // runs with CONMINER_TEST_MODE=1, so this is the real wiring, not the
        // helper in isolation.
        assert_eq!(
            Config::default().dashboard.mcp_url,
            "http://127.0.0.1:1/mcp",
            "the container sets CONMINER_TEST_MODE=1; the default must follow it"
        );
    }
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.line.baud, 115200);
        assert_eq!(c.line.summary(), "115200 8N1");
        assert_eq!(c.mine.similarity, 0.4);
        assert_eq!(c.api.max_raw_lines, 200);
    }

    #[test]
    fn unknown_key_is_rejected_with_the_offending_line() {
        let err = Config::from_toml_str("[mine]\nsimilaritee = 0.5\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidConfig);
        let d = err.detail.unwrap();
        assert_eq!(d["line"], 2);
        assert!(d["text"].as_str().unwrap().contains("similaritee"));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        let err = Config::from_toml_str("[mine]\nsimilarity = 1.9\n").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidConfig);
        assert!(err.message.contains("mine.similarity"));
    }

    #[test]
    fn exclude_globs_keep_conminer_off_a_device() {
        let c = Config::from_toml_str(
            r#"
            [discovery]
            include = ["*"]
            exclude = ["*Quectel*", "usb-FTDI_UPS*"]
            "#,
        )
        .unwrap();
        assert!(c.device_included("usb-FTDI_TTL232R_FT123456-if00-port0"));
        assert!(!c.device_included("usb-Quectel_EM120-if02"));
        assert!(!c.device_included("usb-FTDI_UPS_serial-if00"));
    }

    #[test]
    fn per_device_line_overrides_global() {
        let c = Config::from_toml_str(
            r#"
            [line]
            baud = 115200

            [devices."rb3-ap".line]
            baud = 921600
            "#,
        )
        .unwrap();
        assert_eq!(c.line_for("rb3-ap").baud, 921_600);
        assert_eq!(c.line_for("other").baud, 115_200);
    }

    #[test]
    fn env_overrides_apply() {
        let mut c = Config::default();
        c.apply_env(&[("CONMINER_MCPD_PORT".into(), "9999".into())]);
        assert_eq!(c.mcpd.port, 9999);
    }

    fn iq10_controllers() -> Vec<ControllerProfile> {
        vec![ControllerProfile {
            name: "bantam".into(),
            match_glob: "*Bantam*".into(),
            controls: "*IQ10*".into(),
            power_timeout_s: None,
            power: Some("bantam-power {action} --settle {off_settle}".into()),
            boot_mode: Some("bantam-power mode {mode}".into()),
            power_state: None,
            flash: None,
            boot_modes: vec!["BOOT_MD_EDL".into(), "BOOT_UEFI".into()],
            off_settle_s: 6.0,
            exclude_from_discovery: true,
            mode_enters_immediately: false,
        }]
    }

    #[test]
    fn a_controller_profile_binds_to_the_consoles_it_controls() {
        // The point of the profile: plug in a known board type and its power
        // controls exist without anyone writing per-device config.
        let c = Config {
            controllers: iq10_controllers(),
            ..Default::default()
        };
        let present = ["usb-Microchip_Bantam_IQ10RRDXX34VG8-if00"];
        let console = "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0";

        let h = c
            .power_hook_for(console, console, present)
            .expect("auto-bound power hook");
        assert_eq!(h.template, "bantam-power {action} --settle {off_settle}");
        assert_eq!(h.source, "bantam");
        assert_eq!(h.off_settle_s, 6.0);
        assert_eq!(
            h.controller.as_deref(),
            Some("usb-Microchip_Bantam_IQ10RRDXX34VG8-if00"),
            "the controller's own tty is resolved from what is present"
        );
        assert_eq!(c.boot_modes_for(console, console).len(), 2);
    }

    #[test]
    fn a_console_no_profile_claims_gets_no_controls() {
        // Never a guess: offering power buttons that drive some other board's
        // controller would be worse than offering none.
        let c = Config {
            controllers: iq10_controllers(),
            ..Default::default()
        };
        let other = "usb-FTDI_TTL232R_FT999-if00-port0";
        assert!(c
            .power_hook_for(other, other, ["usb-Microchip_Bantam_X-if00"])
            .is_none());
        assert!(c.boot_modes_for(other, other).is_empty());
    }

    /// Measured on the bravo node: a host with no Bantam anywhere on it, serving
    /// an FTDI TAC board whose two consoles the bantam profile's deliberate
    /// `controls = "*"` claimed. The dashboard said `controller: "bantam"` and
    /// offered five Bantam boot modes next to `has_power_hook: false`.
    #[test]
    fn a_controller_that_is_not_plugged_in_is_not_advertised() {
        let mut profiles = iq10_controllers();
        profiles[0].controls = "*".into(); // the shipped catch-all
        profiles[0].power = Some("bantam-power {action} --port {controller}".into());
        profiles[0].boot_mode = Some("bantam-power mode {mode} --port {controller}".into());
        let c = Config {
            controllers: profiles,
            ..Default::default()
        };
        let console = "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0";
        // Nothing on this host but the board's own two consoles.
        let present = [
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0", None),
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0", None),
        ];

        assert!(
            c.controller_for(console).is_some(),
            "the glob still claims it -- that is what hook resolution needs"
        );
        assert!(
            c.controller_for_at(console, None, present).is_none(),
            "but nothing is present that could drive it, so nothing may be named"
        );
        assert!(
            c.boot_modes_for_at(console, console, None, present)
                .is_empty(),
            "and no mode may be offered"
        );
        // The self-consistency that was broken: what is advertised and what can
        // actually run must agree.
        assert!(c
            .power_hook_for_at(console, console, None, present)
            .is_none());
    }

    /// The other half, so the fix cannot be "always say none": a controller that
    /// IS present must still be named, and its modes still offered.
    #[test]
    fn a_controller_that_is_plugged_in_is_still_advertised() {
        let c = Config {
            controllers: iq10_controllers(),
            ..Default::default()
        };
        let console = "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0";
        let present = [
            ("usb-Microchip_Bantam_IQ10RRDXX34VG8-if00", None),
            (console, None),
        ];
        assert_eq!(
            c.controller_for_at(console, None, present)
                .map(|p| p.name.as_str()),
            Some("bantam")
        );
        assert_eq!(
            c.boot_modes_for_at(console, console, None, present).len(),
            2
        );
    }

    /// A Bughopper-class controller drives the board through the console it
    /// carries: `{device}`, not `{controller}`. Nothing else is plugged in, and
    /// it must still be named -- the presence rule is about being ABLE to act,
    /// not about a second device existing.
    #[test]
    fn a_controller_that_needs_no_second_device_is_advertised_alone() {
        let c = Config {
            controllers: vec![ControllerProfile {
                name: "bughopper".into(),
                match_glob: "*Bughopper*".into(),
                controls: "*Bughopper*".into(),
                power_timeout_s: None,
                power: Some("conminer bughopper-power {action} --device {device}".into()),
                boot_mode: Some("conminer bughopper-power mode {mode} --device {device}".into()),
                power_state: None,
                flash: None,
                boot_modes: vec!["EDL".into()],
                off_settle_s: 6.0,
                exclude_from_discovery: false,
                mode_enters_immediately: true,
            }],
            ..Default::default()
        };
        let console = "usb-Arduino_Bughopper_DK0HDSRI-if00-port0";
        let present = [(console, None)];
        assert_eq!(
            c.controller_for_at(console, None, present)
                .map(|p| p.name.as_str()),
            Some("bughopper")
        );
        assert_eq!(
            c.boot_modes_for_at(console, console, None, present),
            ["EDL"]
        );
    }

    #[test]
    fn a_per_device_hook_outranks_the_profile() {
        // An operator overriding one board must not be silently outvoted.
        let mut c = Config {
            controllers: iq10_controllers(),
            ..Default::default()
        };
        let console = "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0";
        c.devices.insert(
            console.to_string(),
            DeviceOverride {
                hooks: DeviceHooks {
                    power: Some("my-own-script {action}".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let h = c
            .power_hook_for(console, console, ["usb-Microchip_Bantam_X-if00"])
            .unwrap();
        assert_eq!(h.template, "my-own-script {action}");
        assert_eq!(h.source, "device");
    }

    #[test]
    fn a_controller_excludes_itself_from_discovery() {
        // The Bantam's command processor is single-session, so ser2net holding
        // it would break board control. The profile knows that; nobody should
        // have to remember it in an exclude list.
        let c = Config {
            controllers: iq10_controllers(),
            ..Default::default()
        };
        assert!(!c.device_included("usb-Microchip_Bantam_IQ10RRDXX34VG8-if00"));
        assert!(c.device_included("usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0"));
    }

    #[test]
    fn the_settle_delay_is_a_property_of_the_board_not_the_script() {
        let mut profiles = iq10_controllers();
        profiles[0].off_settle_s = 12.5;
        let c = Config {
            controllers: profiles,
            ..Default::default()
        };
        let console = "usb-FTDI_IQ10_UART-SPI_X-if00-port0";
        let h = c
            .power_hook_for(console, console, ["usb-Microchip_Bantam_X-if00"])
            .unwrap();
        assert_eq!(h.off_settle_s, 12.5, "passed to the hook as {{off_settle}}");
    }

    #[test]
    fn ser2net_option_string_matches_line_settings() {
        let l = LineConfig {
            baud: 921_600,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: 1,
            ..Default::default()
        };
        assert_eq!(l.summary(), "921600 7E1");
        assert_eq!(l.ser2net_options(), "921600e71,local");
    }
}
