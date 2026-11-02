//! Suite `credentials` (§8.5, §13 `config`): console login from
//! `credentials.file`.
//!
//! Driven end to end through the MCP handler against a scripted console on a
//! real TCP socket, which stands where ser2net stands. The console runs a login
//! state machine (user, password without echo, shell), so every assertion here
//! is about bytes a board would actually have received.
//!
//! The leak tests search every sink a password could reach: every file under
//! the data directory (registry, device stores, their WAL), an exported session,
//! every tool response the scenario produced, and everything logged through
//! `tracing` at TRACE level while it ran.

use conminer_core::config::Config;
use conminer_core::credentials::{self, Login, MARKER};
use conminer_core::store::registry::{IdentityKind, Registry};
use conminer_testkit::mcp::McpRig;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, OnceLock};

const DEV: &str = "/dev/serial/by-id/usb-Fixture_Login-if01-port0";
const OTHER: &str = "/dev/serial/by-id/usb-Fixture_Other-if00-port0";
const USER: &str = "root";
/// Distinctive, so a substring search cannot match anything else.
const PASSWORD: &str = "Zq7-secret-Kp4w";

// ------------------------------------------------------------- the console --

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Login,
    Password,
    Shell,
}

struct Board {
    phase: Phase,
    line: String,
    user: String,
    /// Everything the runner typed at the board.
    received: Vec<u8>,
    /// A broken console that echoes the password.
    echo_password: bool,
    logins_ok: usize,
}

#[derive(Clone)]
struct FakeBoard(Arc<Mutex<Board>>);

impl FakeBoard {
    fn start(echo_password: bool) -> (Self, u16) {
        let b = FakeBoard(Arc::new(Mutex::new(Board {
            phase: Phase::Login,
            line: String::new(),
            user: String::new(),
            received: Vec::new(),
            echo_password,
            logins_ok: 0,
        })));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let board = b.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let board = board.clone();
                std::thread::spawn(move || board.serve(sock));
            }
        });
        (b, port)
    }

    fn serve(&self, mut sock: std::net::TcpStream) {
        let mut buf = [0u8; 256];
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut out = Vec::new();
            {
                let mut b = self.0.lock().unwrap();
                for &c in &buf[..n] {
                    b.received.push(c);
                    if c == b'\r' || c == b'\n' {
                        let line = std::mem::take(&mut b.line);
                        out.extend(b"\r\n");
                        match b.phase {
                            Phase::Login if line.is_empty() => out.extend(b"fixture login: "),
                            Phase::Login => {
                                b.user = line;
                                b.phase = Phase::Password;
                                out.extend(b"Password: ");
                            }
                            Phase::Password => {
                                if b.user == USER && line == PASSWORD {
                                    b.phase = Phase::Shell;
                                    b.logins_ok += 1;
                                    out.extend(b"Last login: never\r\nroot@fixture:~# ");
                                } else {
                                    b.phase = Phase::Login;
                                    out.extend(b"\r\nLogin incorrect\r\nfixture login: ");
                                }
                            }
                            Phase::Shell => {
                                if line == "uname -a" {
                                    out.extend(
                                        b"Linux fixture 6.18.0 #1 SMP aarch64 GNU/Linux\r\n",
                                    );
                                }
                                out.extend(b"root@fixture:~# ");
                            }
                        }
                        continue;
                    }
                    b.line.push(c as char);
                    if b.phase != Phase::Password || b.echo_password {
                        out.push(c);
                    }
                }
            }
            if sock.write_all(&out).is_err() {
                return;
            }
        }
    }

    fn received(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap().received).into_owned()
    }

    fn logins_ok(&self) -> usize {
        self.0.lock().unwrap().logins_ok
    }

    fn log_out(&self) {
        let mut b = self.0.lock().unwrap();
        b.phase = Phase::Login;
        b.line.clear();
    }
}

// --------------------------------------------------------------- the logs ---

/// Everything `tracing` emitted, process-wide, at TRACE.
fn log_sink() -> Arc<Mutex<Vec<u8>>> {
    static SINK: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let w = sink.clone();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || LogWriter(w.clone()))
            .finish();
        let _ = tracing::subscriber::set_global_default(sub);
        sink
    })
    .clone()
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- the rig ---

struct Fixture {
    rig: McpRig,
    board: FakeBoard,
    creds: std::path::PathBuf,
    _creds_dir: tempfile::TempDir,
    /// Every tool response, for the leak search.
    responses: Mutex<Vec<String>>,
}

