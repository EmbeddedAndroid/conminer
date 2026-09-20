//! Suite `dash` (§17) — the human dashboard.
//!
//! Driven against a **real TCP server standing in for ser2net**, because every
//! interesting property here is about the socket: that one browser or ten cost
//! the console exactly one client, that transmitted bytes reach the port
//! verbatim, and that a device disappearing is noticed rather than waited for.
//!
//! Edge cases: the device list changes only when it really changes (so the page
//! is not woken every second) · a console with no ser2net port is listed but not
//! attachable · an unknown selector refuses the upgrade rather than hanging · scrollback is
//! replayed to a late joiner · `allow_tx = false` drops transmitted bytes
//! instead of quietly succeeding · a dropped port removes the attachment so the
//! next viewer reconnects.

use conminer_core::config::Config;
use conminer_core::store::{IdentityKind, Registry};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A stand-in for one ser2net port: echoes what it is told to say, and records
/// everything written to it.
struct FakePort {
    port: u16,
    /// Bytes received from clients.
    received: mpsc::Receiver<Vec<u8>>,
    /// Send here to make the "console" print.
    say: mpsc::Sender<Vec<u8>>,
    clients: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Every client socket, so a test can model ser2net dropping one.
    socks: std::sync::Arc<std::sync::Mutex<Vec<TcpStream>>>,
    /// Bytes this port has SUCCESSFULLY pushed to clients. A client that never
    /// reads stalls this counter, which is how a missing drain is detected.
    pushed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Flips to stop accepting, so a test can make the port truly unreachable.
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakePort {
    fn start() -> Self {
        Self::bind(TcpListener::bind("127.0.0.1:0").unwrap())
    }

    /// Come back on a port that was retired, which is what a console does when
    /// its board finishes resetting and its tty re-enumerates. Retries briefly:
    /// the old listener is closed by its accept thread, not synchronously.
    fn start_on(port: u16) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => return Self::bind(l),
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => panic!("could not re-bind port {port}: {e}"),
            }
        }
    }

    fn bind(listener: TcpListener) -> Self {
        let port = listener.local_addr().unwrap().port();
        let (rx_tx, received) = mpsc::channel();
        let (say, say_rx) = mpsc::channel::<Vec<u8>>();
        let clients = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pushed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let socks: std::sync::Arc<std::sync::Mutex<Vec<TcpStream>>> = Default::default();
        let accept_socks = socks.clone();
        let accept_clients = clients.clone();
        let accept_stopped = stopped.clone();
        std::thread::spawn(move || {
            // BLOCKING accept, woken by a self-connect in `stop()`.
            //
            // The first version polled with a 5ms sleep so `stop()` could retire
            // the port. That sleep is latency between a client connecting and
            // this thread registering it -- invisible when a suite runs alone,
            // and under a full-workspace load enough that two tests said
            // something to the port before the dashboard had been recorded as a
            // client, so the line went nowhere. Zero-latency accept, and `stop()`
            // knocks on the door to wake it.
            loop {
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => return,
                };
                if accept_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // REGISTER FIRST, COUNT SECOND. The count is what `wait_for_dial`
                // watches, and the socket list is what `say` writes to. Counting
                // first publishes "there is a client" while a line said in that
                // instant would still be written to nobody -- the count would be
                // promising delivery it cannot yet make.
                accept_socks
                    .lock()
                    .unwrap()
                    .push(stream.try_clone().unwrap());
                accept_clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let tx = rx_tx.clone();
                let mut s = stream;
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let _ = tx.send(buf[..n].to_vec());
                    }
                });
            }
        });

        let say_socks = socks.clone();
        let say_pushed = pushed.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = say_rx.recv() {
                // A LINE THE CONSOLE PRINTS IS NEVER WRITTEN INTO A VOID.
                //
                // THE flake in this suite, and it is the harness's, not the
                // server's. The dashboard's `TcpStream::connect` returns as soon
                // as the KERNEL completes the handshake; this accept loop is a
                // separate thread that must still be scheduled before the socket
                // is one this thread can write to. Under `--test-threads=32` on
                // an 18-core box that gap is easily milliseconds -- long enough
                // for the dashboard to have dialled, attached, said hello, and
                // for the test to have said something back, all before the
                // socket is registered here. The line was then written to an
                // empty list and lost FOREVER, and the test sat out its whole
                // read timeout waiting for a frame that could never come.
                //
                // Measured at --test-threads=32: `a_late_joiner_is_shown_the_
                // scrollback` failed 5 times in 20 runs, every one of them with
                // "say 28 bytes to 0 clients" on this thread while the server
                // reported viewers:1, attached:true -- a healthy server, a
                // harness that dropped the console's output on the floor.
                //
                // Waiting for a reader is what the real thing does: ser2net
                // holds the port open and a board's bytes go to whoever is
                // connected. The bound keeps a test that never dials failing on
                // its own assertion rather than hanging here.
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && say_socks.lock().unwrap().is_empty() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                let mut guard = say_socks.lock().unwrap();
                // write_all BLOCKS when the peer stops reading, exactly as
                // ser2net's own write does. That is the point: an undrained
                // console socket stalls this counter.
                guard.retain_mut(|s| s.write_all(&msg).is_ok());
                say_pushed.fetch_add(msg.len(), std::sync::atomic::Ordering::SeqCst);
            }
        });

        Self {
            port,
            received,
            say,
            clients,
            socks,
            pushed,
            stopped,
        }
    }

    /// Bytes this port has managed to push to its clients.
    fn pushed(&self) -> usize {
        self.pushed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Drop every connected client, the way ser2net drops one it has given up
    /// on. The port stays open, so a re-dial can succeed.
    fn drop_clients(&self) {
        let mut guard = self.socks.lock().unwrap();
        for s in guard.iter() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        guard.clear();
    }

    /// Retire the port entirely: no clients, and nothing listening.
    fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake the accept loop so it can see the flag and exit, then make sure
        // nothing is left listening for the next dial.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.drop_clients();
    }

    fn client_count(&self) -> usize {
        self.clients.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn next_received(&self, within: Duration) -> Option<Vec<u8>> {
        self.received.recv_timeout(within).ok()
    }
}

/// AT MOST FOUR DASHBOARDS AT ONCE.
///
/// Each rig is a real server: its own tokio runtime, its own registry, a fake
/// ser2net port with a thread per client. Cargo will happily start thirty of
/// them at once, and the measured result on an 18-core box is that attaches
/// queue and viewers never arrive -- five viewers, two attached, still two sixty
/// seconds later. Green at `--test-threads=4`, one-in-three red at 32, with or
/// without the newest gates: the pressure is the rigs themselves, not any one
/// test.
///
/// Bounded rather than serialised, because several of these tests are ABOUT
/// concurrency (simultaneous viewers on one console) and must keep running
/// concurrently to mean anything.
fn a_few_dashboards_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static SLOTS: OnceLock<Vec<Mutex<()>>> = OnceLock::new();
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let slots = SLOTS.get_or_init(|| (0..4).map(|_| Mutex::new(())).collect());
    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % slots.len();
    slots[i].lock().unwrap_or_else(|e| e.into_inner())
}

/// A dashboard bound to an ephemeral port, with a registry it can read.
struct Rig {
    /// Held for the rig's life: see `a_few_dashboards_at_a_time`.
    _slot: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    base: String,
    data_dir: std::path::PathBuf,
    _rt: tokio::runtime::Runtime,
    _stop: tokio::sync::watch::Sender<bool>,
}

impl Rig {
    fn start(config: Config) -> Self {
        // The port is chosen by bind-then-drop, so it can be stolen before the
        // server rebinds it. That is inherent to picking a port this way, so
        // rather than pretend it cannot happen, try again on a fresh one.
        // A flaky test in the lowest layer of the bench is a real defect: it
        // cost three false diagnoses in one day.
        for attempt in 0..5 {
            if let Some(rig) = Self::try_start(config.clone()) {
                return rig;
            }
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
        panic!("dash rig could not bind a free port after 5 attempts");
    }

    fn try_start(mut config: Config) -> Option<Self> {
        let _slot = a_few_dashboards_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        config.paths.data_dir = dir.path().to_path_buf();
        let data_dir = dir.path().to_path_buf();

        // Bind ONCE and keep it. Binding to pick a port and dropping it leaves a
        // window in which another rig -- there are dozens when the whole
        // workspace runs at once -- binds the same port first, and this rig then
        // asserts against somebody else's devices. Measured: a deterministic
        // dashboard assertion failed exactly once, under full load.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_listener.local_addr().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        config.dashboard.bind = addr.to_string();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (stop, rx) = tokio::sync::watch::channel(false);
        // The listener is handed over directly now; the bind string is only in
        // the config for anything that reads it back.
        let _bind = config.dashboard.bind.clone();
        let d = data_dir.clone();
        rt.spawn(async move {
            let dash = conminer::dash::Dash::new(config, d);
            let _ = dash.refresh();
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            let _ = conminer::dash::serve_on(dash, listener, rx).await;
        });

        let base = format!("http://{addr}");
        // Wait for accept rather than sleeping a fixed amount, and PROVE the
        // server answered rather than merely that something is listening.
        //
        // The old wait accepted any successful connect, which is exactly what
        // made this rig flaky: the port is chosen by binding, dropping and
        // letting the server rebind, so between the drop and the rebind a
        // parallel test can take it. A bare connect then succeeds against the
        // WRONG server and the test proceeds to fail somewhere confusing --
        // three false diagnoses today came from that. Asking /healthz for a
        // conminer answer removes the ambiguity.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut ready = false;
        while Instant::now() < deadline {
            let probe = std::process::Command::new("curl")
                .args(["-s", "--max-time", "5", &format!("{base}/healthz")])
                .output();
            if let Ok(o) = probe {
                if String::from_utf8_lossy(&o.stdout).contains("\"status\"") {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !ready {
            // Someone else took the port between reservation and bind. Not a
            // failure worth reporting -- just try another one.
            return None;
        }
        Some(Self {
            _slot,
            _dir: dir,
            base,
            data_dir,
            _rt: rt,
            _stop: stop,
        })
    }

    fn registry(&self) -> Registry {
        Registry::open(&self.data_dir).unwrap()
    }

    /// Register a console pointing at a fake ser2net port.
    fn add_device(&self, canonical: &str, port: Option<u16>) {
        let mut reg = self.registry();
        let row = reg
            .upsert_device(canonical, None, IdentityKind::ById, None, 1_000)
            .unwrap();
        if let Some(p) = port {
            // `assign_port` takes the first free port at or above the base, and
            // nothing else holds this one, so the console lands exactly on the
            // fake ser2net listener.
            let got = reg.assign_port(row.id, p).unwrap();
            assert_eq!(got, p, "the fake port must be the one assigned");
        }
    }

    /// Register a console at a specific USB topology path, which is what decides
    /// controller binding. Tests that do not pass one cannot express the
    /// two-boards-on-different-branches case at all -- and that is the case that
    /// powered off the wrong board.
    fn add_device_at(&self, canonical: &str, by_path: Option<&str>, port: Option<u16>) {
        let mut reg = self.registry();
        let row = reg
            .upsert_device(canonical, by_path, IdentityKind::ById, None, 1_000)
            .unwrap();
        if let Some(p) = port {
            let got = reg.assign_port(row.id, p).unwrap();
            assert_eq!(got, p, "the fake port must be the one assigned");
        }
    }

    fn get(&self, path: &str) -> (u16, String) {
        let url = format!("{}{path}", self.base);
        let out = std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/stdout", "-w", "\n%{http_code}", &url])
            .output()
            .expect("curl");
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
        (code.trim().parse().unwrap_or(0), body.to_string())
    }

    fn post_body(&self, path: &str, body: &str) -> (u16, String) {
        let url = format!("{}{path}", self.base);
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "--data-binary",
                body,
                "-o",
                "/dev/stdout",
                "-w",
                "\n%{http_code}",
                &url,
            ])
            .output()
            .expect("curl");
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
        (code.trim().parse().unwrap_or(0), body.to_string())
    }

    fn post(&self, path: &str) -> (u16, String) {
        let url = format!("{}{path}", self.base);
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-o",
                "/dev/stdout",
                "-w",
                "\n%{http_code}",
                &url,
            ])
            .output()
            .expect("curl");
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, code) = body.rsplit_once('\n').unwrap_or((body.as_str(), "0"));
        (code.trim().parse().unwrap_or(0), body.to_string())
    }

    fn devices(&self) -> serde_json::Value {
        let (code, body) = self.get("/api/devices");
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Poll until `f` holds, so tests never depend on the refresh interval.
    ///
    /// A poll that cannot reach the server is RETRIED, not failed. Under a full
    /// `cargo test --workspace` run, dozens of test servers and curls are in
    /// flight at once and a single connect can come back with no HTTP status at
    /// all (curl exit, code 0) -- which made this helper fail whichever test was
    /// unlucky, blaming the load on the code under test. The deadline is still
    /// the backstop: a server that never answers still fails, and says so.
    /// Wait for the dashboard's background poller to reach a state.
    ///
    /// The deadline is deliberately generous. This asserts WHAT the page ends up
    /// showing, never how quickly -- the poller refreshes every 100 ms in these
    /// tests, so five seconds is fifty refreshes on an idle box and can still be
    /// too few when the whole workspace suite is running in parallel. That is
    /// exactly how this suite produced one failure in a full run while passing
    /// three times in a row on its own: machine load reading as a defect.
    /// A real hang still fails here, just later and with the same message.
    fn until(&self, what: &str, f: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = String::from("(never got a response)");
        loop {
            let (code, body) = self.get("/api/devices");
            if code == 200 {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    if f(&v) {
                        return v;
                    }
                    last = v.to_string();
                }
            } else {
                last = format!("http {code}: {body}");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {last}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn cfg() -> Config {
    let mut c = Config::default();
    c.ser2net.bind = "127.0.0.1".into();
    c.dashboard.refresh_ms = 100;
    c
}

// ------------------------------------------------------------- device list ---

#[test]
fn the_page_and_the_api_are_served() {
    let rig = Rig::start(cfg());
    let (code, body) = rig.get("/");
    assert_eq!(code, 200);
    assert!(body.contains("conminer"), "the page should render itself");
    // Self-contained on purpose: a lab host usually cannot reach a CDN.
    assert!(
        !body.contains("https://") || !body.contains("<script src"),
        "the dashboard must not load anything off-host"
    );
    assert_eq!(rig.get("/healthz").0, 200);
}

#[test]
fn a_device_appears_without_the_page_asking() {
    let rig = Rig::start(cfg());
    assert_eq!(rig.devices()["devices"].as_array().unwrap().len(), 0);

    rig.add_device("usb-FTDI-console-a", Some(5001));
    let v = rig.until("the new console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    assert_eq!(v["devices"][0]["device"], "usb-FTDI-console-a");
    assert_eq!(v["devices"][0]["port"], 5001);
    assert_eq!(v["devices"][0]["line"], "115200 8N1");
}

#[test]
fn the_revision_only_moves_when_the_device_set_really_changes() {
    // Otherwise every browser is woken once a second forever, and the flash
    // animation that makes a replug obvious becomes meaningless.
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(5001));
    let first = rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    })["revision"]
        .as_u64()
        .unwrap();

    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        rig.devices()["revision"].as_u64().unwrap(),
        first,
        "a steady lab must not generate events"
    );

    rig.add_device("usb-b", Some(5002));
    let after = rig.until("the second console", |v| {
        v["devices"].as_array().unwrap().len() == 2
    });
    assert!(after["revision"].as_u64().unwrap() > first);
}

#[test]
fn a_console_with_no_endpoint_is_listed_but_not_attachable() {
    // Hiding it would be worse: "it is not there" and "it is there but ser2net
    // has not given it a port" are different problems.
    let rig = Rig::start(cfg());
    rig.add_device("usb-no-port", None);
    let v = rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    assert!(v["devices"][0]["port"].is_null());
    assert_eq!(v["devices"][0]["attached"], false);
}

#[test]
fn the_snapshot_reports_the_transmit_policy_and_the_client_budget() {
    let mut c = cfg();
    c.dashboard.allow_tx = false;
    c.ser2net.max_connections = 4;
    let rig = Rig::start(c);
    let v = rig.devices();
    assert_eq!(v["allow_tx"], false);
    assert_eq!(v["max_connections"], 4);
}

#[test]
fn ports_on_one_adapter_share_an_adapter_key() {
    // A quad-UART chip presents four by-id names differing only in `-ifNN`. On a
    // bring-up rig those are one board, and the page groups them accordingly.
    let rig = Rig::start(cfg());
    for i in 0..4 {
        rig.add_device(
            &format!("usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if0{i}-port0"),
            Some(5001 + i as u16),
        );
    }
    rig.add_device("usb-Some_Other_Adapter_XYZ-if00", Some(5099));

    let v = rig.until("five consoles", |v| {
        v["devices"].as_array().unwrap().len() == 5
    });
    let devices = v["devices"].as_array().unwrap();
    let iq10: Vec<&serde_json::Value> = devices
        .iter()
        .filter(|d| d["adapter"] == "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU")
        .collect();
    assert_eq!(
        iq10.len(),
        4,
        "the four IQ10 ports group together: {devices:#?}"
    );
    assert!(
        devices
            .iter()
            .any(|d| d["adapter"] == "usb-Some_Other_Adapter_XYZ"),
        "a different adapter is its own group"
    );
}

#[test]
fn a_device_with_no_interface_suffix_has_no_adapter() {
    // Not grouped under a guess: the Bantam controller is a single-port CDC
    // device and inventing an adapter for it would merge unrelated things.
    let rig = Rig::start(cfg());
    rig.add_device("usb-Microchip_Bantam_IQ10RRDXX000034VG8", Some(5001));
    let v = rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    assert!(v["devices"][0]["adapter"].is_null(), "{v}");
}

#[test]
fn a_controller_is_flagged_so_the_page_shows_a_panel_not_a_dead_row() {
    // A board controller is excluded from capture, so without this it would
    // render as an "ignored" console — a dead row where the power buttons
    // should be.
    let rig = Rig::start(cfg());
    rig.add_device("usb-Microchip_Bantam_IQ10RRDXX34VG8-if00", None);
    rig.add_device("usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if00-port0", Some(5001));

    let v = rig.until("both", |v| v["devices"].as_array().unwrap().len() == 2);
    let devices = v["devices"].as_array().unwrap();
    let ctl = devices
        .iter()
        .find(|d| d["canonical"].as_str().unwrap().contains("Bantam"))
        .expect("the controller");
    assert_eq!(ctl["is_controller"], true, "{ctl}");
    assert_eq!(ctl["controller"], "bantam");
    assert!(
        !ctl["boot_modes"].as_array().unwrap().is_empty(),
        "its panel needs the boot modes to offer"
    );

    let console = devices
        .iter()
        .find(|d| d["canonical"].as_str().unwrap().contains("FTDI"))
        .expect("the console");
    assert_eq!(
        console["is_controller"], false,
        "a console it powers is not itself a controller"
    );
}

