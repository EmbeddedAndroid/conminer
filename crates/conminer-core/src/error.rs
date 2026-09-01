//! Structured error taxonomy (§14.6).
//!
//! Every error an agent can observe carries a stable machine-branchable `code`,
//! a human `message`, and a `hint` telling the agent what to do next. Tools never
//! return a bare string, and never return a stringly-typed failure an agent has
//! to regex.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The complete set of error codes conminer can return to an agent.
///
/// Adding a variant is a breaking API change and must land with a test in the
/// `tools` suite asserting the code round-trips through the MCP layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    // ---- selector / device resolution (§3.1) ----
    /// Selector matched no device.
    UnknownDevice,
    /// Selector matched more than one device; candidates are attached.
    AmbiguousDevice,
    /// Selector matched several devices for a tool that only accepts one.
    GroupSelectorNotAllowed,
    /// Device was known but has gone away (unplugged, excluded, ser2net down).
    DeviceGone,
    /// Nickname already bound to a different canonical id.
    NicknameTaken,

    // ---- session / epoch ----
    UnknownSession,
    SessionActive,
    UnknownBoot,
    UnknownTemplate,
    UnknownRecord,
    UnknownLine,
    UnknownWatch,
    UnknownBaseline,
    UnknownBisect,
    UnknownArgument,

    // ---- cursors and caps (§8, §8.2) ----
    /// Cursor pointed before the retention horizon; agent must re-anchor.
    CursorExpired,
    /// Cursor is not a cursor this device ever issued.
    InvalidCursor,
    /// Response was truncated at the configured cap; a cursor is attached.
    ResultCapped,

    // ---- ingestion ----
    IngestTooLarge,
    /// The raw bytes for this range were pruned; the compressed knowledge
    /// derived from them (templates, epochs, stages) is still here (§F9).
    Pruned,
    IngestFailed,
    NoSuchPath,
    PermissionDenied,

    // ---- interaction (§8.3, §8.5, §15.1) ----
    /// No prompt present; conminer refuses to fire a command into an unknown state.
    NoPrompt,
    /// The console is idling at something that is neither a known prompt nor a
    /// known credential gate.
    UnknownPrompt,
    /// Console is at a credential gate; the board is up but not commandable.
    LoginRequired,
    /// Per-character echo verification failed twice.
    EchoMismatch,
    /// Recovery ladder exhausted.
    Hung,
    /// Mutating tool called without holding the device lease.
    LeaseRequired,
    /// Lease is held by another agent and `steal` was not set.
    LeaseHeld,
    /// The port is claimed for a binary protocol; framing is suspended.
    ExclusiveClaimed,
    /// A power/boot-mode actuation is still running on one of these consoles.
    ///
    /// The hook, its verification and any escalation are one workflow, and a
    /// second actuation on the same board while it runs is a race with the
    /// hardware, not a queue: measured on the Uno Q, an `on` accepted while an
    /// `off` was mid-escalation booted the board, and the escalation's final
    /// press then reset that kernel 28 s later.
    ActuationInFlight,
    /// The board is in a flash/recovery mode (its normal console re-enumerated
    /// away), so there is no OS console to command. A tool that TRANSMITS
    /// (run_command, send) must refuse rather than push a newline into a board
    /// being flashed (report #23). Distinct from `NoPrompt`, which means the
    /// console is present but not at a prompt: here the console itself is gone.
    AwayInEdl,
    /// Low-level `send` passthrough is disabled by config.
    SendDisabled,

    // ---- hooks (§15.2, §15.3) ----
    HookNotConfigured,
    HookFailed,
    HookTimeout,

    // ---- fleet peering (§P1) ----
    /// The node that OWNS this device did not answer. Its data lives only there
    /// and is never faked here.
    PeerUnreachable,
    /// The selector named a node that is not in the fleet.
    UnknownPeer,
    /// One name, several nodes. Never resolved by picking one: `devices.node`
    /// keys on the NAME, so a guess here actuates the wrong board.
    AmbiguousPeer,

    // ---- config / infra (§14.5) ----
    InvalidConfig,
    InvalidArgument,
    Unsupported,
    StorageFull,
    Internal,
}

