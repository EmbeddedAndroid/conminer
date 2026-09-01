//! Interactive command runner (§8.3): a UART command as a *transaction*.
//!
//! Raw `send` over a shared serial line is unreliable by construction — dropped
//! characters, echo interleave, multi-writer collisions, hung foreground
//! processes. So a command here is a transaction with a truthful terminal state:
//! `ok`, `echo_mismatch`, `no_prompt`, `login_required`, `unknown_prompt` or
//! `hung`, always with evidence attached. The agent never has to infer success
//! from silence.
//!
//! The runner opens **its own** connection to the device's ser2net endpoint
//! rather than routing through minerd. That is the architecture working as
//! designed (§2: ser2net is the sharing layer, and it is multi-consumer): the
//! runner sees its own echo at wire latency instead of waiting for another
//! process's commit interval, while minerd captures the same bytes into the
//! store in parallel.

use crate::config::{LineConfig, RunnerConfig};
use crate::error::{ErrorCode, Result, ToolError};
use crate::framer::profile::PromptKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How a transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// Per-character echo verification failed twice on the same character.
    EchoMismatch,
    /// The console was not at a prompt; nothing was sent.
    NoPrompt,
    /// The console is at a credential gate: up, but not commandable.
    LoginRequired,
    /// Something is waiting for input and we do not know what it is.
    UnknownPrompt,
    /// The recovery ladder was exhausted.
    Hung,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::EchoMismatch => "echo_mismatch",
            Outcome::NoPrompt => "no_prompt",
            Outcome::LoginRequired => "login_required",
            Outcome::UnknownPrompt => "unknown_prompt",
            Outcome::Hung => "hung",
        }
    }

    pub fn error_code(self) -> Option<ErrorCode> {
        match self {
            Outcome::Ok => None,
            Outcome::EchoMismatch => Some(ErrorCode::EchoMismatch),
            Outcome::NoPrompt => Some(ErrorCode::NoPrompt),
            Outcome::LoginRequired => Some(ErrorCode::LoginRequired),
            Outcome::UnknownPrompt => Some(ErrorCode::UnknownPrompt),
            Outcome::Hung => Some(ErrorCode::Hung),
        }
    }
}

/// One rung of the hung-command recovery ladder (§8.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rung {
    pub name: String,
    pub sent: String,
    pub recovered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub status: Outcome,
    pub command: String,
    pub output: String,
    /// True when `output` was cut at `max_output_bytes`.
    pub output_capped: bool,
    pub duration_ms: u64,
    pub prompt_matched: Option<String>,
    pub rungs_attempted: Vec<Rung>,
    /// Bytes observed but not accounted to the command — an interleaved async
    /// kernel message, for instance. Kept so nothing is silently discarded.
    pub preamble: String,
    pub detail: Value,
}

impl Transaction {
    /// Attach the two facts that distinguish "I did not understand what I read"
    /// from "I read nothing". Merged into `detail` rather than added as new
    /// top-level fields so existing consumers keep working.
    fn with_io(mut self, bytes_read: usize, endpoint: &str) -> Self {
        if let Value::Object(ref mut m) = self.detail {
            m.insert("bytes_read".into(), json!(bytes_read));
            m.insert("endpoint".into(), json!(endpoint));
        }
        self
    }

    fn failed(status: Outcome, command: &str, tail: String, detail: Value) -> Self {
        Self {
            status,
            command: command.to_string(),
            output: String::new(),
            output_capped: false,
            duration_ms: 0,
            prompt_matched: None,
            rungs_attempted: Vec::new(),
            preamble: tail,
            detail,
        }
    }

    /// The structured error an MCP tool returns for a non-`ok` outcome.
    pub fn as_error(&self) -> Option<ToolError> {
        let code = self.status.error_code()?;
        Some(
            ToolError::new(
                code,
                format!(
                    "command {:?} ended as {}",
                    self.command,
                    self.status.as_str()
                ),
            )
            .with_detail(json!({
                "transaction": self,
            })),
        )
    }
}

