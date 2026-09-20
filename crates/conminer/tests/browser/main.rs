//! Dashboard tests that run the page in a REAL browser.
//!
//! Everything else about the dashboard is tested server-side: the routes answer,
//! and the controls appear in the served HTML. Neither proves the page WORKS --
//! that its JavaScript parses, builds the chassis, wires a click, and sends the
//! request. A syntax error in the page passes every server-side test and breaks
//! the dashboard completely, which has happened here before (a `function open()`
//! shadowing `window.open` sent every console to `/ws/console/undefined`).
//!
//! Chromium headless runs the page for real. Skipped (not failed) where no
//! browser is installed, so a bare checkout still gets a green suite -- but the
//! dev image ships one, so CI and `./cm test` do run it.

use conminer_core::config::Config;
use conminer_core::store::{IdentityKind, Registry};
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

/// A fake ser2net port: accepts, records what the browser sent, and can print.
///
/// The browser suite had no such thing, and that is precisely the hole this
/// exists to close -- every console gate here asserted about the PAGE, so
/// keystrokes reaching the wire were never covered end to end.
struct FakePort {
    port: u16,
    received: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl FakePort {
    fn start() -> Self {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
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
        Self { port, received }
    }

    fn next_received(&self, within: Duration) -> Option<Vec<u8>> {
        self.received.recv_timeout(within).ok()
    }
}

/// A dashboard serving real devices, for the browser to load.
struct Rig {
    base: String,
    _dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    _stop: tokio::sync::watch::Sender<bool>,
}

impl Rig {
    /// A board whose consoles AND controller share one USB branch, so the page
    /// resolves a power hook and renders the controls.
    ///
    /// Without the controller the power bar is correctly hidden -- conminer
    /// refuses to offer a button it cannot aim, after a bug where pressing one
    /// board's power actuated another. So a click test has to build a board that
    /// really owns its controller, not just a console.
    fn start_with_board() -> Self {
        Self::start_at(&[
            (
                "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if00-port0",
                "pci-0000:00:14.0-usb-0:7.1.1:1.0",
            ),
            (
                "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if01-port0",
                "pci-0000:00:14.0-usb-0:7.1.2:1.0",
            ),
            (
                "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_CLICKCTRL-if00",
                "pci-0000:00:14.0-usb-0:7.1.3:1.0",
            ),
        ])
    }

    /// A board owned by ANOTHER node: consoles only, because a controller is
    /// never re-exported (it has no endpoint to serve).
    fn start_with_peer_board() -> Self {
        let rig = Self::start_at(&[(
            "/dev/serial/by-id/usb-FTDI_Local_AAAA-if00-port0",
            "pci-0000:00:14.0-usb-0:1.1:1.0",
        )]);
        {
            let mut reg = Registry::open(rig._dir.path()).unwrap();
            for (i, ifc) in ["if00", "if01"].iter().enumerate() {
                let canonical = format!("peer:alpha//dev/serial/by-id/usb-FTDI_Far-{ifc}-port0");
                let row = reg
                    .upsert_device(&canonical, None, IdentityKind::ById, None, 1_000)
                    .unwrap();
                reg.set_remote_route(
                    row.id,
                    "alpha",
                    Some("192.168.10.10"),
                    &canonical,
                    Some(5001),
                    None,
                    1,
                )
                .unwrap();
                reg.assign_port(row.id, 6200 + i as u16).unwrap();
                reg.set_remote_controls(
                    row.id,
                    Some(&serde_json::json!({
                        "controller": "bantam",
                        // THE REAL MODE LIST, because its LENGTH is load-bearing.
                        // A fixture with one short mode is a fixture that cannot
                        // fail a layout test: the live IQ10 offers five, and
                        // "SS_MD_FASTBOOT" is what turns a tidy two-column grid
                        // into a page that pans sideways on a phone.
                        "boot_modes": [
                            "BOOT_MD_EDL", "BOOT_SS_EDL", "BOOT_UEFI",
                            "MD_FASTBOOT", "SS_MD_FASTBOOT"
                        ],
                        // On the OWNER's host. No row for it exists here.
                        //
                        // THE REAL LENGTH, deliberately: this string is rendered
                        // as the panel's identity line, and a by-id path is one
                        // unbreakable 60-character token. The short stand-in this
                        // used to carry is why the phone gate passed while the
                        // live bench was 657px wide in a 390px viewport.
                        "controller_port":
                            "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_KARUSSELLXXBANTAMTDC00002MZ4N-if00",
                        "has_power_hook": true
                    })),
                )
                .unwrap();
            }
            // A BOARD ADDRESSED BY PATH, not by a by-id name.
            //
            // Its card is titled with the whole canonical -- there is no port
            // number to name it by -- so it carries the longest unbreakable
            // string this page ever renders. On the live bench that is what laid
            // a 390px viewport out at 657px; a fixture without one cannot catch
            // it, and this gate passed for hours while the bench was broken.
            //
            // It used to be marked `gone`, and CANNOT be any more: the page now
            // shows only hardware that is on the bus, so a gone row renders
            // nothing at all and this gate would have quietly lost its
            // worst-case string -- the exact way a layout gate stops testing
            // anything. It is a connected board with a long name instead, which
            // is what the live bench has: bravo's by-path consoles are plugged in.
            let by_path_named = "peer:bravo//dev/serial/by-path/pci-0000:04:00.3-usb-0:4:1.2-port0";
            let row = reg
                .upsert_device(by_path_named, None, IdentityKind::ById, None, 1_000)
                .unwrap();
            reg.set_remote_route(
                row.id,
                "bravo",
                Some("192.168.10.12"),
                by_path_named,
                Some(5001),
                None,
                1,
            )
            .unwrap();
        }
        // WAIT FOR THE PAGE TO BE ABLE TO SEE THEM.
        //
        // These rows are written after the server is up, so they arrive on the
        // dashboard's next background refresh -- and a browser that loads faster
        // than that interval renders a page with only the local console on it.
        // The gate then reports a peer board missing from a page that shows it
        // correctly a moment later. Measured: this failed in 1.2s with a warm
        // chromium and passed with a cold one, which is the worst kind of gate.
        rig.wait_for_devices(4);
        rig
    }