#[test]
fn a_mined_log_file_never_reaches_the_bench_view() {
    // `ingest_file` without a device creates a pseudo-device keyed on the path.
    // That is right for an agent — every tool works on it identically — and
    // wrong for a bench dashboard, where a row that cannot be plugged in,
    // powered or typed at is noise beside the real consoles.
    //
    // Three of these had accumulated on the rig from ordinary use, sitting
    // among the boards as though somebody might go and power one.
    let rig = Rig::start(cfg());
    rig.add_device("file:/tmp/board-boot.log", None);
    rig.add_device("usb-FTDI_IQ10_UART-SPI_X-if00-port0", Some(5001));

    let v = rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let devices = v["devices"].as_array().unwrap();
    assert!(
        !devices
            .iter()
            .any(|d| d["canonical"].as_str().unwrap().starts_with("file:")),
        "a mined log is not a thing on the bench: {devices:?}"
    );
    assert_eq!(
        devices[0]["canonical"].as_str().unwrap(),
        "usb-FTDI_IQ10_UART-SPI_X-if00-port0",
        "the real console stays"
    );
}

/// An internal sibling store is an implementation detail, not a port.
///
/// `snapshot_dmesg` opens `<port>#dmesg` so it can mine without taking the live
/// console's writer lock. On the rig that detail rendered as its own row --
/// `...if02-port0#dmesg`, marked excluded, state "unknown" -- which reads like a
/// broken console rather than a thing that was never a console.
#[test]
fn an_internal_sibling_store_is_not_a_port() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_IQ10_UART-SPI_X-if02-port0", Some(5001));
    rig.add_device("usb-FTDI_IQ10_UART-SPI_X-if02-port0#dmesg", None);

    let v = rig.until("the console alone", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let devices = v["devices"].as_array().unwrap();
    assert!(
        !devices
            .iter()
            .any(|d| d["canonical"].as_str().unwrap().contains('#')),
        "a sibling store is not a port anyone can plug in: {devices:?}"
    );
}

/// A port config deliberately excludes has no console BY DESIGN, so it is not a
/// console the page should offer.
#[test]
fn a_port_excluded_by_config_does_not_appear_on_the_bench_view() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_NordAU_RIDE_SX_SPI_X-if00-port0", None);
    rig.add_device("usb-FTDI_NordAU_RIDE_SX_879X_UART_X-if00-port0", Some(5001));
    // Excluded the way discovery excludes: the flag on the registry row, which
    // is what the dashboard actually reads.
    {
        let mut reg = rig.registry();
        let row = reg
            .device_by_canonical("usb-FTDI_NordAU_RIDE_SX_SPI_X-if00-port0")
            .unwrap()
            .expect("the excluded port");
        reg.set_ignored(row.id, true).unwrap();
    }

    // WAIT FOR THE STATE, NOT FOR A COUNT.
    //
    // `len() == 1` is transiently true for the WRONG reason: the dashboard
    // refreshes on its own timer, and between the two `add_device` calls exactly
    // one device exists -- the SPI port this test is about to exclude. Under a
    // loaded full-workspace run the tick lands in that window, `until` returns
    // happily, and the assertion below reads the wrong row. Measured once in a
    // full run while the suite passed 58/58 five times on its own.
    //
    // The condition now names what it is actually waiting for.
    let v = rig.until("the UART alone", |v| {
        let d = v["devices"].as_array().unwrap();
        d.len() == 1
            && d[0]["canonical"]
                .as_str()
                .is_some_and(|c| c.contains("UART"))
    });
    let devices = v["devices"].as_array().unwrap();
    assert!(
        devices[0]["canonical"]
            .as_str()
            .unwrap()
            .contains("879X_UART"),
        "the excluded SPI port must not be offered as a console: {devices:?}"
    );
}

#[test]
fn power_is_refused_when_the_dashboard_is_not_allowed_to_actuate() {
    // Typing into a console and power-cycling a board are different sizes of
    // mistake, so they are separate switches.
    let mut c = cfg();
    c.dashboard.allow_power = false;
    let rig = Rig::start(c);
    rig.add_device("usb-a", Some(5001));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let (code, body) = rig.post("/api/power/usb-a/cycle");
    assert_eq!(code, 403, "{body}");
    assert!(body.contains("allow_power"));
}

#[test]
fn a_power_press_with_no_mcpd_reachable_fails_loudly() {
    // The button must never report success it did not get: an operator who
    // thinks the board was reset and was not is worse off than one who saw an
    // error.
    let mut c = cfg();
    c.dashboard.mcp_url = "http://127.0.0.1:1/mcp".into();
    let rig = Rig::start(c);
    rig.add_device("usb-a", Some(5001));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let (code, body) = rig.post("/api/power/usb-a/cycle");
    assert_eq!(code, 502, "{body}");
    assert!(body.contains("mcpd"), "{body}");
}

// ------------------------------------------------------------ the console ----

#[test]
fn an_unknown_console_refuses_the_upgrade_rather_than_hanging() {
    let rig = Rig::start(cfg());
    // A real handshake: the upgrade extractor rejects a plain GET with 400
    // before the handler ever sees the selector, so only a genuine WebSocket
    // request tests what happens to an unknown console.
    assert_eq!(ws_status(&rig.base, "nope"), 404);
    let (plain, _) = rig.get("/ws/console/nope");
    assert_eq!(plain, 400, "a non-upgrade GET is refused, not left open");
}

/// THE NAME A BOARD IS KNOWN BY MUST WORK ON THE CONSOLE ROUTE.
///
/// `/ws/console/:selector` resolved only the by-id path, so opening the console
/// by the nickname every other surface accepts -- the one `list_devices` prints
/// and the dashboard itself displays -- answered 404. Found by hand on the
/// bench, which is exactly what this suite exists to make unnecessary.
#[test]
fn a_console_answers_to_its_nickname_on_the_console_route() {
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    add_console(&rig, "usb-a", port.port);
    {
        let mut reg = rig.registry();
        let row = reg.device_by_canonical("usb-a").unwrap().unwrap();
        reg.set_nickname(row.id, "uno-q").unwrap();
    }
    rig.until("the nickname", |v| {
        v["devices"]
            .as_array()
            .map(|ds| ds.iter().any(|d| d["nickname"] == "uno-q"))
            == Some(true)
    });

    assert_eq!(
        ws_status(&rig.base, "uno-q"),
        101,
        "the nickname must open the console, or the web terminal is unreachable \
         by the name people actually use"
    );
    assert_eq!(
        ws_status(&rig.base, "usb-a"),
        101,
        "the path must keep working"
    );
    assert_eq!(ws_status(&rig.base, "no-such-board"), 404);
}

/// ...and the console it opens is the RIGHT one: a nickname must never shadow
/// another board's device path.
#[test]
fn a_device_path_outranks_a_nickname_imitating_it() {
    let a = FakePort::start();
    let b = FakePort::start();
    let rig = Rig::start(cfg());
    add_console(&rig, "usb-a", a.port);
    rig.add_device("usb-b", Some(b.port));
    {
        // usb-b answers to the name "usb-a", which is usb-a's real path.
        let mut reg = rig.registry();
        let row = reg.device_by_canonical("usb-b").unwrap().unwrap();
        reg.set_nickname(row.id, "usb-a").unwrap();
    }
    rig.until("both consoles", |v| {
        v["devices"].as_array().map(|d| d.len()) == Some(2)
    });

    let mut ws = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut ws).expect("hello");
    wait_for_dial(&a);
    a.say.send(b"i am A\r\n".to_vec()).unwrap();
    let payload = read_binary_frame(&mut ws).expect("console output");
    assert_eq!(
        String::from_utf8_lossy(&payload),
        "i am A\r\n",
        "the real path must reach its own board, not the one whose nickname imitates it"
    );
}

#[test]
fn many_viewers_cost_the_port_one_client() {
    // The whole reason the server owns the socket: ser2net's client budget is
    // small and minerd is already holding one of them.
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let mut sockets = Vec::new();
    for _ in 0..5 {
        sockets.push(ws_connect(&rig.base, "usb-a"));
    }
    let v = rig.until("five viewers", |v| v["devices"][0]["viewers"] == 5);
    assert_eq!(v["devices"][0]["attached"], true);
    assert_eq!(
        port.client_count(),
        1,
        "five browsers must not be five ser2net clients"
    );
}

#[test]
fn simultaneous_first_viewers_still_cost_one_client() {
    // Regression: the check-then-dial in `attach` was not serialised, so two
    // browsers opening the same console in the same instant both missed the
    // attachment map and both connected. On a console whose ser2net budget is
    // eight clients — one of which minerd already holds — a page opened on a
    // laptop and a phone at once quietly cost double.
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let base = rig.base.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let base = base.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ws_connect(&base, "usb-a")
            })
        })
        .collect();
    let sockets: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    rig.until("six viewers", |v| v["devices"][0]["viewers"] == 6);
    // Wait for the dial to be REGISTERED before counting it: six viewers is a
    // fact about the dashboard, and the client count is a fact about the port.
    wait_for_dial(&port);
    assert_eq!(
        port.client_count(),
        1,
        "six simultaneous viewers must still be one ser2net client"
    );
    drop(sockets);
}

#[test]
fn console_output_reaches_the_browser() {
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let mut ws = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut ws).expect("hello");
    // Saying anything before the dashboard has dialled writes to nobody: the
    // port has no clients yet and the line is simply lost. Invisible when this
    // suite runs alone; a real failure under a full-workspace load.
    wait_for_dial(&port);
    port.say.send(b"U-Boot 2026.01\r\n".to_vec()).unwrap();

    let payload = read_binary_frame(&mut ws).expect("console output");
    assert_eq!(String::from_utf8_lossy(&payload), "U-Boot 2026.01\r\n");
}

#[test]
fn a_browser_transmits_bytes_verbatim_to_the_port() {
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let mut ws = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut ws).expect("hello");
    // A Ctrl-C and a command, exactly as the page sends them.
    send_binary_frame(&mut ws, &[0x03]);
    send_binary_frame(&mut ws, b"printenv\r");

    let mut got = Vec::new();
    while got.len() < 10 {
        match port.next_received(Duration::from_secs(3)) {
            Some(chunk) => got.extend_from_slice(&chunk),
            None => break,
        }
    }
    assert_eq!(
        got,
        b"\x03printenv\r".to_vec(),
        "bytes must arrive unmodified: {got:?}"
    );
}

#[test]
fn a_read_only_server_drops_transmitted_bytes() {
    let mut c = cfg();
    c.dashboard.allow_tx = false;
    let port = FakePort::start();
    let rig = Rig::start(c);
    rig.add_device("usb-a", Some(port.port));
    // WAIT FOR THE ENDPOINT, not just the row. The port is assigned a moment
    // after the device appears, and a socket opened in between is refused with
    // "no ser2net endpoint" -- which reads as a broken read-only guard rather
    // than a test that arrived early. Seen once under a full parallel run.
    rig.until("the console's endpoint", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["port"].is_number())
    });

    let mut ws = ws_connect(&rig.base, "usb-a");
    let hello = read_text_frame(&mut ws).expect("hello");
    assert!(
        hello.contains("\"allow_tx\":false"),
        "the browser is told before it offers a send button: {hello}"
    );
    send_binary_frame(&mut ws, b"reboot\r");
    assert!(
        port.next_received(Duration::from_millis(600)).is_none(),
        "read-only must actually mean read-only, not merely hide the button"
    );
}

#[test]
fn a_late_joiner_is_shown_the_scrollback() {
    // A console that has gone quiet would otherwise present an empty pane, and
    // the reason you opened it is usually already on screen.
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let mut first = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut first).expect("hello");
    port.say
        .send(b"Kernel panic - not syncing\r\n".to_vec())
        .unwrap();
    read_binary_frame(&mut first).expect("the first viewer sees it live");

    let mut late = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut late).expect("hello");
    let history = read_binary_frame(&mut late).expect("scrollback");
    assert!(
        String::from_utf8_lossy(&history).contains("Kernel panic"),
        "the late viewer must see what already happened"
    );
}

// ------------------------------------------------- a minimal WebSocket client -
//
// Hand-rolled rather than pulled in as a dependency: the suite needs exactly
// three things (handshake, read a frame, write a masked binary frame), and a
// client library would test itself as much as the server.

// PATIENCE IS NOT A BEHAVIOUR. These gates assert WHAT the server does, never
// how fast: a fixed short timeout turns a correctness assertion into a speed
// one, and a loaded box then fails a server that is working perfectly. Measured
// on this bench once it also hosted a live conminer stack -- a 5s socket read
// expired while the frame was on its way, and the suite blamed the dashboard.
const WS_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Perform the handshake and return the HTTP status, without asserting on it.
fn ws_status(base: &str, selector: &str) -> u16 {
    let addr = base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(WS_READ_TIMEOUT)).unwrap();
    s.write_all(handshake(addr, selector).as_bytes()).unwrap();
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).unwrap_or(0) != 1 {
            break;
        }
        header.push(byte[0]);
    }
    String::from_utf8_lossy(&header)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn handshake(addr: &str, selector: &str) -> String {
    format!(
        "GET /ws/console/{selector} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
}

fn ws_connect(base: &str, selector: &str) -> TcpStream {
    ws_connect_query(base, selector, "")
}

fn ws_connect_query(base: &str, selector: &str, query: &str) -> TcpStream {
    let selector = if query.is_empty() {
        selector.to_string()
    } else {
        format!("{selector}?{query}")
    };
    let selector = selector.as_str();
    let addr = base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(WS_READ_TIMEOUT)).unwrap();
    s.write_all(handshake(addr, selector).as_bytes()).unwrap();

    // Consume the handshake response, stopping exactly at the blank line so no
    // frame bytes are swallowed with it.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut byte).unwrap(), 1, "handshake truncated");
        header.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&header);
    assert!(
        text.starts_with("HTTP/1.1 101"),
        "upgrade refused: {}",
        text.lines().next().unwrap_or("")
    );
    s
}

/// Read one frame, returning (opcode, payload).
fn read_frame(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 2];
    s.read_exact(&mut hdr).ok()?;
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        s.read_exact(&mut ext).ok()?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        s.read_exact(&mut ext).ok()?;
        len = u64::from_be_bytes(ext) as usize;
    }
    if masked {
        let mut key = [0u8; 4];
        s.read_exact(&mut key).ok()?;
    }
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).ok()?;
    Some((opcode, payload))
}

fn read_text_frame(s: &mut TcpStream) -> Option<String> {
    loop {
        let (op, payload) = read_frame(s)?;
        match op {
            1 => return Some(String::from_utf8_lossy(&payload).to_string()),
            8 => return None,
            _ => continue,
        }
    }
}

fn read_binary_frame(s: &mut TcpStream) -> Option<Vec<u8>> {
    loop {
        let (op, payload) = read_frame(s)?;
        match op {
            2 => return Some(payload),
            8 => return None,
            _ => continue,
        }
    }
}

fn send_binary_frame(s: &mut TcpStream, payload: &[u8]) {
    let mut frame = vec![0x82]; // FIN + binary
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    assert!(payload.len() < 126, "the suite never needs a long frame");
    frame.push(0x80 | payload.len() as u8);
    frame.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    s.write_all(&frame).unwrap();
    s.flush().unwrap();
}

#[test]
fn telnet_negotiation_never_reaches_the_browser() {
    // ser2net's accepter is telnet because that is what makes 4.x share one
    // connector across clients. The option bytes it sends are protocol, not
    // console output, and a terminal draws them as garbage.
    use conminer::dash::strip_telnet;

    // IAC WILL ECHO, IAC DO ECHO, then real output.
    let raw = b"\xff\xfb\x01\xff\xfd\x01[    1.0] boot\n";
    assert_eq!(strip_telnet(raw), b"[    1.0] boot\n");

    // Subnegotiation runs to IAC SE and is dropped whole.
    let sub = b"\xff\xfa\x2c\x01\x02\xff\xf0hello";
    assert_eq!(strip_telnet(sub), b"hello");

    // A doubled IAC is a literal 0xFF the board actually sent.
    assert_eq!(strip_telnet(b"a\xff\xffb"), b"a\xffb");

    // Console bytes are otherwise untouched, including invalid UTF-8.
    let raw = b"\x80\x81 raw \x00 bytes\r\n";
    assert_eq!(strip_telnet(raw), raw);
}

/// Regression: the dashboard page carries all of its JS inline, so a cached
/// page is a cached *client*. Served with no caching directives at all,
/// browsers fall back to heuristic caching -- measured in the field as a
/// long-lived tab running old JS and reporting "connection closed" while
/// freshly-fetched clients streamed console data fine from the same server.
#[test]
fn the_dashboard_page_is_never_cached() {
    let rig = Rig::start(cfg());
    let out = std::process::Command::new("curl")
        .args(["-sI", &format!("{}/", rig.base)])
        .output()
        .expect("curl");
    let headers = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(headers.contains("200"), "headers = {headers:?}");
    assert!(
        headers.contains("cache-control:"),
        "the dashboard was served with NO cache-control at all: {headers:?}"
    );
    assert!(
        headers.contains("no-store"),
        "the dashboard page must not be cached: {headers:?}"
    );
}

/// Regression: a console must be openable while the board is POWERED OFF.
/// The card's click handler was gated on liveness, so an idle device got no
/// handler at all and clicking it did nothing -- which breaks the main bench
/// workflow: open the console first, then power on and watch the boot.
#[test]
fn a_console_is_openable_before_the_board_says_anything() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        !page.contains("if (live) card.onclick"),
        "the console click handler must not be gated on liveness"
    );
    assert!(
        page.contains("if (d.port) card.onclick"),
        "the console click handler should be gated on having an endpoint"
    );
    // And it must still open its own window rather than pushing the bench aside.
    assert!(
        page.contains("window.open(`/?console="),
        "consoles open in their own window"
    );
}