/// Is this line an asynchronous kernel message rather than console state?
///
/// `[   47.646380] [drm] NORDAUX tout: ...` -- a printk timestamp at the start
/// of the line. Deliberately narrow: only the timestamp form is treated as
/// async, so ordinary output that merely starts with a bracket is untouched.
fn is_async_kernel_line(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix('[') else {
        return false;
    };
    let Some(close) = rest.find(']') else {
        return false;
    };
    let inner = rest[..close].trim();
    // `<digits>.<digits>` and nothing else.
    match inner.split_once('.') {
        Some((sec, frac)) => {
            !sec.is_empty()
                && !frac.is_empty()
                && sec.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// The part of a line before any asynchronous kernel message that landed on it.
///
/// Returns the line unchanged when the printk is at the start (that line IS a
/// kernel message, not a prompt wearing one) or when there is none.
fn strip_trailing_kernel_noise(line: &str) -> &str {
    // char_indices, NOT byte arithmetic. Console bytes are not always valid
    // UTF-8 -- a baud mismatch or a truncated multi-byte sequence decodes to
    // U+FFFD, which is three bytes -- and slicing at a byte offset inside one
    // panics. The first version started its search at byte 1 and took down every
    // console_state call on a line beginning with a replacement character.
    line.char_indices()
        .skip(1) // index 0 is a kernel LINE, not a prompt wearing one
        .find(|&(at, c)| c == '[' && is_async_kernel_line(&line[at..]))
        // NOT trimmed: prompt patterns end in "# " and the space is theirs.
        .map(|(at, _)| &line[..at])
        .unwrap_or(line)
}

/// Is this line undecodable noise rather than something the console typed?
///
/// Measured on the IQ10: restarting the stack re-opened the FTDI, and the two
/// NEWEST lines on the device were
///
///   \u{fffd}\u{fffd}\x03\u{fffd}\u{fffd}\x03\u{fffd}\u{fffd}\x01...
///
/// -- UART noise from the reconnect, sitting on top of a perfectly good shell
/// prompt. Picking the last line meant picking that, so the prompt below it was
/// never considered and console_state answered `unstable` at a live shell.
///
/// Line noise is not what a console is "waiting at", exactly like an async
/// kernel message. A board whose output is ACTUALLY garbage -- a baud mismatch
/// after a strap change -- is caught earlier and separately, by the garbage
/// detector, which is a claim about the whole stream rather than one line.
fn is_line_noise(line: &str) -> bool {
    let mut bad = 0usize;
    let mut total = 0usize;
    for c in line.chars() {
        total += 1;
        if c == '\u{fffd}' || (c.is_control() && c != '\t') {
            bad += 1;
        }
    }
    total > 0 && bad * 3 >= total
}

/// A prompt the runner recognises, and whether it means "commandable".
#[derive(Debug, Clone)]
pub struct Prompt {
    pub re: regex::Regex,
    pub raw: String,
    pub kind: PromptKind,
}

#[derive(Debug, Clone)]
pub struct Prompts(pub Vec<Prompt>);

impl Prompts {
    /// Classify the tail of the console.
    ///
    /// The three answers are deliberately distinct (§8.5): a shell prompt means
    /// go, a credential gate means the board is up but not commandable, and an
    /// unfamiliar idle line means "something is waiting for input and I do not
    /// know what" — which is a refusal, not a guess.
    pub fn classify(&self, tail: &str) -> Option<&Prompt> {
        // Match against the last non-empty line: a prompt is what the console
        // is *sitting at*, not something that scrolled past.
        //
        // ...but skip timestamped kernel messages when deciding which line that
        // is. A board that logs continuously never leaves its prompt as the last
        // line: measured on the IQ10 AP console, DP AUX timeouts arrive about
        // four times a second forever, so the shell prompt was always buried and
        // every run_command failed UNKNOWN_PROMPT against a perfectly healthy
        // shell. Kernel output is asynchronous by nature -- it is not what the
        // console is waiting at, so it should not decide what the console is
        // waiting at.
        let last = tail
            .lines()
            .rfind(|l| !l.trim().is_empty() && !is_async_kernel_line(l) && !is_line_noise(l))
            .or_else(|| tail.lines().rfind(|l| !l.trim().is_empty()))?;
        // A PRINTK CAN LAND ON THE PROMPT'S OWN LINE. Measured on the IQ10, half
        // an hour after it settled at a shell:
        //
        //   ESC[?2004hroot@debian-trixie-arm64:~# [  862.282959] phy phy-fc3a00...
        //
        // The shell is still sitting at that prompt -- the kernel simply
        // scribbled across the line, because printk does not care where the
        // cursor is. But a prompt pattern is anchored at end-of-line (it has to
        // be; that is what distinguishes a prompt from the word "root@host" in
        // some log message), so it cannot match, and console_state fell through
        // to `unstable` at a perfectly healthy shell. Round 3 taught this
        // function to look past kernel lines; this is the same fact one level
        // down, WITHIN a line.
        let trimmed = strip_trailing_kernel_noise(last);
        if let Some(p) = self
            .0
            .iter()
            .find(|p| p.re.is_match(last) || p.re.is_match(trimmed) || p.re.is_match(tail))
        {
            return Some(p);
        }
        // ASYNC OUTPUT IS NOT ALWAYS TIMESTAMPED.
        //
        // Everything above skips kernel-style `[  862.28]` lines, because async
        // output is not what the console is waiting at. An RTOS has the same
        // fact with none of the punctuation: measured on the Uno-Q, whose shell
        // sits at `sirocco> ` while the app prints bare `APP admit` / `CONSOLE`
        // lines after it. Nothing there looks like a kernel message, so the
        // prompt was two lines up and invisible, and the same console read
        // `at_prompt_with_traffic` from one call and `streaming` from the next.
        //
        // So walk back through the window. This can only ADD a match where there
        // was none -- the single-line test above still decides every case it can
        // -- and the window itself is the bound: a prompt that has scrolled out
        // of it is genuinely gone, which is what a long-running command should
        // look like.
        //
        // A prompt carrying a TYPED COMMAND is not matched, and that is the
        // safety rail: patterns are anchored, so `sirocco> reboot` fails
        // `^sirocco> $`. A board that was told to do something does not read as
        // idle just because its prompt is still on screen.
        tail.lines().rev().find_map(|l| {
            if l.trim().is_empty() {
                return None;
            }
            let t = strip_trailing_kernel_noise(l);
            self.0.iter().find(|p| p.re.is_match(l) || p.re.is_match(t))
        })
    }

    pub fn commandable(&self, tail: &str) -> Option<&Prompt> {
        self.classify(tail).filter(|p| p.kind.is_commandable())
    }
}

/// Options for one `run_command` call.
#[derive(Debug, Clone)]
pub struct CommandOptions {
    pub cfg: RunnerConfig,
    pub line: LineConfig,
    /// Per-character echo verification. Turned off for no-echo consoles, which
    /// downgrades to pacing-only mode.
    pub echo: bool,
    pub timeout_s: u64,
    pub max_output_bytes: usize,
    /// Send even at an unknown prompt. Explicit, never a default.
    pub force: bool,
}

impl CommandOptions {
    pub fn new(cfg: RunnerConfig, line: LineConfig) -> Self {
        Self {
            timeout_s: cfg.command_timeout_s,
            cfg,
            line,
            echo: true,
            max_output_bytes: 64 * 1024,
            force: false,
        }
    }
}

/// A transport the runner writes to and reads from. Abstracted so the §13
/// `runner` suite can drive every rung of the recovery ladder against a scripted
/// console, deterministically and without hardware.
#[allow(async_fn_in_trait)]
pub trait Transport: Send {
    async fn write_all(&mut self, data: &[u8]) -> Result<()>;
    /// Read whatever is available, waiting at most `timeout`. `Ok(None)` on
    /// timeout with nothing read.
    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>>;
}

/// TCP transport to a ser2net endpoint.
pub struct TcpTransport(pub TcpStream);

impl TcpTransport {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let sock = TcpStream::connect(endpoint).await.map_err(|e| {
            ToolError::new(
                ErrorCode::DeviceGone,
                format!("cannot reach {endpoint}: {e}"),
            )
            .with_hint("is ser2net running, and does the device still exist?")
        })?;
        let _ = sock.set_nodelay(true);
        Ok(Self(sock))
    }
}

/// Transport that READS from the console broker and WRITES to ser2net.
///
/// mcpd was the last consumer opening its own ser2net connection to a console
/// that minerd was already reading. Two readers of one device is the contention
/// class the broker exists to remove: when the device open failed, each reader
/// independently concluded "the board is quiet" while the tty was producing
/// data.
///
/// Reads and writes deliberately travel different paths. Transmit stays on a
/// direct ser2net connection so a broker outage can never swallow a keystroke or
/// a command headed for a board; only the read side is shared. If the broker is
/// unavailable this degrades to reading that same ser2net socket, so mcpd keeps
/// working when minerd is down.
pub struct BrokeredTransport {
    write: tokio::net::tcp::OwnedWriteHalf,
    read: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    /// True when the read side came from the broker, for diagnostics that would
    /// otherwise have to guess which path a console took.
    pub brokered: bool,
}

impl BrokeredTransport {
    pub async fn connect(
        endpoint: &str,
        broker_sock: &std::path::Path,
        device: &str,
    ) -> Result<Self> {
        let sock = TcpStream::connect(endpoint).await.map_err(|e| {
            ToolError::new(
                ErrorCode::DeviceGone,
                format!("cannot reach {endpoint}: {e}"),
            )
            .with_hint("is ser2net running, and does the device still exist?")
        })?;
        let _ = sock.set_nodelay(true);
        let (tcp_read, write) = sock.into_split();

        match crate::broker::connect(broker_sock, device).await {
            Ok(sub) => Ok(Self {
                write,
                read: Box::pin(sub),
                brokered: true,
            }),
            Err(_) => Ok(Self {
                write,
                read: Box::pin(tcp_read),
                brokered: false,
            }),
        }
    }
}

impl Transport for BrokeredTransport {
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.write.write_all(data).await.map_err(ToolError::from)?;
        self.write.flush().await.map_err(ToolError::from)
    }

    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        match tokio::time::timeout(timeout, self.read.read(buf)).await {
            Err(_) => Ok(None),
            Ok(Ok(0)) => Err(ToolError::new(
                ErrorCode::DeviceGone,
                "the console closed the connection",
            )),
            Ok(Ok(n)) => Ok(Some(n)),
            Ok(Err(e)) => Err(ToolError::from(e)),
        }
    }
}