impl ErrorCode {
    /// Default remediation hint. Call sites may override with something specific.
    pub fn default_hint(self) -> &'static str {
        use ErrorCode::*;
        match self {
            UnknownDevice => "call list_devices() to see valid selectors",
            AmbiguousDevice => "pick one of the candidates, or use its canonical id",
            GroupSelectorNotAllowed => "this tool acts on one device; narrow the selector",
            DeviceGone => "check list_devices(); the device may have been unplugged",
            PeerUnreachable => "the owning node is not answering; its data is not cached here",
            UnknownPeer => "call peers() to see the live fleet",
            AmbiguousPeer => {
                "two nodes are answering to one name; give each its own \
                              CONMINER_PEERS_NAME, or address the node by instance id"
            }
            NicknameTaken => "choose another nickname, or re-point it with force",
            UnknownSession => "call list_sessions(device) for valid session ids",
            SessionActive => "call end_session() first, or pass force",
            UnknownBoot => "call list_boots(device) for valid boot ids",
            UnknownTemplate => "call list_templates() for valid template ids",
            UnknownRecord => "call list_templates() then get_records() for valid ids",
            UnknownLine => "use an anchor returned by search() or get_records()",
            UnknownWatch => "call list_watches() for valid names",
            UnknownBaseline => "call list_baselines(), or set one with set_baseline()",
            UnknownBisect => {
                "call list_bisects() for valid names, or start one with bisect_start()"
            }
            UnknownArgument => "the referenced object does not exist on this device",
            CursorExpired => "re-anchor: call the tool without a cursor to get a fresh one",
            InvalidCursor => "cursors are opaque; pass back exactly what a tool returned",
            ResultCapped => "pass the returned cursor to fetch the next page",
            Pruned => {
                "the verbatim bytes for this range have been pruned; templates, epochs, \
                       stages and metrics derived from them remain"
            }
            IngestTooLarge => "split the file, or raise ingest.max_gb",
            IngestFailed => "check the ingest job error detail",
            NoSuchPath => "the path must be visible inside the minerd container",
            PermissionDenied => "check file ownership and the container's mounts",
            NoPrompt => "the console is not at a prompt; inspect the attached tail",
            UnknownPrompt => "inspect the tail, then teach it via classify_prompt()",
            // Names the real knob. This used to say "or call login()", and there
            // is no `login` tool -- a hint that sends the reader hunting through
            // the tool list for something that was never built costs more than
            // no hint at all.
            LoginRequired => {
                "the console is at a login prompt: put credentials for it in the file named by \
                 credentials.file in conminer.toml (a 0600 file, never the registry), then retry"
            }
            EchoMismatch => "the line may be lossy; raise runner.char_delay_ms or set echo=off",
            Hung => "the recovery ladder was exhausted; consider recover=power",
            LeaseRequired => "call acquire(device) before mutating tools",
            LeaseHeld => "wait for expiry, or call acquire(steal=true)",
            ExclusiveClaimed => {
                // This named `release_exclusive(device)`, which has never been a
                // tool: an agent holding a claim whose owner had died read the
                // hint, found nothing in tools/list, and had no recovery at all
                // (report #27). A hint is only useful if what it names exists.
                "clear a claim whose holder is gone with \
                 claim_exclusive(device, release: true), or release(device), \
                 which drops the lease and the claim together"
            }
            ActuationInFlight => {
                "wait for the running actuation to finish (its `since_ms` and action are in \
                 detail), then poll console_state or list_boots for the outcome"
            }
            AwayInEdl => {
                "the board is in EDL/flash mode; wait for it to leave recovery (poll \
                 console_state until capture_state is not away_in_edl), then retry"
            }
            SendDisabled => "set runner.allow_raw_send=true to enable the passthrough",
            HookNotConfigured => "define the hook in conminer.toml under [devices.<id>.hooks]",
            HookFailed => "inspect the hook's exit code and stderr in the attached detail",
            HookTimeout => "raise hooks.*_timeout_s, or fix the hook",
            InvalidConfig => "run `conminer check-config` for the offending key",
            InvalidArgument => "check the tool's schema in tools/list",
            Unsupported => "this build or profile does not support that operation",
            StorageFull => "free space on the conminer data volume; capture has stopped",
            Internal => "this is a bug; the detail field carries the context",
        }
    }

    /// Every code, so a check over the whole surface cannot silently miss one.
    ///
    /// Hand-maintained lists drift: a hint that named a tool which had never
    /// existed survived because nothing walked the set (report #27).
    pub fn all() -> &'static [ErrorCode] {
        &[
            ErrorCode::UnknownDevice,
            ErrorCode::AmbiguousDevice,
            ErrorCode::GroupSelectorNotAllowed,
            ErrorCode::DeviceGone,
            ErrorCode::NicknameTaken,
            ErrorCode::UnknownSession,
            ErrorCode::SessionActive,
            ErrorCode::UnknownBoot,
            ErrorCode::UnknownTemplate,
            ErrorCode::UnknownRecord,
            ErrorCode::UnknownLine,
            ErrorCode::UnknownWatch,
            ErrorCode::UnknownBaseline,
            ErrorCode::UnknownBisect,
            ErrorCode::UnknownArgument,
            ErrorCode::CursorExpired,
            ErrorCode::InvalidCursor,
            ErrorCode::ResultCapped,
            ErrorCode::IngestTooLarge,
            ErrorCode::IngestFailed,
            ErrorCode::NoSuchPath,
            ErrorCode::PermissionDenied,
            ErrorCode::NoPrompt,
            ErrorCode::UnknownPrompt,
            ErrorCode::LoginRequired,
            ErrorCode::EchoMismatch,
            ErrorCode::Hung,
            ErrorCode::LeaseRequired,
            ErrorCode::LeaseHeld,
            ErrorCode::ExclusiveClaimed,
            ErrorCode::ActuationInFlight,
            ErrorCode::AwayInEdl,
            ErrorCode::SendDisabled,
            ErrorCode::HookNotConfigured,
            ErrorCode::HookFailed,
            ErrorCode::HookTimeout,
            ErrorCode::InvalidConfig,
            ErrorCode::InvalidArgument,
            ErrorCode::Unsupported,
            ErrorCode::StorageFull,
            ErrorCode::Internal,
        ]
    }

    pub fn as_str(self) -> &'static str {
        // Round-trips through the same serde mapping the wire uses.
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => Box::leak(s.into_boxed_str()),
            _ => "INTERNAL",
        }
    }
}