/// Regression: a top-level `function open()` in the page becomes `window.open`,
/// shadowing the browser's popup API. The card handler's `window.open(url, ...)`
/// then invoked the page's own function with the URL *string*, which has no
/// `.device`, producing a socket URL of `/ws/console/undefined` and rendering
/// the console inline instead of in its own window.
#[test]
fn the_page_does_not_shadow_window_open() {
    let page = include_str!("../../src/dashboard.html");
    // Anchored to a real declaration at line start: prose about the bug
    // legitimately mentions the old name.
    assert!(
        !page.lines().any(|l| l.starts_with("function open(")),
        "a global `function open()` shadows window.open -- name it openConsole()"
    );
    assert!(
        page.contains("function openConsole("),
        "the console opener should be openConsole()"
    );
    assert!(
        page.contains("window.open(`/?console="),
        "cards must open a real popup window"
    );
}

/// A blocked popup must not swallow the click: browsers can refuse
/// `window.open`, and a console that then does nothing at all is worse than one
/// that opens in the inline pane.
#[test]
fn a_blocked_popup_falls_back_to_the_inline_console() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        page.contains("if (!w) openConsole(d);"),
        "no fallback when the popup is blocked"
    );
}

/// Regression: the power panel greyed itself out and never came back. The fetch
/// had no client deadline (a blocking power hook left it pending forever) and
/// the re-enable targeted a node list captured before a live device update could
/// rebuild the panel, so it revived detached buttons.
#[test]
fn the_power_panel_always_recovers() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        page.contains("new AbortController()"),
        "power fetch needs a client deadline"
    );
    assert!(
        page.contains("ctl.abort()"),
        "the deadline must actually abort the request"
    );
    assert!(
        page.contains("document.querySelectorAll(sel).forEach(b => { b.disabled = false; });"),
        "re-enable must re-query the DOM, not revive a stale node list"
    );
}

/// Regression: dark must be THE default, not "dark unless the OS says light".
/// A `@media (prefers-color-scheme: light)` block re-applied the bright palette
/// on any light-mode desktop, so the page still looked white after the switch
/// to a dark default -- which is precisely what was asked to go away.
#[test]
fn dark_is_the_default_theme_and_the_os_cannot_override_it() {
    let page = include_str!("../../src/dashboard.html");
    // The dark background is defined on bare :root, so it applies unconditionally.
    let root = page.split(":root {").nth(1).expect("no bare :root block");
    let root = &root[..root.find('}').expect("unterminated :root")];
    assert!(
        root.contains("--bg: #0e1117"),
        "bare :root must carry the dark palette: {root:?}"
    );

    // No media query may re-apply a palette; light is opt-in via data-theme only.
    assert!(
        !page.contains("@media (prefers-color-scheme: light)"),
        "an OS light preference must not override the dark default"
    );
    assert!(
        page.contains("[data-theme=\"light\"]"),
        "light must remain available explicitly"
    );
}

/// A controller and the consoles it drives belong to one board, and the page
/// must show that. Previously the group was a bare margin with no surface,
/// border or divider, so a controller floated beside its ports with nothing
/// tying them together.
#[test]
fn an_adapter_group_is_a_visible_enclosure() {
    let page = include_str!("../../src/dashboard.html");

    // Each group is a chassis: its own surface and border, mounted in a rack.
    let css = page.split(".adapter {").nth(1).expect("no .adapter rule");
    let css = &css[..css.find('}').expect("unterminated .adapter")];
    assert!(css.contains("border"), ".adapter needs a border: {css:?}");
    assert!(
        css.contains("background"),
        ".adapter needs its own surface: {css:?}"
    );
    assert!(page.contains(".rack {"), "chassis must mount in a rack");
    assert!(
        page.contains("rackOf(grid)"),
        "groups must be appended into the rack"
    );
    assert!(
        page.contains("adapter-led"),
        "a chassis needs a status lamp"
    );

    // And its label is separated from its contents.
    let head = page
        .split(".adapter-head {")
        .nth(1)
        .expect("no .adapter-head rule");
    let head = &head[..head.find('}').expect("unterminated .adapter-head")];
    assert!(
        head.contains("border-bottom"),
        "the group header needs a divider: {head:?}"
    );

    // EVERY group is enclosed, unconditionally.
    //
    // This used to assert the gate `devices.length > 1 || hasController`, which
    // enclosed a controller-and-console pair but left one case loose: a single
    // console with no controller -- exactly what a bare CMSIS-DAP debug probe
    // is. On the bravo bench that one device floated beside the rack while every
    // other port sat in a chassis. The gate is gone rather than widened, so
    // there is no case left to get wrong; re-introducing any condition here
    // fails this test.
    assert!(
        !page.contains("devices.length > 1 || hasController"),
        "the enclosure must not be gated: a lone console is rack hardware too"
    );
    assert!(
        !page.contains("if (devices.length"),
        "no device-count condition may decide whether a group is enclosed"
    );
}

/// A controller can also be a console. The Bantam is control-only and excluded
/// from discovery, so a control panel is its whole story -- but a Bughopper's
/// FTDI *is* the board's UART. Rendering only the panel made its console
/// silently absent from the page while the API reported it listening.
#[test]
fn a_controller_that_is_also_a_console_gets_both_a_panel_and_a_port() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        !page.contains("if (d.is_controller) { grid.appendChild(controllerCard(d)); continue; }"),
        "an unconditional `continue` hides the console of a controller that has one"
    );
    assert!(
        page.contains("if (!d.port) continue;"),
        "a controller WITH an endpoint must fall through and also render a port card"
    );
}

/// A board's harness is several USB devices behind one hub: on the IQ10 the
/// FT4232 carrying four UARTs sits at 3.2.2 while the Bantam that powers the
/// same board sits at 3.2.4. They are ONE board and must share a group.
/// A device plugged straight into a root-level port is its own board -- naive
/// parent-stripping would take 3.3 up to 3 and merge every board on the host.
#[test]
fn devices_sharing_a_downstream_hub_are_one_board() {
    use conminer::dash::topology_group;

    let ft = topology_group(Some("pci-0000:00:14.0-usb-0:3.2.2:1.1-port0"));
    let bantam = topology_group(Some("pci-0000:00:14.0-usb-0:3.2.4:1.0"));
    assert_eq!(
        ft, bantam,
        "controller and consoles behind hub 3.2 are one board"
    );
    assert_eq!(ft.as_deref(), Some("3.2"));

    // Root-level port: its own board, NOT merged with everything under 3.
    let bughopper = topology_group(Some("pci-0000:00:14.0-usb-0:3.3:1.0-port0"));
    assert_eq!(bughopper.as_deref(), Some("3.3"));
    assert_ne!(
        bughopper, ft,
        "a separate board must not join the IQ10's chassis"
    );

    // usbv2 aliases resolve the same way, and no topology is not a crash.
    assert_eq!(
        topology_group(Some("pci-0000:00:14.0-usbv2-0:3.2.2:1.2-port0")).as_deref(),
        Some("3.2")
    );
    assert_eq!(topology_group(None), None);
}

// The chassis summary's counting rule is proven in the BROWSER suite, by
// `the_chassis_summary_counts_a_controller_that_is_also_a_console`. It lived
// here as a grep over dashboard.html, and that grep asserted the page still read
// `x.state === "listening"` -- pinning the aliveness bug in place, so fixing it
// broke the test that was supposed to protect it.

/// A power action detaches the controller's FTDI, so the console port vanishes
/// for a few seconds on every off / cycle / EDL. Tearing the attachment down
/// closed every browser's socket and made the operator reconnect by hand each
/// time. The attachment must survive the drop: hold the viewers, keep the
/// scrollback, and re-dial underneath them.
#[test]
fn a_console_survives_the_port_vanishing() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs"))
        .expect("dash.rs");

    assert!(
        !src.contains("// Drop the attachment"),
        "a dropped port must not tear down the attachment"
    );
    assert!(
        src.contains("reconnecting…"),
        "the viewer should be told, not disconnected"
    );
    assert!(src.contains("console back"), "and told when it returns");
    // Keystrokes must follow the socket across a reconnect, or the console
    // silently becomes read-only after the first power action.
    assert!(
        src.contains("writer_for_tx"),
        "the write half must be swappable, not captured once"
    );
}

/// Keys the board never receives are features that silently do not work. Tab is
/// the one that matters most (completion) and must keep preventDefault, or the
/// browser steals it for focus navigation; Home/End/Delete are what break line
/// editing, and the function keys are what break menuconfig and vim's help.
#[test]
fn the_terminal_sends_the_full_xterm_key_set() {
    let page = include_str!("../../src/dashboard.html");

    // Tab completion: 0x09 reaches the port, and the browser does not eat it.
    assert!(page.contains("Tab: [0x09]"), "Tab must be sent verbatim");
    assert!(
        page.contains("e.preventDefault()"),
        "Tab must not move focus"
    );
    // Shift-Tab is a distinct sequence, not a Tab.
    assert!(
        page.contains("[0x1b, 0x5b, 0x5a]"),
        "Shift-Tab (CSI Z) missing"
    );
    // Line editing and TUI navigation.
    for (name, seq) in [
        ("Home", "[0x1b, 0x5b, 0x48]"),
        ("End", "[0x1b, 0x5b, 0x46]"),
        ("Delete", "[0x1b, 0x5b, 0x33, 0x7e]"),
        ("PageUp", "[0x1b, 0x5b, 0x35, 0x7e]"),
    ] {
        assert!(page.contains(seq), "{name} sequence missing");
    }
    // F1-F4 use SS3, not CSI -- getting this wrong breaks menuconfig.
    assert!(page.contains("F1: [0x1b, 0x4f, 0x50]"), "F1 must be SS3 P");
    assert!(
        page.contains("F5: [0x1b, 0x5b, 0x31, 0x35, 0x7e]"),
        "F5 must be CSI 15~"
    );
}