impl Transport for TcpTransport {
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_all(data).await.map_err(ToolError::from)?;
        self.0.flush().await.map_err(ToolError::from)
    }

    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        match tokio::time::timeout(timeout, self.0.read(buf)).await {
            Err(_) => Ok(None),
            Ok(Ok(0)) => Err(ToolError::new(
                ErrorCode::DeviceGone,
                "the console closed the connection",
            )),
            Ok(Ok(n)) => {
                // Answer the negotiation before anything else: ser2net holds the
                // console until we do, so a silent client reads nothing at all.
                let refusals = telnet_refusals(&buf[..n]);
                if !refusals.is_empty() {
                    let _ = self.0.write_all(&refusals).await;
                    let _ = self.0.flush().await;
                }
                // ser2net's accepter is `telnet(rfc2217=false)`, so a fresh
                // connection opens with IAC negotiation. The runner dials its
                // own connection (see the module header), so it sees that
                // negotiation as if it were console bytes: measured on the IQ10
                // as `run_command` failing UNKNOWN_PROMPT with a preamble of
                // IAC WILL/DO pairs and no output. Whether a given connection
                // sees it depends on timing, which made this look like a flaky
                // board rather than a protocol bug.
                let cleaned = strip_telnet(&buf[..n]);
                if cleaned.len() != n {
                    buf[..cleaned.len()].copy_from_slice(&cleaned);
                }
                if cleaned.is_empty() {
                    // The whole chunk was negotiation. Report "nothing yet"
                    // rather than 0, which the caller reads as a closed console.
                    return Ok(None);
                }
                Ok(Some(cleaned.len()))
            }
            Ok(Err(e)) => Err(e.into()),
        }
    }
}