    /// Block until the API's own view satisfies `f`.
    ///
    /// Anything written to the registry after the server started arrives on its
    /// next background refresh, and a browser loads faster than that: assert
    /// before this and the gate describes a page that is merely early.
    fn wait_for(&self, what: &str, f: impl Fn(&serde_json::Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let out = Command::new("curl")
                .args([
                    "-s",
                    "--max-time",
                    "2",
                    &format!("{}/api/devices", self.base),
                ])
                .output();
            if let Ok(o) = out {
                if let Ok(v) =
                    serde_json::from_slice::<serde_json::Value>(&o.stdout).map_err(|_| ())
                {
                    if f(&v) {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the dashboard never published {what}");
    }

    /// Block until the API publishes `n` devices, so a render cannot outrun the
    /// refresh that puts them there.
    fn wait_for_devices(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen = 0;
        while Instant::now() < deadline {
            let out = Command::new("curl")
                .args([
                    "-s",
                    "--max-time",
                    "2",
                    &format!("{}/api/devices", self.base),
                ])
                .output();
            if let Ok(o) = out {
                let body = String::from_utf8_lossy(&o.stdout);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    seen = v["devices"].as_array().map(Vec::len).unwrap_or(0);
                    if seen >= n {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the dashboard never published {n} devices (saw {seen})");
    }

    fn start_with_devices(devices: &[&str]) -> Self {
        let pairs: Vec<(&str, &str)> = devices.iter().map(|d| (*d, "")).collect();
        Self::start_at(&pairs)
    }

    /// One console wired to a real listener, so what the page sends can be seen.
    fn start_with_console_on(canonical: &str, port: u16) -> Self {
        // Registered WITHOUT a port, so the fake listener's port is the one the
        // row ends up with: `assign_port` hands back the first free port at or
        // above what is asked, and a row that already had one keeps it.
        let rig = Self::start_at_unported(&[(canonical, "pci-0000:00:14.0-usb-0:7.1.1:1.0")]);
        {
            let mut reg = Registry::open(rig._dir.path()).unwrap();
            let row = reg.resolve(canonical).unwrap();
            let got = reg.assign_port(row.id, port).unwrap();
            assert_eq!(got, port, "the fake port must be the one assigned");
        }
        rig.wait_for("the console on its own port", |v| {
            v["devices"]
                .as_array()
                .is_some_and(|a| a.iter().any(|d| d["port"].as_u64() == Some(port as u64)))
        });
        rig
    }

    fn start_at(devices: &[(&str, &str)]) -> Self {
        Self::start_inner(devices, true)
    }

    /// A chassis whose CONTROLLER IS THE CONSOLE: one device that both powers
    /// the board and serves its UART. That is exactly what a Bughopper is, and
    /// it is the case the summary used to undercount as "0 ports".
    fn start_with_controller_console(canonical: &str) -> Self {
        Self::start_inner_with(
            &[(canonical, "pci-0000:00:14.0-usb-0:9.1.1:1.0")],
            true,
            true,
        )
    }

    fn start_at_unported(devices: &[(&str, &str)]) -> Self {
        Self::start_inner(devices, false)
    }

    /// A board held on a PEER's behalf: no topology of our own, and the owner's
    /// answer about how it is driven carried verbatim, exactly as inventory
    /// sync leaves it.
    fn start_peer_board(
        node: &str,
        controller: &str,
        label: Option<&str>,
        remotes: &[&str],
    ) -> Self {
        let rig = Self::start_inner(&[], true);
        {
            let mut reg = Registry::open(rig._dir.path()).unwrap();
            conminer_core::peers::registry::upsert_advert(
                &mut reg,
                &conminer_core::peers::registry::Advert {
                    instance_id: format!("id-{node}"),
                    name: node.into(),
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
            let mut controls = serde_json::json!({
                "controller": "bantam",
                "controller_port": controller,
                "boot_modes": [],
                "has_power_hook": true,
            });
            if let Some(l) = label {
                controls["controller_label"] = serde_json::json!(l);
            }
            for (i, remote) in remotes.iter().enumerate() {
                let row = reg
                    .upsert_device(
                        &format!("peer:{node}/{remote}"),
                        None,
                        IdentityKind::ById,
                        None,
                        1_000,
                    )
                    .unwrap();
                reg.set_remote_origin(row.id, node, Some("192.168.10.10"), remote, Some(5001))
                    .unwrap();
                reg.assign_port(row.id, 6200 + i as u16).unwrap();
                reg.set_state(row.id, "listening").unwrap();
                reg.set_remote_controls(row.id, Some(&controls)).unwrap();
            }
        }
        rig
    }

    /// Pull the cable, the way discovery records it.
    fn unplug(&self, canonical: &str) {
        let mut reg = Registry::open(self._dir.path()).unwrap();
        let row = reg.resolve(canonical).unwrap();
        reg.set_state(row.id, "gone").unwrap();
    }

    /// Poll the device API until it says what the test is waiting for.
    fn until_api(&self, what: &str, f: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = String::new();
        while Instant::now() < deadline {
            let out = Command::new("curl")
                .args([
                    "-s",
                    "--max-time",
                    "2",
                    &format!("{}/api/devices", self.base),
                ])
                .output();
            if let Ok(o) = out {
                last = String::from_utf8_lossy(&o.stdout).to_string();
                if f(&last) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}: {last}");
    }

    /// Publish capture health the way minerd does, so a test can ask the page
    /// what it does with it.
    fn publish_capture(&self, canonical: &str, state: conminer_core::live::CaptureState) {
        let mut reg = Registry::open(self._dir.path()).unwrap();
        let row = reg.resolve(canonical).unwrap();
        conminer_core::live::publish_capture_state(&mut reg, row.id, state).unwrap();
    }

    fn start_inner(devices: &[(&str, &str)], assign_ports: bool) -> Self {
        Self::start_inner_with(devices, assign_ports, false)
    }

    fn start_inner_with(
        devices: &[(&str, &str)],
        assign_ports: bool,
        port_controllers: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        // Dial the fake port on loopback, as the dash suite does.
        config.ser2net.bind = "127.0.0.1".into();
        config.paths.data_dir = dir.path().to_path_buf();

        // Register before serving, so the first page load already has content.
        {
            let mut reg = Registry::open(dir.path()).unwrap();
            for (i, (d, path)) in devices.iter().enumerate() {
                let by_path = (!path.is_empty()).then_some(*path);
                let row = reg
                    .upsert_device(d, by_path, IdentityKind::ById, None, 1_000)
                    .unwrap();
                // The controller is a command processor, not a console; giving it
                // a port would be wrong and it is excluded from discovery anyway.
                if assign_ports && (port_controllers || !d.contains("Bantam")) {
                    let _ = reg.assign_port(row.id, 6100 + i as u16);
                }
            }
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        config.dashboard.bind = addr.to_string();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (stop, rx) = tokio::sync::watch::channel(false);
        let bind = config.dashboard.bind.clone();
        let data = dir.path().to_path_buf();
        rt.spawn(async move {
            let dash = conminer::dash::Dash::new(config, data);
            let _ = dash.refresh();
            let _ = conminer::dash::serve(dash, &bind, rx).await;
        });

        let base = format!("http://{addr}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let probe = Command::new("curl")
                .args(["-s", "--max-time", "1", &format!("{base}/healthz")])
                .output();
            if probe.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("status")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Self {
            base,
            _dir: dir,
            _rt: rt,
            _stop: stop,
        }
    }
}

fn chromium() -> Option<String> {
    cdp::chromium_bin()
}

/// Drop `<script>` blocks: the page's own source is part of the DOM, and
/// matching against it tests the source rather than the render.
fn strip_scripts(dom: &str) -> String {
    // BOTH script AND style. The page inlines its CSS, and that CSS names every
    // class it can style -- so asserting a class name against the raw dump
    // passes whether or not the element was ever built. Measured: a faceplate
    // assertion went on passing with the faceplate disabled, because
    // `.node-head { … }` was still in the stylesheet.
    fn drop_blocks(input: &str, open: &str, close: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(i) = rest.find(open) {
            out.push_str(&rest[..i]);
            match rest[i..].find(close) {
                Some(j) => rest = &rest[i + j + close.len()..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }
    drop_blocks(
        &drop_blocks(dom, "<script", "</script>"),
        "<style",
        "</style>",
    )
}

/// Render the page, RETRYING until it has actually finished building.
///
/// `--dump-dom` snapshots whatever exists when the virtual-time budget expires.
/// The page builds itself from `/api/devices`, so on a loaded box -- several
/// chromiums at once, which is what a parallel suite does -- the snapshot can
/// catch a half-built document: measured as a controller panel present and its
/// port cards missing, which reads exactly like a rendering bug and is not one.
///
/// `data-ready` is set on <body> at the end of a render pass, so it is the
/// signal that the DOM is worth asserting against. Waiting for evidence beats
/// waiting for a guessed duration.
///
/// It is a MARKER, not display text. This waited on the words "live /" from the
/// count pill until rewording that pill would have turned every gate here into
/// a screenshot of a half-built page -- passing, and asserting nothing.
fn render(bin: &str, url: &str) -> String {
    let mut last = String::new();
    for _ in 0..6 {
        last = render_once(bin, url);
        if last.contains("data-ready=\"1\"") {
            return last;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    last
}

/// Run the page and return whatever the snippet printed via console.log.
fn render_once(bin: &str, url: &str) -> String {
    // --dump-dom prints the DOM *after* scripts have run, which is the whole
    // point: it reflects what the page BUILT, not what was served.
    // HARD TIMEOUT around the browser. Headless chromium can hang indefinitely
    // in a container (zygote/shm quirks), and a test that hangs is worse than no
    // test: it blocks the whole suite instead of failing. `timeout` kills it and
    // the empty output fails the assertion, which is the outcome we want.
    let out = Command::new("timeout")
        .arg("25")
        .arg(bin)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--virtual-time-budget=4000",
            "--dump-dom",
            url,
        ])
        .output()
        .expect("run chromium");
    let dom = String::from_utf8_lossy(&out.stdout).to_string();
    // A DEAD BROWSER MUST NOT LOOK LIKE A PASS. Empty output would satisfy any
    // "does not contain X" assertion, which is how a browser suite silently
    // stops testing anything -- the same vacuous-pass trap as grepping for the
    // wrong success signal.
    assert!(
        dom.contains("<html") && dom.len() > 200,
        "the browser returned no usable DOM ({} bytes). It timed out or failed to \
         start; fix the browser before trusting any result from this suite.",
        dom.len()
    );
    dom
}

/// ONE REAL BROWSER AT A TIME.
///
/// Every test here starts its own chromium. Run under `cargo test --workspace`
/// the harness starts them all at once -- one per core -- and the box runs out
/// of whatever a browser needs: measured, nine of eleven failed with "no usable
/// DOM (0 bytes)" while each passed alone in four seconds. A suite that fails
/// because of its own parallelism teaches an operator to ignore it, which is
/// worse than being slow. So they queue.
fn one_browser_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The page must parse and BUILD ITSELF. A JS error leaves an empty shell that
/// every server-side test still passes.
#[test]
fn the_dashboard_page_renders_its_devices_in_a_real_browser() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_with_devices(&[
        "/dev/serial/by-id/usb-FTDI_BrowserBoard_AAAA-if00-port0",
        "/dev/serial/by-id/usb-FTDI_BrowserBoard_AAAA-if01-port0",
    ]);

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));

    // The page fetches /api/devices and builds the rack. If the JS threw, none
    // of this is in the DOM even though the HTML was served fine.
    assert!(
        dom.contains("BrowserBoard") || dom.contains("adapter"),
        "the page did not build its device list; the JS likely threw:\n{}",
        &dom[..dom.len().min(1200)]
    );
}

/// FOUND ON THE DASHBOARD, by looking at it: two CONTROLLER panels for one chip.
///
/// A TAC is a single FT4232H whose channels are two UARTs and two GPIO ports, so
/// both console rows are `is_controller` and both resolve to the same controller
/// instance. Rendered per row, that is two identical panels for one board, each
/// with its own power buttons -- and the second press looks like it is aimed at
/// a second board.
///
/// Counted in a real browser, because the bug is in what the page BUILDS: the
/// API rows are individually correct and a server-side test sees nothing wrong.
#[test]
fn one_controller_panel_per_chip_not_one_per_console() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    // The bravo host's real pair: two consoles on one TAC.
    let rig = Rig::start_at(&[
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0",
            "pci-0000:04:00.3-usb-0:4:1.0",
        ),
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0",
            "pci-0000:04:00.3-usb-0:4:1.1",
        ),
    ]);

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    // Count the built ELEMENT, not the word: the page's CSS and comments say
    // "controller" too, and the first version of this test counted those.
    let panels = dom.matches("class=\"ctl-tag\"").count();
    assert_eq!(
        panels,
        1,
        "one TAC chip must render ONE controller panel, found {panels}:\n{}",
        &dom[..dom.len().min(2000)]
    );
    // …and both consoles must still be on the page: the dedupe must drop the
    // duplicate PANEL, never a port card.
    // The harness numbers its fake consoles from 6100.
    assert!(
        dom.contains("6100") && dom.contains("6101"),
        "both consoles must still render:\n{}",
        &dom[..dom.len().min(2000)]
    );
}

/// REPORTED FROM THE PAGE, looking at the bravo bench: still two CONTROLLER
/// panels for one chip.
///
/// The first fix deduped per chassis group, and `renderCards` runs once per
/// group. Stale rows for the same FT4232H that carry no `by_path` -- ftdi_sio
/// rebinding churn leaves `…-if02` beside `…-if02-port0` -- have no topology, so
/// they group under the by-id prefix instead of the hub port. Two groups, two
/// dedupe sets, two panels, one physical controller.
///
/// The rows here are the bravo host's, verbatim, including the stale pair.
#[test]
fn one_controller_panel_per_chip_across_chassis_groups() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_at(&[
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0",
            "pci-0000:04:00.3-usb-0:4:1.0",
        ),
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0",
            "pci-0000:04:00.3-usb-0:4:1.1",
        ),
        // The GPIO channels, excluded from discovery: no port, no endpoint.
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if02-port0",
            "pci-0000:04:00.3-usb-0:4:1.2",
        ),
        // …and the stale pair, with NO topology at all.
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if02",
            "",
        ),
        (
            "/dev/serial/by-id/usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if03",
            "",
        ),
    ]);

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let panels = dom.matches("class=\"ctl-tag\"").count();
    assert_eq!(
        panels, 1,
        "one FT4232H must render ONE controller panel however its rows group, \
         found {panels}"
    );
}

/// A BARE SERIAL PORT IS STILL RACK HARDWARE.
///
/// The chassis box used to be skipped for a group with one card and no
/// controller -- which is exactly what a CMSIS-DAP debug probe is. The one
/// device on the bravo bench that is just a serial port rendered as a loose card
/// floating beside the rack, while every other port sat in a chassis.
#[test]
fn a_console_with_no_controller_still_mounts_in_the_rack() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_at(&[(
        "/dev/serial/by-id/usb-MBED_MBED_CMSIS-DAP_9009022103BB6BBBFE65044A-if01",
        "",
    )]);
    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);
    assert!(
        body.contains("class=\"adapter"),
        "a lone console must still mount as a chassis:\n{}",
        &body[..body.len().min(1500)]
    );
    assert!(
        body.contains("6100"),
        "and its port must be on the page:\n{}",
        &body[..body.len().min(1500)]
    );
}

/// EVERY NODE WEARS THE SAME FACEPLATE, including this one.
///
/// The local bench used to be an unlabelled pile above the peers, so the page
/// read as "some hardware, then somebody else's hardware" rather than as one
/// rack of machines. A node's plate carries the four facts that matter when
/// something is wrong: which node, at what address, on which build, and whether
/// it is answering.
#[test]
fn every_node_including_the_local_one_has_a_faceplate() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_at(&[(
        "/dev/serial/by-id/usb-FTDI_LocalBoard_AAAA-if00-port0",
        "pci-0000:00:14.0-usb-0:1:1.0",
    )]);
    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);
    assert!(
        body.contains("class=\"node-head"),
        "the local node must have a faceplate:\n{}",
        &body[..body.len().min(1500)]
    );
    assert!(
        body.contains("class=\"node-lamp"),
        "with a status lamp:\n{}",
        &body[..body.len().min(1200)]
    );
    assert!(
        body.contains("class=\"node-build"),
        "and its build fingerprint, so skew is visible on the page:\n{}",
        &body[..body.len().min(1200)]
    );
}

/// The console link must resolve to a real device, not `undefined`.
///
/// This exact bug shipped once: a `function open(d)` in the page shadowed
/// `window.open`, so every console opened inline and the socket went to
/// `/ws/console/undefined`. Server-side tests saw nothing wrong.
#[test]
fn console_links_carry_a_real_device_in_a_real_browser() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    // Two consoles, so the page builds a chassis exactly as it does on the rig.
    let rig = Rig::start_with_devices(&[
        "/dev/serial/by-id/usb-FTDI_LinkBoard_BBBB-if00-port0",
        "/dev/serial/by-id/usb-FTDI_LinkBoard_BBBB-if01-port0",
    ]);
    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));

    // Strip <script> blocks before looking. The page's own source is part of the
    // DOM, and it MENTIONS `console/undefined` in a comment explaining the bug
    // this test guards -- so a naive substring search flags the documentation as
    // the defect. Test what the page RENDERED, not what it says about itself.
    let rendered = strip_scripts(&dom);
    assert!(
        !rendered.contains("console/undefined") && !rendered.contains("console/null"),
        "a console link resolved to nothing -- the selector did not reach the handler"
    );
    // And the board really is on the page, so the check above is not vacuous.
    assert!(
        rendered.contains("BBBB"),
        "the device never rendered, so nothing was actually checked"
    );
}

