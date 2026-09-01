//! A minimal DevTools-protocol client, enough to click things.
//!
//! `--dump-dom` renders a page but cannot interact with it, so it can prove the
//! dashboard BUILDS and no more. Proving that the button a human presses is
//! actually wired to the endpoint needs a real click on a real element, and that
//! means CDP.
//!
//! The alternative -- a `?autoclick=power:off:<dev>` hook in the page -- was
//! rejected deliberately: it would turn a URL into a hardware actuation
//! primitive, so anything that could get a browser to open a link could power
//! off a board on a dashboard that is LAN-open by design. A test is not worth
//! that.
//!
//! Deliberately small: navigate, evaluate, done. No target lifecycle, no
//! domains beyond Runtime/Page.
//!
//! Included by more than one suite, so each one uses a different subset.
#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Where the browser is, if there is one.
///
/// Lives here rather than in one suite's `main.rs` because two suites need it
/// now: a second copy is a second thing to keep in step with the image.
pub fn chromium_bin() -> Option<String> {
    for c in ["chromium", "chromium-browser", "google-chrome"] {
        if Command::new("which")
            .arg(c)
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some(c.to_string());
        }
    }
    None
}

pub struct Browser {
    child: Child,
    ws_url: String,
    rt: tokio::runtime::Runtime,
    /// Where the tab already is, so a second click on the same page does not
    /// reload it. A dashboard press does not navigate, so neither should the
    /// test: the reload cost ~2.9 s EVERY click (page build + /api/devices +
    /// a fixed settle), and an EDL step performs four of them.
    at: std::cell::RefCell<Option<String>>,
}

impl Browser {
    /// Launch headless chromium with the debugging port open.
    ///
    /// Retried, because a cold chromium on a loaded box is slow to bind its
    /// debug port and the whole suite launches several at once. A launch that
    /// gives up too early fails the test with "chromium would not start", which
    /// says nothing about the code under test.
    pub fn launch(bin: &str) -> Option<Self> {
        for attempt in 0..3 {
            if let Some(b) = Self::launch_once(bin) {
                return Some(b);
            }
            std::thread::sleep(Duration::from_millis(500 * (attempt + 1)));
        }
        None
    }

