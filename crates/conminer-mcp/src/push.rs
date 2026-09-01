//! §K4. Watch push: delivering firings nobody is waiting for.
//!
//! `follow` covers a parked agent. An unattended overnight soak has no agent at
//! all, and until somebody polls, a board that started flapping at 2am is a fact
//! nobody holds. A watch that can POST closes that gap.
//!
//! Three rules shape it, and each exists because the obvious version is wrong:
//!
//! **Push never consumes a firing.** `poll_watch` stays the source of truth and
//! delivery keeps its own high-water mark. A receiver that was down must not
//! cost the operator the evidence — the whole point of a durable watch is that
//! the record survives the listener.
//!
//! **Firings coalesce.** The ADP's USB flap fires every 2-4 seconds. One POST
//! per firing is a denial-of-service on the receiver dressed up as helpfulness,
//! so a window's worth arrives as one message that says how many it carries.
//!
//! **The URL is allowlisted, mandatorily.** These payloads carry console
//! content. On a LAN-open endpoint an arbitrary URL is an exfiltration
//! primitive that anybody who can reach the API can arm.

use conminer_core::error::{ErrorCode, Result, ToolError};
use serde_json::{json, Value};
use std::time::Duration;

/// Retry schedule from §K4: three attempts after the first, backing off hard.
///
/// A receiver that is down stays down for a while; hammering it helps nobody
/// and the firing is not lost either way.
pub const BACKOFF_S: [u64; 3] = [5, 25, 125];

/// Per-POST budget. Long enough for a sleepy sink, short enough that the
/// scanner thread is never held hostage by one.
pub const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// `sha256=<hex>` over the exact bytes posted.
///
/// Over the BODY, not over the fields: a receiver verifies what arrived rather
/// than re-serialising and hoping its JSON writer matches ours.
pub fn signature(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    // HMAC-SHA256, written out rather than pulled in: one dependency fewer for
    // twenty lines, and the construction is fixed by RFC 2104.
    const BLOCK: usize = 64;
    let mut key = secret.as_bytes().to_vec();
    if key.len() > BLOCK {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(BLOCK, 0);
    let mut ipad = vec![0x36u8; BLOCK];
    let mut opad = vec![0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(body);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner);
    let mac = outer.finalize();
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// The body one delivery carries.
pub fn body(watch: &str, device: &str, firings: &[Value], window_s: i64) -> Value {
    let n = firings.len();
    json!({
        "watch": watch,
        "device": device,
        "firings": firings,
        // Said out loud so a receiver that sees one message knows whether it
        // stands for one event or forty.
        "pending_note": if n > 1 {
            format!("coalesced {n} firings in {window_s}s")
        } else {
            format!("{n} firing")
        },
    })
}

/// Check a URL against the operator's allowlist before it is ever stored.
///
/// At CREATION time, not at delivery: a watch that will refuse to post at 3am
/// is worse than one that refuses to be created now, when somebody is reading
/// the error.
/// §M2. Does this URL point at the conminer container itself?
///
/// Loopback is the one address class that CANNOT reach a receiver on the lab
/// host: mcpd resolves the URL from inside its own container, so `127.0.0.1` is
/// mcpd. The allowlist ships loopback patterns (they are right when conminer
/// runs as a bare binary, and for a sidecar sharing the network namespace),
/// which is exactly what makes the mistake look permitted.
pub fn is_container_loopback(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]") || host.starts_with("127.")
}

/// What to tell a caller who just pointed a webhook at loopback (§M2).
pub const LOOPBACK_NOTE: &str = concat!(
    "this URL resolves INSIDE the conminer container, so 127.0.0.1 is mcpd itself, not the ",
    "lab host. A receiver running on the host needs that host's LAN address (or a compose ",
    "service name); posts to loopback will fail with no connection.",
);

pub fn check_allowed(cfg: &conminer_core::config::NotifyConfig, url: &str) -> Result<()> {
    if cfg.url_allowed(url) {
        return Ok(());
    }
    Err(ToolError::new(
        ErrorCode::InvalidArgument,
        format!("{url:?} is not in the notify allowlist"),
    )
    .with_hint("add a matching pattern to `[notify] allow` in conminer.toml")
    .with_detail(json!({
        "allow": cfg.allow,
        "why": "watch payloads carry console content, so an unrestricted URL on a LAN-open \
                endpoint is an exfiltration primitive: the allowlist is mandatory rather than \
                advisory",
        // Said HERE too, because somebody whose URL was just rejected is about
        // to reach for the entry that looks safest and is the one that cannot
        // work (§M2).
        "note_before_you_reach_for_loopback": LOOPBACK_NOTE,
    })))
}

/// POST one delivery. Returns the HTTP status, or None if it never got that far.
///
/// Hand-rolled over a TCP socket for the same reason the rest of this codebase
/// is: the dependency budget buys capture, not conveniences. Only what a local
/// sink needs is implemented, and anything unexpected is reported rather than
/// guessed at.
pub async fn post(url: &str, body: &[u8], secret: Option<&str>) -> Option<u16> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 80u16),
    };
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(s) = secret {
        req.push_str(&format!("X-Conminer-Signature: {}\r\n", signature(s, body)));
    }
    req.push_str("\r\n");

    let io = tokio::time::timeout(POST_TIMEOUT, async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect((host, port)).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        s.write_all(body).await.ok()?;
        let mut buf = vec![0u8; 512];
        let n = s.read(&mut buf).await.ok()?;
        Some(String::from_utf8_lossy(&buf[..n]).to_string())
    })
    .await
    .ok()??;

    // "HTTP/1.1 200 OK" -> 200
    io.split_whitespace().nth(1).and_then(|c| c.parse().ok())
}