mod cdp;

/// Clicking POWER OFF on the main dashboard must reach `/api/power`.
///
/// This is the wire nothing else covers. The controls are known to exist in the
/// served page and `/api/power` is tested server-side, but until now the link
/// between them was asserted only by reading the page source -- and a handler
/// wired to the wrong thing looks identical in source review. It shipped once
/// already, when a `function open(d)` shadowed `window.open`.
///
/// mcpd is not running behind this rig, so the request FAILS at the proxy. That
/// is the point: the failure only happens if the click actually issued the
/// request, and its shape tells us the selector reached the endpoint.
#[test]
fn clicking_power_off_on_the_dashboard_calls_the_power_endpoint() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    // Record every fetch the PAGE makes, then click the real control and report
    // what it asked for. Wrapping fetch is how we observe the wire without
    // adding a test-only hook to the page itself.
    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => { window.__calls.push(String(a[0])); return real(...a); };
        // VISIBLE buttons only. The console view's power bar exists in the DOM
        // before any device is bound to it -- hidden and unwired -- so a naive
        // search finds that one, clicks nothing, and the test reports a broken
        // wire that is not broken.
        const vis = b => b.offsetParent !== null;
        const find = () => [...document.querySelectorAll("button")].filter(vis)
          .find(b => /^\s*off\s*$/i.test(b.textContent||""));
        // WAIT FOR THE PAGE, do not assume a settle was long enough. The page
        // builds itself from `/api/devices`, and under a loaded box -- several
        // chromiums at once, which is exactly what a parallel suite does -- that
        // fetch lands after any fixed delay one picks. The old version read an
        // empty document and reported a missing button on a page whose buttons
        // work: a false failure, which costs more than the flake it hides.
        return new Promise(r => {
          const t0 = Date.now();
          const tick = () => {
            const off = find();
            if (off) {
              off.click();
              setTimeout(() => r(JSON.stringify({calls: window.__calls})), 1500);
            } else if (Date.now() - t0 > 15000) {
              r(JSON.stringify({error: "no OFF button",
                seen: [...document.querySelectorAll("button")].map(b => b.textContent)}));
            } else {
              setTimeout(tick, 200);
            }
          };
          tick();
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(
        !text.is_empty(),
        "the page returned nothing; CDP evaluate failed"
    );
    assert!(
        !text.contains("no OFF button"),
        "no power control on the page: {text}"
    );
    assert!(
        text.contains("/api/power/") && text.contains("/off"),
        "the OFF button did not call the power endpoint; it asked for: {text}"
    );
    // And it must name a real device, not `undefined` -- the cross-board bug in
    // miniature: a control that fires at nothing, or at the wrong board.
    assert!(
        !text.contains("power/undefined") && !text.contains("power/null"),
        "the power button fired without a device: {text}"
    );
}

/// The pop-out console window has its OWN power bar, and it is a separate
/// code path from the dashboard's -- so it needs its own proof.
#[test]
fn the_console_windows_power_bar_calls_the_power_endpoint() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    // The console window is the same page in console mode; open it the way the
    // dashboard does rather than reaching into internals.
    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => { window.__calls.push(String(a[0])); return real(...a); };
        const links = [...document.querySelectorAll("a,button")];
        const open = links.find(e => /console|open/i.test(e.textContent||"") ||
                                     /console/i.test(e.getAttribute("href")||""));
        if (open) open.click();
        return new Promise(r => setTimeout(() => {
          // Only the bar that is actually shown, and only once a device is
          // bound to it -- the unbound bar is deliberately inert.
          const vis = b => b.offsetParent !== null;
          const btns = [...document.querySelectorAll("button")].filter(vis);
          const off = btns.find(b => /^\s*off\s*$/i.test(b.textContent||""));
          if (off) off.click();
          setTimeout(() => r(JSON.stringify({
            calls: window.__calls,
            buttons: btns.map(b => (b.textContent||"").trim()).filter(Boolean),
          })), 1200);
        }, 800));
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(
        !text.is_empty(),
        "the page returned nothing; CDP evaluate failed"
    );
    assert!(
        text.contains("/api/power/") && text.contains("/off"),
        "the console view's power control did not reach the endpoint; got: {text}"
    );
    assert!(
        !text.contains("power/undefined") && !text.contains("power/null"),
        "the console power control fired without a device: {text}"
    );
}

/// A PEER'S BOARD MUST BE CLICKABLE, not merely visible.
///
/// The page hangs power controls on the CONTROLLER's row, and a controller is
/// never re-exported to a peer: it has no endpoint, so shipping it would claim
/// to serve a console nobody serves. The board then had no button anywhere,
/// while MCP and the dashboard API both drove it perfectly by routing to its
/// owner. Measured from charlie against alpha's IQ10 -- with the Bughopper
/// beside it working, because its controller IS its console and so arrived as a
/// row.
///
/// Asserted in a REAL BROWSER because the page decides this, not the server: a
/// first version of this gate checked the served source for the code that does
/// it, and went on passing with that code disabled.
#[test]
fn a_peers_board_has_power_buttons_even_though_its_controller_is_elsewhere() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_with_peer_board();
    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);
    assert!(
        body.contains("peer:alpha//dev/serial/by-id/usb-FTDI_Far-if00-port0"),
        "the peer's board must be on the page at all:\n{body}"
    );
    // A power control BOUND TO THAT BOARD. `data-power` is what a click needs.
    assert!(
        body.contains(
            r#"data-power="off" data-device="peer:alpha//dev/serial/by-id/usb-FTDI_Far-if00-port0""#
        ) || body.contains(
            r#"data-device="peer:alpha//dev/serial/by-id/usb-FTDI_Far-if00-port0" data-power="off""#
        ),
        "a peer's board needs a power control bound to it, or it can only be \
         driven from somewhere else:\n{body}"
    );
    // …and its boot modes, including the clear that releases a latched strap.
    assert!(
        body.contains(r#"data-power="mode:BOOT_MD_EDL""#),
        "the owner's boot modes must be offered too:\n{body}"
    );
    assert!(
        body.contains(r#"data-power="mode:clear""#),
        "…with the clear that gets the board back out:\n{body}"
    );
}

/// EACH HOST'S FACEPLATE OPENS ITS OWN RACK.
///
/// The faceplate is a divider: it says where one machine's hardware ends and the
/// next machine's begins, like the bar you put on a checkout belt. It was
/// appended to the page while every node's chassis went into the FIRST rack on
/// it -- so the local rack swallowed the peers' equipment and both peer
/// faceplates ended up stranded at the bottom of the page, dividing nothing.
///
/// Asserted on the rendered DOM: one rack per node, each opening with its own
/// faceplate, and that node's board mounted inside THAT rack.
#[test]
fn every_host_has_its_own_rack_opened_by_its_faceplate() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_with_peer_board();
    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));

    // Two nodes on this page: the local bench and the peer that owns FTDI_Far.
    let racks: Vec<&str> = dom.split(r#"class="node-section"#).skip(1).collect();
    assert!(
        racks.len() >= 2,
        "one rack per node, found {}:\n{}",
        racks.len(),
        &dom[..dom.len().min(1200)]
    );
    // Every frame is capped by its nameplate, with the uprights below it.
    for (i, r) in racks.iter().enumerate() {
        let head = r.find("node-head").unwrap_or(usize::MAX);
        let bay = r.find("class=\"rack").unwrap_or(usize::MAX);
        let card = r.find("class=\"adapter").unwrap_or(usize::MAX);
        assert!(
            head < bay,
            "rack {i}'s nameplate must cap the frame, with the uprights beneath it"
        );
        assert!(
            head != usize::MAX,
            "rack {i} has no faceplate:\n{}",
            &r[..r.len().min(400)]
        );
        assert!(
            head < card,
            "rack {i}'s faceplate must come before its equipment, not after it"
        );
    }
    // …and the peer's board is in the PEER's rack, not hoisted into the local one.
    let peer_rack = racks
        .iter()
        .find(|r| r.contains("alpha"))
        .expect("a rack for the peer");
    assert!(
        peer_rack.contains("usb-FTDI_Far-if00-port0"),
        "the peer's board must mount in the peer's own rack:\n{}",
        &peer_rack[..peer_rack.len().min(600)]
    );
}

/// The CONTROLLER panel names the CONTROLLER.
///
/// That panel is drawn from a representative console of the board, so the
/// obvious wiring -- hand the chip the same row the panel was built from -- puts
/// a second editor on the console's name under a heading that says CONTROLLER,
/// and renames the wrong device. The controller row is a real device an agent
/// can address by that name over MCP, so this has to hit its own selector.
///
/// Proven by clicking the real chip in a real browser and reading the request
/// the page made: the name in the URL is the Bantam's, never the console's.
#[test]
fn naming_from_the_controller_panel_renames_the_controller_not_the_console() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => { window.__calls.push(String(a[0])); return real(...a); };
        const chip = () => document.querySelector(".ctl .label");
        return new Promise(r => {
          const t0 = Date.now();
          const tick = () => {
            const c = chip();
            if (c) {
              // A press, not a click: the editor opens on mousedown so a redraw
              // between press and release cannot swallow the interaction.
              c.dispatchEvent(new MouseEvent("mousedown", {bubbles: true}));
              const input = document.querySelector(".ctl .label-edit");
              if (!input) { r(JSON.stringify({error: "chip did not open an editor"})); return; }
              input.value = "left-bantam";
              input.dispatchEvent(new KeyboardEvent("keydown", {key: "Enter", bubbles: true}));
              setTimeout(() => r(JSON.stringify({calls: window.__calls})), 1500);
            } else if (Date.now() - t0 > 15000) {
              r(JSON.stringify({error: "no label chip on the controller panel",
                seen: [...document.querySelectorAll(".ctl")].map(e => e.className)}));
            } else {
              setTimeout(tick, 200);
            }
          };
          tick();
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    let label_call = text
        .split('"')
        .find(|s| s.contains("/api/label/"))
        .unwrap_or_default()
        .to_string();
    assert!(
        !label_call.is_empty(),
        "the chip did not call the label endpoint: {text}"
    );
    assert!(
        label_call.contains("Bantam"),
        "the controller panel renamed something other than the controller: {label_call}"
    );
    assert!(
        !label_call.contains("ClickBoard"),
        "it renamed the CONSOLE the panel was drawn from: {label_call}"
    );

    // A PEER'S BOARD IS THE CASE THAT DISCRIMINATES.
    //
    // Locally the panel is drawn from the controller's OWN row, so binding the
    // chip to that row or to its `controller_port` are the same edit and the
    // click above cannot tell a correct wiring from a careless one. For a peer's
    // board there IS no local controller row -- it is never re-exported, having
    // no endpoint -- so the panel hangs off one of the board's consoles. A chip
    // there would rename that console under a heading that says CONTROLLER, and
    // the name would resolve to a device on the wrong host. So the panel offers
    // none, while the console's own card still does.
    let peer = Rig::start_with_peer_board();
    let count = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const panels = document.querySelectorAll(".ctl").length;
          if (panels) {
            r(JSON.stringify({
              panels,
              panel_chips: document.querySelectorAll(".ctl .label").length,
              panel_tags: document.querySelectorAll(".ctl .tags").length,
              card_chips: document.querySelectorAll(".card .label").length,
            }));
          } else if (Date.now() - t0 > 15000) {
            r(JSON.stringify({error: "no controller panel for the peer's board"}));
          } else { setTimeout(tick, 200); }
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", peer.base),
        std::time::Duration::from_millis(1500),
        count,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.contains("\"error\""), "{text}");
    assert!(
        text.contains("\"panel_chips\":0") && text.contains("\"panel_tags\":0"),
        "a peer board's controller panel must offer no naming controls -- there is no \
         local controller row for them to edit: {text}"
    );
    assert!(
        !text.contains("\"card_chips\":0"),
        "...while the peer console's OWN card still names itself: {text}"
    );
}