/// The refusal a client owes ser2net for each negotiation it is offered.
///
/// ser2net's accepter is `telnet(rfc2217=false)`, and it withholds console data
/// until the client answers. Measured on the IQ10: plain `nc` to port 5003 read
/// **zero bytes in five seconds** while minerd captured 130863 bytes on the same
/// port, and a client that answered the IAC DO/WILL immediately received console
/// output. The runner stripped negotiation but never replied, so it was starved
/// the same way and every `run_command` failed as NO_PROMPT with an empty buffer.
///
/// Refusing everything (WONT/DONT) is the correct minimal answer here: a raw
/// console needs no telnet options, it just needs the negotiation to conclude.
pub fn telnet_refusals(input: &[u8]) -> Vec<u8> {
    const IAC: u8 = 255;
    const WILL: u8 = 251;
    const WONT: u8 = 252;
    const DO: u8 = 253;
    const DONT: u8 = 254;
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < input.len() {
        if input[i] == IAC {
            let (cmd, opt) = (input[i + 1], input[i + 2]);
            match cmd {
                DO => out.extend_from_slice(&[IAC, WONT, opt]),
                WILL => out.extend_from_slice(&[IAC, DONT, opt]),
                _ => {}
            }
            if (WILL..=DONT).contains(&cmd) {
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Remove telnet IAC sequences from a console byte stream.
///
/// Shared by the runner and the dashboard: both dial ser2net's telnet accepter,
/// and two copies of this would drift.
/// True when a console's bytes are ser2net's device-open failure banner rather
/// than anything the board said.
///
/// ser2net does not retry a failed open and does not necessarily log it either:
/// it answers the CLIENT with this text and keeps the accepter bound, so the
/// port looks perfectly healthy from outside. Measured on the rig, five of six
/// RIDE consoles were serving this banner while conminer mined it as console
/// output -- ~81KB of "board output" that was pure error text on every one of
/// them, with only the always-on safety monitor carrying anything real.
///
/// Two things go wrong if this is not recognised, and the second is the
/// dangerous one:
///   * templates and boot logs fill with error text, and
///   * anything that verifies an action by "did bytes arrive" can be satisfied
///     by the banner, reporting a power action as confirmed when the console
///     never opened at all.
///
/// The check runs on telnet-stripped bytes because the banner arrives right
/// behind the IAC negotiation.
pub fn is_open_failure_banner(input: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&strip_telnet(input)).to_ascii_lowercase();
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    text.contains("device open failure")
        || (text.contains("open failure") && text.contains("already in use"))
}

pub fn strip_telnet(input: &[u8]) -> Vec<u8> {
    const IAC: u8 = 255;
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != IAC {
            out.push(input[i]);
            i += 1;
            continue;
        }
        match input.get(i + 1) {
            // Escaped literal 0xFF.
            Some(&IAC) => {
                out.push(IAC);
                i += 2;
            }
            // WILL/WONT/DO/DONT take one option byte.
            Some(&(251..=254)) => i += 3,
            // Subnegotiation runs to IAC SE.
            Some(&250) => {
                let mut j = i + 2;
                while j + 1 < input.len() && !(input[j] == IAC && input[j + 1] == 240) {
                    j += 1;
                }
                i = j + 2;
            }
            // Any other two-byte command.
            Some(_) => i += 2,
            // Truncated at the chunk boundary: drop it rather than emit a stray
            // 0xFF the terminal would draw.
            None => i += 1,
        }
    }
    out
}

/// Telnet stripping for a byte STREAM, rather than a single buffer.
///
/// [`strip_telnet`] is right for a message you hold in full: anything cut off at
/// the end is junk it drops. A capture socket is the other case -- bytes arrive
/// in whatever sizes the kernel hands over, so an `IAC WILL SGA` can and does
/// straddle two reads, and dropping the tail there loses a real console byte
/// while emitting the option byte as if the board had sent it.
///
/// WHY THE CAPTURE PATH NEEDS THIS AT ALL: ser2net's accepter is
/// `telnet(rfc2217=false)`, so every connection opens with a negotiation burst.
/// The runner has stripped it since it dialled its own connection; minerd never
/// did, and fed it straight to the framer. On the bravo node's first boot that
/// burst -- `FF FB 03 FF FD 03 FF FB 01 FF FD 01 FF FB 00 FF FD 00 00` -- became
/// the first thing ever recorded on the board's console, ahead of the kernel
/// log. It went unnoticed on hosts whose devices were first mined months ago,
/// because it lands once per attach at the very head of the stream.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TelnetFilter {
    state: TelnetState,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TelnetState {
    /// Ordinary console bytes.
    #[default]
    Data,
    /// Seen IAC; the next byte says what kind of command.
    Iac,
    /// Seen IAC WILL/WONT/DO/DONT; one option byte follows.
    Opt,
    /// Inside a subnegotiation, which runs to IAC SE.
    Sub,
    /// Inside a subnegotiation, having just seen IAC.
    SubIac,
}

impl TelnetFilter {
    /// Feed a chunk; get back only what the board actually sent.
    ///
    /// The state carries across calls, so a sequence split anywhere -- even
    /// between the IAC and its command byte -- is stripped exactly once and
    /// costs no console data on either side of the split.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        const IAC: u8 = 255;
        const SE: u8 = 240;
        const SB: u8 = 250;
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            self.state = match self.state {
                TelnetState::Data => {
                    if b == IAC {
                        TelnetState::Iac
                    } else {
                        out.push(b);
                        TelnetState::Data
                    }
                }
                TelnetState::Iac => match b {
                    // IAC IAC is an escaped literal 0xFF: real console data.
                    IAC => {
                        out.push(IAC);
                        TelnetState::Data
                    }
                    SB => TelnetState::Sub,
                    251..=254 => TelnetState::Opt,
                    _ => TelnetState::Data,
                },
                TelnetState::Opt => TelnetState::Data,
                TelnetState::Sub => {
                    if b == IAC {
                        TelnetState::SubIac
                    } else {
                        TelnetState::Sub
                    }
                }
                TelnetState::SubIac => {
                    if b == SE {
                        TelnetState::Data
                    } else {
                        TelnetState::Sub
                    }
                }
            };
        }
        out
    }

    /// True when a sequence is still open across the chunk boundary.
    pub fn mid_sequence(&self) -> bool {
        self.state != TelnetState::Data
    }
}

/// The transaction engine.
pub struct Runner<T: Transport> {
    io: T,
    prompts: Prompts,
    opts: CommandOptions,
    /// Everything read but not yet consumed by a step.
    buf: String,
    /// Bytes read from the transport across this transaction, and where they
    /// were read from. Carried into every failure: NO_PROMPT is reachable ONLY
    /// when the buffer is empty, and without `bytes_read` an agent cannot tell
    /// "I do not recognise this prompt" from "I received nothing at all". That
    /// distinction cost three wrong fixes on the IQ10 before anyone read the
    /// branch condition, so the answer now travels with the error.
    bytes_read: usize,
    endpoint: String,
}

impl<T: Transport> Runner<T> {
    /// Record where this runner's transport is pointed, so failures can say so.
    /// Without it an agent seeing NO_PROMPT cannot tell a wrong endpoint from a
    /// silent board.
    pub fn at(mut self, endpoint: &str) -> Self {
        self.endpoint = endpoint.to_string();
        self
    }

    pub fn new(io: T, prompts: Prompts, opts: CommandOptions) -> Self {
        Self {
            io,
            prompts,
            opts,
            buf: String::new(),
            bytes_read: 0,
            endpoint: String::new(),
        }
    }