    fn launch_once(bin: &str) -> Option<Self> {
        let port = pick_port()?;
        let profile = std::env::temp_dir().join(format!("cdp-{}-{port}", std::process::id()));
        let child = Command::new(bin)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                &format!("--remote-debugging-port={port}"),
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Poll for the endpoint rather than sleeping: chromium's startup time
        // varies wildly under load and a fixed wait is either slow or flaky.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let mut ws_url = None;
        // 40s, not 10: measured on a box running the rest of the suite in
        // parallel, chromium needed well past ten seconds just to bind.
        for _ in 0..400 {
            if let Some(u) = page_target(port) {
                ws_url = Some(u);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Some(Self {
            child,
            ws_url: ws_url?,
            rt,
            at: std::cell::RefCell::new(None),
        })
    }

    /// Navigate, then evaluate an expression and return it as JSON.
    ///
    /// The settle delay is not decoration: the page fetches `/api/devices` and
    /// builds itself asynchronously, so evaluating immediately after navigate
    /// reads an empty document and every assertion becomes vacuous.
    pub fn eval_after_load(&self, url: &str, settle: Duration, expr: &str) -> Value {
        self.eval_within(url, settle, expr, Duration::from_secs(20))
    }

    /// As above, with a stated budget for the evaluation itself.
    ///
    /// The default is fine for reading a rendered page. It is not fine for a
    /// click that actuates hardware: `power off` alone spends up to eight
    /// seconds verifying, and cutting the socket at twenty would kill the
    /// request mid-flight and report a broken button that works.
    /// Evaluate with a PHONE's viewport, not a desktop window scaled down.
    ///
    /// `Emulation.setDeviceMetricsOverride` is what makes `innerWidth`, the
    /// media queries and the layout viewport agree with a real handset; resizing
    /// the browser window does not, and a gate written against the window is
    /// measuring something no phone will ever show.
    pub fn eval_on_phone(&self, url: &str, settle: Duration, expr: &str) -> Value {
        self.eval_impl(url, settle, expr, Duration::from_secs(20), Some((390, 844)))
    }

    pub fn eval_within(&self, url: &str, settle: Duration, expr: &str, budget: Duration) -> Value {
        self.eval_impl(url, settle, expr, budget, None)
    }

    fn eval_impl(
        &self,
        url: &str,
        settle: Duration,
        expr: &str,
        budget: Duration,
        phone: Option<(u32, u32)>,
    ) -> Value {
        let ws = self.ws_url.clone();
        let url = url.to_string();
        let expr = expr.to_string();
        // Navigate only when the tab is somewhere else -- but ALWAYS when the
        // viewport is being changed, so the page lays out under the new metrics
        // instead of keeping the layout it was built with.
        let need_nav = phone.is_some() || self.at.borrow().as_deref() != Some(url.as_str());
        if need_nav {
            // A phone view leaves the tab under an override the next caller does
            // not want; forget where we are so it navigates (and re-lays out).
            *self.at.borrow_mut() = phone.is_none().then(|| url.clone());
        }
        self.rt.block_on(async move {
            let (mut sock, _) = tokio_tungstenite::connect_async(&ws)
                .await
                .expect("connect to chromium");

            if let Some((w, h)) = phone {
                let metrics = json!({
                    "id": 4,
                    "method": "Emulation.setDeviceMetricsOverride",
                    "params": {
                        "width": w, "height": h, "deviceScaleFactor": 3, "mobile": true,
                    },
                });
                sock.send(metrics.to_string().into())
                    .await
                    .expect("emulate");
                // WAIT FOR THE ACK BEFORE NAVIGATING. Fire-and-forget raced the
                // load: the page was laid out at the window's width and only the
                // layout viewport ended up at the phone's, so `innerWidth` read
                // 430 against a 390 viewport and the gate failed on geometry
                // that no phone would ever produce.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    let Ok(Some(Ok(msg))) =
                        tokio::time::timeout(Duration::from_secs(5), sock.next()).await
                    else {
                        break;
                    };
                    if serde_json::from_str::<Value>(&msg.to_string()).is_ok_and(|v| v["id"] == 4) {
                        break;
                    }
                }
            }

            if need_nav {
                let nav = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
                sock.send(nav.to_string().into()).await.expect("navigate");
                tokio::time::sleep(settle).await;
            }

            let ev = json!({
                "id": 2,
                "method": "Runtime.evaluate",
                "params": {"expression": expr, "returnByValue": true, "awaitPromise": true},
            });
            sock.send(ev.to_string().into()).await.expect("evaluate");

            // Read until our id comes back; CDP interleaves unsolicited events.
            let deadline = tokio::time::Instant::now() + budget;
            while tokio::time::Instant::now() < deadline {
                // The per-message wait has to be the whole budget too: with
                // `awaitPromise`, a long actuation sends NOTHING until it
                // finishes, so a short read timeout gives up on a call that is
                // working perfectly.
                let Ok(Some(Ok(msg))) = tokio::time::timeout(budget, sock.next()).await else {
                    break;
                };
                let Ok(v): Result<Value, _> = serde_json::from_str(&msg.to_string()) else {
                    continue;
                };
                if v["id"] == 2 {
                    return v["result"]["result"]["value"].clone();
                }
            }
            Value::Null
        })
    }

    /// Navigate, run `setup`, then TYPE -- as a keyboard does -- and return the
    /// value of `check`.
    ///
    /// Dispatching a KeyboardEvent at an element from script bypasses focus
    /// entirely: it proves the handler is wired and nothing whatsoever about
    /// whether a person typing at the page can reach it. Chromium routes these
    /// to whatever actually holds focus, which is the part under test.
    pub fn press_keys(
        &self,
        url: &str,
        settle: Duration,
        setup: &str,
        text: &str,
        check: &str,
    ) -> Value {
        let ws = self.ws_url.clone();
        let url = url.to_string();
        let (setup, check, text) = (setup.to_string(), check.to_string(), text.to_string());
        *self.at.borrow_mut() = Some(url.clone());
        self.rt.block_on(async move {
            let (mut sock, _) = tokio_tungstenite::connect_async(&ws)
                .await
                .expect("connect to chromium");
            let nav = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
            sock.send(nav.to_string().into()).await.expect("navigate");
            tokio::time::sleep(settle).await;

            async fn call(
                sock: &mut tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                id: u64,
                msg: Value,
                budget: Duration,
            ) -> Value {
                sock.send(msg.to_string().into()).await.expect("cdp send");
                let deadline = tokio::time::Instant::now() + budget;
                while tokio::time::Instant::now() < deadline {
                    let Ok(Some(Ok(m))) = tokio::time::timeout(budget, sock.next()).await else {
                        break;
                    };
                    let Ok(v): Result<Value, _> = serde_json::from_str(&m.to_string()) else {
                        continue;
                    };
                    if v["id"] == id {
                        return v["result"]["result"]["value"].clone();
                    }
                }
                Value::Null
            }

            let evaluate = |expr: String, id: u64| {
                json!({
                    "id": id,
                    "method": "Runtime.evaluate",
                    "params": {"expression": expr, "returnByValue": true, "awaitPromise": true},
                })
            };
            let ready = call(&mut sock, 2, evaluate(setup, 2), Duration::from_secs(30)).await;
            if ready.as_str().is_some_and(|s| s.contains("error")) {
                return ready;
            }

            let mut id = 10;
            for ch in text.chars() {
                let (key, code, vk, txt) = match ch {
                    '\r' => (
                        "Enter".to_string(),
                        "Enter".to_string(),
                        13u32,
                        "\r".to_string(),
                    ),
                    c => (
                        c.to_string(),
                        format!("Key{}", c.to_ascii_uppercase()),
                        c.to_ascii_uppercase() as u32,
                        c.to_string(),
                    ),
                };
                for kind in ["keyDown", "keyUp"] {
                    id += 1;
                    let ev = json!({
                        "id": id,
                        "method": "Input.dispatchKeyEvent",
                        "params": {
                            "type": kind,
                            "key": key,
                            "code": code,
                            "text": if kind == "keyDown" { txt.clone() } else { String::new() },
                            "windowsVirtualKeyCode": vk,
                            "nativeVirtualKeyCode": vk,
                        },
                    });
                    sock.send(ev.to_string().into()).await.expect("key");
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }

            call(&mut sock, 3, evaluate(check, 3), Duration::from_secs(20)).await
        })
    }

    /// Photograph the page and write a PNG.
    ///
    /// Geometry assertions catch a gap that is 30px wide; they do not catch a
    /// layout that measures perfectly and looks wrong. When the question is "does
    /// this look right", the answer has to be an image somebody can look at.
    pub fn screenshot(
        &self,
        url: &str,
        settle: Duration,
        out: &std::path::Path,
        phone: bool,
    ) -> bool {
        let ws = self.ws_url.clone();
        let url = url.to_string();
        let need_nav = self.at.borrow().as_deref() != Some(url.as_str());
        if need_nav {
            *self.at.borrow_mut() = Some(url.clone());
        }
        let data: Option<String> = self.rt.block_on(async move {
            let (mut sock, _) = tokio_tungstenite::connect_async(&ws).await.ok()?;
            if phone {
                let metrics = json!({
                    "id": 4,
                    "method": "Emulation.setDeviceMetricsOverride",
                    "params": {"width": 390, "height": 844, "deviceScaleFactor": 3, "mobile": true},
                });
                sock.send(metrics.to_string().into()).await.ok()?;
            }
            if need_nav || phone {
                let nav = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
                sock.send(nav.to_string().into()).await.ok()?;
                tokio::time::sleep(settle).await;
            }
            // The WHOLE page, not the viewport: a rack runs off the bottom of any
            // window worth using, and the seams below the fold are the ones that
            // went unnoticed.
            let shot = json!({
                "id": 3,
                "method": "Page.captureScreenshot",
                "params": {"format": "png", "captureBeyondViewport": true},
            });
            sock.send(shot.to_string().into()).await.ok()?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while tokio::time::Instant::now() < deadline {
                let Ok(Some(Ok(msg))) =
                    tokio::time::timeout(Duration::from_secs(20), sock.next()).await
                else {
                    break;
                };
                let Ok(v): Result<Value, _> = serde_json::from_str(&msg.to_string()) else {
                    continue;
                };
                if v["id"] == 3 {
                    return v["result"]["data"].as_str().map(str::to_string);
                }
            }
            None
        });
        let Some(b64) = data else { return false };
        // Decoded by the base64 that is already in this image rather than by a
        // new crate dependency: this is a looking-at-it helper, not a shipped
        // code path, and the suite's dependency list is not the place to pay for
        // it.
        let tmp = out.with_extension("b64");
        if std::fs::write(&tmp, b64).is_err() {
            return false;
        }
        let ok = Command::new("base64")
            .arg("-d")
            .arg(&tmp)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .is_some_and(|o| std::fs::write(out, o.stdout).is_ok());
        let _ = std::fs::remove_file(&tmp);
        ok
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pick_port() -> Option<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let p = l.local_addr().ok()?.port();
    drop(l);
    Some(p)
}

fn page_target(port: u16) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "1",
            &format!("http://127.0.0.1:{port}/json/list"),
        ])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?
        .iter()
        .find(|t| t["type"] == "page")
        .and_then(|t| t["webSocketDebuggerUrl"].as_str())
        .map(str::to_string)
}