/// A LABEL YOU CAN ACTUALLY TYPE.
///
/// `render()` empties the whole grid and runs on every device event -- about
/// once a second on a live bench. The editor lives inside that grid, so it was
/// torn out a keystroke after it opened, and the `blur` that followed SAVED
/// whatever had been typed: reported from the rack as "I can only get one
/// character in before it commits".
///
/// So this test does what a person does: open the editor, type, let a refresh
/// land mid-edit, keep typing, then press Enter -- and it demands the WHOLE name
/// on the wire. A gate that types without a refresh in the middle would have
/// passed against the broken page.
#[test]
fn typing_a_label_survives_the_refresh_that_used_to_eat_it() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => {
          window.__calls.push({url: String(a[0]), body: a[1] && a[1].body});
          return real(...a);
        };
        const type = (input, text) => {
          // One character at a time, each followed by the refresh tick that a
          // live bench delivers. `render()` is the page's own redraw.
          for (const ch of text) {
            input.value += ch;
            input.dispatchEvent(new Event("input", {bubbles: true}));
            render();
          }
        };
        return new Promise(r => {
          const t0 = Date.now();
          const tick = () => {
            const chip = document.querySelector(".card .label");
            if (!chip) {
              if (Date.now() - t0 > 15000) { r(JSON.stringify({error: "no label chip"})); }
              else setTimeout(tick, 200);
              return;
            }
            // Press, exactly as a mouse does.
            chip.dispatchEvent(new MouseEvent("mousedown", {bubbles: true}));
            const input = document.querySelector(".card .label-edit");
            if (!input) { r(JSON.stringify({error: "the chip did not open an editor"})); return; }
            type(input, "bench-one");
            if (!input.isConnected) {
              r(JSON.stringify({error: "the refresh destroyed the field mid-edit",
                                typed: input.value}));
              return;
            }
            if (document.activeElement !== input) {
              r(JSON.stringify({error: "the field lost focus mid-edit"}));
              return;
            }
            input.dispatchEvent(new KeyboardEvent("keydown", {key: "Enter", bubbles: true}));
            setTimeout(() => r(JSON.stringify({
              calls: window.__calls.filter(c => c.url.includes("/api/label/")),
              // ...and the page starts redrawing again once the edit is over.
              redraws: !!document.querySelector(".card .label"),
            })), 1200);
          };
          tick();
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    assert!(
        text.contains("bench-one"),
        "the label saved was not what was typed: {text}"
    );
    // Exactly one save, not one per keystroke.
    let saves = text.matches("/api/label/").count();
    assert_eq!(saves, 1, "one edit is one save: {text}");
    assert!(
        text.contains("\"redraws\":true"),
        "the page must go back to redrawing once the edit ends: {text}"
    );
}

/// The same, for the label CHIPS -- adding one and taking one off.
///
/// `+ label` opens its own editor and `×` removes a label; both live in the same
/// grid that redraws itself under them. The add box was eating text exactly like
/// the name field, and the `×` needed the button to survive from press to
/// release, which a redraw does not guarantee.
#[test]
fn adding_and_removing_a_label_chip_survives_the_refresh_too() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => {
          window.__calls.push({url: String(a[0]), body: a[1] && a[1].body});
          return real(...a);
        };
        return new Promise(r => {
          const t0 = Date.now();
          const tick = () => {
            const add = document.querySelector(".card .tag-add");
            if (!add) {
              if (Date.now() - t0 > 15000) { r(JSON.stringify({error: "no + label control"})); }
              else setTimeout(tick, 200);
              return;
            }
            add.dispatchEvent(new MouseEvent("mousedown", {bubbles: true}));
            const input = document.querySelector(".card .tag-edit");
            if (!input) { r(JSON.stringify({error: "+ label did not open an editor"})); return; }
            for (const ch of "rack=r2") {
              input.value += ch;
              input.dispatchEvent(new Event("input", {bubbles: true}));
              render();   // the refresh tick, mid-word
            }
            if (!input.isConnected) {
              r(JSON.stringify({error: "the refresh destroyed the tag field", typed: input.value}));
              return;
            }
            input.dispatchEvent(new KeyboardEvent("keydown", {key: "Enter", bubbles: true}));
            setTimeout(() => r(JSON.stringify({
              calls: window.__calls.filter(c => c.url.includes("/api/tags/")),
            })), 1200);
          };
          tick();
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    // The whole `key=value` reached the wire, split into a selector -- not the
    // first letter, and not one request per keystroke.
    assert!(
        text.contains("rack") && text.contains("r2"),
        "the label added was not what was typed: {text}"
    );
    assert_eq!(
        text.matches("/api/tags/").count(),
        1,
        "one edit is one save: {text}"
    );
}

/// NO GAPS IN THE RACK.
///
/// Each host's frame is a nameplate with a bay under it, and those frames stack:
/// a strip of page showing through above a plate reads as a hole in the rack.
/// Three separate rules used to add space there -- the grid's gutter, the
/// section's own margins, and the bay's bottom margin -- so the dividers floated
/// with a gap above each one.
///
/// Measured in a real browser, because this is geometry: the top of each plate
/// must meet the bottom of whatever is above it, and the bay must start at the
/// bottom edge of its own plate.
#[test]
fn the_rack_stacks_flush_with_no_gap_above_a_divider() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    // Two hosts, so there is a seam between two frames to measure.
    let rig = Rig::start_with_peer_board();

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const secs = [...document.querySelectorAll(".node-section")];
          if (secs.length < 2) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "fewer than two node frames"}));
            else setTimeout(tick, 200);
            return;
          }
          const seams = [];
          for (let i = 1; i < secs.length; i++) {
            seams.push(secs[i].getBoundingClientRect().top -
                       secs[i-1].getBoundingClientRect().bottom);
          }
          // ...and inside a frame: plate bottom to bay top.
          const inner = secs.map(s => {
            const head = s.querySelector(".node-head"), bay = s.querySelector(".rack");
            if (!head || !bay) return null;
            return bay.getBoundingClientRect().top - head.getBoundingClientRect().bottom;
          });
          r(JSON.stringify({seams, inner}));
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).expect("geometry json");
    for (what, key) in [
        ("between two hosts' frames", "seams"),
        ("plate to bay", "inner"),
    ] {
        for gap in v[key].as_array().expect(key) {
            let gap = gap.as_f64().unwrap_or(f64::NAN);
            assert!(
                gap.abs() < 1.0,
                "{what}: {gap}px of page showing through -- the rack must be continuous: {text}"
            );
        }
    }
}

/// Photograph the rack so a person can look at it.
///
/// Ignored by default: it asserts nothing, it produces a picture. Geometry gates
/// prove there is no gap; only an image shows whether the result reads as a rack.
///
///     cargo test --test browser -- --ignored rack_screenshot
///
/// Writes `exports/rack.png`.
#[test]
#[ignore]
fn rack_screenshot() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    // Point it at a REAL node with `CONMINER_SHOT_URL` when the question is what
    // the bench actually looks like; the fixture is for layout work offline.
    let live = std::env::var("CONMINER_SHOT_URL").ok();
    let rig = (live.is_none()).then(Rig::start_with_peer_board);
    let url = live.unwrap_or_else(|| format!("{}/?nostream=1", rig.as_ref().unwrap().base));
    // `CONMINER_SHOT_PHONE=1` photographs it as a handset sees it.
    let phone = std::env::var("CONMINER_SHOT_PHONE").is_ok();
    let out = std::path::PathBuf::from("/work/exports").join(if phone {
        "rack-phone.png"
    } else {
        "rack.png"
    });
    let _ = std::fs::create_dir_all("/work/exports");
    assert!(
        browser.screenshot(&url, std::time::Duration::from_millis(2500), &out, phone),
        "no screenshot was written"
    );
    eprintln!("wrote {}", out.display());
}

/// AN EDITOR MUST NEVER FREEZE THE BENCH.
///
/// Keeping the grid still while somebody types is right; keeping it still
/// forever is a dashboard that quietly stops telling the truth about power and
/// presence. If the counter that tracks open editors ever leaks -- an input
/// removed by something other than its own handler -- the page must notice and
/// carry on.
#[test]
fn a_leaked_editor_cannot_stop_the_page_updating() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const chip = document.querySelector(".card .label");
          if (!chip) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "no label chip"}));
            else setTimeout(tick, 200);
            return;
          }
          chip.dispatchEvent(new MouseEvent("mousedown", {bubbles: true}));
          const input = document.querySelector(".card .label-edit");
          if (!input) { r(JSON.stringify({error: "no editor opened"})); return; }
          // Rip it out the way nothing should, but might: no commit, no blur
          // handler run, so the counter is left claiming an editor is open.
          input.remove();
          // Clear the click-hold first: it is a separate, self-expiring pause,
          // and leaving it set here would prove nothing about the leaked editor.
          state.hold = 0;
          const held = renderPaused();
          // The next tick must draw. Prove it by changing the model first.
          state.devices[0].line = "9600 8N1";
          render();
          const drawn = document.body.innerHTML.includes("9600 8N1");
          r(JSON.stringify({held, drawn, editing: state.editing}));
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    assert!(
        text.contains("\"held\":false"),
        "a vanished editor must not still be holding the page: {text}"
    );
    assert!(
        text.contains("\"drawn\":true"),
        "the page must resume drawing after an editor leaks: {text}"
    );
}