/// vim, menuconfig and top switch the terminal to the ALTERNATE SCREEN and then
/// address a fixed grid by row and column. That is a different model from
/// scrollback -- there is no "append a line", only "put this character at
/// 12,40" -- and emulating it in the scrollback model is what made vim
/// unusable: cursor moves became stray text and the display never converged.
#[test]
fn a_full_screen_application_gets_a_real_grid() {
    let page = include_str!("../../src/dashboard.html");

    // A screen model exists, with the operations a TUI actually issues.
    assert!(page.contains("const screen = {"), "no grid model");
    assert!(
        page.contains("?1049"),
        "the alternate screen must be recognised"
    );
    for op in ["eraseDisplay", "eraseLine", "scrollDown", "scroll(n)"] {
        assert!(page.contains(op), "missing screen op: {op}");
    }
    // Cursor addressing: CSI H is the one everything depends on.
    assert!(
        page.contains(r#"case "H": case "f":"#),
        "no cursor addressing"
    );
    // Scroll region, which is how vim keeps a status line still.
    assert!(page.contains(r#"case "r":"#), "no scroll region");

    // The grid is what gets painted while it is active. The painter is
    // incremental for scrollback and takes its own early branch for an
    // application, so this reads that branch rather than one expression. What
    // it draws is proven in a real browser by
    // `browser::the_alternate_screen_round_trip_restores_the_scrollback`.
    let paint = page
        .split("  paint() {")
        .nth(1)
        .expect("the terminal painter");
    let grid_branch = paint
        .split("if (screen.active) {")
        .nth(1)
        .expect("paint must branch for an application that owns the terminal")
        .split("return;")
        .next()
        .expect("and that branch must end the paint");
    assert!(
        grid_branch.contains("screen.render()"),
        "paint must render the grid when an application owns the terminal"
    );
    // ...and scrollback must survive underneath, so :q returns the log.
    assert!(
        page.contains("scrollback is kept untouched"),
        "leaving the alternate screen must restore the log, not clear it"
    );
    // Auto-follow fights a fixed grid: the application's branch returns before
    // the painter ever touches the scroll position.
    assert!(
        !grid_branch.contains("scrollTop"),
        "must not autoscroll a grid"
    );
    assert!(
        paint.contains("state.follow"),
        "the scrollback, by contrast, still follows"
    );
}

/// conminer's own messages belong BESIDE the console, never inside it. Written
/// into the stream they corrupt what the board actually said: they survive in
/// scrollback, they end up in a copy-paste of a boot log, and under a
/// full-screen application they land in the middle of the grid.
#[test]
fn conminer_notices_do_not_pollute_the_console_stream() {
    let page = include_str!("../../src/dashboard.html");

    assert!(
        !page.contains(r#"[conminer] ${msg.message}"#),
        "status messages must not be written into the terminal stream"
    );
    assert!(
        !page.contains("[conminer] connection closed"),
        "a closed socket must not leave text in the board's log"
    );
    assert!(
        page.contains(r#"function note("#),
        "notices need somewhere else to go"
    );
    assert!(page.contains(r#"id="cnote""#), "and an element to go to");
}

/// Direct typing replaced the compose-then-send box: it is a slower path to the
/// same bytes and a second place for state to disagree. Ctrl-C stays -- it is
/// the one key a stuck console needs and an operator cannot always type it.
#[test]
fn the_console_has_no_redundant_compose_box() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        !page.contains(r#"id="entry""#),
        "the text field is redundant with direct typing"
    );
    assert!(!page.contains(r#"id="send""#), "so is its Send button");
    // Ctrl-C needs no button: the terminal encodes it from the keyboard.
    assert!(
        !page.contains(r#"id="ctrlc""#),
        "the Ctrl-C button is redundant with key encoding"
    );
    assert!(
        page.contains("k.charCodeAt(0) - 96"),
        "Ctrl-<letter> must still be encoded"
    );
    // The hint is an invitation to type, so it only makes sense while sending.
    assert!(
        page.contains(r#"id="typehint""#),
        "the hint needs an id to gate"
    );
    assert!(
        page.contains(r#"$("typehint").classList.toggle("hidden", !state.send)"#),
        "the hint must be hidden in read-only mode"
    );
}

/// There is ONE power bar, rebound by wirePower() to whichever console was
/// opened last. With two controllers on a bench that is a hazard: the panel
/// looks identical whichever board it is about to actuate, and the binding
/// lives in invisible state. Found while stress-testing the click path -- the
/// button could not be attributed to a board at all.
#[test]
fn power_buttons_name_the_board_they_will_actuate() {
    let page = include_str!("../../src/dashboard.html");

    // Readable by a human...
    assert!(
        page.contains(r#"id="powertarget""#),
        "the panel must show its target"
    );
    // Named by its PORT. A nickname used to be shown instead, so the one line
    // that says which board is about to be actuated could read "adp-ventuno"
    // while four FTDI interfaces sat behind it. The label may ride along, but
    // the port is what identifies the target.
    assert!(
        page.contains(r#"target.textContent = d.device.split("/").pop()"#),
        "and keep it current, by port"
    );
    assert!(
        !page.contains(r#"target.textContent = d.nickname"#),
        "a label must never stand in for the port on the actuation path"
    );
    // ...and by a test, so a click can be proven to hit the intended device.
    assert!(
        page.contains("bar.dataset.device = d.device"),
        "panel must record its device"
    );
    assert!(
        page.contains("b.dataset.device = d.device"),
        "each button must record its device"
    );
}

/// Both power surfaces must be safe and attributable.
///
/// The chassis card builds its own buttons bound per-board at creation -- that
/// was always correct, but they carried no identity, so nothing could verify
/// WHICH board a click would actuate. The console bar is the opposite: ONE bar
/// reused by whichever console is open, bound only when wirePower() runs, so
/// before that a click would do nothing or hit the board bound last.
#[test]
fn both_power_bars_are_bound_before_they_are_clickable() {
    let page = include_str!("../../src/dashboard.html");

    // Chassis buttons: identifiable, and carrying the device they drive.
    assert!(
        page.contains("b.dataset.power = power; b.dataset.device = d.device;"),
        "chassis power buttons must record the board they actuate"
    );
    // Boot-mode buttons too -- EDL on the wrong board is the worst version of this.
    assert!(
        page.contains("`mode:${m}`"),
        "boot-mode buttons need identity as well"
    );

    // The shared console bar ships disabled and is only enabled once bound.
    let head = page
        .split(r#"data-power="on""#)
        .nth(1)
        .expect("no console bar");
    assert!(
        head[..40.min(head.len())].contains("disabled"),
        "the shared bar must start disabled: {:?}",
        &head[..40.min(head.len())]
    );
    assert!(
        page.contains(
            r#"for (const b of bar.querySelectorAll("[data-power]")) b.disabled = false;"#
        ),
        "and only become clickable when wirePower binds it to a device"
    );
}

// ---------------------------------------------------------------------------
// Dashboard hardware-button tests.
//
// These drive the REAL dashboard routes over HTTP, because that is the path a
// human takes and it is NOT the path the MCP tools take. Both were green while
// the button was silently actuating a different board:
//
//   pressing power-off on the Bughopper board powered off the IQ10, and answered
//   ok:true verified:true.
//
// The board that got hit was the one whose controller happened to sit at the
// hook script's default port. Every assertion below exists because that
// happened on real hardware.
// ---------------------------------------------------------------------------

/// Two boards on DIFFERENT USB branches. The board with no controller of its own
/// must be REFUSED, not quietly serviced by the other board's controller.
///
/// This is the regression for the cross-board actuation incident. It is the
/// dashboard's own route, so a fix that only lands in the MCP path cannot make
/// it pass.
#[test]
fn a_dashboard_power_button_never_actuates_a_board_it_does_not_own() {
    let rig = Rig::start(Config::default());

    // Board A: console + its controller on branch ...-1.2
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorX_BoardA_UART_AAAA-if00-port0",
        Some("pci-0000:00:14.0-usb-0:1.2.1:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_BOARDA-if00",
        Some("pci-0000:00:14.0-usb-0:1.2.2:1.0"),
        None,
    );
    // Board B: console on branch ...-3.3, with NO controller anywhere near it.
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorY_BoardB_UART_BBBB-if00-port0",
        Some("pci-0000:00:14.0-usb-0:3.3.1:1.0"),
        None,
    );

    let (code, body) = rig.post("/api/power/BBBB-if00-port0/off");

    // The precise failure was `ok: true` on a board with no controller of its
    // own. The dashboard proxies hardware actions to mcpd, which is not running
    // in this rig, so what must hold HERE is that the dashboard never reports
    // success on its own and never names another board's controller. The
    // resolution rule itself is pinned in the config suite
    // (a_power_hook_needing_a_controller_is_refused_when_none_can_be_resolved
    // and the_bantam_templates_pass_the_controller_port).
    assert!(
        !body.contains("\"ok\":true"),
        "board B has no controller on its USB branch; the button must never \
         report success, got HTTP {code}: {body}"
    );
    assert!(
        !body.contains("BOARDA"),
        "board B's button reached board A's controller: {body}"
    );
}

/// The happy path still has to work, or the guard above is just an outage.
#[test]
fn a_dashboard_power_button_still_works_for_a_board_that_owns_its_controller() {
    let rig = Rig::start(Config::default());
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorX_BoardA_UART_AAAA-if00-port0",
        Some("pci-0000:00:14.0-usb-0:1.2.1:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_BOARDA-if00",
        Some("pci-0000:00:14.0-usb-0:1.2.2:1.0"),
        None,
    );

    let (_code, body) = rig.post("/api/power/AAAA-if00-port0/off");
    // Hardware actions are proxied to mcpd, absent here, so the meaningful
    // dashboard-side property is that the route EXISTS and forwards rather than
    // rejecting the selector outright -- a 404/unknown-device here would mean the
    // button is dead for a board that does have a controller.
    let lowered = body.to_ascii_lowercase();
    assert!(
        !lowered.contains("unknown device") && !lowered.contains("not found"),
        "a board with its own controller must not have a dead power button: {body}"
    );
}

/// Every board the dashboard offers a power button for must resolve a hook that
/// names a controller. A button that renders but cannot name its target is the
/// exact shape of the incident.
#[test]
fn no_board_offers_a_power_button_it_cannot_aim() {
    let rig = Rig::start(Config::default());
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorY_BoardB_UART_BBBB-if00-port0",
        Some("pci-0000:00:14.0-usb-0:3.3.1:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_ELSEWHERE-if00",
        Some("pci-0000:00:14.0-usb-0:9.9.9:1.0"),
        None,
    );

    let (_c, devices) = rig.get("/api/devices");
    // If the UI says this board has a power hook, the button must work; if it
    // does not, the UI must not offer one. Either is fine -- claiming a hook and
    // then refusing is what strands a user.
    if devices.contains("BBBB") && devices.contains("\"has_power_hook\":true") {
        let (_c2, body) = rig.post("/api/power/BBBB-if00-port0/off");
        assert!(
            !body.contains("ELSEWHERE"),
            "advertised a power button and then aimed it at another branch: {body}"
        );
    }
}

/// The broker's whole reason for existing: N consumers, ONE reader of the
/// device.
///
/// Every consumer used to open its own ser2net connection to the same console --
/// minerd for capture, dashd for the web terminal, mcpd for probes. That made
/// contention on the tty everyone's problem and nobody's job: when the device
/// open failed, each consumer independently concluded "the board is quiet",
/// while the tty itself was producing 102190 bytes.
#[test]
fn many_consumers_share_one_reader_through_the_broker() {
    use conminer_core::broker::Hub;

    let hub = Hub::new();
    let device = "/dev/serial/by-id/usb-FTDI_RIDE-if00-port0";

    // Stand in for dashd, mcpd and a second browser tab.
    let mut dashd = hub.subscribe(device);
    let mut mcpd = hub.subscribe(device);
    let mut second_tab = hub.subscribe(device);
    assert_eq!(
        hub.subscriber_count(device),
        3,
        "three consumers, and still only minerd reads the device"
    );

    // minerd, the single reader, republishes what it read.
    hub.publish(device, b"S - QC_IMAGE_VERSION_STRING=nordau\r\n");

    for (who, rx) in [
        ("dashd", &mut dashd),
        ("mcpd", &mut mcpd),
        ("second tab", &mut second_tab),
    ] {
        let got = rx
            .try_recv()
            .unwrap_or_else(|e| panic!("{who} saw nothing: {e}"));
        assert!(
            String::from_utf8_lossy(&got).contains("QC_IMAGE_VERSION_STRING"),
            "{who} must see the console without opening its own connection"
        );
    }
}

/// PRESENCE AND HEALTH ARE DIFFERENT QUESTIONS, and the page asks both.
///
/// They shared one column once. Migration 6 split them; the payload kept
/// shipping only presence, so the page went on deciding "is this console alive"
/// from a field that discovery owns and overwrites. On the bench every row read
/// `state: not_listening` -- health, frozen, in the presence field.
#[test]
fn the_payload_carries_capture_health_beside_presence() {
    let rig = Rig::start(cfg());
    let canonical = "usb-health";
    rig.add_device(canonical, Some(5099));

    // minerd's path, not a hand-written column: what the dashboard reports has
    // to be what capture actually published.
    {
        let mut reg = rig.registry();
        let row = reg.resolve(canonical).unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            row.id,
            conminer_core::live::CaptureState::Streaming,
        )
        .unwrap();
    }

    rig.until("capture health", |v| {
        v["devices"][0]["capture_state"] == "streaming"
    });
    let (_, body) = rig.get("/api/devices");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let d = &v["devices"][0];
    assert_eq!(
        d["capture_state"], "streaming",
        "capture health must reach the page: {d}"
    );
    assert_ne!(
        d["state"], "streaming",
        "presence must not be carrying health: {d}"
    );
}

// ---------------------------------------------------- keystrokes reach boards -
//
// Every test below drives the REAL path: browser socket -> dashd -> ser2net.
// The bug they exist for shipped because the only "TX survives the broker"
// assertion in this suite was a grep for a comment in dash.rs. The comment was
// still there, still true as an intention, and the console was read-only for
// days. Source text is not behaviour; none of these pass without the bytes
// arriving at the other end.

/// A broker on a real Unix socket, so the dashboard takes its production path.
struct FakeBroker {
    hub: std::sync::Arc<conminer_core::broker::Hub>,
    dir: tempfile::TempDir,
    _stop: tokio::sync::watch::Sender<bool>,
    _rt: tokio::runtime::Runtime,
}

impl FakeBroker {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hub = conminer_core::broker::Hub::new();
        let path = conminer_core::broker::socket_path(dir.path());
        let (stop, rx) = tokio::sync::watch::channel(false);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = hub.clone();
        let p = path.clone();
        rt.spawn(async move {
            let _ = conminer_core::broker::serve(h, p, rx).await;
        });
        // WAIT FOR THE SOCKET. A dashboard that starts first falls back to a
        // direct read, and the whole test then proves the wrong path.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !path.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "the broker socket never appeared");
        Self {
            hub,
            dir,
            _stop: stop,
            _rt: rt,
        }
    }

    fn run_dir(&self) -> std::path::PathBuf {
        self.dir.path().to_path_buf()
    }
}

/// A rig whose console reads through a broker, exactly as a production node does.
fn rig_with_broker(broker: &FakeBroker) -> Rig {
    let mut config = cfg();
    config.paths.run_dir = broker.run_dir();
    Rig::start(config)
}

/// Register a console and wait until the dashboard has actually picked it up.
/// Connecting before the refresh lands is a 404 on the socket, and the failure
/// reads like a broken selector rather than a race.
fn add_console(rig: &Rig, canonical: &str, port: u16) {
    rig.add_device(canonical, Some(port));
    rig.until("the console", |v| {
        v["devices"].as_array().map(|a| !a.is_empty()) == Some(true)
    });
}

/// Read frames until one satisfies `want`, or give up.
///
/// The read timeout is the WHOLE budget, not a slice of it: a timeout part way
/// through a frame leaves the stream misaligned on a header, and every frame
/// after it is garbage. Retrying past that turns a slow test into a lying one.
fn wait_for_frame(ws: &mut TcpStream, within: Duration, want: impl Fn(&[u8]) -> bool) -> bool {
    let deadline = Instant::now() + within;
    ws.set_read_timeout(Some(within)).unwrap();
    while Instant::now() < deadline {
        match read_frame(ws) {
            Some((_, payload)) if want(&payload) => return true,
            Some(_) => continue,
            None => return false,
        }
    }
    false
}

/// Wait until the dashboard has actually dialled the fake ser2net port.
///
/// Saying anything before that writes to nobody: the port has no clients yet and
/// the line is simply lost. (Cost one full-suite failure that passed alone.)
fn wait_for_dial(port: &FakePort) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && port.client_count() == 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        port.client_count() > 0,
        "the dashboard never dialled the console"
    );
}

/// THE HARNESS MUST NOT DROP THE CONSOLE'S OUTPUT ON THE FLOOR.
///
/// Gate for the flake that cost this suite one run in four at
/// `--test-threads=32`. A dashboard's `TcpStream::connect` returns when the
/// kernel completes the handshake; the fake port's accept loop is a separate
/// thread that registers the socket some time later. Between those two moments
/// the console is connected as far as everyone can tell -- the server dials,
/// attaches and says hello -- while a line said to the port has no registered
/// socket to go to. Written to an empty list, the line is gone forever, and the
/// test that was waiting for it waits out its entire read timeout.
///
/// Evidence, at `--test-threads=32`: `a_late_joiner_is_shown_the_scrollback`
/// failed 5 times in 20 runs, every failure showing `say 28 bytes to 0 clients`
/// on the port while the server published `viewers:1, attached:true`. The server
/// was never the problem.
///
/// The ordering is forced here rather than waited for, so the property is
/// checked on every run instead of gambled on under load.
#[test]
fn a_line_said_before_the_dial_is_not_lost() {
    let port = FakePort::start();
    port.say
        .send(b"Kernel panic - not syncing\r\n".to_vec())
        .unwrap();
    // Let the say thread reach the write with nobody connected. This is the
    // interleaving a loaded box produces by accident; making it deterministic is
    // the whole point of the gate.
    std::thread::sleep(Duration::from_millis(100));

    let mut client = TcpStream::connect(("127.0.0.1", port.port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 128];
    let n = client.read(&mut buf).unwrap_or(0);
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("Kernel panic"),
        "the port wrote a console line to nobody and lost it: got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

/// Wait for bytes containing `needle` to arrive at the port.
fn port_received(port: &FakePort, needle: &[u8], within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Some(chunk) = port.next_received(Duration::from_millis(250)) {
            seen.extend_from_slice(&chunk);
            if seen.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }
    false
}

/// THE REGRESSION, in one test: with the broker as the reader -- which is every
/// node in the fleet -- a keystroke typed in the web terminal must reach the
/// board.
///
/// It did not. Reading from the broker leaves the ser2net connection unread;
/// TCP flow control then closed it, and nothing re-dialled it because the
/// reconnect logic lived in the reader, which was watching a different socket
/// entirely. Output kept scrolling from the broker, so the console looked
/// perfectly healthy while being completely deaf.
#[test]
fn a_keystroke_reaches_the_port_when_the_reader_is_the_broker() {
    let broker = FakeBroker::start();
    let port = FakePort::start();
    let rig = rig_with_broker(&broker);
    let canonical = "usb-txgate-aaaa";
    add_console(&rig, canonical, port.port);

    let mut ws = ws_connect(&rig.base, canonical);

    // PROVE THE READER IS THE BROKER before trusting anything else here: a
    // fallback to a direct read would make this test pass while covering the
    // path that never broke.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut subscribed = false;
    while Instant::now() < deadline && !subscribed {
        subscribed = broker.hub.subscriber_count(canonical) > 0;
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(subscribed, "the dashboard never subscribed to the broker");
    wait_for_dial(&port);
    broker
        .hub
        .publish(canonical, b"published-by-the-broker\r\n");
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(5), |p| {
            String::from_utf8_lossy(p).contains("published-by-the-broker")
        }),
        "the browser must be reading through the broker"
    );

    // And now the direction that was silently dead.
    send_binary_frame(&mut ws, b"version\r");
    assert!(
        port_received(&port, b"version\r", Duration::from_secs(5)),
        "the keystroke never reached the console"
    );
}

/// The ser2net socket must be drained even when nothing needs to read it.
///
/// An unread receive buffer is not free: the window closes, ser2net blocks
/// writing, and it eventually drops the client that carries every keystroke.
/// Here the port is the one that stalls, which is exactly how it fails in
/// production, one buffer earlier.
#[test]
fn the_ser2net_socket_is_drained_when_reading_from_the_broker() {
    let broker = FakeBroker::start();
    let port = FakePort::start();
    let rig = rig_with_broker(&broker);
    let canonical = "usb-drain-bbbb";
    add_console(&rig, canonical, port.port);

    let _ws = ws_connect(&rig.base, canonical);
    wait_for_dial(&port);
    assert_eq!(port.client_count(), 1, "one viewer is one ser2net client");

    // Far more than any socket buffer pair will hold.
    const CHUNK: usize = 64 * 1024;
    const TOTAL: usize = 4 * 1024 * 1024;
    for _ in 0..(TOTAL / CHUNK) {
        port.say.send(vec![b'.'; CHUNK]).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && port.pushed() < TOTAL {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        port.pushed(),
        TOTAL,
        "the console's output stalled: {} of {TOTAL} bytes got out, so nobody is \
         draining the socket that carries keystrokes",
        port.pushed()
    );
}

/// ser2net drops the write connection -- restart, tty contention, a client it
/// gave up on. The next keystroke must land anyway.
///
/// It used to be dropped forever: the writer emptied its slot on the failed
/// write and waited for a reader that was never going to refill it.
#[test]
fn a_keystroke_lands_after_ser2net_drops_the_write_socket() {
    let broker = FakeBroker::start();
    let port = FakePort::start();
    let rig = rig_with_broker(&broker);
    let canonical = "usb-redial-cccc";
    add_console(&rig, canonical, port.port);

    let mut ws = ws_connect(&rig.base, canonical);
    wait_for_dial(&port);
    send_binary_frame(&mut ws, b"first\r");
    assert!(
        port_received(&port, b"first\r", Duration::from_secs(5)),
        "the console was deaf before the drop, so this proves nothing"
    );

    let before = port.client_count();
    port.drop_clients();
    std::thread::sleep(Duration::from_millis(200));

    send_binary_frame(&mut ws, b"second\r");
    assert!(
        port_received(&port, b"second\r", Duration::from_secs(10)),
        "a keystroke after a dropped ser2net connection was swallowed"
    );
    assert!(
        port.client_count() > before,
        "the writer must have dialled a fresh connection"
    );
}

/// OUTPUT MUST COME BACK AFTER THE CONSOLE DROPS, AND COME BACK PROMPTLY.
///
/// Every power action drops this connection, and the reader re-dials
/// underneath the viewers. If that re-dial sits out a backoff, the operator
/// watches a dead terminal through the part of the boot they most wanted to
/// see -- reported from the bench as the console feeling laggy and arriving in
/// big chunks. The write path after a drop is covered above; this is the read
/// path, which had no test at all.
#[test]
fn console_output_resumes_promptly_after_the_port_drops_it() {
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    let canonical = "usb-redial-read";
    add_console(&rig, canonical, port.port);

    let mut ws = ws_connect(&rig.base, canonical);
    read_text_frame(&mut ws).expect("hello");
    wait_for_dial(&port);
    port.say.send(b"before\r\n".to_vec()).unwrap();
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(5), |p| {
            String::from_utf8_lossy(p).contains("before")
        }),
        "the console was silent before the drop, so this proves nothing"
    );

    let before = port.client_count();
    port.drop_clients();

    // The reader must come back on its own, without a viewer reconnecting.
    let redial = Instant::now();
    let deadline = redial + Duration::from_secs(5);
    while port.client_count() <= before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        port.client_count() > before,
        "the reader never re-dialled after the console dropped"
    );
    assert!(
        redial.elapsed() < Duration::from_secs(3),
        "the re-dial took {:?}: a viewer watching a boot loses it to a backoff",
        redial.elapsed()
    );

    port.say.send(b"after\r\n".to_vec()).unwrap();
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(10), |p| {
            String::from_utf8_lossy(p).contains("after")
        }),
        "output after a dropped console never reached the browser"
    );
}

/// THE BOOT AN OPERATOR IS WATCHING FOR, END TO END THROUGH THE BROWSER.
///
/// The console does not merely drop during a power action -- it GOES AWAY. The
/// board resets, its USB re-enumerates, ser2net cannot open the tty at all for
/// several seconds, and then the board comes back and prints its bootloader.
/// That is the window an operator most wants to see and the one that was being
/// lost: measured on the bench as an epoch holding 32 lines that began
/// mid-kernel, against 258 lines with the firmware banner when it worked.
///
/// Everything else here drops a client and re-accepts immediately. This retires
/// the port entirely, waits past the point a doubling backoff would have
/// escalated, brings it back, and requires the firmware banner to reach the
/// browser.
#[test]
fn the_firmware_banner_after_an_outage_reaches_the_browser() {
    let port = FakePort::start();
    let number = port.port;
    let rig = Rig::start(cfg());
    let canonical = "usb-outage";
    add_console(&rig, canonical, number);

    let mut ws = ws_connect(&rig.base, canonical);
    read_text_frame(&mut ws).expect("hello");
    wait_for_dial(&port);
    port.say.send(b"pre-reset\r\n".to_vec()).unwrap();
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(5), |p| {
            String::from_utf8_lossy(p).contains("pre-reset")
        }),
        "the console was silent before the outage, so this proves nothing"
    );

    // The board resets: the console is not merely closed, it is GONE.
    port.stop();
    std::thread::sleep(Duration::from_millis(1500));

    // ...and comes back, printing its firmware banner immediately, as a board
    // does the moment its UART is alive again.
    let back = FakePort::start_on(number);
    // HOW LONG the reader takes to notice is the whole point: a doubling
    // backoff is still "eventually", and eventually is after the boot.
    let returned = Instant::now();
    wait_for_dial(&back);
    let redial = returned.elapsed();
    assert!(
        redial < Duration::from_secs(4),
        "the reader took {redial:?} to re-attach after the console returned: the board \
         prints its bootloader immediately, so that window is the boot"
    );
    back.say
        .send(b"B -    752009 - SEC Image Loaded, Start\r\n".to_vec())
        .unwrap();

    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(15), |p| {
            String::from_utf8_lossy(p).contains("SEC Image Loaded")
        }),
        "the firmware banner printed after the console returned never reached the browser: \
         this is the part of the boot an operator is watching for"
    );
}

/// A keystroke that genuinely cannot be delivered must SAY SO.
///
/// Silence is the worst possible answer: the operator watches output scroll,
/// types, sees nothing happen, and has no way to tell a deaf console from a
/// board that is ignoring them.
#[test]
fn an_undeliverable_keystroke_is_reported_to_the_browser() {
    let broker = FakeBroker::start();
    let port = FakePort::start();
    let rig = rig_with_broker(&broker);
    let canonical = "usb-deaf-dddd";
    add_console(&rig, canonical, port.port);

    let mut ws = ws_connect(&rig.base, canonical);
    wait_for_dial(&port);
    port.stop(); // nothing listening, nothing connected
    std::thread::sleep(Duration::from_millis(200));

    send_binary_frame(&mut ws, b"into-the-void\r");
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(10), |p| {
            String::from_utf8_lossy(p).contains("keystroke not delivered")
        }),
        "an undeliverable keystroke must be reported, never swallowed"
    );
}

/// The dashboard must keep working when minerd is down, because that is exactly
/// when someone is staring at a console trying to find out why.
///
/// Executed now, in both directions. This was a grep over dash.rs, which is how
/// a read-only console shipped with a green suite.
#[test]
fn the_dashboard_falls_back_when_the_broker_is_absent() {
    let port = FakePort::start();
    // No broker anywhere: run_dir has no socket in it.
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg();
    config.paths.run_dir = dir.path().to_path_buf();
    let rig = Rig::start(config);
    let canonical = "usb-nobroker-eeee";
    add_console(&rig, canonical, port.port);

    let mut ws = ws_connect(&rig.base, canonical);
    wait_for_dial(&port);
    // RX: read directly from ser2net.
    port.say.send(b"direct-read\r\n".to_vec()).unwrap();
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(5), |p| {
            String::from_utf8_lossy(p).contains("direct-read")
        }),
        "an absent broker must degrade to a direct read, not kill the console"
    );
    // TX: a broker outage can never be allowed to swallow a keystroke.
    send_binary_frame(&mut ws, b"still-typing\r");
    assert!(
        port_received(&port, b"still-typing\r", Duration::from_secs(5)),
        "TX must stay independent of the broker"
    );
}