// ------------------------------------------------------------ the sweep -----

/// Devices known to have a watch with push armed, and when that was last
/// established.
static ARMED: std::sync::Mutex<Option<(std::time::Instant, Vec<i64>)>> =
    std::sync::Mutex::new(None);

/// How long the armed-device list is trusted before it is rebuilt.
///
/// Short, because this cache can only ever be wrong in the direction of NOT
/// delivering, and a watch that silently waits two minutes is indistinguishable
/// from one that is broken. 30s bounds that to something a person watching a
/// sink will read as "a moment" rather than as a fault.
const ARMED_TTL: Duration = Duration::from_secs(30);

/// Note that this device now has (or may have) a pushing watch.
///
/// Called when one is created, so a new watch starts delivering on the next
/// sweep rather than after the cache expires.
pub fn invalidate_armed_cache() {
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn device_may_have_armed_watch(
    ctx: &crate::state::Context,
    d: &conminer_core::store::DeviceRow,
) -> bool {
    let mut guard = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = guard
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < ARMED_TTL);
    if !fresh {
        // One rebuild pass, then quiet again until it expires.
        let mut ids = Vec::new();
        if let Ok(all) = ctx.registry().all_devices() {
            for dev in all.iter().filter(|x| !x.ignored) {
                if ctx
                    .with_store(dev, |st| st.watches_to_deliver())
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    ids.push(dev.id);
                }
            }
        }
        *guard = Some((std::time::Instant::now(), ids));
    }
    guard
        .as_ref()
        .map(|(_, ids)| ids.contains(&d.id))
        .unwrap_or(false)
}