/// A CHIP NOBODY TYPED CANNOT OFFER A DELETE BUTTON.
///
/// `usb_ports` is written by discovery from the hub topology and rewritten every
/// time the device is seen. Rendering it as an operator label put an unasked-for
/// chip on every card on the bench, each with an `×` that promised a removal the
/// next sweep would undo.
#[test]
fn a_derived_tag_is_shown_but_never_offered_for_removal() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();
    {
        // Both kinds on one device: one discovery wrote, one a person did.
        let mut reg = Registry::open(rig._dir.path()).unwrap();
        let d = reg.resolve("ClickBoard_CCCC-if00").unwrap();
        reg.set_tags(
            d.id,
            &std::collections::BTreeMap::from([
                ("usb_ports".to_string(), "2-3.1.1".to_string()),
                ("rack".to_string(), "r2".to_string()),
            ]),
        )
        .unwrap();
    }
    rig.wait_for("the tags", |v| {
        v["devices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d["tags"]["usb_ports"].is_string()))
    });

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const chips = [...document.querySelectorAll(".card .tag")];
          const usb = chips.find(c => c.textContent.startsWith("usb_ports"));
          const rack = chips.find(c => c.textContent.startsWith("rack"));
          if (!usb || !rack) {
            if (Date.now() - t0 > 15000) {
              r(JSON.stringify({error: "chips missing", seen: chips.map(c => c.textContent)}));
            } else setTimeout(tick, 200);
            return;
          }
          r(JSON.stringify({
            usb_shown: true,
            usb_removable: !!usb.querySelector(".tag-x"),
            rack_removable: !!rack.querySelector(".tag-x"),
          }));
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.contains("\"error\""), "{text}");
    assert!(
        text.contains("\"usb_shown\":true"),
        "the topology is still worth showing: {text}"
    );
    assert!(
        text.contains("\"usb_removable\":false"),
        "a derived tag must not offer a removal the next sweep undoes: {text}"
    );
    assert!(
        text.contains("\"rack_removable\":true"),
        "...while a label a person typed still comes off: {text}"
    );
}

/// THE PHONE IS THE COMMON CASE.
///
/// This page gets read standing at the rack, one-handed. Three things decide
/// whether that works, and all three are measurable in a real browser under a
/// real handset viewport:
///
///  * nothing scrolls sideways -- a page that pans is a page you cannot read
///    while holding a probe;
///  * the controls are thumb-sized, because these buttons cut power to hardware
///    and "Off" sits beside "Reset";
///  * the rack still reads as a rack, frame and all.
#[test]
fn the_dashboard_fits_a_phone_without_scrolling_sideways() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    // `CONMINER_PHONE_URL` points the same measurements at a real node, which is
    // the only way to catch what a fixture's short strings hide.
    let live = std::env::var("CONMINER_PHONE_URL").ok();
    let rig = live.is_none().then(Rig::start_with_peer_board);

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const cards = [...document.querySelectorAll(".card")];
          if (!cards.length) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "no cards on the page"}));
            else setTimeout(tick, 200);
            return;
          }
          const doc = document.documentElement;
          // Anything wider than the screen, and what it is -- naming the culprit
          // is the difference between a fix and a hunt.
          const wide = [...document.querySelectorAll("body *")]
            .filter(e => e.getBoundingClientRect().right > doc.clientWidth + 1)
            .slice(0, 5)
            .map(e => e.className || e.tagName);
          const acts = [...document.querySelectorAll(".act")];
          const small = acts
            .filter(b => b.offsetParent !== null && b.getBoundingClientRect().height < 38)
            .map(b => `${b.textContent}:${Math.round(b.getBoundingClientRect().height)}`);
          const widest = [...document.querySelectorAll("body *")]
            .map(e => [Math.round(e.getBoundingClientRect().width), e.className || e.tagName])
            .sort((a, b) => b[0] - a[0])[0];
          r(JSON.stringify({
            innerWidth: window.innerWidth,
            visual: window.visualViewport && Math.round(window.visualViewport.width),
            screenW: screen.width,
            dpr: devicePixelRatio,
            bodyW: Math.round(document.body.getBoundingClientRect().width),
            widest,
            pageWidth: doc.scrollWidth,
            clientWidth: doc.clientWidth,
            wide,
            acts: acts.length,
            small,
            // The rack survives the narrow layout.
            rails: getComputedStyle(document.querySelector(".rack"), "::before").width,
            racks: document.querySelectorAll(".node-section > .rack").length,
          }));
        };
        tick();
      }))()
    "#;
    let url = live
        .clone()
        .unwrap_or_else(|| format!("{}/?nostream=1", rig.as_ref().unwrap().base));
    let out = browser.eval_on_phone(&url, std::time::Duration::from_millis(2000), probe);
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "CDP evaluate returned nothing");
    assert!(!text.contains("\"error\""), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).expect("json");

    // The LAYOUT viewport is the phone's. `innerWidth` is not the thing to
    // assert: chromium's mobile emulation zooms out to fit content that
    // overflows, so it reports the overflow rather than the device -- 430 for a
    // 390px phone. That is a symptom of the next assertion, not a viewport.
    assert_eq!(
        v["clientWidth"], 390,
        "the layout viewport must be a phone's: {text}"
    );
    let (page, client) = (
        v["pageWidth"].as_f64().unwrap_or(0.0),
        v["clientWidth"].as_f64().unwrap_or(0.0),
    );
    assert!(
        page <= client + 1.0,
        "the page is {page}px wide in a {client}px viewport, so it pans sideways. \
         Too wide: {}",
        v["wide"]
    );
    assert!(
        v["acts"].as_u64().unwrap_or(0) >= 4,
        "the power controls must still be there on a phone: {text}"
    );
    assert_eq!(
        v["small"].as_array().map(Vec::len),
        Some(0),
        "power controls under 38px are a mis-tap waiting to happen: {}",
        v["small"]
    );
    assert!(
        v["racks"].as_u64().unwrap_or(0) >= 2,
        "each host keeps its own frame on a phone: {text}"
    );
}

/// Wait until the PAGE has seen this device go away, then report what it did.
///
/// The condition matters more than it looks: the socket in this rig closes on
/// its own (nothing is listening upstream), so a probe that exits on "the page
/// is waiting" measures the close handler and never the device-gone path -- and
/// passes with the pane-closing bug put back. Measured: it did.
fn probe_gone(device: &str) -> String {
    format!(
        r#"
        (() => new Promise(r => {{
          const t0 = Date.now();
          const sawItGo = () => !state.devices.some(
            d => d.device === "{device}" && d.state !== "gone" && d.port !== null);
          const tick = () => {{
            if (sawItGo() || Date.now() - t0 > 12000) {{
              // One more beat, so the page has acted on what it saw.
              setTimeout(() => r(JSON.stringify({{
                sawItGo: sawItGo(),
                waiting: state.waiting,
                open: !document.getElementById("console").classList.contains("hidden"),
                kept: document.getElementById("term").textContent.includes("BOOTLOG-MARKER"),
                status: (document.getElementById("cstatus").textContent || ""),
              }})), 400);
            }} else setTimeout(tick, 200);
          }};
          tick();
        }}))()
        "#
    )
}

/// THE CONSOLE SURVIVES ITS BOARD.
///
/// On a Bughopper the FTDI *is* the board's UART, so `power off` takes the tty
/// with it: the device goes `gone`, ser2net drops the port and the socket
/// closes. The page used to close the pane on that -- blanking the screen an
/// operator was reading -- and never came back when the board did. Reported
/// from the bench: "the page goes blank and never reconnects".
///
/// Driven exactly as the bench does it: open a console, take the device away,
/// bring it back. The device list arrives over the live event stream, so this
/// test does NOT pass `nostream=1`.
#[test]
fn a_console_survives_its_board_going_away_and_comes_back_by_itself() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let rig = Rig::start_with_board();
    let console = "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if00-port0";

    // Open the console and put something on the screen, so "the screen survived"
    // is a fact about content rather than about an empty box still existing.
    let probe = r#"
      (() => new Promise(r => {
        window.__sockets = 0;
        const RealWS = window.WebSocket;
        window.WebSocket = function (...a) { window.__sockets++; return new RealWS(...a); };
        window.WebSocket.prototype = RealWS.prototype;
        const t0 = Date.now();
        const tick = () => {
          const link = [...document.querySelectorAll(".card .name, .card a")]
            .find(e => (e.textContent || "").includes("port 0"));
          if (!link) {
            if (Date.now() - t0 > 15000) { r(JSON.stringify({error: "no console to open"})); }
            else setTimeout(tick, 200);
            return;
          }
          const d = state.devices.find(x => x.device.includes("CCCC-if00"));
          openConsole(d);
          term.write(new TextEncoder().encode("BOOTLOG-MARKER\r\n"));
          setTimeout(() => r(JSON.stringify({
            open: !document.getElementById("console").classList.contains("hidden"),
            text: document.getElementById("term").textContent.includes("BOOTLOG-MARKER"),
            sockets: window.__sockets,
          })), 600);
        };
        tick();
      }))()
    "#;
    let url = format!("{}/", rig.base);
    let opened = browser.eval_after_load(&url, std::time::Duration::from_millis(1500), probe);
    let opened = opened.as_str().unwrap_or_default().to_string();
    assert!(!opened.contains("\"error\""), "{opened}");
    assert!(
        opened.contains("\"open\":true"),
        "the console must open: {opened}"
    );
    assert!(
        opened.contains("\"text\":true"),
        "with our marker on screen: {opened}"
    );

    // POWER OFF, as the controller does it: the tty is gone.
    {
        let mut reg = Registry::open(rig._dir.path()).unwrap();
        let row = reg.resolve("CCCC-if00").unwrap();
        reg.set_state(row.id, "gone").unwrap();
    }
    let after_off = browser.eval_after_load(
        &url,
        std::time::Duration::from_millis(0),
        &probe_gone(console),
    );
    let after_off = after_off.as_str().unwrap_or_default().to_string();
    assert!(
        after_off.contains("\"sawItGo\":true"),
        "the page never saw the board go, so this proves nothing: {after_off}"
    );
    assert!(
        after_off.contains("\"open\":true"),
        "the pane must survive the board: {after_off}"
    );
    assert!(
        after_off.contains("\"kept\":true"),
        "...and so must what was on the screen: {after_off}"
    );
    assert!(
        after_off.contains("\"waiting\":true"),
        "...and it must say it is waiting: {after_off}"
    );

    // POWER ON: the device returns.
    {
        let mut reg = Registry::open(rig._dir.path()).unwrap();
        let row = reg.resolve("CCCC-if00").unwrap();
        reg.set_state(row.id, "listening").unwrap();
    }
    let after_on = browser.eval_after_load(
        &url,
        std::time::Duration::from_millis(0),
        r#"
        (() => new Promise(r => {
          const t0 = Date.now();
          const before = window.__sockets;
          const tick = () => {
            if (window.__sockets > before || Date.now() - t0 > 12000) {
              r(JSON.stringify({
                redialled: window.__sockets > before,
                open: !document.getElementById("console").classList.contains("hidden"),
                kept: document.getElementById("term").textContent.includes("BOOTLOG-MARKER"),
              }));
            } else setTimeout(tick, 200);
          };
          tick();
        }))()
        "#,
    );
    let after_on = after_on.as_str().unwrap_or_default().to_string();
    assert!(
        after_on.contains("\"redialled\":true"),
        "the page must dial again by itself when the board returns: {after_on}"
    );
    assert!(
        after_on.contains("\"kept\":true"),
        "...without throwing away the screen: {after_on}"
    );
}