/// `/api/events` is how every open browser learns a device changed.
///
/// It had no test at all -- found by the surface suite's route-coverage check,
/// which is the point of having one. If this stream dies, dashboards silently
/// stop updating and show stale state forever, which is worse than an error
/// because nothing looks wrong.
#[test]
fn the_events_stream_answers_and_stays_open() {
    let rig = Rig::start(Config::default());
    rig.add_device("/dev/serial/by-id/usb-VendorX_Events_AAAA-if00-port0", None);

    // Bounded: it is a stream, so the test takes what it can and moves on.
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "2",
            "-o",
            "/dev/stdout",
            "-w",
            "\n%{http_code}",
            &format!("{}/api/events", rig.base),
        ])
        .output()
        .expect("curl");
    let body = String::from_utf8_lossy(&out.stdout).to_string();

    // Check the STATUS LINE, not the body.
    //
    // The first version grepped the whole response for "404"/"500", and the
    // stream carries device JSON -- timestamps, ports, byte counts -- any of
    // which can contain those digits. It passed alone and failed under load
    // purely on what the boards happened to be emitting, which is a test that
    // reports the data as a defect.
    //
    // curl writes the status last; on a still-open stream it times out and
    // reports 000, which is the HEALTHY outcome for server-sent events.
    let status = body.rsplit('\n').next().unwrap_or("").trim().to_string();
    assert!(
        status == "000" || status.starts_with('2'),
        "the events stream must exist and not error, got status {status:?}"
    );
}

/// N7: a power reading with no age is unusable by automation.
///
/// The lamp is cached (a controller query costs ~1-2s, and a page that blocks on
/// one is worse than a light a few seconds stale), so a caller reading `power`
/// straight after its own power-off can be handed the reading from BEFORE the
/// action and cannot tell. Publishing when the reading was taken turns that
/// unknowable race into a checkable condition.
#[test]
fn every_device_reports_when_its_power_was_last_sensed() {
    let rig = Rig::start(Config::default());
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorZ_BoardC_UART_CCCC-if00-port0",
        Some("pci-0000:00:14.0-usb-0:4.1.1:1.0"),
        None,
    );

    // The device set is discovered on a refresh, not at start.
    let devices = rig.until("the board to appear", |v| {
        v["devices"].as_array().is_some_and(|ds| {
            ds.iter()
                .any(|d| d["device"].as_str().is_some_and(|n| n.contains("CCCC")))
        })
    });
    let list = devices["devices"].as_array().expect("a device list");
    for d in list {
        assert!(
            d.get("power_sensed_at").is_some(),
            "every device must carry the age of its power reading, even as null \
             before the first sweep: {d}"
        );
    }
}

// ------------------------------------------------------- power is exact ---
//
// Measured on alpha, and the reason this section exists: the NordAU RIDE SX
// was powered ON -- its Bantam on ttyACM1 said so, and `diagnose` on its own
// console said `power=on` -- while all six of its dashboard rows read "off".
// The IQ10 next to it was genuinely off, and its answer was being published for
// the NordAU as well.
//
// A board reporting another board's power is indistinguishable from a lie, and
// it is worse than an honest "unknown": the whole point of the indicator is to
// let somebody decide whether to walk over and press a button.

/// One console, as the sweep sees it.
fn console(
    canonical: &str,
    profile: &str,
    controller_port: Option<&str>,
) -> conminer::dash::DashDevice {
    conminer::dash::DashDevice {
        // §P1: a local device, which is what every pre-fleet test means.
        node: None,
        node_host: None,
        // A local chassis is headed by its own key, so no override.
        adapter_label: None,
        // No sweep has asked this fixture's controller anything.
        boot_overrides: None,
        // Plugged in: these fixtures model a bench with the cable in.
        present: true,
        device: canonical.into(),
        canonical: canonical.into(),
        nickname: None,
        target: None,
        adapter: None,
        port: None,
        boot_modes: Vec::new(),
        has_power_hook: true,
        power_sensed_at: None,
        power: None,
        controller: Some(profile.into()),
        controller_port: controller_port.map(str::to_string),
        controller_label: None,
        controller_tags: Default::default(),
        is_file: false,
        is_controller: false,
        line: String::new(),
        state: "listening".into(),
        capture_state: None,
        ignored: false,
        observed: serde_json::json!({}),
        tags: Default::default(),
        last_seen: 0,
        viewers: 0,
        attached: false,
    }
}

/// THE BUG: two boards, two Bantams, one profile name.
#[test]
fn two_boards_sharing_a_controller_profile_never_share_a_power_reading() {
    // Both controllers come from the same profile, so both consoles carry
    // `controller: "bantam"`. That name is not an identity: it says what KIND of
    // controller this is, not which one.
    let devices = vec![
        console("usb-FTDI_IQ10-if00-port0", "bantam", Some("/dev/ttyACM0")),
        console("usb-FTDI_IQ10-if01-port0", "bantam", Some("/dev/ttyACM0")),
        console("usb-FTDI_NordAU-if00-port0", "bantam", Some("/dev/ttyACM1")),
        console("usb-FTDI_NordAU-if01-port0", "bantam", Some("/dev/ttyACM1")),
    ];
    let groups = conminer::dash::group_by_controller(&devices);

    assert_eq!(
        groups.len(),
        2,
        "one group per CONTROLLER, and there are two controllers here. Keying on \
         the profile name collapses both boards into one, probes a single \
         console and publishes its answer for the other board: {groups:?}"
    );
    for (controller, consoles) in &groups {
        let boards: std::collections::BTreeSet<bool> =
            consoles.iter().map(|c| c.contains("IQ10")).collect();
        assert_eq!(
            boards.len(),
            1,
            "group {controller} mixes two boards: {consoles:?}"
        );
    }
    // And each board is still probed ONCE, which is the reason to group at all.
    assert!(
        groups.values().all(|c| c.len() == 2),
        "both consoles of a board share its one reading: {groups:?}"
    );
}

/// An unresolvable controller must group a console ALONE, never under a name it
/// shares with anything else -- that is how the original bug was built.
#[test]
fn a_console_with_no_resolved_controller_is_probed_on_its_own() {
    let devices = vec![
        console("usb-A-if00-port0", "bantam", None),
        console("usb-B-if00-port0", "bantam", None),
    ];
    let groups = conminer::dash::group_by_controller(&devices);
    assert_eq!(
        groups.len(),
        2,
        "unresolved means probe each separately: slower, and correct. Any shared \
         fallback key re-creates the cross-board bug: {groups:?}"
    );
}

/// A reading with no upper age is the second way this field lies: a wedged
/// prober or a dead mcpd leaves the last value sitting there looking current.
#[test]
fn a_power_reading_that_has_gone_stale_reports_unknown_not_its_last_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg();
    c.paths.data_dir = dir.path().to_path_buf();
    let dash = conminer::dash::Dash::new(c, dir.path().to_path_buf());
    let dev = vec!["usb-FTDI_NordAU-if00-port0".to_string()];

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    dash.publish_power(&dev, Some("on".into()), now);
    assert_eq!(
        dash.fresh_power(&dev[0]).as_deref(),
        Some("on"),
        "a reading taken just now is exactly what should be served"
    );

    // The same reading, five minutes old. Nothing about the board changed; what
    // changed is that we can no longer claim to know.
    dash.publish_power(&dev, Some("on".into()), now - 300_000);
    assert_eq!(
        dash.fresh_power(&dev[0]),
        None,
        "an expired reading must read as unknown, not as the state it last saw"
    );
}

/// Pressing power must not leave the pre-action value on the page.
#[test]
fn actuating_a_board_drops_its_stale_reading_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg();
    c.paths.data_dir = dir.path().to_path_buf();
    let dash = conminer::dash::Dash::new(c, dir.path().to_path_buf());
    // A whole board: six consoles, one controller, one power state.
    let board: Vec<String> = (0..6)
        .map(|i| format!("usb-FTDI_NordAU-if{i:02}-port0"))
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    dash.publish_power(&board, Some("on".into()), now);

    dash.invalidate_power(&board);

    for c in &board {
        assert_eq!(
            dash.fresh_power(c),
            None,
            "{c} still reports the state from before the button was pressed; \
             for the seconds until the re-probe lands we genuinely do not know"
        );
    }
}

/// A console must not inherit the power buttons of a controller that is GONE.
///
/// Measured on alpha: a Nucleo dev board, plugged into the same USB hub as an
/// IQ10's Bantam, rendered with `controller: bantam`, `controller_label: RRD`
/// and `has_power_hook: true` -- while that Bantam had been unplugged for five
/// and a half days. Pressing power on the Nucleo's row addressed a controller
/// that was not there, and would have addressed whatever board took its place.
///
/// The cause was the candidate list: `refresh()` resolved controllers against
/// every row the registry had ever held, in a variable called `present`.
///
/// THE FIRST HALF OF THIS TEST IS LOad-BEARING. Asserting only "no controller
/// once it is gone" would pass just as happily if the binding never happened at
/// all, which is how three gates in this codebase passed while testing nothing.
/// So it proves the binding EXISTS while the cable is in, then proves it goes.
#[test]
fn a_console_does_not_bind_to_a_controller_that_has_been_unplugged() {
    let rig = Rig::start(Config::default());
    let console = "/dev/serial/by-id/usb-STMicroelectronics_STLINK-V3_0045-if02";
    let bantam = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00";
    // Same downstream hub (3.2), which is exactly what the topology rule binds on.
    rig.add_device_at(
        console,
        Some("pci-0000:00:14.0-usb-0:3.2.1:1.0-port0"),
        None,
    );
    rig.add_device_at(bantam, Some("pci-0000:00:14.0-usb-0:3.2.2:1.0"), None);

    let row_of = |v: &serde_json::Value, needle: &str| -> serde_json::Value {
        v["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["canonical"].as_str().unwrap().contains(needle))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };

    // While both are plugged in, the binding is real and correct.
    let v = rig.until("the console binds its controller", |v| {
        row_of(v, "STLINK")["controller_port"].is_string()
    });
    let bound = row_of(&v, "STLINK");
    assert_eq!(
        bound["has_power_hook"], true,
        "precondition: a present Bantam gives this console power controls"
    );

    // Cable out. Discovery records that as `gone`; nothing else changes.
    {
        let mut reg = rig.registry();
        let id = reg.device_by_canonical(bantam).unwrap().unwrap().id;
        reg.set_state(id, "gone").unwrap();
    }

    let v = rig.until("the console lets go of it", |v| {
        row_of(v, "STLINK")["controller_port"].is_null()
    });
    let now = row_of(&v, "STLINK");
    assert!(
        now["controller"].is_null(),
        "an absent controller must not be named as this console's: {now}"
    );
    assert_eq!(
        now["has_power_hook"], false,
        "and must offer no power button: pressing it would address hardware \
         that is not on the bench: {now}"
    );
    assert_eq!(
        now["boot_modes"].as_array().map(Vec::len),
        Some(0),
        "nor a boot-mode menu that cannot run: {now}"
    );
    // The console itself is untouched: this is about the CONTROLLER's absence.
    assert_eq!(
        now["present"], true,
        "the console is still plugged in: {now}"
    );
}

/// Hardware that left the bench comes OFF the page, and stays in the registry.
///
/// An operator, looking at alpha: "why are there all these uart consoles still
/// listed... that device isn't connected, I move hardware around so it comes and
/// goes". The page had accumulated 16 consoles and two controller panels for a
/// rig with ONE cable in it. A dashboard that lists every board it has ever seen
/// has stopped describing the bench.
///
/// The second half is the other half of the deal: the ROW survives. Its
/// nickname, port assignment and capture history are still there and
/// `list_devices` still answers for it, because an agent reaching for a boot
/// report from last week must not be told the board never existed.
#[test]
fn hardware_that_left_the_bench_leaves_the_page_but_not_the_registry() {
    let rig = Rig::start(Config::default());
    let here = "/dev/serial/by-id/usb-VendorX_Here_AAAA-if00-port0";
    let left = "/dev/serial/by-id/usb-VendorX_Left_BBBB-if00-port0";
    rig.add_device_at(here, Some("pci-0000:00:14.0-usb-0:1.2.1:1.0"), None);
    rig.add_device_at(left, Some("pci-0000:00:14.0-usb-0:1.3.1:1.0"), None);
    // Seeded with last_seen=1000 (1970), so this is long gone, not mid-replug.
    {
        let mut reg = rig.registry();
        let id = reg.device_by_canonical(left).unwrap().unwrap().id;
        reg.set_nickname(id, "bench-left").unwrap();
        reg.set_state(id, "gone").unwrap();
    }

    let v = rig.until("the departed board to leave the page", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let names: Vec<&str> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["canonical"].as_str().unwrap())
        .collect();
    assert!(
        names.iter().all(|n| !n.contains("BBBB")),
        "a board that is not on the bench must not be on the page: {names:?}"
    );
    assert!(names.iter().any(|n| n.contains("AAAA")), "{names:?}");

    // ...but the registry still has it, with everything it knew.
    let row = rig
        .registry()
        .device_by_canonical(left)
        .unwrap()
        .expect("the row must survive: history and identity outlive the cable");
    assert_eq!(row.nickname.as_deref(), Some("bench-left"));
}

/// A BOARD MID POWER-CYCLE MUST NOT BLINK OUT OF THE RACK.
///
/// This is the case that makes "hide what is absent" hard, and the one the operator
/// named as must-not-regress. A UART bridge powered BY the board loses power
/// with it: the chip drops off the bus and, for those seconds, looks exactly
/// like a pulled cable. Hiding it would make the rack flicker on every power
/// button press, and would remove the row a person is watching precisely
/// because they just pressed it.
///
/// The bound is `discovery.removal_grace_ms`, reused rather than reinvented: it
/// is already chosen so that "a hook press is shorter", and it is already what
/// the capture supervisor uses to hold the console open across the same event.
/// One definition of transient absence for the page and the capture loop.
#[test]
fn a_board_mid_power_cycle_stays_on_the_page() {
    let rig = Rig::start(Config::default());
    let dev = "/dev/serial/by-id/usb-Arduino_Bughopper_DK0HDSRI-if00-port0";
    rig.add_device_at(dev, Some("pci-0000:00:14.0-usb-0:9.1.1:1.0"), None);
    let v = rig.until("the board", |v| v["devices"].as_array().unwrap().len() == 1);
    assert_eq!(v["devices"][0]["present"], true, "precondition");

    // The bridge drops off the bus THIS INSTANT: last seen now, state gone.
    {
        let mut reg = rig.registry();
        let id = reg.device_by_canonical(dev).unwrap().unwrap().id;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        reg.upsert_device(
            dev,
            Some("pci-0000:00:14.0-usb-0:9.1.1:1.0"),
            IdentityKind::ById,
            None,
            now,
        )
        .unwrap();
        reg.set_state(id, "gone").unwrap();
    }

    // WAIT FOR THE WRITE TO REACH THE SNAPSHOT BEFORE ASSERTING ANYTHING.
    //
    // Without this the gate reads the previous snapshot, sees the board still
    // listed for the ORIGINAL reason -- it was still present -- and passes
    // having tested nothing at all.
    //
    // The wait ends on either outcome, so a filter that wrongly drops the row
    // finishes the wait too and fails on the assertion below rather than timing
    // out with nothing to say.
    let v = rig.until("the dashboard to notice the bridge drop off the bus", |v| {
        let rows = v["devices"].as_array().unwrap();
        rows.is_empty() || rows[0]["present"] == serde_json::json!(false)
    });
    assert_eq!(
        v["devices"].as_array().unwrap().len(),
        1,
        "a board that dropped off the bus a moment ago is mid power-cycle, not \
         gone from the bench: {v}"
    );

    // ...and it must STAY, across several more refreshes.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(120));
        let (code, body) = rig.get("/api/devices");
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["devices"].as_array().unwrap().len(),
            1,
            "it must not blink out of the rack a refresh later either: {v}"
        );
    }
}