    /// Read whatever is available for up to `d`, appending to the buffer.
    async fn soak(&mut self, d: Duration) -> Result<()> {
        let mut raw = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            match self.io.read(&mut raw, left).await? {
                None => return Ok(()),
                Some(n) => {
                    self.bytes_read += n;
                    self.buf.push_str(&String::from_utf8_lossy(&raw[..n]));
                }
            }
        }
    }

    /// Wait until the console has been quiet for `settle_quiet_ms`.
    async fn settle(&mut self) -> Result<()> {
        let quiet = Duration::from_millis(self.opts.cfg.settle_quiet_ms.max(1));
        // A console is not required to ever go quiet. The IQ10 AP console emits
        // DP AUX timeouts about four times a second, forever, so waiting for
        // silence waits for something that never arrives and the command dies
        // without ever probing for a prompt. Bound the wait: after this, work
        // with the buffer we have -- a prompt among noise is still a prompt.
        let deadline = tokio::time::Instant::now() + quiet.saturating_mul(20);
        let mut raw = [0u8; 4096];
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            match self.io.read(&mut raw, quiet).await? {
                None => return Ok(()),
                Some(n) => {
                    self.bytes_read += n;
                    self.buf.push_str(&String::from_utf8_lossy(&raw[..n]));
                }
            }
        }
    }

    fn tail(&self) -> String {
        let n = self.buf.len().saturating_sub(2048);
        self.buf[n..].to_string()
    }

    /// Step 2: assert the console is idle at a prompt we understand.
    ///
    /// Never fires a command into an unknown state. If the line has been quiet
    /// and no prompt is buffered, a bare newline probe is sent — that is a
    /// question, not a command.
    // A failed transaction IS the return value here: it carries the tail, the
    // rungs attempted and the evidence a caller needs, and boxing it would buy
    // a pointer at the cost of every construction site. Newer clippy flags the
    // Err size; the shape is deliberate.
    #[allow(clippy::result_large_err)]
    async fn assert_idle(&mut self) -> std::result::Result<String, Transaction> {
        self.settle().await.map_err(|e| {
            Transaction::failed(Outcome::NoPrompt, "", self.tail(), json!({"io": e.message}))
        })?;

        if self.prompts.classify(&self.tail()).is_none() {
            // Nothing recognisable buffered: ask.
            //
            // THE CALLER'S BUDGET IS THE ANSWER'S BUDGET.
            //
            // This waited `settle_quiet_ms` (~200ms-1s) no matter what the
            // caller asked for, so a board slower than that was reported
            // NO_PROMPT while being perfectly alive -- and the agent, told the
            // console was not at a prompt, filed it again (#15, #16, #35).
            // Measured on the Uno-Q: its first echo of a bare newline arrives
            // 3.84s later, against a probe that gave up at 1.0s while the call
            // itself carried `timeout_s: 20`.
            //
            // So the probe waits as long as the caller said it could, in small
            // steps, and stops the moment a prompt appears. A fast console is
            // no slower than before: this returns on the first recognisable
            // tail, not at the deadline.
            let probe = self.opts.line.tx_line_ending.clone();
            if self.io.write_all(probe.as_bytes()).await.is_ok() {
                let step = Duration::from_millis(self.opts.cfg.settle_quiet_ms.max(200));
                let budget = Duration::from_secs(self.opts.timeout_s.max(1));
                let deadline = std::time::Instant::now() + budget;
                loop {
                    let _ = self.soak(step).await;
                    if self.prompts.classify(&self.tail()).is_some() {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }

        let tail = self.tail();
        match self.prompts.classify(&tail) {
            Some(p) if p.kind.is_commandable() => Ok(p.raw.clone()),
            Some(p) if p.kind == PromptKind::CredentialGate => Err(Transaction::failed(
                Outcome::LoginRequired,
                "",
                tail,
                json!({
                    "gate": p.raw,
                    "why": "the board is up but not commandable without credentials",
                }),
            )),
            Some(p) => Err(Transaction::failed(
                Outcome::UnknownPrompt,
                "",
                tail,
                json!({"matched": p.raw, "kind": p.kind.as_str()}),
            )),
            None if self.opts.force => Ok(String::new()),
            None if !self.buf.trim().is_empty() => Err(Transaction::failed(
                Outcome::UnknownPrompt,
                "",
                tail,
                json!({
                    "why": "the console is idling at a line matching neither a known prompt \
                            nor a known credential gate",
                    "teach": "call classify_prompt() to teach it, or pass force:true",
                }),
            )),
            None => Err(Transaction::failed(
                Outcome::NoPrompt,
                "",
                tail,
                json!({"why": "no prompt appeared after a newline probe"}),
            )),
        }
    }

    /// Step 3: send character-at-a-time with per-character echo verification.
    // A failed transaction IS the return value here: it carries the tail, the
    // rungs attempted and the evidence a caller needs, and boxing it would buy
    // a pointer at the cost of every construction site. Newer clippy flags the
    // Err size; the shape is deliberate.
    #[allow(clippy::result_large_err)]
    async fn send_command(&mut self, cmd: &str) -> std::result::Result<(), Transaction> {
        let delay = Duration::from_millis(self.opts.cfg.char_delay_ms);
        let echo_wait = Duration::from_millis(self.opts.cfg.echo_timeout_ms.max(1));

        for (i, ch) in cmd.char_indices() {
            let bytes = cmd.as_bytes()[i..i + ch.len_utf8()].to_vec();
            let mut attempt = 0;
            loop {
                if let Err(e) = self.io.write_all(&bytes).await {
                    return Err(Transaction::failed(
                        Outcome::EchoMismatch,
                        cmd,
                        self.tail(),
                        json!({"io": e.message, "at_char": i}),
                    ));
                }
                if !self.opts.echo {
                    // No-echo console: pacing only, which is the honest
                    // downgrade — we cannot verify what we cannot observe.
                    break;
                }
                let before = self.buf.len();
                let _ = self.soak(echo_wait).await;
                let echoed = self.buf[before..].contains(ch);
                if echoed {
                    break;
                }
                attempt += 1;
                if attempt >= 2 {
                    // Retried once and still lost: abort rather than send a
                    // half-formed command into a shell.
                    return Err(Transaction::failed(
                        Outcome::EchoMismatch,
                        cmd,
                        self.tail(),
                        json!({
                            "at_char": i,
                            "expected": ch.to_string(),
                            "observed": self.buf[before..].to_string(),
                            "attempts": attempt,
                        }),
                    ));
                }
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(())
    }

    /// Steps 4-6: terminate, capture until the prompt returns, and report.
    pub async fn run(mut self, cmd: &str) -> Result<Transaction> {
        let started = std::time::Instant::now();

        let prompt = match self.assert_idle().await {
            Ok(p) => p,
            Err(mut t) => {
                t.command = cmd.to_string();
                t.duration_ms = started.elapsed().as_millis() as u64;
                // Every failure carries how much was read and from where.
                return Ok(t.with_io(self.bytes_read, &self.endpoint));
            }
        };

        // Everything up to here is context, not command output.
        let preamble = self.tail();
        self.buf.clear();

        if let Err(mut t) = self.send_command(cmd).await {
            t.duration_ms = started.elapsed().as_millis() as u64;
            t.preamble = preamble;
            return Ok(t.with_io(self.bytes_read, &self.endpoint));
        }
        self.io
            .write_all(self.opts.line.tx_line_ending.clone().as_bytes())
            .await?;

        // Capture until the prompt returns.
        let deadline = std::time::Instant::now() + Duration::from_secs(self.opts.timeout_s);
        let mut matched = None;
        while std::time::Instant::now() < deadline {
            self.soak(Duration::from_millis(100)).await?;
            if let Some(p) = self.prompts.commandable(&self.buf) {
                matched = Some(p.raw.clone());
                break;
            }
            if self.buf.len() > self.opts.max_output_bytes * 4 {
                break;
            }
        }

        if matched.is_none() {
            let mut t = self.recover(cmd).await?;
            t.duration_ms = started.elapsed().as_millis() as u64;
            t.preamble = preamble;
            return Ok(t);
        }

        let (output, capped) = self.take_output(cmd);
        Ok(Transaction {
            status: Outcome::Ok,
            command: cmd.to_string(),
            output,
            output_capped: capped,
            duration_ms: started.elapsed().as_millis() as u64,
            prompt_matched: matched.or(Some(prompt)),
            rungs_attempted: Vec::new(),
            preamble,
            detail: json!({}),
        })
    }

    /// Trim the echoed command and the trailing prompt out of the captured text.
    /// Remove DEC private mode sequences (`ESC [ ? … h|l`) from command output.
    ///
    /// A shell with bracketed paste enabled brackets every command with
    /// `ESC[?2004h` / `ESC[?2004l`, so `run_command("cat …")` returns output
    /// beginning `[?2004l` — noise that is not the board's answer, and that an
    /// agent parsing the result has to know to ignore.
    ///
    /// Only the DEC *private mode* family is dropped, not escapes generally.
    /// These set terminal state (paste bracketing, cursor keys, alt screen) and
    /// never carry content, whereas SGR colour runs are part of what the board
    /// actually printed — kernel taint colouring, U-Boot's red errors — and the
    /// linesplit layer deliberately treats escapes as content, not structure.
    fn strip_dec_private_modes_impl(s: &str) -> String {
        if !s.contains("\x1b[?") {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == 0x1b && i + 2 < b.len() && b[i + 1] == b'[' && b[i + 2] == b'?' {
                // Consume through the final byte (@..~), which terminates the CSI.
                let mut j = i + 3;
                while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                    j += 1;
                }
                i = if j < b.len() { j + 1 } else { b.len() };
                continue;
            }
            let ch_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
        out
    }

    /// Byte offset just past the echoed command when an async message split it.
    ///
    /// A Linux console prints kernel messages whenever it likes, including in
    /// the middle of echoing back what was typed. Measured on the IQ10 AP
    /// console, `cat /sys/kernel/debug/clk/clk_summary | head -8` echoed as
    /// `cat /sys/kernel/debu[   53.806314] platform 3d6a000.gmu: …\r\ng/clk/…`,
    /// so the contiguous command never appears and the exact-match strip above
    /// leaves the echo debris sitting at the top of the output.
    ///
    /// So match the command's characters *in order*, tolerating anything
    /// injected between them, and report where the last one lands. Only ever
    /// consulted after both exact matches fail, and only the echo region is
    /// scanned: the interleaved kernel line itself stays in the output, because
    /// it is real board output and dropping it would hide a message that may be
    /// exactly what the caller needed to see.
    fn take_output(&mut self, cmd: &str) -> (String, bool) {
        let raw = Self::strip_dec_private_modes_impl(&std::mem::take(&mut self.buf));
        let mut body = raw.as_str();
        if let Some(rest) = body.strip_prefix(cmd) {
            body = rest.trim_start_matches(['\r', '\n']);
        } else if let Some(idx) = body.find(cmd) {
            body = body[idx + cmd.len()..].trim_start_matches(['\r', '\n']);
        } else if let Some(end) = echo_end_interleaved(body, cmd) {
            body = body[end..].trim_start_matches(['\r', '\n']);
        }
        // Drop the final prompt line the console printed to say it was ready.
        let mut lines: Vec<&str> = body.split('\n').collect();
        if let Some(last) = lines.last() {
            if self.prompts.commandable(last).is_some() {
                lines.pop();
            }
        }
        let mut out = lines.join("\n");
        let capped = out.len() > self.opts.max_output_bytes;
        if capped {
            out.truncate(self.opts.max_output_bytes);
            out.push_str("\n… output capped; use get_records/get_context for the rest");
        }
        (out.trim_end().to_string(), capped)
    }

    /// The hung-command recovery ladder (§8.3).
    ///
    /// Each rung is attempted in order, recorded, and stops at the first that
    /// restores the prompt. Exhausting it is a truthful `hung`, not a guess.
    async fn recover(&mut self, cmd: &str) -> Result<Transaction> {
        let mut rungs = Vec::new();

        // Rung 1: the prompt may merely have scrolled away.
        let ladder: Vec<(String, String)> = std::iter::once((
            "newline_probe".to_string(),
            self.opts.line.tx_line_ending.clone(),
        ))
        .chain(
            self.opts
                .cfg
                .escape_set
                .clone()
                .into_iter()
                .map(|e| (e.clone(), escape_bytes(&e))),
        )
        .collect();

        for (name, seq) in ladder {
            let _ = self.io.write_all(seq.as_bytes()).await;
            let _ = self
                .soak(Duration::from_millis(
                    self.opts.cfg.settle_quiet_ms.max(300),
                ))
                .await;
            let recovered = self.prompts.commandable(&self.buf).is_some();
            rungs.push(Rung {
                name: name.clone(),
                sent: seq.escape_debug().to_string(),
                recovered,
            });
            if recovered {
                let (output, capped) = self.take_output(cmd);
                return Ok(Transaction {
                    status: Outcome::Ok,
                    command: cmd.to_string(),
                    output,
                    output_capped: capped,
                    duration_ms: 0,
                    prompt_matched: self.prompts.commandable("").map(|p| p.raw.clone()),
                    rungs_attempted: rungs,
                    preamble: String::new(),
                    detail: json!({"recovered_by": name}),
                });
            }
        }

        let tail = self.tail();
        Ok(Transaction {
            status: Outcome::Hung,
            command: cmd.to_string(),
            output: tail.clone(),
            output_capped: false,
            duration_ms: 0,
            prompt_matched: None,
            rungs_attempted: rungs,
            preamble: String::new(),
            detail: json!({
                "why": "the prompt did not return and every recovery rung failed",
                "next": "consider recover:\"power\" to invoke the device's reset hook. If a reset does not bring the console back, escalate to a POWER CYCLE rather than resetting again: measured on the IQ10, the board wedges after roughly eight consecutive resets and every further reset still reports success while opening an epoch that captures nothing. Only a cycle recovers it.",
                "output_so_far": tail,
            }),
        })
    }
}

/// Turn a config escape name (`C-c`, `C-\`, `~.`) into the bytes to send.
pub fn escape_bytes(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("C-") {
        if let Some(c) = rest.chars().next() {
            let up = c.to_ascii_uppercase() as u8;
            // Control characters are the letter minus 0x40.
            if (0x40..=0x5f).contains(&up) {
                return ((up - 0x40) as char).to_string();
            }
        }
    }
    name.to_string()
}

#[cfg(test)]
mod telnet_filter_tests {
    use super::*;

    /// The exact burst ser2net sends on connect, from the bravo node's wire.
    const NEGOTIATION: &[u8] = &[
        0xFF, 0xFB, 0x03, 0xFF, 0xFD, 0x03, 0xFF, 0xFB, 0x01, 0xFF, 0xFD, 0x01, 0xFF, 0xFB, 0x00,
        0xFF, 0xFD, 0x00,
    ];

    #[test]
    fn the_opening_burst_yields_no_console_bytes() {
        let mut f = TelnetFilter::default();
        assert!(f.push(NEGOTIATION).is_empty());
        assert!(!f.mid_sequence());
        assert_eq!(f.push(b"login: "), b"login: ".to_vec());
    }

    /// The reason this is a stream filter and not [`strip_telnet`].
    ///
    /// A 64 KB read can end anywhere, including between an IAC and its command.
    /// Byte-at-a-time is the worst case and must behave identically to one
    /// chunk -- otherwise the console loses a byte on one side of the split and
    /// gains an option byte on the other.
    #[test]
    fn a_sequence_split_anywhere_survives_the_boundary() {
        let stream: Vec<u8> = NEGOTIATION
            .iter()
            .copied()
            .chain(b"[    0.1] boot\n".iter().copied())
            .collect();
        for split in 0..stream.len() {
            let mut f = TelnetFilter::default();
            let mut out = f.push(&stream[..split]);
            out.extend(f.push(&stream[split..]));
            assert_eq!(
                out,
                b"[    0.1] boot\n".to_vec(),
                "split at {split} changed the console output"
            );
        }
    }

    #[test]
    fn an_escaped_ff_is_console_data_and_survives() {
        let mut f = TelnetFilter::default();
        assert_eq!(f.push(&[b'a', 0xFF, 0xFF, b'b']), vec![b'a', 0xFF, b'b']);
    }

    #[test]
    fn a_subnegotiation_is_dropped_whole_even_when_it_spans_chunks() {
        let mut f = TelnetFilter::default();
        // IAC SB 44 <payload> IAC SE, cut in the middle of the payload.
        assert!(f.push(&[0xFF, 0xFA, 44, 1, 2]).is_empty());
        assert!(f.mid_sequence());
        assert_eq!(f.push(&[3, 0xFF, 0xF0, b'o', b'k']), b"ok".to_vec());
    }

    /// A literal 0xFF inside a subnegotiation payload is escaped as IAC IAC and
    /// must not be mistaken for the terminator's IAC.
    #[test]
    fn an_escaped_ff_inside_a_subnegotiation_does_not_end_it() {
        let mut f = TelnetFilter::default();
        assert_eq!(
            f.push(&[0xFF, 0xFA, 44, 0xFF, 0xFF, 9, 0xFF, 0xF0, b'x']),
            b"x".to_vec()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompts() -> Prompts {
        Prompts(vec![
            Prompt {
                re: regex::Regex::new(r"(^|\n)# $").unwrap(),
                raw: "# ".into(),
                kind: PromptKind::Shell,
            },
            Prompt {
                re: regex::Regex::new(r"(^|\n)[\w.-]+ login: *$").unwrap(),
                raw: "login: ".into(),
                kind: PromptKind::CredentialGate,
            },
        ])
    }

    /// A console slower than `settle_quiet_ms` is not a console without a
    /// prompt (reports #15, #16, #35).
    ///
    /// This transport answers a newline after 2s, like the Uno-Q whose first
    /// echo was measured at 3.84s. The probe used to wait a fixed ~200ms-1s and
    /// call it NO_PROMPT; with the caller's `timeout_s` honoured it waits, sees
    /// the prompt, and the command runs.
    struct SlowConsole {
        wrote_at: Option<std::time::Instant>,
        delay: Duration,
        sent: bool,
    }

    impl Transport for SlowConsole {
        async fn write_all(&mut self, _data: &[u8]) -> Result<()> {
            self.wrote_at.get_or_insert_with(std::time::Instant::now);
            Ok(())
        }
        async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
            let ready = self.wrote_at.is_some_and(|t| t.elapsed() >= self.delay);
            if ready && !self.sent {
                self.sent = true;
                let out = b"\n# ";
                buf[..out.len()].copy_from_slice(out);
                return Ok(Some(out.len()));
            }
            tokio::time::sleep(timeout.min(Duration::from_millis(50))).await;
            Ok(None)
        }
    }

    #[tokio::test]
    async fn a_console_slower_than_the_settle_window_still_reaches_its_prompt() {
        let cfg = RunnerConfig {
            settle_quiet_ms: 200,
            ..Default::default()
        };
        let opts = CommandOptions {
            timeout_s: 10, // what the caller asked for
            ..CommandOptions::new(cfg, LineConfig::default())
        };
        let io = SlowConsole {
            wrote_at: None,
            delay: Duration::from_secs(2),
            sent: false,
        };
        let mut r = Runner::new(io, prompts(), opts);
        let got = r.assert_idle().await;
        assert!(
            got.is_ok(),
            "a prompt arriving inside the caller's budget must be seen, not called NO_PROMPT: {:?}",
            got.err().map(|t| t.status)
        );
    }

    #[test]
    fn control_escapes_map_to_real_bytes() {
        assert_eq!(escape_bytes("C-c"), "\u{3}");
        assert_eq!(escape_bytes("C-d"), "\u{4}");
        assert_eq!(escape_bytes("C-\\"), "\u{1c}");
        assert_eq!(escape_bytes("~."), "~.");
    }

    #[test]
    fn a_credential_gate_is_classified_but_never_commandable() {
        let p = prompts();
        assert_eq!(
            p.classify("board login: ").unwrap().kind,
            PromptKind::CredentialGate
        );
        assert!(p.commandable("board login: ").is_none());
        assert!(p.commandable("# ").is_some());
    }

    #[test]
    fn a_prompt_that_merely_scrolled_past_is_not_the_current_state() {
        let p = prompts();
        // The `# ` here is history, and the console is now emitting a log line.
        assert!(p
            .commandable("# ls\nfoo bar\n[  12.0] usb 1-1: new device\n")
            .is_none());
    }

    #[test]
    fn outcomes_map_onto_the_error_taxonomy() {
        assert_eq!(Outcome::Ok.error_code(), None);
        assert_eq!(Outcome::NoPrompt.error_code(), Some(ErrorCode::NoPrompt));
        assert_eq!(
            Outcome::LoginRequired.error_code(),
            Some(ErrorCode::LoginRequired)
        );
        assert_eq!(Outcome::Hung.error_code(), Some(ErrorCode::Hung));
    }
}

pub(crate) fn echo_end_interleaved(body: &str, cmd: &str) -> Option<usize> {
    let cmd_trimmed = cmd.trim();
    if cmd_trimmed.is_empty() {
        return None;
    }
    let mut want = cmd_trimmed.chars();
    let mut next = want.next()?;
    for (i, c) in body.char_indices() {
        // A newline ends the echo line: past it, this is output, not echo.
        if c == next {
            match want.next() {
                Some(n) => next = n,
                // Matched the whole command; the echo ends after this char.
                None => return Some(i + c.len_utf8()),
            }
        }
    }
    None
}

#[cfg(test)]
mod echo_split_tests {
    use super::echo_end_interleaved;

    /// The literal buffer captured from the IQ10 AP console on 2026-08-11: a gmu
    /// message landed inside the echo of `cat …/clk_summary`, so the contiguous
    /// command never appears and the exact-match strip leaves debris on top.
    #[test]
    fn split_echo_from_real_hardware_is_located() {
        let cmd = "cat /sys/kernel/debug/clk/clk_summary | head -8";
        let body = "cat /sys/kernel/debu[   53.806314] platform 3d6a000.gmu: NORD \
                    JTAG-HOLD 45s: AO(1f888)=0x0\r\ng/clk/clk_summary | head -8\r\n\
                    \r   clock  enable  prepare\r\n   gcc_ufs  1  1\r\n";
        // The exact-match strip that ships ahead of this must genuinely fail,
        // otherwise the fallback is never reached and this test proves nothing.
        assert!(
            body.find(cmd).is_none(),
            "exact match unexpectedly succeeded"
        );

        let end = echo_end_interleaved(body, cmd).expect("echo end not found");
        let rest = body[end..].trim_start_matches(['\r', '\n']);
        assert!(rest.starts_with("   clock  enable"), "rest = {rest:?}");
        assert!(
            !rest.contains("clk_summary"),
            "echo debris survived: {rest:?}"
        );
        // The interleaved kernel line is real board output: it must NOT be eaten.
        assert!(body[..end].contains("NORD JTAG-HOLD"));
    }

    #[test]
    fn a_clean_echo_needs_no_fallback_and_an_absent_command_yields_none() {
        assert_eq!(echo_end_interleaved("ls\r\nbin dev\r\n", "ls"), Some(2));
        assert_eq!(
            echo_end_interleaved("totally unrelated\r\n", "ls -la /tmp"),
            None
        );
        assert_eq!(echo_end_interleaved("anything", "   "), None);
    }
}

#[cfg(test)]
mod open_banner_tests {
    use super::*;

    /// Verbatim from the rig: the banner arrives directly behind the telnet IAC
    /// negotiation, which is why the check has to strip telnet first.
    #[test]
    fn the_banner_behind_telnet_negotiation_is_recognised() {
        let mut wire = vec![
            0xff, 0xfb, 0x03, 0xff, 0xfd, 0x03, 0xff, 0xfb, 0x01, 0xff, 0xfe, 0x01,
        ];
        wire.extend_from_slice(b"Device open failure: Object was already in use\r\n");
        assert!(is_open_failure_banner(&wire));
    }

    #[test]
    fn real_board_output_is_never_mistaken_for_the_banner() {
        // A real XBL boot log from this board, and a normal shell line.
        assert!(!is_open_failure_banner(
            b"S - QC_IMAGE_VERSION_STRING=non_gearvm_nordau-000-dev_build\r\n"
        ));
        assert!(!is_open_failure_banner(b"Total SM Health: 0\r\n"));
        assert!(!is_open_failure_banner(b""));
        // Telnet negotiation alone is not a failure -- it is just a quiet board.
        assert!(!is_open_failure_banner(&[
            0xff, 0xfb, 0x03, 0xff, 0xfd, 0x03
        ]));
    }
}

#[cfg(test)]
mod prompt_under_traffic_tests {
    use super::*;

    fn rtos_prompts() -> Prompts {
        Prompts(vec![Prompt {
            re: regex::Regex::new("^sirocco> $").unwrap(),
            raw: "^sirocco> $".into(),
            kind: crate::framer::profile::PromptKind::RtosShell,
        }])
    }

    /// A PROMPT WITH ASYNC OUTPUT PRINTED AFTER IT IS STILL A PROMPT.
    ///
    /// Everything here skipped kernel-style `[  862.28]` lines, on the correct
    /// reasoning that async output is not what the console is waiting at. An
    /// RTOS has the same fact with none of the punctuation: the Uno-Q sits at
    /// `sirocco> ` while its app prints bare `APP admit` / `CONSOLE` lines, so
    /// the prompt ended up two lines up and invisible -- and one call read
    /// `at_prompt_with_traffic` while the next read `streaming`.
    #[test]
    fn a_prompt_under_untimestamped_traffic_is_still_found() {
        let p = rtos_prompts();
        assert!(
            p.classify("sirocco> \nAPP admit\nCONSOLE").is_some(),
            "an RTOS prompt with plain output after it must still be recognised"
        );
        assert!(
            p.commandable("sirocco> \nAPP admit\nCONSOLE").is_some(),
            "and it is commandable: run_command asserts the prompt on the wire anyway"
        );
    }

    /// THE SAFETY RAIL. A prompt carrying a typed command is a board that was
    /// told to do something, not an idle one -- and anchored patterns are what
    /// keep the backwards scan from claiming otherwise.
    #[test]
    fn a_prompt_with_a_command_typed_at_it_is_not_idle() {
        let p = rtos_prompts();
        assert!(
            p.classify("sirocco> reboot\nRebooting...").is_none(),
            "a command was typed; the board is not sitting at that prompt"
        );
    }

    /// A prompt that has scrolled out of the window is genuinely gone, which is
    /// what a long-running command must look like.
    #[test]
    fn a_prompt_outside_the_window_is_not_resurrected() {
        let p = rtos_prompts();
        let scrolled = (0..12)
            .map(|i| format!("flash: writing block {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            p.classify(&scrolled).is_none(),
            "nothing in this window is a prompt"
        );
    }
}