/// TYPING IN THE WEB CONSOLE REACHES THE BOARD.
///
/// The suite covered the page and the server separately -- clicks, layout,
/// scrollback, and (in the dash suite) a WebSocket binary frame reaching the
/// port -- and NOTHING covered a keystroke travelling from the terminal element
/// through dashd to the wire. So a regression that broke exactly that shipped,
/// and was found by a person typing into a console instead of by this suite.
///
/// Drives it the way a person does: open the console, switch to Sending, press
/// a key, and require the byte at a real listener on the other end.
#[test]
fn a_keystroke_in_the_web_terminal_reaches_the_port() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    // `CONMINER_TX_URL` points this at a real node, which is how a regression
    // that only shows on the bench gets reproduced here rather than by a person
    // typing into a console.
    let live = std::env::var("CONMINER_TX_URL").ok();
    let port = FakePort::start();
    let console = "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if00-port0";
    let rig = live
        .is_none()
        .then(|| Rig::start_with_console_on(console, port.port));

    // Open the console and switch to Sending, then STOP. The keys themselves
    // come from chromium's input pipeline, not from script, so whether the
    // terminal actually holds focus is part of what this proves.
    let setup = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          // The fixture's console when it is there, otherwise any live one --
          // the same setup has to work against a real node.
          const d = state.devices.find(x => x.device.includes("CCCC-if00"))
            || state.devices.find(x => x.port && !x.node && !x.ignored && x.state !== "gone");
          if (!d || !d.port) {
            if (Date.now() - t0 > 15000) r("error: no console with a port");
            else setTimeout(tick, 200);
            return;
          }
          openConsole(d);
          const send = () => {
            if (!state.ws || state.ws.readyState !== WebSocket.OPEN) {
              if (Date.now() - t0 > 15000) { r("error: socket never opened"); return; }
              setTimeout(send, 200); return;
            }
            document.getElementById("mode").click();
            setTimeout(() => r("ready"), 300);
          };
          send();
        };
        tick();
      }))()
    "#;
    let check = r#"JSON.stringify({
        sending: state.send,
        mode_disabled: document.getElementById("mode").disabled,
        focused: document.activeElement === document.getElementById("term"),
        ws: state.ws ? state.ws.readyState : null,
      })"#;
    let url = live
        .clone()
        .unwrap_or_else(|| format!("{}/", rig.as_ref().unwrap().base));
    let out = browser.press_keys(
        &url,
        std::time::Duration::from_millis(1500),
        setup,
        "x\r",
        check,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.starts_with("error"), "{text}");
    assert!(
        text.contains("\"focused\":true"),
        "the terminal must hold focus or no keystroke can ever reach it: {text}"
    );
    // Against a live node there is no fake listener to inspect; the page's own
    // account is all this can check, and it is enough to see TX refused.
    if live.is_some() {
        assert!(
            text.contains("\"sending\":true"),
            "the page would not switch to Sending: {text}"
        );
        return;
    }

    // THE BYTES, AT THE OTHER END. Everything above is the page's own account of
    // itself; this is the wire.
    let mut got = Vec::new();
    while let Some(chunk) = port.next_received(Duration::from_secs(3)) {
        got.extend_from_slice(&chunk);
        if got.contains(&b'x') {
            break;
        }
    }
    assert!(
        got.contains(&b'x'),
        "the keystroke never reached the port. Page said: {text}, wire saw: {got:?}"
    );
}

/// THE RACK'S LAMPS COME FROM CAPTURE HEALTH, NOT PRESENCE.
///
/// "Is this console alive" was read from `state`, which discovery owns and
/// overwrites with presence. It only ever worked because both answers shared one
/// column; once health moved to its own, every lamp on the rack was one
/// discovery sweep from going dark while the consoles carried on capturing.
#[test]
fn the_rack_lights_from_capture_health_not_presence() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let talking = "/dev/serial/by-id/usb-FTDI_LAMPA_1111-if00-port0";
    let silent = "/dev/serial/by-id/usb-FTDI_LAMPB_2222-if00-port0";
    let rig = Rig::start_with_devices(&[talking, silent]);
    // Both are present and discovered. They differ ONLY in capture health.
    rig.publish_capture(talking, conminer_core::live::CaptureState::Streaming);
    rig.publish_capture(silent, conminer_core::live::CaptureState::NotListening);
    rig.wait_for_devices(2);

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const boxes = [...document.querySelectorAll(".adapter")];
          if (boxes.length < 2) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "fewer than two chassis"}));
            else setTimeout(tick, 200);
            return;
          }
          r(JSON.stringify(boxes.map(b => ({
            name: (b.querySelector(".adapter-name") || {}).textContent || "",
            sub: (b.querySelector(".adapter-sub") || {}).textContent || "",
            led: !!b.querySelector(".adapter-led.live"),
            dark: b.className.includes("dark"),
          }))));
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.contains("\"error\""), "{text}");
    let boxes: serde_json::Value = serde_json::from_str(&text).unwrap();
    let boxes = boxes.as_array().unwrap();
    let find = |needle: &str| {
        boxes
            .iter()
            .find(|b| b["name"].as_str().unwrap_or_default().contains(needle))
            .unwrap_or_else(|| panic!("no chassis named {needle}: {text}"))
            .clone()
    };
    let a = find("LAMPA");
    let b = find("LAMPB");
    assert_eq!(
        a["led"], true,
        "a streaming console must light its chassis: {text}"
    );
    assert_eq!(
        a["dark"], false,
        "a streaming console's chassis is not dark: {text}"
    );
    assert!(
        a["sub"].as_str().unwrap().contains("1 live"),
        "a streaming console counts as live: {text}"
    );
    assert_eq!(
        b["led"], false,
        "a console that is not listening must not light: {text}"
    );
    assert!(
        b["sub"].as_str().unwrap().contains("0 live"),
        "not_listening is not live: {text}"
    );
}

/// A CONTROLLER THAT IS ALSO A CONSOLE STILL COUNTS AS A PORT.
///
/// The summary once counted `!is_controller`, so a Bughopper -- one FTDI that
/// both powers the board and carries its UART -- read "0 ports" while serving
/// one. This was a grep over dashboard.html, and that grep also pinned the
/// aliveness bug in place by asserting the page still read health out of the
/// presence field. Executed instead: a real chassis, in a real browser.
#[test]
fn the_chassis_summary_counts_a_controller_that_is_also_a_console() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let Some(browser) = cdp::Browser::launch(&bin) else {
        panic!("chromium would not start with a debugging port");
    };
    let both = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_BOTHDUTY-if00";
    let rig = Rig::start_with_controller_console(both);
    rig.publish_capture(both, conminer_core::live::CaptureState::Streaming);
    rig.wait_for_devices(1);

    let probe = r#"
      (() => new Promise(r => {
        const t0 = Date.now();
        const tick = () => {
          const box = document.querySelector(".adapter");
          const sub = box && box.querySelector(".adapter-sub");
          if (!sub || !sub.textContent) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "no chassis summary"}));
            else setTimeout(tick, 200);
            return;
          }
          r(JSON.stringify({sub: sub.textContent, led: !!box.querySelector(".adapter-led.live")}));
        };
        tick();
      }))()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/", rig.base),
        std::time::Duration::from_millis(1500),
        probe,
    );
    let text = out.as_str().unwrap_or_default().to_string();
    assert!(!text.contains("\"error\""), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let sub = v["sub"].as_str().unwrap_or_default();
    assert!(
        sub.contains("1 port"),
        "a controller that serves a UART is a port: {text}"
    );
    assert!(
        sub.contains("1 live"),
        "and it is live when it is capturing: {text}"
    );
    assert!(
        sub.contains("controller"),
        "while still being named a controller: {text}"
    );
    assert_eq!(v["led"], true, "its chassis lamp is lit: {text}");
}

/// THE PAGE ITSELF, on the complaint that started this: a rig with one cable in
/// it rendered as a full rack.
///
/// Server-side gates prove the API drops hardware that is no longer on the bus.
/// They cannot prove the PAGE stops drawing its buttons, and the page is what an
/// operator acts on.
#[test]
fn the_page_shows_only_hardware_that_is_on_the_bus() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let console = "/dev/serial/by-id/usb-VendorX_BoardA_UART_AAAA-if00-port0";
    let bantam = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_RRD-if00";
    let rig = Rig::start_at(&[
        (console, "pci-0000:00:14.0-usb-0:3.2.1:1.0"),
        (bantam, "pci-0000:00:14.0-usb-0:3.2.2:1.0"),
    ]);

    // PRECONDITION. If the panel never rendered, the assertion after the unplug
    // would pass for the wrong reason.
    //
    // Stripped, ALWAYS: the page inlines its own JavaScript, so the raw dump
    // contains any phrase this gate looks for whether or not an element was ever
    // built with it. That is the trap `strip_scripts` exists for.
    let body = strip_scripts(&render(&bin, &format!("{}/?nostream=1", rig.base)));
    assert_eq!(
        body.matches("ctl-tag").count(),
        1,
        "precondition: a present Bantam draws exactly one controller panel:\n{}",
        &body[..body.len().min(2000)]
    );
    assert!(
        body.contains("AAAA"),
        "precondition: the console renders:\n{}",
        &body[..body.len().min(2000)]
    );

    // Both cables out, long enough ago that this is not a power cycle: the rows
    // were seeded with last_seen=1000 (1970).
    rig.unplug(bantam);
    rig.unplug(console);
    rig.until_api("the bench to empty", |b| {
        serde_json::from_str::<serde_json::Value>(b)
            .ok()
            .and_then(|v| v["devices"].as_array().map(Vec::len))
            == Some(0)
    });

    let body = strip_scripts(&render(&bin, &format!("{}/?nostream=1", rig.base)));
    assert_eq!(
        body.matches("ctl-tag").count(),
        0,
        "an unplugged controller must draw NO panel: its buttons would actuate \
         hardware that is not on the bench:\n{}",
        &body[..body.len().min(3000)]
    );
    assert!(
        !body.contains("AAAA"),
        "a board that left the bench must not still be listed:\n{}",
        &body[..body.len().min(3000)]
    );
    assert!(
        !body.contains("Bantam_RRD"),
        "nor the controller that drove it:\n{}",
        &body[..body.len().min(3000)]
    );
}

/// The chassis headings the page actually drew, in order.
///
/// Counting `class="adapter` would also count `adapter-head`, `-led`, `-name`
/// and `-sub`, so one chassis reads as five. The heading text is what a person
/// looks at, so that is what these gates assert on.
fn chassis_headings(body: &str) -> Vec<String> {
    body.split("class=\"adapter-name\"")
        .skip(1)
        .filter_map(|rest| {
            let open = rest.find('>')? + 1;
            let end = rest[open..].find("</span>")? + open;
            Some(rest[open..end].to_string())
        })
        .collect()
}

/// A peer's board must not be drawn as two nameless chassis.
///
/// Rendered in a real browser, because this is a bug you can only see by
/// looking at the page. The owner drew its board as ONE chassis with the ports
/// named and the controller labelled; its peer drew the same hardware as two
/// chassis headed by raw by-id paths with no controller name anywhere, because
/// a peer's row has no local `by_path` to group by and the controller's own row
/// is deliberately never imported.
#[test]
fn a_peers_board_renders_as_one_named_chassis_not_two_raw_ones() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    // One board, two chips, one controller.
    let rig = Rig::start_peer_board(
        "alpha",
        "/dev/serial/by-id/usb-Microchip_Bantam_CTRL0001-if00",
        Some("BOARD-A"),
        &[
            "/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if00-port0",
            "/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if01-port0",
            "/dev/serial/by-id/usb-FTDI_RIDE_SPI_BBBB-if00-port0",
        ],
    );
    rig.until_api("the peer's board", |s| s.contains("alpha"));

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);

    let heads = chassis_headings(&body);
    assert_eq!(
        heads.len(),
        1,
        "one board is one chassis, however many chips it has; drew {heads:?}"
    );
    assert_eq!(
        heads[0], "alpha/BOARD-A",
        "the chassis must be headed by the owner's name for its controller"
    );
}