/// A BOARD IN EDL STAYS ON THE PAGE, HOWEVER LONG THE FLASH.
///
/// The other must-not-regress case. In a download mode the board is emphatically
/// still plugged in -- its UART has re-enumerated as a QDL/fastboot/DFU gadget,
/// which minerd sees on the board's own USB ports and publishes as
/// `away_in_edl`. NO TIMER may apply to it: a Firehose flash runs for many
/// minutes, and a board blinking out of the rack mid-flash is both alarming and
/// exactly when someone is watching it.
///
/// The row here is seeded with last_seen=1000 (1970), so the power-cycle window
/// is long past and `away_in_edl` is the ONLY thing keeping it on the page. That
/// is what makes this gate about EDL rather than about the clock.
#[test]
fn a_board_in_edl_stays_on_the_page_however_long_the_flash() {
    let rig = Rig::start(Config::default());
    let dev = "/dev/serial/by-id/usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if00-port0";
    rig.add_device_at(dev, Some("pci-0000:00:14.0-usb-0:3.2.1:1.0"), None);
    rig.until("the board", |v| v["devices"].as_array().unwrap().len() == 1);

    // EDL took the UART: the tty is gone, and minerd says why.
    {
        let mut reg = rig.registry();
        let id = reg.device_by_canonical(dev).unwrap().unwrap().id;
        reg.set_state(id, "gone").unwrap();
        conminer_core::live::publish_capture_state(
            &mut reg,
            id,
            conminer_core::live::CaptureState::AwayInEdl,
        )
        .unwrap();
    }

    // Same discipline as the power-cycle gate: wait for the snapshot to carry
    // the EDL state, or for the row to vanish, before believing anything.
    let v = rig.until("the dashboard to see the board enter EDL", |v| {
        let rows = v["devices"].as_array().unwrap();
        rows.is_empty() || rows[0]["capture_state"] == serde_json::json!("away_in_edl")
    });
    assert_eq!(
        v["devices"].as_array().unwrap().len(),
        1,
        "a board being flashed is plugged in, and must stay in the rack: {v}"
    );
    assert_eq!(
        v["devices"][0]["present"], false,
        "precondition: its tty really is gone, so ONLY away_in_edl is holding \
         it on the page -- otherwise this gate is about the clock, not EDL: {v}"
    );

    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(120));
        let (code, body) = rig.get("/api/devices");
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["devices"].as_array().unwrap().len(),
            1,
            "a flash runs for minutes; no timer may take the board off the \
             page part-way through: {v}"
        );
    }
}

/// The end-to-end half of the same rule, against the real Bantam profile.
///
/// The unit test above proves the grouping rule; this proves the input it works
/// on is actually resolved per board. Two Bantams on two USB branches must give
/// their boards DIFFERENT controller instances -- if resolution collapses them,
/// correct grouping downstream cannot save it.
#[test]
fn two_boards_each_resolve_their_own_controller_instance() {
    let rig = Rig::start(Config::default());
    // Board A and its Bantam, on one branch of the hub.
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorX_BoardA_UART_AAAA-if00-port0",
        Some("pci-0000:00:14.0-usb-0:1.2.1:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_BOARDA-if00",
        Some("pci-0000:00:14.0-usb-0:1.2.2:1.0"),
        None,
    );
    // Board B and its Bantam, on another. Same profile name, different board.
    rig.add_device_at(
        "/dev/serial/by-id/usb-VendorY_BoardB_UART_BBBB-if00-port0",
        Some("pci-0000:00:14.0-usb-0:3.4.1:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_BOARDB-if00",
        Some("pci-0000:00:14.0-usb-0:3.4.2:1.0"),
        None,
    );

    let v = rig.until("all four", |v| v["devices"].as_array().unwrap().len() == 4);
    let devices = v["devices"].as_array().unwrap();
    let port_of = |needle: &str| -> Option<String> {
        devices
            .iter()
            .find(|d| d["canonical"].as_str().unwrap().contains(needle))
            .and_then(|d| d["controller_port"].as_str())
            .map(str::to_string)
    };
    let (a, b) = (port_of("AAAA"), port_of("BBBB"));
    assert!(
        a.is_some() && b.is_some(),
        "both boards resolve a controller: a={a:?} b={b:?}"
    );
    assert_ne!(
        a, b,
        "two boards resolved to the SAME controller instance, so one of them is \
         about to be told the other's power state"
    );
    assert!(
        a.as_deref().unwrap().contains("BOARDA") && b.as_deref().unwrap().contains("BOARDB"),
        "each board must resolve to ITS OWN Bantam: a={a:?} b={b:?}"
    );
}

/// FOUND ON HARDWARE, on the bravo node's first deploy.
///
/// A host with no Bantam anywhere on it, serving an FTDI TAC board. The bantam
/// profile ships `controls = "*"` on purpose, so it claimed both of that board's
/// consoles, and the dashboard API answered `controller: "bantam"` with the five
/// Bantam boot modes -- directly beside `has_power_hook: false`. An agent
/// reading that list would pick a mode this board cannot enter; a human would
/// look for a power button that is right to be missing.
#[test]
fn a_board_whose_controller_is_absent_is_advertised_as_having_none() {
    let rig = Rig::start(Config::default());
    // Both UARTs of one FTDI, and nothing else on the host.
    rig.add_device_at(
        "/dev/serial/by-id/usb-FTDI_TTL232R_FT9ZZZ01-if00-port0",
        Some("pci-0000:04:00.3-usb-0:4:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-FTDI_TTL232R_FT9ZZZ02-if00-port0",
        Some("pci-0000:04:00.3-usb-0:5:1.0"),
        None,
    );

    let v = rig.until("both consoles", |v| {
        v["devices"].as_array().unwrap().len() == 2
    });
    for d in v["devices"].as_array().unwrap() {
        let who = d["canonical"].as_str().unwrap();
        assert!(
            d["controller"].is_null(),
            "{who} names a controller that is not on this host: {:?}",
            d["controller"]
        );
        assert_eq!(
            d["boot_modes"].as_array().map(Vec::len),
            Some(0),
            "{who} offers boot modes nothing can select: {:?}",
            d["boot_modes"]
        );
        assert_eq!(
            d["has_power_hook"], false,
            "{who} must agree with itself about being uncontrolled"
        );
    }
}

/// A bench is a stack, not a masonry wall. `repeat(auto-fit, minmax(250px, …))`
/// split a rack into as many columns as the window was wide, so on a wide
/// monitor one board's consoles sat beside another board's and the reading
/// order stopped matching the hardware.
#[test]
fn the_rack_stacks_vertically_instead_of_splitting_into_columns() {
    let page = include_str!("../../src/dashboard.html");
    assert!(
        page.contains("grid-template-columns: 1fr;"),
        "the card grid must be a single column"
    );
    assert!(
        !page.contains("repeat(auto-fit"),
        "an auto-fit column track is what split the rack horizontally"
    );
}

/// THE WHOLE POINT, end to end: deploy, and a known board type arrives with
/// working controls that nobody configured.
///
/// An FTDI TAC board is one chip carrying both the consoles and the GPIO that
/// powers the board, so discovery finding the UARTs is the same event as the
/// controller arriving. No device mapping, no per-board config, no entry in
/// conminer.toml -- the by-id name carries the vendor's own product string and
/// the profile keys on it.
#[test]
fn a_tac_board_arrives_with_working_controls_and_no_configuration() {
    let rig = Rig::start(Config::default());
    rig.add_device_at(
        "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0",
        Some("pci-0000:04:00.3-usb-0:4:1.0"),
        None,
    );
    rig.add_device_at(
        "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0",
        Some("pci-0000:04:00.3-usb-0:4:1.1"),
        None,
    );

    let v = rig.until("both consoles", |v| {
        v["devices"].as_array().unwrap().len() == 2
    });
    for d in v["devices"].as_array().unwrap() {
        let who = d["canonical"].as_str().unwrap();
        assert_eq!(d["controller"], "tac", "{who} did not bind the TAC profile");
        assert_eq!(
            d["has_power_hook"], true,
            "{who} has a controller but no usable power hook"
        );
        let modes: Vec<&str> = d["boot_modes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(modes, ["EDL", "SAIL_EDL", "UEFI", "FASTBOOT"], "{who}");
        assert_eq!(
            d["is_controller"], true,
            "{who} is its own controller, and the page must render it as one"
        );
    }
}

/// FOUND ON HARDWARE by probing the fix instead of trusting it.
///
/// A `power on` against the IQ10 took **71.9 seconds** to return: the hook
/// powers the board, waits out the settle and verifies the effect. Invalidation
/// keyed on the RESPONSE therefore left the pre-action reading on the page for
/// that entire minute-plus -- and that is the worst possible moment for it,
/// because pressing the button is exactly when somebody is watching the lamp.
/// An operator sees "off" for a minute after pressing power-on and concludes the
/// press did nothing.
///
/// So the stale reading must be dropped BEFORE the call is dispatched.
#[test]
fn the_stale_reading_is_dropped_before_a_slow_actuation_not_after_it() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs")).unwrap();
    let f = src
        .split("async fn hardware_action")
        .nth(1)
        .expect("hardware_action must exist");
    let body = &f[..f.find("\nasync fn ").unwrap_or(f.len())];

    let dispatch = body
        .find("call_mcp(&base, tool, args)")
        .expect("the tool dispatch");
    let invalidate = body
        .find("d.invalidate_power(")
        .expect("the actuation must invalidate the board's power reading");
    assert!(
        invalidate < dispatch,
        "invalidation must come BEFORE the dispatch: the call itself took 71.9s \
         on hardware, and everything served during it was the state the button \
         was in the middle of changing"
    );
}

/// A stand-in for mcpd that records every tool call dashd makes.
///
/// The dash rig has no mcpd, so the acquire/act/release sequence is otherwise
/// unobservable in-process -- and it is the sequence, not any single call, that
/// was wrong.
struct FakeMcp {
    url: String,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

impl FakeMcp {
    fn start(device: &str) -> Self {
        Self::start_answering(device, Default::default())
    }

    /// As `start`, but answering named tools with a given `structuredContent`
    /// (and `isError` when that content carries an `error`), so a test can put
    /// words in mcpd's mouth and watch what dashd does with them.
    fn start_answering(
        device: &str,
        answers: std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Default::default();
        let seen = calls.clone();
        let dev = device.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(body) = text.split("\r\n\r\n").nth(1) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                        let name = v["params"]["name"].as_str().unwrap_or("?").to_string();
                        let args = v["params"]["arguments"].clone();
                        seen.lock().unwrap().push((name, args));
                    }
                }
                let last = seen.lock().unwrap().last().map(|(n, _)| n.clone());
                let canned = last.and_then(|n| answers.get(&n).cloned());
                let payload = match canned {
                    Some(content) => serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {"isError": content.get("error").is_some(),
                                   "structuredContent": content},
                    }),
                    None => serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {"isError": false,
                                   "structuredContent": {"device": dev, "ok": true}},
                    }),
                }
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        Self { url, calls }
    }

    fn names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn args_of(&self, name: &str) -> Option<serde_json::Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, a)| a.clone())
    }
}

/// FOUND ON HARDWARE while probing the power fix.
///
/// The dashboard steals the lease for a press -- correctly, a human at the bench
/// outranks an agent's reservation -- and then never gave it back. Measured on
/// the rig: one press left `dashboard` holding the IQ10 console with TWELVE
/// MINUTES still on the clock, and `release` refuses without the holder's token,
/// so the next agent to touch that board got `LEASE_HELD` by a browser nobody
/// was sitting at. Every press laid one of these down.
#[test]
fn a_button_press_hands_the_console_back_when_it_is_done() {
    let device = "usb-FTDI_IQ10_UART-SPI_X-if00-port0";
    let mcp = FakeMcp::start(device);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    rig.add_device(device, Some(5001));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    let (code, _body) = rig.post(&format!("/api/power/{device}/off"));
    assert_eq!(code, 200, "the press must reach mcpd");

    let names = mcp.names();
    assert!(
        names.contains(&"acquire".to_string()),
        "a mutating tool needs the lease: {names:?}"
    );
    assert!(
        names.contains(&"release".to_string()),
        "the press is over; the console must be handed back rather than held \
         until the lease expires: {names:?}"
    );
    let acq = names.iter().position(|n| n == "acquire").unwrap();
    let act = names.iter().position(|n| n == "power").expect("the action");
    let rel = names.iter().position(|n| n == "release").unwrap();
    assert!(
        acq < act && act < rel,
        "order must be acquire, act, release: {names:?}"
    );

    // And the lease it takes is bounded, so a crash between act and release
    // cannot strand the board for the default TTL either.
    let ttl = mcp.args_of("acquire").and_then(|a| a["ttl_s"].as_u64());
    assert!(
        ttl.is_some_and(|t| t <= 600),
        "a press-lease must be bounded well under the default TTL, got {ttl:?}"
    );
}

/// The release must happen even when the action FAILS -- a refused or broken
/// press is exactly when a stranded lease is least expected and most annoying.
#[test]
fn a_failed_press_still_hands_the_console_back() {
    let device = "usb-FTDI_IQ10_UART-SPI_Y-if00-port0";
    let mcp = FakeMcp::start(device);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    rig.add_device(device, Some(5001));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });

    // An unknown selector: mcpd would refuse this in reality.
    let _ = rig.post("/api/power/no-such-console/off");
    let names = mcp.names();
    assert!(
        names.contains(&"release".to_string()),
        "even a press that goes nowhere must not leave the lease behind: {names:?}"
    );
}

/// FOUND ON HARDWARE: the same membership rule, written twice, disagreed.
///
/// A controller carries its own tty as its `controller_port`, so a group built
/// by matching that field alone swept the CONTROLLER ROW in with the board's
/// consoles. The periodic sweep excludes controllers, so the value could only
/// ever arrive from a post-action fan-out and then expire: measured on the rig,
/// the IQ10's Bantam row read "off" immediately after a press and was back to
/// unknown 35s later. A row that answers differently depending on how recently
/// somebody pressed a button is not a power indicator.
#[test]
fn a_post_action_update_reaches_exactly_the_rows_the_sweep_would_have() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg();
    c.paths.data_dir = dir.path().to_path_buf();
    let dash = conminer::dash::Dash::new(c, dir.path().to_path_buf());

    let mut board = vec![
        console("usb-IQ10-if00-port0", "bantam", Some("/dev/ttyACM0")),
        console("usb-IQ10-if01-port0", "bantam", Some("/dev/ttyACM0")),
    ];
    // The controller itself, which points at its own tty.
    let mut ctrl = console("usb-Bantam_IQ10-if00", "bantam", Some("/dev/ttyACM0"));
    ctrl.is_controller = true;
    board.push(ctrl);
    // ...and a mined log that happens to resolve to the same controller.
    let mut mined = console("file:/tmp/board-boot.log", "bantam", Some("/dev/ttyACM0"));
    mined.is_file = true;
    board.push(mined);

    let swept: std::collections::BTreeSet<String> = conminer::dash::group_by_controller(&board)
        .into_values()
        .flatten()
        .collect();
    let fanned: std::collections::BTreeSet<String> = dash
        .consoles_sharing_controller_in("usb-IQ10-if00-port0", &board)
        .into_iter()
        .collect();

    assert_eq!(
        fanned, swept,
        "the post-action fan-out and the periodic sweep must agree on which rows \
         belong to a board; where they disagree a value appears after a press \
         and vanishes on the next expiry"
    );
    assert!(
        !fanned.iter().any(|c| c.contains("Bantam")),
        "a controller is not a board it powers: {fanned:?}"
    );
    assert!(
        !fanned.iter().any(|c| c.starts_with("file:")),
        "a mined log is not on any board: {fanned:?}"
    );
}

// ------------------------------------------------------------ §P1 fleet ------

/// A peer's board appears on the page, in its own rack, labelled with the node
/// and the host it lives on.
///
/// The page is where somebody decides to press a power button, so "which host
/// is this?" cannot be an inference. A remote board that renders like a local
/// one is how a person power-cycles the wrong lab.
#[test]
fn a_peers_rack_renders_below_the_local_bench_with_its_host() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_Local-if00-port0", Some(5001));

    // A peer, and one board it owns, exactly as inventory sync would leave them.
    {
        let mut reg = rig.registry();
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &conminer_core::peers::registry::Advert {
                instance_id: "id-alpha".into(),
                name: "alpha".into(),
                version: "0.2.0".into(),
                mcp_url: "http://192.168.10.10:8090/mcp".into(),
                dash_url: "http://192.168.10.10:8080".into(),
                ser2net_host: "192.168.10.10".into(),
                ser2net_ports: vec![],
            },
            conminer_core::peers::registry::PeerSource::Static,
            Some("192.168.10.10"),
            0,
        )
        .unwrap();
        let row = reg
            .upsert_device(
                "peer:alpha//dev/serial/by-id/usb-FTDI_Remote-if00-port0",
                None,
                conminer_core::store::IdentityKind::ById,
                None,
                0,
            )
            .unwrap();
        reg.set_remote_origin(
            row.id,
            "alpha",
            Some("192.168.10.10"),
            "/dev/serial/by-id/usb-FTDI_Remote-if00-port0",
            Some(5001),
        )
        .unwrap();
        reg.assign_port(row.id, 5001).unwrap();
        reg.set_state(row.id, "listening").unwrap();
    }

    let v = rig.until("the remote board", |v| {
        v["devices"]
            .as_array()
            .map(|d| d.iter().any(|x| x["node"] == "alpha"))
            .unwrap_or(false)
    });

    // The API says which node owns it and where that node is.
    let remote = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["node"] == "alpha")
        .expect("the remote row");
    assert_eq!(
        remote["node_host"], "192.168.10.10",
        "a remote asset must carry its host: {remote}"
    );
    // ...and the fleet itself is in the snapshot, for the rack header.
    let peers = v["peers"].as_array().expect("peers in the snapshot");
    assert_eq!(peers.len(), 1, "{v}");
    assert_eq!(peers[0]["node"], "alpha");
    assert_eq!(peers[0]["host"], "192.168.10.10");
    assert_eq!(peers[0]["devices"], 1, "the rack header counts its boards");

    // The page carries the code that renders them: a rack per peer, a host
    // label, and a liveness pill with three states rather than a boolean.
    let (code, body) = rig.get("/");
    assert_eq!(code, 200);
    for needle in ["rack-head", "rack-pill", "rack-sub", "state.peers"] {
        assert!(
            body.contains(needle),
            "the page must render racks: {needle}"
        );
    }
    assert!(
        body.contains("not answering"),
        "advertising and answering are different failures and the page must say which"
    );
}