/// Advance every armed watch and deliver what it found.
///
/// Runs on its own timer rather than on `poll_watch`, and that is the whole
/// feature: a watch that only advances when somebody polls is exactly the thing
/// an unattended soak does not have. Capture is untouched — this reads and
/// writes the watch tables, never the ingest path.
pub async fn sweep(ctx: &crate::state::Context) {
    let devices = match ctx.registry().all_devices() {
        Ok(d) => d,
        Err(_) => return,
    };
    for d in devices {
        if d.ignored {
            continue;
        }
        // DO NOT OPEN A STORE TO LEARN THERE IS NOTHING TO DO.
        //
        // The first version probed every device's store on every sweep, and
        // opening a store is not free: `open_sqlite` sets `journal_mode=WAL`,
        // which takes a brief WRITE lock. Seventeen stores every five seconds
        // meant a steady drum of write locks against a bench that was also
        // being actuated -- measured on the ADP, where a `power on` came back
        // "database is locked" and the selftest failed a board that was working
        // perfectly.
        //
        // Which devices have an armed watch changes only when somebody creates
        // one, so it is cached and refreshed slowly. A bench with no pushing
        // watches now opens nothing at all.
        if !device_may_have_armed_watch(ctx, &d) {
            continue;
        }
        let armed = ctx
            .with_store(&d, |st| st.watches_to_deliver())
            .unwrap_or_default();
        for w in armed {
            let (name, url, secret, min_interval_s, last_at, fails) = (
                w.name,
                w.url,
                w.secret,
                w.min_interval_s,
                w.last_delivery_at,
                w.failed_streak,
            );
            let now = ctx.now();
            // §L1. THE BACKOFF IS A SCHEDULE, NOT A SLEEP.
            //
            // While a failing receiver was retried inline, this loop sat in
            // `sleep` for up to 5+25+125 s holding every other watch behind it:
            // measured here as a live watch's first post landing 32 s after its
            // firing because a dead endpoint was still being retried ahead of
            // it. Head-of-line blocking in a notifier is indistinguishable, from
            // the outside, from the notifier being broken.
            //
            // So a failed attempt is recorded and the NEXT attempt is simply not
            // due yet. The wait between attempts is the same 5/25/125 s; what
            // changed is who waits -- this watch, rather than the bench.
            let due_in_s = if fails > 0 {
                BACKOFF_S[((fails as usize) - 1).min(BACKOFF_S.len() - 1)]
            } else {
                0
            };
            // COALESCE. The ADP's flap fires every 2-4s; one post per firing is
            // a denial of service on the receiver wearing a helpful face.
            let wait_s = min_interval_s.max(due_in_s as i64);
            if last_at > 0 && now - last_at < wait_s * 1000 {
                continue;
            }
            // Advance the scanner first: a watch nobody polls has seen nothing.
            let scanned = ctx.with_store(&d, |st| {
                let w = st.watch(&name)?;
                // No prompt patterns: a `{prompt:true}` predicate is a
                // poll-time question about what the console is sitting at, and
                // an unattended sweep has no business answering it. Predicates
                // that do not need prompts are unaffected.
                let prompts = conminer_core::follow::PromptSet::empty();
                let (hits, scanned_to) = conminer_core::follow::scan_hits(
                    st,
                    w.scanned_to,
                    &conminer_core::follow::Predicate::parse(&w.predicate)?,
                    1000,
                    &prompts,
                    now,
                )?;
                st.record_watch_hits(w.id, &hits, scanned_to, now)?;
                st.undelivered_hits(&name, 200)
            });
            let Ok(firings) = scanned else { continue };
            if firings.is_empty() {
                continue;
            }
            let high = firings.last().and_then(|f| f["id"].as_i64());
            let payload = body(&name, d.display_name(), &firings, min_interval_s);
            let bytes = serde_json::to_vec(&payload).unwrap_or_default();

            // One attempt per sweep. The firing is NEVER consumed by an
            // unacknowledged post -- poll_watch must still return it, or a flaky
            // receiver would cost the operator the evidence.
            let status = post(&url, &bytes, secret.as_deref()).await;
            if !status.is_some_and(|s| (200..300).contains(&s)) {
                tracing::warn!(
                    watch = %name,
                    attempt = fails + 1,
                    ?status,
                    "watch delivery failed; the next attempt is scheduled, not slept"
                );
            }
            let ok = status.is_some_and(|s| (200..300).contains(&s));
            let _ = ctx.with_store(&d, |st| {
                st.note_delivery(&name, status, ok.then_some(high).flatten(), ctx.now())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2: a known-answer check, so the HMAC is verified
    /// against the standard rather than against itself.
    #[test]
    fn hmac_matches_the_published_vector() {
        let got = signature("Jefe", b"what do ya want for nothing?");
        assert_eq!(
            got,
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// A key longer than the block size is hashed first, per RFC 2104 — the
    /// case a hand-rolled HMAC usually gets wrong.
    #[test]
    fn a_long_key_is_hashed_not_truncated() {
        let long = "a".repeat(200);
        let short = "a".repeat(20);
        assert_ne!(signature(&long, b"x"), signature(&short, b"x"));
        // And it is stable.
        assert_eq!(signature(&long, b"x"), signature(&long, b"x"));
    }

    #[test]
    fn coalescing_is_stated_in_the_body() {
        let f = vec![json!({"at": 1}), json!({"at": 2}), json!({"at": 3})];
        let b = body("flap", "/dev/ttyUSB0", &f, 60);
        assert_eq!(b["firings"].as_array().unwrap().len(), 3);
        assert!(b["pending_note"]
            .as_str()
            .unwrap()
            .contains("coalesced 3 firings in 60s"));
    }
}