/// A peer's controller is NAMED, but not renameable from here.
///
/// The label chip writes to the local registry, and a peer's controller has no
/// row here: naming it would create a label that resolves to nothing. So the
/// name is shown read-only; without it the panel reads "board controller" on
/// every node but its owner.
#[test]
fn a_peers_controller_shows_its_name_without_offering_to_rename_it() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_peer_board(
        "alpha",
        "/dev/serial/by-id/usb-Microchip_Bantam_CTRL0001-if00",
        Some("BOARD-A"),
        &["/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if00-port0"],
    );
    rig.until_api("the peer's board", |s| s.contains("alpha"));

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);
    assert!(
        body.contains("ctl-owned-label") && body.contains("BOARD-A"),
        "the owner's controller name must be on the page:\n{}",
        &body[..body.len().min(2000)]
    );
    // The editable chip belongs to rows we own. Its absence here is the point.
    let ctl_panel = body.split("class=\"ctl-head").nth(1).unwrap_or("");
    let ctl_panel = &ctl_panel[..ctl_panel.len().min(600)];
    assert!(
        !ctl_panel.contains("label-chip"),
        "a peer's controller must not offer a rename that resolves to nothing:\n{ctl_panel}"
    );
}

/// An owner that never named its controller still gets a readable chassis.
///
/// The fallback must not go back to the full peer path: the by-id tail is
/// short, stable, and already how the bench talks about a chip.
#[test]
fn an_unnamed_peer_controller_still_beats_a_raw_peer_path() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium in this image");
        return;
    };
    let rig = Rig::start_peer_board(
        "alpha",
        "/dev/serial/by-id/usb-Microchip_Bantam_CTRL0001-if00",
        None,
        &["/dev/serial/by-id/usb-FTDI_RIDE_UART_AAAA-if00-port0"],
    );
    rig.until_api("the peer's board", |s| s.contains("alpha"));

    let dom = render(&bin, &format!("{}/?nostream=1", rig.base));
    let body = strip_scripts(&dom);
    let heads = chassis_headings(&body);
    assert_eq!(
        heads,
        vec!["alpha usb-Microchip_Bantam_CTRL0001-if00".to_string()],
        "an unnamed controller is headed by its node and the by-id tail, never \
         by the full peer path"
    );
}

// ------------------------------------------------ the terminal must keep up ---

/// A port that TALKS: small chunks, as a UART hands them over, then a marker.
///
/// `FakePort` only listens, which is all a TX test needs. Rendering cost is
/// about the other direction, and it depends on how the bytes ARRIVE: a console
/// delivers tens of tiny frames a second, not one tidy buffer.
struct ChattyPort {
    port: u16,
}

impl ChattyPort {
    /// Serve `chunks` frames of `chunk` bytes, a newline every eighth, then
    /// `marker`. Each chunk is flushed on its own so it reaches the page as its
    /// own frame.
    fn start(chunks: usize, chunk: &'static str, marker: &'static str) -> Self {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let _ = s.set_nodelay(true);
                std::thread::spawn(move || {
                    // Let the viewer attach before the burst begins.
                    std::thread::sleep(Duration::from_millis(400));
                    for i in 0..chunks {
                        let nl = if i % 8 == 7 { "\r\n" } else { "" };
                        if s.write_all(format!("{chunk}{nl}").as_bytes()).is_err() {
                            return;
                        }
                        let _ = s.flush();
                        // PACED, like the UART it stands in for. Written back to
                        // back, TCP coalesces these into a few large reads and the
                        // page sees a handful of frames: the first version of this
                        // fixture did exactly that, and the gate built on it passed
                        // against the very painter it exists to catch.
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    let _ = s.write_all(format!("\r\n{marker}\r\n").as_bytes());
                    let _ = s.flush();
                    // Hold the socket: a close would make the page reconnect.
                    std::thread::sleep(Duration::from_secs(120));
                });
            }
        });
        Self { port }
    }
}

const ONE_BOARD: &[(&str, &str)] = &[(
    "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if00-port0",
    "pci-0000:00:14.0-usb-0:7.1.1:1.0",
)];

/// Evaluate `body` (the inside of a Promise executor taking `r`) on a loaded
/// dashboard with the console pane showing, and return its JSON.
fn on_terminal(browser: &cdp::Browser, base: &str, body: &str, budget: Duration) -> Value {
    let expr = format!(
        r#"(() => new Promise(r => {{
            const enc = new TextEncoder();
            const el = document.getElementById("term");
            document.getElementById("console").classList.remove("hidden");
            const frame = () => new Promise(f => requestAnimationFrame(() => requestAnimationFrame(f)));
            const fill = (n) => {{
              let s = "";
              for (let i = 0; i < n; i++) s += `[ ${{i}}.000000] scmi_protocol scmi_dev.6: Message for 44 type 0 is not expected!\r\n`;
              term.write(enc.encode(s));
            }};
            {body}
        }}))()"#
    );
    let out = browser.eval_within(
        &format!("{base}/?nostream=1"),
        Duration::from_millis(1500),
        &expr,
        budget,
    );
    serde_json::from_str(out.as_str().unwrap_or("null")).unwrap_or(Value::Null)
}

/// The cost of a frame must not depend on how much history is on screen.
///
/// Reported from the bench as "the web UI is very, very slow, and on telnet it
/// is fast". The server was innocent: a passive second viewer on the live
/// console received every byte with zero drift against the board's own kernel
/// timestamps. The page was not. `term.write` ended in a full repaint, so every
/// WebSocket frame rebuilt one <div> per scrollback line with a forced layout
/// either side. Measured in chromium: 0.9 ms per frame at 100 lines, 6.5 ms at
/// 1000, 23.9 ms at the 4000-line cap, and a board that never stops talking
/// keeps the scrollback pinned at the cap. A boot log of about 2750 frames then
/// took over a minute to draw.
///
/// 300 frames into a full scrollback cost 7 s before. The bound is generous on
/// purpose: it has to fail the old painter on a fast machine and pass the new
/// one on a slow one, and there are two orders of magnitude between them.
#[test]
fn a_console_frame_costs_the_same_at_any_scrollback_depth() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset(); fill(4000); term.flush();
          const t0 = performance.now();
          for (let i = 0; i < 300; i++) term.write(enc.encode(`[ 9${i}.123456] scmi_protocol scmi_dev.6: M`));
          term.flush();
          void el.scrollHeight;
          const ms = performance.now() - t0;
          r(JSON.stringify({ms, rows: el.childElementCount,
                            last: el.lastElementChild.textContent.slice(-40)}));
        })();
        "#,
        Duration::from_secs(120),
    );
    assert_eq!(
        v["rows"], 4000,
        "precondition: the scrollback must be at its cap: {v}"
    );
    assert!(
        v["last"]
            .as_str()
            .unwrap_or_default()
            .contains("scmi_dev.6: M"),
        "precondition: the frames must actually have been drawn: {v}"
    );
    let ms = v["ms"].as_f64().unwrap_or(f64::MAX);
    eprintln!("terminal: 300 frames into a full scrollback took {ms:.1} ms");
    assert!(
        ms < 1500.0,
        "300 small frames into a full scrollback took {ms:.0} ms; the painter is \
         paying for history again (it was 7000 ms when every frame rebuilt it): {v}"
    );
}

/// Many frames, one paint, and only the rows that changed.
///
/// The structural half of the gate above, and the one that cannot be flattered
/// by a fast machine: a row the output never touched must be the same node
/// afterwards, and a burst that lands inside one animation frame must cost one
/// paint however many frames it was.
#[test]
fn a_burst_of_frames_is_painted_once_and_only_where_it_changed() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset(); fill(500); term.flush();
          await frame();
          const first = el.firstElementChild, mid = el.children[250];
          const before = term.paints;
          for (let i = 0; i < 200; i++) term.write(enc.encode("x"));
          term.write(enc.encode(" BURST-END\r\n"));
          const drawn_synchronously = el.textContent.includes("BURST-END");
          await frame();
          r(JSON.stringify({
            paints: term.paints - before,
            drawn_synchronously,
            drawn: el.textContent.includes("x".repeat(200) + " BURST-END"),
            first_kept: el.firstElementChild === first,
            mid_kept: el.children[250] === mid,
            rows: el.childElementCount, lines: term.lines.length,
          }));
        })();
        "#,
        Duration::from_secs(60),
    );
    assert_eq!(
        v["drawn"], true,
        "the burst must end up on screen, whole: {v}"
    );
    assert_eq!(
        v["paints"], 1,
        "201 frames inside one animation frame are one paint, not 201: {v}"
    );
    assert_eq!(
        v["drawn_synchronously"], false,
        "a paint per frame is the bug; drawing belongs to the animation frame: {v}"
    );
    assert_eq!(
        v["first_kept"], true,
        "an untouched row must not be rebuilt: {v}"
    );
    assert_eq!(
        v["mid_kept"], true,
        "an untouched row must not be rebuilt: {v}"
    );
    assert_eq!(
        v["rows"], v["lines"],
        "the DOM must mirror the model row for row: {v}"
    );
}

/// The cap trims the top of the DOM; it does not rebuild what stays.
///
/// This is the steady state of a board that never stops talking: every new
/// line pushes one off the top. If that fell back to a full rebuild the fix
/// would hold everywhere except the one case that was reported.
#[test]
fn the_scrollback_cap_trims_the_top_without_rebuilding_the_rest() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset(); fill(term.cap + 50); term.flush();
          await frame();
          const held = el.children[100];
          const held_text = held.textContent;
          const before = term.paints;
          for (let i = 0; i < 7; i++) term.write(enc.encode(`trim-line-${i}\r\n`));
          await frame();
          r(JSON.stringify({
            rows: el.childElementCount, cap: term.cap,
            paints: term.paints - before,
            // Seven rows came off the top, so the held row moved up by seven
            // and is still the very same node.
            held_moved: el.children[93] === held,
            held_text_kept: el.children[93].textContent === held_text,
            tail: el.children[el.childElementCount - 2].textContent,
            model_matches: [...el.children].every((n, i) =>
              n.textContent === (term.lines[i].map(x => x.t).join("") || "\u200b")),
          }));
        })();
        "#,
        Duration::from_secs(60),
    );
    assert_eq!(
        v["rows"], v["cap"],
        "the DOM must hold exactly the cap: {v}"
    );
    assert_eq!(
        v["held_moved"], true,
        "a surviving row must be the same node, shifted: {v}"
    );
    assert_eq!(v["held_text_kept"], true, "{v}");
    assert_eq!(
        v["tail"], "trim-line-6",
        "the newest line must be the last full row: {v}"
    );
    assert_eq!(v["paints"], 1, "{v}");
    assert_eq!(
        v["model_matches"], true,
        "after trimming, every DOM row must still be the model's row: {v}"
    );
}

/// A burst bigger than the whole scrollback must not leave stale rows behind.
#[test]
fn a_burst_larger_than_the_scrollback_replaces_it_entirely() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset();
          term.write(enc.encode("OLD-ROW-A\r\nOLD-ROW-B\r\n")); term.flush();
          await frame();
          let s = ""; for (let i = 0; i < term.cap + 500; i++) s += `new-${i}\r\n`;
          term.write(enc.encode(s));
          await frame();
          r(JSON.stringify({
            rows: el.childElementCount, cap: term.cap,
            stale: el.textContent.includes("OLD-ROW"),
            first: el.firstElementChild.textContent,
            model_first: term.lines[0].map(x => x.t).join(""),
          }));
        })();
        "#,
        Duration::from_secs(60),
    );
    assert_eq!(v["rows"], v["cap"], "{v}");
    assert_eq!(
        v["stale"], false,
        "rows the cap trimmed must be gone from the page: {v}"
    );
    assert_eq!(v["first"], v["model_first"], "{v}");
}