/// A NODE THAT CALLS IN IS NOT A BROKEN NODE.
///
/// The faceplate had three states, and a node behind one-way connectivity fell
/// into the wrong one: `ok` is "can we reach it", which is false for bravo for
/// ever, so it painted amber -- "advertising but its mcpd is not answering" --
/// beside a rack of boards that power on when you click them. The reverse
/// channel is a fourth, true state.
#[test]
fn a_peer_that_only_calls_in_is_shown_as_reachable_not_as_a_dead_service() {
    let rig = Rig::start(cfg());
    let now = conminer_core::clock::system().now_wall_ms();
    {
        let mut reg = rig.registry();
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &conminer_core::peers::registry::Advert {
                instance_id: "id-bravo".into(),
                name: "bravo".into(),
                version: "testbuild".into(),
                mcp_url: "http://192.168.10.11:8090/mcp".into(),
                dash_url: String::new(),
                ser2net_host: "192.168.10.11".into(),
                ser2net_ports: vec![],
            },
            // Push: it announced itself, which says nothing about the way back.
            conminer_core::peers::registry::PeerSource::Push,
            Some("192.168.10.11"),
            now,
        )
        .unwrap();
    }

    // Before it polls: we cannot reach it and it is not asking for work.
    let v = rig.until("the peer", |v| {
        v["peers"].as_array().is_some_and(|a| a.len() == 1)
    });
    let p = &v["peers"].as_array().expect("peers")[0];
    assert_eq!(p["answering"], false, "{p}");
    assert_eq!(p["reverse"], false, "{p}");

    // Now it asks for work, which is the only proof the reverse path exists.
    {
        let mut reg = rig.registry();
        conminer_core::peers::registry::note_poll(&mut reg, "bravo", now).unwrap();
    }
    let v = rig.until("the reverse channel", |v| {
        v["peers"]
            .as_array()
            .is_some_and(|a| a[0]["reverse"] == true)
    });
    let p = &v["peers"].as_array().expect("peers")[0];
    assert_eq!(
        p["answering"], false,
        "it still cannot be dialled, and saying otherwise would be a lie: {p}"
    );
    assert_eq!(
        p["reverse"], true,
        "…but its boards are driveable, and the rack must say so: {p}"
    );

    // And the page has a lamp state for it, distinct from the amber one.
    let (_, body) = rig.get("/");
    assert!(
        body.contains("node-lamp.relay") && body.contains("peer.reverse"),
        "the faceplate must render the reverse state"
    );
}

/// A PEER'S CONSOLES SHARE ONE CONTROLLER, AND THE POWER SWEEP MUST KNOW IT.
///
/// The sweep probes once per controller INSTANCE and fans the answer out, which
/// is what stopped one board reporting another's power locally. For a peer's
/// board the instance cannot be resolved here at all -- the controller is
/// plugged into somebody else's host -- so every remote console became its own
/// group and its own forwarded `diagnose`. Measured on alpha: eleven probes
/// per five-second sweep, each a round trip, so no reading ever landed inside
/// the thirty-second freshness window and the entire peer rack rendered with no
/// power state. The owner's instance now travels with the row.
#[test]
fn a_peers_consoles_are_probed_once_per_board_not_once_per_console() {
    let rig = Rig::start(cfg());
    {
        let mut reg = rig.registry();
        // Two consoles of one remote board, plus one console of another, all
        // owned by the same peer.
        for (canonical, port) in [
            (
                "peer:alpha//dev/serial/by-id/usb-FTDI_A-if00-port0",
                "/dev/ctl-A",
            ),
            (
                "peer:alpha//dev/serial/by-id/usb-FTDI_A-if01-port0",
                "/dev/ctl-A",
            ),
            (
                "peer:alpha//dev/serial/by-id/usb-FTDI_B-if00-port0",
                "/dev/ctl-B",
            ),
        ] {
            let row = reg
                .upsert_device(canonical, None, IdentityKind::ById, None, 1_000)
                .unwrap();
            reg.set_remote_route(
                row.id,
                "alpha",
                Some("192.168.10.10"),
                canonical,
                Some(5001),
                None,
                1,
            )
            .unwrap();
            reg.set_remote_controls(
                row.id,
                Some(&serde_json::json!({
                    "controller": "bantam",
                    "boot_modes": ["BOOT_MD_EDL"],
                    "controller_port": port,
                    "has_power_hook": true
                })),
            )
            .unwrap();
        }
    }
    // WAIT FOR THE CONTROLS, NOT MERELY THE ROWS. Each row is written, then its
    // owner's controls are written a moment later, and the refresh in between
    // publishes a console whose controller is not known yet -- unprobeable, and
    // correctly left out of the grouping. Counting rows alone let the assertion
    // read that intermediate state and report a grouping bug that is not there.
    let v = rig.until("the peer's boards, with their controls", |v| {
        v["devices"].as_array().is_some_and(|a| {
            a.iter()
                .filter(|d| d["node"] == "alpha" && d["controller_port"].is_string())
                .count()
                == 3
        })
    });
    let devices: Vec<conminer::dash::DashDevice> =
        serde_json::from_value(v["devices"].clone()).expect("devices");
    let groups = conminer::dash::group_by_controller(&devices);
    let remote: Vec<(&String, &Vec<String>)> = groups
        .iter()
        .filter(|(k, _)| k.starts_with("peer:alpha/"))
        .collect();
    assert_eq!(
        remote.len(),
        2,
        "two remote boards must be two probes, not three consoles: {groups:?}"
    );
    let sizes: Vec<usize> = remote.iter().map(|(_, v)| v.len()).collect();
    assert!(
        sizes.contains(&2) && sizes.contains(&1),
        "the two consoles of one board must share a probe: {groups:?}"
    );
}

/// …and a peer's controller can never be confused with a local one at the same
/// path. Two hosts both have `/dev/ttyUSB0`; merging those groups would publish
/// one bench's power state onto another's.
#[test]
fn a_remote_controller_never_merges_with_a_local_one_at_the_same_path() {
    let rig = Rig::start(cfg());
    {
        let mut reg = rig.registry();
        let row = reg
            .upsert_device(
                "peer:alpha//dev/serial/by-id/usb-FTDI_Remote-if00-port0",
                None,
                IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_remote_route(
            row.id,
            "alpha",
            Some("192.168.10.10"),
            "/dev/serial/by-id/usb-FTDI_Remote-if00-port0",
            Some(5001),
            None,
            1,
        )
        .unwrap();
        reg.set_remote_controls(
            row.id,
            Some(&serde_json::json!({
                "controller": "bantam",
                "boot_modes": [],
                // The SAME tty path a local controller would resolve to.
                "controller_port": "/dev/ttyUSB0",
                "has_power_hook": true
            })),
        )
        .unwrap();
    }
    let v = rig.until("the remote board", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["node"] == "alpha"))
    });
    let row = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["node"] == "alpha")
        .unwrap();
    assert_eq!(
        row["controller_port"], "peer:alpha//dev/ttyUSB0",
        "a peer's controller instance must be namespaced by its owner: {row}"
    );
}

/// A LATCHED STRAP MUST BE CLEARABLE FROM THE PAGE THAT SET IT.
///
/// Selecting a boot mode on a Bantam only ARMS it; the strap stays set and every
/// later boot lands there. The page rendered a button for each mode and none for
/// `clear`, so a person could put a board into EDL from the dashboard and then
/// needed a terminal to get it back -- with the board looking broken in between,
/// returning to EDL after every reset. Found by the actuation matrix, which
/// could not complete an EDL sweep through the UI for exactly this reason.
#[test]
fn the_boot_mode_bar_offers_clear_beside_the_modes_that_need_it() {
    let rig = Rig::start(cfg());
    let (code, body) = rig.get("/");
    assert_eq!(code, 200);
    assert!(
        body.contains(r#"[...modes, "clear"]"#),
        "the page must offer `clear` wherever it offers a mode"
    );
    assert!(
        body.contains(r#"m === "clear" ? "Clear" : m"#),
        "…and label it for a person, not as a raw mode name"
    );
}

/// A PEER'S BOARD IS NEVER ATTRIBUTED TO ONE OF OUR CONTROLLERS.
///
/// When the owner does not name a controller for one of its boards, the honest
/// answer here is "none" -- not whatever profile happens to match on THIS host.
/// Measured on alpha the moment the owner's instance started travelling with
/// the row: bravo's CMSIS-DAP board, which bravo says has no controller, resolved
/// against a Bantam plugged into alpha, and the power sweep then published
/// that Bantam's state as the state of a board on another machine.
#[test]
fn a_peers_board_with_no_controller_does_not_borrow_one_of_ours() {
    let rig = Rig::start(cfg());
    // A local controller that a naive resolution would happily match.
    rig.add_device("usb-Microchip_Technology_Inc._Bantam_LOCAL-if00", None);
    {
        let mut reg = rig.registry();
        let row = reg
            .upsert_device(
                "peer:bravo//dev/serial/by-id/usb-MBED_MBED_CMSIS-DAP_XYZ-if01",
                None,
                IdentityKind::ById,
                None,
                1_000,
            )
            .unwrap();
        reg.set_remote_route(
            row.id,
            "bravo",
            Some("192.168.10.11"),
            "/dev/serial/by-id/usb-MBED_MBED_CMSIS-DAP_XYZ-if01",
            Some(5003),
            None,
            1,
        )
        .unwrap();
        // The owner's answer: no controller, no hook, no modes.
        reg.set_remote_controls(
            row.id,
            Some(&serde_json::json!({
                "controller": null, "boot_modes": [],
                "controller_port": null, "has_power_hook": false
            })),
        )
        .unwrap();
    }
    let v = rig.until("the peer's board", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["node"] == "bravo"))
    });
    let row = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["node"] == "bravo")
        .unwrap();
    assert!(
        row["controller_port"].is_null(),
        "a peer's board must not borrow a local controller: {row}"
    );
    assert_eq!(row["controller"], serde_json::Value::Null, "{row}");
    assert_eq!(row["has_power_hook"], false, "{row}");
}

/// LABELS ARE EDITABLE FROM THE API, not only from an agent's tool call.
///
/// Naming is how a bench with thirty consoles stays navigable, and it was
/// reachable only over MCP -- so the person standing at the rack, looking at the
/// page that shows the wrong name, had no way to fix it there.
///
/// This suite runs the dashboard WITHOUT an mcpd behind it, so what it can prove
/// is what the dashboard owns: the routes exist, they carry the selector and the
/// body to the right tool, and a failure to reach mcpd is reported rather than
/// swallowed. The semantics of the change -- bare labels, key=value, removal --
/// are proven against a real registry in the `tools` suite.
#[test]
fn the_label_and_tag_routes_exist_and_report_an_unreachable_mcpd() {
    let mut c = cfg();
    c.dashboard.mcp_url = "http://127.0.0.1:1/mcp".into();
    let rig = Rig::start(c);
    for (path, body) in [
        ("/api/label/usb-a", "bench-1"),
        ("/api/tags/usb-a", r#"{"tags":{"rack":"r2"}}"#),
    ] {
        let (code, out) = rig.post_body(path, body);
        assert_ne!(code, 404, "{path} must be routed: {out}");
        assert_eq!(
            code, 502,
            "an unreachable mcpd must be said out loud: {out}"
        );
        assert!(out.contains("could not reach mcpd"), "{out}");
    }
}

/// A CONTROLLER HAS ITS OWN NAME, and the page must carry it.
///
/// The controller row is deliberately not a card here -- it captures nothing, so
/// it is filtered out with the other portless rows -- yet it is the thing an
/// operator points at when they say "the one on the left", and it is a device an
/// agent can address by that name over MCP. Its panel is therefore the only
/// place on the page where it can be named, and the name it shows has to come
/// from the CONTROLLER's row: the panel is drawn from a representative console,
/// so publishing nothing here leaves the page free to edit the console's name
/// under a heading that says CONTROLLER.
#[test]
fn a_controllers_own_name_and_labels_ride_on_the_panel_its_board_draws() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-Microchip_Bantam_IQ10RRDXX34VG8-if00", None);
    rig.add_device("usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if00-port0", Some(5001));
    {
        let mut reg = rig.registry();
        let ctl = reg
            .resolve("Bantam")
            .expect("the controller row is in the registry even though it is not a card");
        reg.set_nickname(ctl.id, "left-bantam").unwrap();
        reg.set_tags(
            ctl.id,
            &std::collections::BTreeMap::from([("bay".to_string(), "1".to_string())]),
        )
        .unwrap();
    }

    let v = rig.until("the console", |v| {
        v["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["controller_label"] == "left-bantam")
    });
    let console = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["canonical"].as_str().unwrap().contains("FTDI"))
        .expect("the console");
    assert_eq!(
        console["controller_label"], "left-bantam",
        "the panel drawn from this console must show the CONTROLLER's name: {console}"
    );
    assert_eq!(console["controller_tags"]["bay"], "1", "{console}");
    // And it is the controller's name, not this console's: the console is
    // unnamed, so anything the page shows for it comes from the wrong row.
    assert!(
        console["nickname"].is_null(),
        "naming the controller must not have named the console: {console}"
    );
    // The selector the page will edit against is the controller's own path.
    assert!(
        console["controller_port"]
            .as_str()
            .unwrap_or_default()
            .contains("Bantam"),
        "{console}"
    );
}

/// Naming a board must not evict whoever is driving it.
///
/// `hardware_action` STEALS the lease for a button press, on the rule that a
/// person at the bench outranks a reservation. Renaming touches no hardware, so
/// borrowing that path would have an operator tidying labels bump an agent
/// mid-boot -- which is the sort of thing that gets a tool switched off.
#[test]
fn labelling_does_not_touch_the_lease() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs"))
        .expect("dash.rs");
    let meta = src
        .split("async fn meta_action")
        .nth(1)
        .and_then(|t| t.split("\n}").next())
        .expect("meta_action");
    assert!(
        !meta.contains("acquire") && !meta.contains("steal"),
        "labelling must not take or steal a device lease: {meta}"
    );
    assert!(
        !meta.contains("invalidate_power"),
        "…nor disturb the power reading of a board it never touched: {meta}"
    );
}

/// …and the behaviour that guard buys, at the layer where it matters.
///
/// A dashboard built from defaults, asked to power something off, must fail to
/// reach any mcpd at all. This suite runs inside the container that shares a
/// network with the live stack, so if the default ever names that service again,
/// this press goes to real hardware and this test is the thing that notices.
#[test]
fn a_default_dashboard_cannot_reach_a_live_mcpd() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_Fixture_ZZZZ-if00-port0", Some(5001));
    rig.until("the console", |v| {
        v["devices"].as_array().unwrap().len() == 1
    });
    let (code, body) = rig.post("/api/power/usb-FTDI_Fixture_ZZZZ-if00-port0/off");
    assert_eq!(
        code, 502,
        "a test rig must not have an mcpd to talk to: {body}"
    );
    assert!(body.contains("could not reach mcpd"), "{body}");
}

/// A RECONNECTING BROWSER IS SHOWN WHAT IT MISSED.
///
/// The dashboard's own scrollback dies with the attachment, and the attachment
/// dies exactly when the interesting thing happens: a power cycle takes the tty,
/// ser2net drops the port, and whatever the dash was holding goes with it.
/// minerd's store does not -- it keeps capturing -- so the store is the witness
/// for the gap between the board returning and the browser reconnecting.
///
/// Asked for by cursor, so a reconnect gets exactly its own gap rather than a
/// fixed tail somebody has to guess the size of.
#[test]
fn a_reconnecting_viewer_is_replayed_the_gap_from_the_store() {
    let port = FakePort::start();
    let rig = Rig::start(cfg());
    rig.add_device("usb-a", Some(port.port));
    rig.until("the console's endpoint", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["port"].is_number())
    });

    // A first viewer, which is handed its anchor after the greeting.
    let mut first = ws_connect(&rig.base, "usb-a");
    read_text_frame(&mut first).expect("hello");
    let cursor = (0..12)
        .find_map(|_| {
            let f = read_text_frame(&mut first)?;
            serde_json::from_str::<serde_json::Value>(&f)
                .ok()
                .filter(|v| v["type"] == "cursor")
                .and_then(|v| v["cursor"].as_str().map(str::to_string))
        })
        .expect("the anchor for a future reconnect");
    drop(first);

    // The board talks while nobody is watching. minerd is what captures it, so
    // write through the store the way minerd does.
    {
        let reg = Registry::open(rig.data_dir.as_path()).unwrap();
        let row = reg.resolve("usb-a").unwrap();
        let path = rig.data_dir.join(&row.db_file);
        let mut st = conminer_core::store::DeviceStore::open(&path, &row.canonical, true).unwrap();
        let session = st
            .begin_session(
                conminer_core::store::SessionSource::Live,
                1_000,
                Some("minerd"),
                None,
                None,
            )
            .unwrap();
        st.append_lines(
            session,
            None,
            &[conminer_core::store::PendingLine {
                bytes: b"MISSED-WHILE-AWAY: U-Boot 2024.01",
                terminator: conminer_core::linesplit::Terminator::Lf,
                truncated: false,
                continuation: false,
                ts_mono: 1_000,
                ts_wall: 1_000,
                stage_id: None,
            }],
        )
        .unwrap();
    }

    // Reconnect asking for the gap.
    let mut back = ws_connect_query(&rig.base, "usb-a", &format!("since={cursor}"));
    let greeting = read_text_frame(&mut back).expect("a first frame");
    assert!(
        greeting.contains("\"hello\""),
        "the reconnect was refused before it could be replayed: {greeting}"
    );
    // READ EVERY FRAME, WHATEVER ITS KIND. `read_text_frame` skips binary frames
    // by consuming them, so scanning for the announcement with it would swallow
    // the very bytes this test is about -- which is exactly what it did.
    let mut said_replay = false;
    let mut saw_the_line = false;
    for _ in 0..6 {
        let Some((op, payload)) = read_frame(&mut back) else {
            break;
        };
        match op {
            1 => {
                let t = String::from_utf8_lossy(&payload).to_string();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if v["type"] == "replay" {
                        said_replay = true;
                        assert!(v["lines"].as_i64().unwrap_or(0) >= 1, "{v}");
                    }
                }
            }
            2 if String::from_utf8_lossy(&payload).contains("MISSED-WHILE-AWAY") => {
                saw_the_line = true;
                break;
            }
            _ => {}
        }
    }
    assert!(
        said_replay,
        "a reconnect with a cursor must be told it is a replay"
    );
    assert!(
        saw_the_line,
        "...and must actually be shown what it missed, from the store"
    );
}

/// THE FEED MUST NAME THE NODE IT CAME FROM.
///
/// A watcher polls this across three nodes, so the label is how it tells them
/// apart. It read the raw config field, which is empty whenever the name lives
/// in the persisted identity -- which is this whole fleet -- so every node
/// labelled itself "". Caught by looking at the live endpoint after a deploy,
/// not by any test, which is why this one exists.
#[test]
fn the_reports_feed_names_the_node_even_when_the_name_lives_in_the_identity() {
    // No name in the config at all: exactly the deployed shape.
    let mut config = cfg();
    config.peers.name = String::new();
    let rig = Rig::start(config);

    let (code, body) = rig.get("/api/reports");
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let node = v["node"].as_str().unwrap_or_default();
    assert!(
        !node.is_empty(),
        "a feed aggregated across nodes cannot label them all \"\": {v}"
    );
    // And it is the SAME name the rest of the dashboard reports, or the watcher
    // cannot line the two up.
    let (_, devices) = rig.get("/api/devices");
    let d: serde_json::Value = serde_json::from_str(&devices).unwrap();
    assert_eq!(
        node,
        d["node"].as_str().unwrap_or_default(),
        "the feed and the device list must agree about who this is"
    );
}