/// The error every conminer tool returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: String,
    /// Structured payload: candidate device lists, observed tails, hook stderr,
    /// the offending config line. Agents branch on `code` and read `detail`.
    ///
    /// Boxed because `ToolError` is the error type of nearly every function in
    /// the crate: an inline `Value` would make every `Result` in the codebase
    /// pay for a payload that is almost always absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Box<serde_json::Value>>,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: code.default_hint().to_string(),
            detail: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(Box::new(detail));
        self
    }

    pub fn internal(e: impl fmt::Display) -> Self {
        Self::new(ErrorCode::Internal, e.to_string())
    }

    pub fn invalid_arg(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, msg)
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<rusqlite::Error> for ToolError {
    fn from(e: rusqlite::Error) -> Self {
        // Disk-full must fail loud and be distinguishable (§13 `store`).
        let msg = e.to_string();
        if msg.contains("disk is full") || msg.contains("database or disk is full") {
            ToolError::new(ErrorCode::StorageFull, msg)
        } else {
            ToolError::new(ErrorCode::Internal, msg)
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        let code = match e.kind() {
            NotFound => ErrorCode::NoSuchPath,
            PermissionDenied => ErrorCode::PermissionDenied,
            _ => ErrorCode::Internal,
        };
        ToolError::new(code, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ToolError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_serialize_as_screaming_snake() {
        assert_eq!(ErrorCode::UnknownDevice.as_str(), "UNKNOWN_DEVICE");
        assert_eq!(ErrorCode::CursorExpired.as_str(), "CURSOR_EXPIRED");
        assert_eq!(ErrorCode::IngestTooLarge.as_str(), "INGEST_TOO_LARGE");
        assert_eq!(ErrorCode::NoPrompt.as_str(), "NO_PROMPT");
    }

    #[test]
    fn every_code_has_a_nonempty_hint() {
        // Enumerated by hand so adding a variant without a hint fails to compile
        // elsewhere and fails this test if someone stubs it with "".
        for &c in ErrorCode::all() {
            assert!(!c.default_hint().is_empty(), "{c:?} has no hint");
            assert!(!c.as_str().is_empty());
        }
    }

    #[test]
    fn tool_error_round_trips_through_json() {
        let e = ToolError::new(ErrorCode::AmbiguousDevice, "3 candidates")
            .with_detail(serde_json::json!({"candidates": ["a", "b", "c"]}));
        let s = serde_json::to_string(&e).unwrap();
        let back: ToolError = serde_json::from_str(&s).unwrap();
        assert_eq!(back.code, ErrorCode::AmbiguousDevice);
        assert_eq!(back.detail.unwrap()["candidates"][2], "c");
    }
}