/// Leaving a full-screen application brings the scrollback back intact.
///
/// The grid and the scrollback are different documents sharing one element, so
/// the switch is the one place a full rebuild is right, and it has to happen in
/// BOTH directions: an incremental paint after `:q` would splice scrollback
/// rows into whatever vim left on screen.
#[test]
fn the_alternate_screen_round_trip_restores_the_scrollback() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset();
          term.write(enc.encode("before-one\r\nbefore-two\r\n$ vim\r\n"));
          await frame();
          term.write(enc.encode("\x1b[?1049h\x1b[2J\x1b[1;1HVIM-SCREEN-TEXT"));
          await frame();
          const in_app = {grid: el.textContent.includes("VIM-SCREEN-TEXT"),
                          scrollback_hidden: !el.textContent.includes("before-one")};
          term.write(enc.encode("\x1b[?1049l"));
          term.write(enc.encode("after-quit\r\n"));
          await frame();
          r(JSON.stringify({
            in_app,
            back: el.textContent.includes("before-one") && el.textContent.includes("before-two"),
            grid_gone: !el.textContent.includes("VIM-SCREEN-TEXT"),
            after: el.textContent.includes("after-quit"),
            rows: el.childElementCount, lines: term.lines.length,
          }));
        })();
        "#,
        Duration::from_secs(60),
    );
    assert_eq!(
        v["in_app"]["grid"], true,
        "the application's screen must be drawn: {v}"
    );
    assert_eq!(v["in_app"]["scrollback_hidden"], true, "{v}");
    assert_eq!(v["back"], true, "the scrollback must return intact: {v}");
    assert_eq!(
        v["grid_gone"], true,
        "and the application's screen must be gone: {v}"
    );
    assert_eq!(v["after"], true, "{v}");
    assert_eq!(v["rows"], v["lines"], "{v}");
}

/// The bytes kept for "save" are bounded, newest kept.
///
/// A console left open on a board that never stops talking grew this without
/// limit, a tab's worth of memory per day on the bench's chattiest console.
#[test]
fn the_saved_console_bytes_are_bounded_and_keep_the_newest() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_at(ONE_BOARD);
    let v = on_terminal(
        &browser,
        &rig.base,
        r#"
        (async () => {
          term.reset();
          term.rawCap = 4096;
          for (let i = 0; i < 400; i++) term.write(enc.encode(`line-${i}-${"p".repeat(40)}\r\n`));
          const blob = await new Blob(term.raw).text();
          r(JSON.stringify({bytes: term.rawBytes, cap: term.rawCap,
                            newest: blob.includes("line-399-"), oldest: blob.includes("line-0-")}));
        })();
        "#,
        Duration::from_secs(60),
    );
    assert!(
        v["bytes"].as_u64().unwrap_or(u64::MAX) <= v["cap"].as_u64().unwrap_or(0) + 64,
        "the save buffer must stay within its budget: {v}"
    );
    assert_eq!(v["newest"], true, "and keep what was said last: {v}");
    assert_eq!(
        v["oldest"], false,
        "at the expense of what was said first: {v}"
    );
}

/// End to end, through the real socket: a chatty console is on screen promptly.
///
/// The gates above drive `term.write` directly. This one sends 1500 small
/// chunks the way a UART does, through dashd and the page's own WebSocket, into
/// a scrollback already at its cap, and requires the last of them on screen
/// within seconds. Every frame used to cost a full rebuild of 4000 rows, which
/// put this at over half a minute.
#[test]
fn a_chatty_console_is_drawn_as_fast_as_it_arrives() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let console = "/dev/serial/by-id/usb-FTDI_ClickBoard_CCCC-if00-port0";
    let port = ChattyPort::start(
        1500,
        "[  146.284938] scmi_protocol scmi_dev.6: M",
        "CHATTY-END-MARKER",
    );
    let rig = Rig::start_with_console_on(console, port.port);
    let probe = r#"
      (() => new Promise(r => {
        const enc = new TextEncoder();
        const t0 = Date.now();
        const tick = () => {
          const d = state.devices.find(x => x.device.includes("CCCC-if00"));
          if (!d || !d.port) {
            if (Date.now() - t0 > 15000) r(JSON.stringify({error: "no console"}));
            else setTimeout(tick, 100);
            return;
          }
          // Start from the reported condition: a scrollback already at its cap.
          let s = "";
          for (let i = 0; i < term.cap; i++) s += `[ ${i}.000000] history line ${i} of a board that never stops talking\r\n`;
          openConsole(d);
          term.write(enc.encode(s));
          const opened = performance.now();
          const wait = () => {
            const el = document.getElementById("term");
            if (el.textContent.includes("CHATTY-END-MARKER")) {
              r(JSON.stringify({ms: performance.now() - opened, paints: term.paints,
                                frames: term.raw.length, rows: el.childElementCount}));
            } else if (performance.now() - opened > 60000) {
              r(JSON.stringify({error: "marker never drawn", paints: term.paints}));
            } else setTimeout(wait, 50);
          };
          wait();
        };
        tick();
      }))()
    "#;
    let out = browser.eval_within(
        &format!("{}/", rig.base),
        Duration::from_millis(1500),
        probe,
        Duration::from_secs(90),
    );
    let v: Value = serde_json::from_str(out.as_str().unwrap_or("null")).unwrap_or(Value::Null);
    assert!(v.get("error").is_none(), "{v}");
    // PRECONDITION: the page really did receive many small frames. Without this
    // the gate measures nothing: a burst that arrives as a few large frames is
    // cheap under ANY painter.
    assert!(
        v["frames"].as_u64().unwrap_or(0) > 500,
        "the fixture must deliver many small frames, as a UART does, or this proves \
         nothing about the cost of a frame: {v}"
    );
    let ms = v["ms"].as_f64().unwrap_or(f64::MAX);
    eprintln!(
        "chatty console: {} frames on screen in {ms:.0} ms",
        v["frames"]
    );
    assert!(
        ms < 12000.0,
        "1500 small frames through the socket took {ms:.0} ms to reach the screen; \
         with a full scrollback that was over 30 s when every frame rebuilt it: {v}"
    );
}

// -------------------------------------------- what the controller is holding ---

/// The controller panel says what is held across boots, and says it apart from
/// everything about EDL, power or capture.
///
/// A board whose controller holds MD_EDL sits in ROM EDL with a silent console
/// through any number of power cycles while the page shows a powered board and
/// nothing wrong. Rendered in a real browser because the claim is about what a
/// person looking at the page can see.
///
/// The third state is the one that matters most: a controller that did not
/// answer must read as UNKNOWN, never as "none held".
#[test]
fn the_controller_panel_shows_what_is_held_and_unknown_is_never_none() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_with_board();
    let probe = r#"
      (() => {
        const show = (reading) => {
          for (const d of state.devices) d.boot_overrides = reading;
          state.server_now = 1700000012000;
          render();
          const row = document.querySelector(".ctl-row.ovr");
          return row ? {state: row.dataset.state, text: row.textContent,
                        held: [...row.querySelectorAll(".ovr-chip.held")].map(c => c.textContent),
                        label: row.querySelector(".lbl").textContent} : null;
        };
        const base = {supported: true, overrides: {}, asserted: [], unknown: [],
                      read_at_ms: 1700000000000, effect: "x"};
        return JSON.stringify({
          held: show({...base, state: "latched", asserted: ["MD_EDL"]}),
          clear: show({...base, state: "clear"}),
          unknown: show({...base, state: "unknown", error: "controller did not answer"}),
          unsupported: show({supported: false, state: "unsupported", why: "no read hook"}),
          not_read: show(null),
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        Duration::from_millis(1500),
        probe,
    );
    let v: Value = serde_json::from_str(out.as_str().unwrap_or("null")).unwrap_or(Value::Null);
    assert!(
        v["held"].is_object(),
        "the panel must have a held-across-boots row: {v}"
    );

    assert_eq!(v["held"]["held"], serde_json::json!(["MD_EDL held"]), "{v}");
    assert!(
        v["held"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Survives power cycles"),
        "a held line must say what it MEANS: {v}"
    );
    assert!(
        v["held"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("12 s ago"),
        "and how old the controller reading is, by the server's clock: {v}"
    );
    assert!(
        !v["held"]["label"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("edl"),
        "the row is about what the controller holds, not about observed EDL: {v}"
    );

    assert!(
        v["clear"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("none held"),
        "{v}"
    );
    for unsure in ["unknown", "unsupported", "not_read"] {
        let text = v[unsure]["text"].as_str().unwrap_or_default();
        assert!(
            !text.contains("none held"),
            "`{unsure}` must never render as none held; nobody has checked: {v}"
        );
        assert!(
            v[unsure]["held"].as_array().is_some_and(|a| a.is_empty()),
            "{v}"
        );
    }
}

/// Normal boot is its own press and its own endpoint.
///
/// Not a Clear followed by a Cycle from the page: two requests have a gap
/// between them that anything can get into, and mcpd's promise (release, PROVE
/// it, then cycle, under one claim) cannot be kept by a browser.
#[test]
fn the_normal_boot_button_is_one_request_to_its_own_endpoint() {
    let _serial = one_browser_at_a_time();
    let Some(bin) = chromium() else {
        eprintln!("SKIP: no chromium");
        return;
    };
    let browser = cdp::Browser::launch(&bin).expect("chromium");
    let rig = Rig::start_with_board();
    let probe = r#"
      (() => {
        window.__calls = [];
        const real = window.fetch;
        window.fetch = (...a) => {
          window.__calls.push({url: String(a[0]), method: (a[1] || {}).method || "GET"});
          return real(...a);
        };
        return new Promise(r => {
          const t0 = Date.now();
          const tick = () => {
            const b = [...document.querySelectorAll("button.act")]
              .find(x => /^\s*normal boot\s*$/i.test(x.textContent || ""));
            if (!b) {
              if (Date.now() - t0 > 10000) r(JSON.stringify({error: "no Normal boot button"}));
              else setTimeout(tick, 200);
              return;
            }
            b.click();
            setTimeout(() => r(JSON.stringify({
              calls: window.__calls.filter(c => c.method === "POST"),
              title: b.title,
            })), 1500);
          };
          tick();
        });
      })()
    "#;
    let out = browser.eval_after_load(
        &format!("{}/?nostream=1", rig.base),
        Duration::from_millis(1500),
        probe,
    );
    let v: Value = serde_json::from_str(out.as_str().unwrap_or("null")).unwrap_or(Value::Null);
    assert!(v.get("error").is_none(), "{v}");
    let posts: Vec<&str> = v["calls"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["url"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(posts.len(), 1, "one press is one request: {v}");
    // The panel is drawn from the controller's own row, so that is the device a
    // press names (the Power buttons beside it do the same). What matters is
    // that it is a real device of THIS board and never `undefined`.
    assert!(
        posts[0].contains("/api/normal_boot/") && posts[0].to_lowercase().contains("click"),
        "to its own endpoint, naming a real device of this board: {v}"
    );
    assert!(
        !posts[0].contains("undefined") && !posts[0].contains("null"),
        "a press that names no device actuates nothing, or the wrong board: {v}"
    );
    assert!(
        v["title"]
            .as_str()
            .unwrap_or_default()
            .contains("Aborts before cycling"),
        "and the button must say what it refuses to do: {v}"
    );
}