/// §R. THE TRIAGE FEED. A queue nobody reads is worse than pasting messages by
/// hand, because it feels like progress. This is the endpoint a watcher polls
/// every run, so it has to answer even when nothing has been filed, and it has
/// to put regressions where they cannot be missed.
#[test]
fn the_reports_feed_serves_the_open_queue_and_flags_regressions() {
    let rig = Rig::start(cfg());

    // Empty is a valid answer and must not be an error.
    let (code, body) = rig.get("/api/reports");
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["open"], 0);
    assert_eq!(v["regressions"], 0);

    // One open report, and one that came back on the build that fixed it.
    {
        use conminer_core::reports::{self as rep, NewReport};
        let mut reg = rig.registry();
        rep::file(
            &mut reg,
            &NewReport {
                title: "power off hangs with no response".into(),
                build: Some("build-1".into()),
                ..Default::default()
            },
            1_000,
        )
        .unwrap();
        let (came_back, _) = rep::file(
            &mut reg,
            &NewReport {
                title: "edl verdict blames ser2net".into(),
                build: Some("build-1".into()),
                ..Default::default()
            },
            1_000,
        )
        .unwrap();
        rep::resolve(
            &mut reg,
            came_back.id,
            "fixed",
            Some("build-2"),
            None,
            None,
            2_000,
        )
        .unwrap();
        let (_, outcome) = rep::file(
            &mut reg,
            &NewReport {
                title: "edl verdict blames ser2net".into(),
                build: Some("build-2".into()),
                ..Default::default()
            },
            3_000,
        )
        .unwrap();
        assert_eq!(
            outcome,
            rep::Filed::Regression,
            "fixture premise: the second filing must be a regression"
        );
    }

    let (code, body) = rig.get("/api/reports");
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["open"], 2, "both are open: {v}");
    assert_eq!(
        v["regressions"], 1,
        "the one that came back on the build that claimed to fix it: {v}"
    );
    let titles: Vec<&str> = v["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap_or_default())
        .collect();
    assert!(titles.iter().any(|t| t.contains("power off")), "{titles:?}");
    assert!(
        v["reports"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["distinct_reporters"].as_i64().unwrap_or(0) >= 1),
        "each row carries who hit it: {v}"
    );
}

/// THE PEER-CONSOLE-BLANK REGRESSION, in one test: a viewer watching a REMOTE
/// (peer) board in the web UI must receive the owner's relayed bytes.
///
/// It did not. `attach()` always preferred the broker for RX, and
/// `broker::subscribe` makes a topic on demand -- so for a peer canonical, which
/// THIS node's minerd never captures or publishes, the subscription SUCCEEDED
/// and then delivered nothing forever, while the ser2net relay socket that
/// actually carried the owner's bytes was drained and discarded. Every proxied
/// tool worked; the web console was blank. Found on alpha watching bravo's IQ8.
///
/// Two independent assertions, either of which fails on the old code: the
/// relayed bytes reach the viewer, AND the dashboard never subscribed the peer
/// canonical to the broker.
#[test]
fn a_remote_console_reads_its_relay_not_the_empty_broker() {
    let broker = FakeBroker::start();
    let port = FakePort::start(); // stands in for this node's local re-export of the owner's console
    let rig = rig_with_broker(&broker);
    let canonical = "peer-bravo-iq8-relay";
    rig.add_device(canonical, Some(port.port));
    // Mark it owned by a peer: its bytes arrive on the relay socket, and this
    // node's broker never publishes it.
    {
        let mut reg = rig.registry();
        let id = reg.device_by_canonical(canonical).unwrap().unwrap().id;
        reg.set_remote_origin(
            id,
            "bravo",
            Some("192.0.2.1"),
            "usb-owner-canonical",
            Some(5001),
        )
        .unwrap();
    }
    rig.until("the remote console", |v| {
        v["devices"]
            .as_array()
            .map(|a| a.iter().any(|d| d["node"] == "bravo"))
            == Some(true)
    });

    let mut ws = ws_connect(&rig.base, canonical);
    wait_for_dial(&port);

    // The relay pushes the owner's bytes, exactly as alpha's ser2net relay of
    // bravo's port would.
    port.say
        .send(b"c0[     1.330252s]XBL_LDR BOOT SUCCESS\r\n".to_vec())
        .unwrap();
    assert!(
        wait_for_frame(&mut ws, Duration::from_secs(5), |p| {
            String::from_utf8_lossy(p).contains("XBL_LDR BOOT SUCCESS")
        }),
        "a remote console's relayed bytes must reach the viewer -- on the old code the relay \
         socket was drained while an empty broker subscription was read instead"
    );

    // The discriminator: a remote console must NOT be sourced from the broker.
    // On the old code the dashboard subscribed the peer canonical (which nothing
    // ever publishes) and the count here would be > 0.
    assert_eq!(
        broker.hub.subscriber_count(canonical),
        0,
        "a remote console must read its relay directly, never subscribe the broker"
    );
}

/// A peer's board is ONE chassis, headed by the name its owner gives it.
///
/// `topology_group` needs `by_path`, the cable's position on THIS host, and a
/// row held on a peer's behalf has none. So the adapter key fell through to
/// `adapter_of`, a textual split at `-ifNN` of the peer id, and two things broke
/// at once: the chassis was headed by a raw by-id path, and a board whose ports
/// live on two FTDI chips split into two chassis, because only the topology
/// knows they are one board.
///
/// The owner drew the board as one chassis of seven named ports, while its peer
/// drew the SAME hardware as two chassis, one per FTDI chip, with four ports
/// and two, and no controller name anywhere. The owner already says which
/// controller INSTANCE drives each
/// console, and one controller is one board.
#[test]
fn a_peers_two_chip_board_is_one_chassis_named_by_its_owner() {
    const CTL: &str = "/dev/serial/by-id/usb-Microchip_Bantam_CTRL0001-if00";
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_Local-if00-port0", Some(5001));
    {
        let mut reg = rig.registry();
        conminer_core::peers::registry::upsert_advert(
            &mut reg,
            &conminer_core::peers::registry::Advert {
                instance_id: "id-alpha".into(),
                name: "alpha".into(),
                version: "0.2.0".into(),
                mcp_url: "http://192.168.10.10:8090/mcp".into(),
                dash_url: "http://192.168.10.10:8080".into(),
                ser2net_host: "192.168.10.10".into(),
                ser2net_ports: vec![],
            },
            conminer_core::peers::registry::PeerSource::Static,
            Some("192.168.10.10"),
            0,
        )
        .unwrap();
        // One board, two chips, one controller.
        for (i, remote) in [
            "/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if00-port0",
            "/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if01-port0",
            "/dev/serial/by-id/usb-FTDI_RIDE_SPI_BBBB-if00-port0",
        ]
        .iter()
        .enumerate()
        {
            let row = reg
                .upsert_device(
                    &format!("peer:alpha/{remote}"),
                    None,
                    conminer_core::store::IdentityKind::ById,
                    None,
                    0,
                )
                .unwrap();
            reg.set_remote_origin(row.id, "alpha", Some("192.168.10.10"), remote, Some(5001))
                .unwrap();
            reg.assign_port(row.id, 5010 + i as u16).unwrap();
            reg.set_state(row.id, "listening").unwrap();
            // What the owner said about driving it, carried verbatim.
            reg.set_remote_controls(
                row.id,
                Some(&serde_json::json!({
                    "controller": "bantam",
                    "controller_port": CTL,
                    "controller_label": "BOARD-A",
                    "boot_modes": [],
                    "has_power_hook": true,
                })),
            )
            .unwrap();
        }
    }

    let v = rig.until("the remote board", |v| {
        v["devices"]
            .as_array()
            .map(|d| d.iter().any(|x| x["node"] == "alpha"))
            .unwrap_or(false)
    });
    let remote: Vec<&serde_json::Value> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["node"] == "alpha")
        .collect();
    assert_eq!(
        remote.len(),
        3,
        "the fixture must have all three ports: {v}"
    );

    let keys: std::collections::BTreeSet<&str> = remote
        .iter()
        .filter_map(|x| x["adapter"].as_str())
        .collect();
    assert_eq!(
        keys.len(),
        1,
        "one board is one chassis: two chips must not split into two groups, got {keys:?}"
    );
    let key = keys.iter().next().unwrap();
    assert!(
        key.contains(CTL),
        "and the group is the controller INSTANCE the owner named: {key:?}"
    );

    for x in &remote {
        assert_eq!(
            x["adapter_label"], "alpha/BOARD-A",
            "the chassis is headed by the owner's name for its controller: {x}"
        );
        assert_eq!(
            x["controller_label"], "BOARD-A",
            "and the controller panel says what the owner calls it: {x}"
        );
    }
}

/// The local bench must render exactly as it did before.
///
/// The label override exists only for a row whose key is somebody else's
/// controller path. A local chassis prettifies its own key, and setting a label
/// for it would put a second name on the thing the page already names.
#[test]
fn a_local_chassis_carries_no_label_override() {
    let rig = Rig::start(cfg());
    rig.add_device("usb-FTDI_Quad_UART-SPI_SN000002-if00-port0", Some(5001));
    let v = rig.until("the local board", |v| {
        v["devices"]
            .as_array()
            .map(|d| !d.is_empty())
            .unwrap_or(false)
    });
    for d in v["devices"].as_array().unwrap() {
        assert!(
            d["adapter_label"].is_null(),
            "a local chassis names itself from its own key: {d}"
        );
    }
}

// ------------------------------------------------------- boot overrides ---

/// One board: two consoles and the Bantam that drives them, on one USB branch,
/// so the default controller profile binds and the sweep has a board to ask about.
fn a_bantam_board(rig: &Rig) -> (&'static str, &'static str) {
    const AP: &str = "/dev/serial/by-id/usb-FTDI_OVR_Board_AAAA-if00-port0";
    const SM: &str = "/dev/serial/by-id/usb-FTDI_OVR_Board_AAAA-if01-port0";
    rig.add_device_at(AP, Some("pci-0000:00:14.0-usb-0:6.1.1:1.0"), Some(5031));
    rig.add_device_at(SM, Some("pci-0000:00:14.0-usb-0:6.1.2:1.0"), Some(5032));
    rig.add_device_at(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_OVRBOARD-if00",
        Some("pci-0000:00:14.0-usb-0:6.1.3:1.0"),
        None,
    );
    (AP, SM)
}

fn held_md_edl() -> serde_json::Value {
    serde_json::json!({
        "supported": true, "state": "latched",
        "overrides": {"MD_EDL": 1, "SS_EDL": 0, "UEFI": 0, "FASTBOOT_MD": 0},
        "asserted": ["MD_EDL"], "unknown": [],
        "effect": "the controller is HOLDING MD_EDL",
        "read_at_ms": 1_700_000_000_000_i64, "age_ms": 1234,
        "source": "controller_read",
    })
}

/// The status refresh is strictly read-only, and it is how the page learns what
/// a controller is holding.
///
/// Asserted on what dashd SENDS: across several sweeps of a board whose
/// controller holds MD_EDL, every call to mcpd is `diagnose`, which takes no
/// lease and actuates nothing. Anything else here (an acquire, a boot_mode, a
/// power) would mean looking at the page can change a board, and a status path
/// that could release an override would knock a board out of the EDL a flash is
/// relying on from a page reload.
#[test]
fn the_status_sweep_reads_what_is_held_and_never_sends_anything_else() {
    let mut answers = std::collections::HashMap::new();
    answers.insert(
        "diagnose".to_string(),
        serde_json::json!({"power": "on", "edl": false, "boot_overrides": held_md_edl()}),
    );
    let mcp = FakeMcp::start_answering("unused", answers);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    let (ap, sm) = a_bantam_board(&rig);

    let v = rig.until("the sweep to publish what the controller holds", |v| {
        v["devices"].as_array().is_some_and(|a| {
            a.iter()
                .any(|d| d["canonical"] == ap && d["boot_overrides"]["state"] == "latched")
        })
    });
    let row = |name: &str| {
        v["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["canonical"] == name)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(
        row(ap)["boot_overrides"]["asserted"],
        serde_json::json!(["MD_EDL"])
    );
    assert_eq!(
        row(sm)["boot_overrides"]["asserted"],
        serde_json::json!(["MD_EDL"]),
        "the overrides belong to the BOARD: its other console must show them too: {v}"
    );
    // And the controller's own row. On a local bench the panel that shows this is
    // drawn from that row, not from a console: filed per console, the reading
    // reached every row except the one the panel reads, and it said "not read
    // yet" for ever beside consoles that knew the answer.
    let ctl = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_OVRBOARD-if00";
    assert_eq!(
        row(ctl)["is_controller"],
        true,
        "precondition: the fixture's controller row is on the page: {v}"
    );
    assert_eq!(
        row(ctl)["boot_overrides"]["asserted"],
        serde_json::json!(["MD_EDL"]),
        "the controller's own row is what the panel is drawn from: {v}"
    );
    assert!(
        row(ap)["boot_overrides"].get("age_ms").is_none(),
        "mcpd's per-reply age must not reach the snapshot, or every sweep reads as a change"
    );
    // The observation stays its own field: nothing about EDL was merged in.
    assert_eq!(row(ap)["power"], "on", "{v}");

    // Let a couple more sweeps go by, then look at everything dashd ever sent.
    std::thread::sleep(Duration::from_secs(11));
    let names = mcp.names();
    assert!(
        names.len() >= 2,
        "precondition: the sweep really ran: {names:?}"
    );
    assert!(
        names.iter().all(|n| n == "diagnose"),
        "a status refresh sent mcpd something other than a read: {names:?}"
    );
}

/// A press shows what the controller holds NOW, without waiting for the sweep.
///
/// `boot_mode` reads the controller back after acting and says so in its reply.
/// Leaving the page on the pre-press reading until the next window looks exactly
/// like the press not working.
#[test]
fn a_boot_mode_press_publishes_its_own_readback_at_once() {
    let mut answers = std::collections::HashMap::new();
    // The sweep sees nothing held...
    answers.insert(
        "diagnose".to_string(),
        serde_json::json!({"power": "on", "boot_overrides": {
            "supported": true, "state": "clear", "overrides": {}, "asserted": [], "unknown": [],
            "read_at_ms": 1_700_000_000_000_i64}}),
    );
    // ...and the press reports the line it just asserted.
    answers.insert(
        "boot_mode".to_string(),
        serde_json::json!({"mode": "BOOT_MD_EDL", "boot_overrides": held_md_edl()}),
    );
    let mcp = FakeMcp::start_answering("unused", answers);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    let (ap, sm) = a_bantam_board(&rig);
    rig.until("the board", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["canonical"] == ap))
    });

    let enc = ap.replace('/', "%2F");
    let (code, _) = rig.post(&format!("/api/boot_mode/{enc}/BOOT_MD_EDL"));
    assert_eq!(code, 200);
    let v = rig.until("the press's readback on BOTH consoles", |v| {
        v["devices"].as_array().is_some_and(|a| {
            [ap, sm].iter().all(|n| {
                a.iter()
                    .any(|d| d["canonical"] == *n && d["boot_overrides"]["state"] == "latched")
            })
        })
    });
    assert!(v["devices"].is_array());
}

/// The Normal boot press is ONE mcpd actuation, under the usual press lease.
///
/// The sequencing (release, verify, cycle, abort before cycling) lives in mcpd
/// under a single claim and is gated there. What this holds is that the page
/// cannot turn it back into separate presses with a gap between them.
#[test]
fn a_normal_boot_press_is_one_actuation_not_a_clear_and_a_cycle() {
    let mut answers = std::collections::HashMap::new();
    answers.insert(
        "normal_boot".to_string(),
        serde_json::json!({"device": "x", "boot_id": 7, "normal_boot": {"boot_overrides": {
            "supported": true, "state": "clear", "overrides": {}, "asserted": [], "unknown": [],
            "read_at_ms": 1_700_000_000_000_i64}}}),
    );
    let mcp = FakeMcp::start_answering("unused", answers);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    let (ap, _) = a_bantam_board(&rig);
    rig.until("the board", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["canonical"] == ap))
    });

    let enc = ap.replace('/', "%2F");
    let (code, body) = rig.post(&format!("/api/normal_boot/{enc}"));
    assert_eq!(code, 200, "{body}");
    let acts: Vec<String> = mcp
        .names()
        .into_iter()
        .filter(|n| n != "diagnose")
        .collect();
    assert_eq!(
        acts,
        vec!["acquire", "normal_boot", "release"],
        "one actuation between the lease and its return, and no boot_mode or power beside it"
    );
}

/// An ABORTED normal boot is reported as a failure, and what is still held is
/// on the page at once.
#[test]
fn an_aborted_normal_boot_fails_loudly_and_shows_what_is_still_held() {
    let mut answers = std::collections::HashMap::new();
    answers.insert(
        "normal_boot".to_string(),
        serde_json::json!({"error": {
            "code": "NORMAL_BOOT_ABORTED",
            "message": "normal boot stopped at `verify_overrides`: ... Power was NOT cycled",
            "detail": {"step": "verify_overrides", "power_cycled": false,
                       "boot_overrides": held_md_edl()}}}),
    );
    let mcp = FakeMcp::start_answering("unused", answers);
    let mut c = cfg();
    c.dashboard.allow_power = true;
    c.dashboard.mcp_url = mcp.url.clone();
    let rig = Rig::start(c);
    let (ap, _) = a_bantam_board(&rig);
    rig.until("the board", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["canonical"] == ap))
    });

    let enc = ap.replace('/', "%2F");
    let (code, body) = rig.post(&format!("/api/normal_boot/{enc}"));
    assert_eq!(
        code, 400,
        "an abort is a failed press, not a quiet success: {body}"
    );
    assert!(
        body.contains("NOT cycled"),
        "and the page is told why: {body}"
    );
    rig.until("what is still held", |v| {
        v["devices"].as_array().is_some_and(|a| {
            a.iter()
                .any(|d| d["canonical"] == ap && d["boot_overrides"]["state"] == "latched")
        })
    });
    assert!(
        mcp.names().contains(&"release".to_string()),
        "a failed press still hands the console back"
    );
}