fn write_creds(path: &std::path::Path, body: &str, mode: u32) {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn entry_for(dev: &str, password: &str) -> String {
    format!("[devices.\"{dev}\"]\nuser = \"{USER}\"\npassword = \"{password}\"\n")
}

impl Fixture {
    fn new(echo_password: bool) -> Self {
        let _ = log_sink();
        let (board, port) = FakeBoard::start(echo_password);
        let creds_dir = tempfile::tempdir().unwrap();
        let creds = creds_dir.path().join("credentials.toml");
        write_creds(&creds, &entry_for(DEV, PASSWORD), 0o600);

        let mut c = Config::default();
        c.ser2net.connect_host = "127.0.0.1".into();
        c.credentials.file = creds.display().to_string();
        c.runner.char_delay_ms = 0;
        c.runner.echo_timeout_ms = 100;
        c.runner.settle_quiet_ms = 50;
        let rig = McpRig::with_config(c);
        {
            let mut reg = Registry::open(rig.dir.path()).unwrap();
            let d = reg
                .upsert_device(DEV, None, IdentityKind::ById, None, 0)
                .unwrap();
            reg.set_state(d.id, "listening").unwrap();
            reg.assign_port(d.id, port).unwrap();
        }
        rig.call("acquire", json!({"device": DEV, "ttl_s": 600}));
        Self {
            rig,
            board,
            creds,
            _creds_dir: creds_dir,
            responses: Mutex::new(Vec::new()),
        }
    }

    fn raw(&self, name: &str, args: Value) -> Value {
        let v = self.rig.raw(name, args);
        self.responses.lock().unwrap().push(v.to_string());
        v
    }

    fn run(&self, cmd: &str) -> Value {
        self.raw(
            "run_command",
            json!({"device": DEV, "command": cmd, "timeout_s": 5, "verbose": true}),
        )
    }

    /// Every byte this scenario left anywhere a password must never be.
    fn assert_no_leak(&self) {
        let mut hits = Vec::new();
        for f in walk(self.rig.dir.path()) {
            let bytes = std::fs::read(&f).unwrap_or_default();
            if contains(&bytes, PASSWORD.as_bytes()) {
                hits.push(format!("file {}", f.display()));
            }
        }
        for (i, r) in self.responses.lock().unwrap().iter().enumerate() {
            if r.contains(PASSWORD) {
                hits.push(format!("tool response #{i}: {r}"));
            }
        }
        if contains(&log_sink().lock().unwrap(), PASSWORD.as_bytes()) {
            hits.push("tracing log".into());
        }
        assert!(hits.is_empty(), "the password leaked into: {hits:#?}");
    }

    /// The sinks must have been written to at all, or "no leak" is vacuous.
    fn store_bytes(&self) -> Vec<u8> {
        walk(self.rig.dir.path())
            .into_iter()
            .flat_map(|f| std::fs::read(f).unwrap_or_default())
            .collect()
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            // A symlink is a name, not a sink; the file it names is judged
            // where it lives.
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn sc(v: &Value) -> &Value {
    &v["structuredContent"]
}

// ------------------------------------------------------------------- tests --

/// The whole point: a console at a login prompt is logged in from the file and
/// the command runs, with the password sent to the board and nowhere else.
#[test]
fn run_command_logs_in_at_a_gate_and_the_password_reaches_only_the_board() {
    let fx = Fixture::new(false);
    let r = fx.run("uname -a");
    assert_eq!(r["isError"], false, "run_command at a login prompt: {r}");
    let t = &sc(&r)["transaction"];
    assert_eq!(t["status"], "ok", "{t}");
    assert!(
        t["output"]
            .as_str()
            .unwrap()
            .contains("Linux fixture 6.18.0"),
        "the command ran in the logged-in shell: {t}"
    );
    assert_eq!(
        t["detail"]["login"],
        json!({"user": USER, "sent": [USER, MARKER], "result": "ok"}),
        "the transaction records the login with the password as the marker: {t}"
    );
    assert_eq!(fx.board.logins_ok(), 1);
    let typed = fx.board.received();
    assert_eq!(
        typed.matches(PASSWORD).count(),
        1,
        "the board received the password exactly once: {typed:?}"
    );

    // Already logged in: the next command does not log in again.
    let r2 = fx.run("uname -a");
    assert_eq!(sc(&r2)["transaction"]["status"], "ok", "{r2}");
    assert!(
        sc(&r2)["transaction"]["detail"].get("login").is_none(),
        "{r2}"
    );
    assert_eq!(fx.board.received().matches(PASSWORD).count(), 1);

    // The event log carries the marker (so the store was written), never the
    // password.
    let stored = fx.store_bytes();
    assert!(
        contains(&stored, MARKER.as_bytes()),
        "the run_command event must record the login with the marker"
    );
    let _ = fx.raw("timeline", json!({"device": DEV}));
    let _ = fx.raw("console_state", json!({"device": DEV}));
    let _ = fx.raw("get_recent", json!({"device": DEV}));

    // An exported session is a sink too.
    let (_, session) = fx.rig.ingest("boot.log", "fixture login: \n", Some(DEV));
    let out = fx.rig.dir.path().join("export.jsonl");
    let ex = fx.raw(
        "export_session",
        json!({"device": DEV, "session": session, "path": out.display().to_string()}),
    );
    assert_eq!(ex["isError"], false, "{ex}");
    assert!(out.exists());

    fx.assert_no_leak();
}

/// A wrong password is reported once, never typed in a loop, and the error
/// carries no secret.
#[test]
fn a_rejected_login_is_reported_and_not_retried() {
    let fx = Fixture::new(false);
    write_creds(&fx.creds, &entry_for(DEV, "Wr0ng-Pass-Xy"), 0o600);
    let r = fx.run("uname -a");
    assert_eq!(r["isError"], true, "{r}");
    let e = &sc(&r)["error"];
    assert_eq!(e["code"], "LOGIN_REQUIRED", "{e}");
    let t = &e["detail"]["transaction"];
    assert_eq!(t["detail"]["login"]["result"], "rejected", "{t}");
    let typed = fx.board.received();
    assert_eq!(
        typed.matches("Wr0ng-Pass-Xy").count(),
        1,
        "typed once: {typed:?}"
    );
    assert!(!typed.contains("uname"), "no command after a failed login");
    assert!(!r.to_string().contains("Wr0ng-Pass-Xy"), "{r}");
    fx.assert_no_leak();
}

/// A console that wrongly echoes the password still never returns it.
#[test]
fn a_console_that_echoes_the_password_is_redacted() {
    let fx = Fixture::new(true);
    let r = fx.run("uname -a");
    assert_eq!(sc(&r)["transaction"]["status"], "ok", "{r}");
    let s = r.to_string();
    assert!(!s.contains(PASSWORD), "{s}");
    assert!(
        s.contains(MARKER),
        "the echo was there and was replaced, so this test is not vacuous: {s}"
    );
    // The raw RX stream is the board's own bytes (§8.5) and is not rewritten,
    // so only responses, events and logs are searched here.
    for (i, resp) in fx.responses.lock().unwrap().iter().enumerate() {
        assert!(!resp.contains(PASSWORD), "response #{i}");
    }
    assert!(!contains(&log_sink().lock().unwrap(), PASSWORD.as_bytes()));
}

/// No entry for this console: LOGIN_REQUIRED exactly as before, and nothing
/// but the newline probe is typed.
#[test]
fn a_console_without_an_entry_still_returns_login_required() {
    let fx = Fixture::new(false);
    write_creds(&fx.creds, &entry_for(OTHER, PASSWORD), 0o600);
    let r = fx.run("uname -a");
    let e = &sc(&r)["error"];
    assert_eq!(e["code"], "LOGIN_REQUIRED", "{r}");
    let typed = fx.board.received();
    assert!(
        typed.chars().all(|c| c == '\r' || c == '\n'),
        "only the newline probe may reach a gate without credentials: {typed:?}"
    );
    assert!(
        e["hint"].as_str().unwrap().contains("credentials.file"),
        "{e}"
    );
}

/// No file configured at all: the original behaviour.
#[test]
fn an_unset_credentials_file_changes_nothing() {
    assert!(matches!(Login::resolve("", &[DEV]), Login::None));
}

/// Mode other than 0600: refused, the reason says why, and nothing is typed.
#[test]
fn a_credentials_file_with_the_wrong_mode_is_refused() {
    let fx = Fixture::new(false);
    for mode in [0o644, 0o640, 0o400, 0o700] {
        fx.board.log_out();
        write_creds(&fx.creds, &entry_for(DEV, PASSWORD), mode);
        let r = fx.run("uname -a");
        let e = &sc(&r)["error"];
        assert_eq!(e["code"], "LOGIN_REQUIRED", "mode {mode:o}: {r}");
        let why = e["detail"]["transaction"]["detail"]["credentials"]["refused"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            why.contains(&format!("mode {mode:04o}")),
            "mode {mode:o} must be named: {why:?}"
        );
        assert!(!fx.board.received().contains(PASSWORD), "mode {mode:o}");
        assert!(!fx.board.received().contains(USER), "mode {mode:o}");
    }
    fx.assert_no_leak();
}

/// Owned by anyone but the uid conminer runs as: refused.
#[test]
fn a_credentials_file_owned_by_another_user_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    write_creds(&p, &entry_for(DEV, PASSWORD), 0o600);
    let me = credentials::effective_uid();
    assert!(
        credentials::load_as(&p, me).is_ok(),
        "precondition: own file loads"
    );
    let e = credentials::load_as(&p, me.wrapping_add(1)).unwrap_err();
    assert_eq!(e.code, conminer_core::error::ErrorCode::InvalidConfig);
    assert!(
        e.message.contains(&format!("owned by uid {me}")),
        "{}",
        e.message
    );
    assert!(!e.message.contains(PASSWORD));
}

/// A directory, a symlinked directory or a missing path is not a credentials
/// file, and a malformed file's error never quotes the line it failed on.
#[test]
fn a_bad_credentials_file_is_refused_without_quoting_it() {
    let dir = tempfile::tempdir().unwrap();
    let me = credentials::effective_uid();
    assert!(credentials::load_as(dir.path(), me).is_err(), "a directory");
    assert!(
        credentials::load_as(&dir.path().join("absent"), me).is_err(),
        "a missing file"
    );

    let p = dir.path().join("bad.toml");
    // The password line is the malformed one.
    write_creds(
        &p,
        &format!("[devices.\"{DEV}\"]\nuser = \"{USER}\"\npassword = {PASSWORD}\n"),
        0o600,
    );
    let e = credentials::load_as(&p, me).unwrap_err();
    assert!(e.message.contains("line 3"), "{}", e.message);
    assert!(!e.message.contains(PASSWORD), "{}", e.message);
    assert!(!format!("{e:?}").contains(PASSWORD));

    // Unknown keys are typos, not silently ignored.
    write_creds(
        &p,
        &format!("[devices.\"{DEV}\"]\nuser = \"{USER}\"\npasword = \"{PASSWORD}\"\n"),
        0o600,
    );
    let e = credentials::load_as(&p, me).unwrap_err();
    assert!(!e.message.contains(PASSWORD), "{}", e.message);
}

/// A console is found by its label as well as its port path.
#[test]
fn an_entry_matches_by_label() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    write_creds(&p, &entry_for("board-ap", PASSWORD), 0o600);
    match Login::resolve(p.to_str().unwrap(), &[DEV, "board-ap"]) {
        Login::Use(c) => assert_eq!(c.user, USER),
        other => panic!("expected a login: {other:?}"),
    }
    assert!(matches!(
        Login::resolve(p.to_str().unwrap(), &[DEV, ""]),
        Login::None
    ));
}

/// Tools that read or write host files refuse the credentials file, under its
/// own name and through a symlink.
#[test]
fn file_tools_refuse_the_credentials_file() {
    let fx = Fixture::new(false);
    let link = fx.rig.dir.path().join("innocent.log");
    std::os::unix::fs::symlink(&fx.creds, &link).unwrap();
    for p in [fx.creds.display().to_string(), link.display().to_string()] {
        for (tool, args) in [
            ("ingest_file", json!({"path": p, "device": DEV})),
            ("ingest_pstore", json!({"path": p, "device": DEV})),
            (
                "push_file",
                json!({"device": DEV, "local_path": p, "remote_path": "/tmp/x"}),
            ),
            (
                "transfer_file",
                json!({"device": DEV, "remote_path": "/tmp/x", "local_path": p}),
            ),
            (
                "export_session",
                json!({"device": DEV, "session": 1, "path": p}),
            ),
        ] {
            let r = fx.raw(tool, args);
            assert_eq!(r["isError"], true, "{tool} {p}: {r}");
            assert_eq!(
                sc(&r)["error"]["code"],
                "PERMISSION_DENIED",
                "{tool} {p}: {r}"
            );
        }
    }
    // The file is untouched and nothing was typed.
    let body = std::fs::read_to_string(&fx.creds).unwrap();
    assert!(body.contains(PASSWORD));
    assert!(!fx.board.received().contains(PASSWORD));
    fx.assert_no_leak();
}
