//! Transports (§3, §8): streamable HTTP for remote agents, stdio for
//! `docker exec`, plus the observability endpoints of §14.2.
//!
//! Both transports share one `Handler`, so a tool behaves identically however it
//! is reached — a difference between "over HTTP" and "over stdio" would be a bug
//! an agent could not see coming.

use crate::handler::Handler;
use crate::notify::Broadcaster;
use crate::protocol::{Request, Response, PARSE_ERROR};
use crate::state::Context;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Clone)]
pub struct Server {
    handler: Arc<Handler>,
    events: Broadcaster,
}

impl Server {
    pub fn new(ctx: Context) -> Self {
        Self {
            handler: Arc::new(Handler::new(ctx)),
            events: Broadcaster::new(),
        }
    }

    pub fn handler(&self) -> &Arc<Handler> {
        &self.handler
    }

    pub fn events(&self) -> &Broadcaster {
        &self.events
    }

    /// Handle one raw JSON-RPC line/body. Returns the serialized reply, or
    /// `None` for a notification.
    /// Dispatch a request that arrived from another node (§P1).
    ///
    /// The origin rides in a thread-local for the duration of the call, which is
    /// the same mechanism the per-call response options already use, and which
    /// is why it cannot leak into the next request on this thread.
    pub fn dispatch_raw_from(&self, body: &str, origin: &str) -> Option<String> {
        self.dispatch_raw_from_path(body, origin, "")
    }

    /// As `dispatch_raw_from`, but also told the path the call has taken (§P2).
    pub fn dispatch_raw_from_path(&self, body: &str, origin: &str, path: &str) -> Option<String> {
        crate::tools::set_origin(origin);
        crate::tools::set_call_path(path);
        let out = self.dispatch_raw(body);
        crate::tools::set_origin("");
        crate::tools::set_call_path("");
        out
    }

    pub fn dispatch_raw(&self, body: &str) -> Option<String> {
        // A batch is a JSON array; JSON-RPC allows it and some clients send one.
        let value: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    serde_json::to_string(&Response::err(
                        Value::Null,
                        PARSE_ERROR,
                        format!("invalid JSON: {e}"),
                        None,
                    ))
                    .expect("serializable"),
                )
            }
        };

        if let Some(arr) = value.as_array() {
            let mut out = Vec::new();
            for item in arr {
                if let Some(r) = self.dispatch_value(item.clone()) {
                    out.push(r);
                }
            }
            return (!out.is_empty()).then(|| serde_json::to_string(&out).expect("serializable"));
        }

        self.dispatch_value(value)
            .map(|r| serde_json::to_string(&r).expect("serializable"))
    }

    fn dispatch_value(&self, v: Value) -> Option<Response> {
        match serde_json::from_value::<Request>(v) {
            Ok(req) => self.handler.handle(req),
            Err(e) => Some(Response::err(
                Value::Null,
                crate::protocol::INVALID_REQUEST,
                format!("malformed request: {e}"),
                None,
            )),
        }
    }

    /// stdio transport: newline-delimited JSON, the form `docker exec` uses.
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.dispatch_raw(&line) {
                stdout.write_all(reply.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }

    /// Streamable HTTP transport plus `/healthz` and `/metrics` (§14.2).
    pub async fn serve_http(self, addr: SocketAddr) -> anyhow::Result<()> {
        let self_ctx = self.handler.context().clone();
        let app = Router::new()
            .route("/mcp", post(post_mcp).get(get_mcp))
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics))
            .with_state(self);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "mcpd listening");
        // §K4. Watch push runs on its own timer. Advancing watches only when
        // somebody polls is precisely what an unattended overnight soak cannot
        // do, so the sweep is what makes a pushing watch mean anything.
        {
            let ctx = self_ctx.clone();
            tokio::spawn(async move {
                loop {
                    // 15s, not 5: delivery is a notification, and the sweep shares a
                    // bench with actuations that need the write lock.
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    crate::push::sweep(&ctx).await;
                }
            });
        }
        axum::serve(listener, app).await?;
        Ok(())
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/mcp", post(post_mcp).get(get_mcp))
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics))
            .with_state(self)
    }
}

async fn post_mcp(
    State(srv): State<Server>,
    headers: axum::http::HeaderMap,
    body: String,
) -> AxumResponse {
    // §P1. WHO IS ASKING, if this came from another node. Read here and carried
    // per call, so a proxied request leases under the caller's fleet identity
    // and a local one that follows it does not inherit that identity.
    let origin = headers
        .get(conminer_core::peers::client::ORIGIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // §P2. And WHERE IT HAS BEEN, so a relay loop is refused at the moment of
    // forwarding rather than showing up as a hang.
    let path = headers
        .get(conminer_core::peers::client::PATH_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // Tool calls are synchronous SQLite work; keep them off the reactor.
    let reply =
        tokio::task::spawn_blocking(move || srv.dispatch_raw_from_path(&body, &origin, &path))
            .await
            .unwrap_or_else(|e| {
                Some(
                    serde_json::to_string(&Response::err(
                        Value::Null,
                        crate::protocol::INTERNAL_ERROR,
                        format!("handler panicked: {e}"),
                        None,
                    ))
                    .expect("serializable"),
                )
            });
    match reply {
        // A notification gets 202 with no body, as the transport spec requires.
        None => StatusCode::ACCEPTED.into_response(),
        Some(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
    }
}

/// SSE stream of server-initiated notifications (§8: novel template, stage
/// transition), so a supervising agent reacts without polling.
async fn get_mcp(State(srv): State<Server>) -> AxumResponse {
    use futures::StreamExt;
    let rx = srv.events.subscribe();
    let stream = tokio_stream_from(rx).map(|n| {
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&n).unwrap_or_else(|_| "{}".into())
        )))
    });
    AxumResponse::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .expect("valid response")
}

fn tokio_stream_from(
    mut rx: tokio::sync::broadcast::Receiver<crate::protocol::Notification>,
) -> impl futures::Stream<Item = crate::protocol::Notification> {
    futures::stream::poll_fn(move |cx| {
        let fut = rx.recv();
        futures::pin_mut!(fut);
        match futures::FutureExt::poll_unpin(&mut fut, cx) {
            std::task::Poll::Ready(Ok(n)) => std::task::Poll::Ready(Some(n)),
            // A slow subscriber that fell behind stays connected; it will resync
            // from the next event rather than being dropped mid-boot.
            std::task::Poll::Ready(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
}

async fn healthz(State(srv): State<Server>) -> AxumResponse {
    let ctx = srv.handler.context().clone();
    let result = tokio::task::spawn_blocking(move || {
        let devices = ctx.registry().all_devices().map(|d| d.len());
        devices
    })
    .await;
    match result {
        Ok(Ok(n)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({"status": "ok", "devices": n}).to_string(),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "application/json")],
            json!({"status": "unhealthy", "error": e.message}).to_string(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("healthcheck task failed: {e}"),
        )
            .into_response(),
    }
}

/// Prometheus exposition (§14.2). Deliberately includes the counters that make
/// silent failure visible: dropped bytes, template-count growth, queue depth.
async fn metrics(State(srv): State<Server>) -> AxumResponse {
    let ctx = srv.handler.context().clone();
    let body = tokio::task::spawn_blocking(move || {
        let mut out = String::new();
        out.push_str("# HELP conminer_devices Devices known to the registry\n");
        out.push_str("# TYPE conminer_devices gauge\n");
        let devices = ctx.registry().all_devices().unwrap_or_default();
        out.push_str(&format!("conminer_devices {}\n", devices.len()));

        out.push_str("# HELP conminer_lines_total Raw lines stored, per device\n");
        out.push_str("# TYPE conminer_lines_total counter\n");
        out.push_str("# HELP conminer_templates Distinct templates, per device\n");
        out.push_str("# TYPE conminer_templates gauge\n");
        out.push_str(
            "# HELP conminer_fragmentation_ratio Templates per message family; \
             climbing means the no-masking cost is biting\n",
        );
        out.push_str("# TYPE conminer_fragmentation_ratio gauge\n");
        out.push_str("# HELP conminer_db_bytes Device database size on disk\n");
        out.push_str("# TYPE conminer_db_bytes gauge\n");

        for d in &devices {
            let name = d.display_name().replace('"', "");
            if let Ok(s) = ctx.with_store(d, |st| st.stats(None)) {
                out.push_str(&format!(
                    "conminer_lines_total{{device=\"{name}\"}} {}\n",
                    s.lines
                ));
                out.push_str(&format!(
                    "conminer_templates{{device=\"{name}\"}} {}\n",
                    s.templates
                ));
                out.push_str(&format!(
                    "conminer_fragmentation_ratio{{device=\"{name}\"}} {:.4}\n",
                    s.fragmentation_ratio
                ));
                out.push_str(&format!(
                    "conminer_db_bytes{{device=\"{name}\"}} {}\n",
                    s.db_bytes
                ));
            }
        }
        out
    })
    .await
    .unwrap_or_else(|e| format!("# metrics unavailable: {e}\n"));

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conminer_core::config::Config;
    use conminer_core::framer::ProfileSet;

    fn server() -> (tempfile::TempDir, Server) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let ctx = Context::open(
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        (dir, Server::new(ctx))
    }

    #[test]
    fn malformed_json_gets_a_parse_error_not_a_panic() {
        let (_d, s) = server();
        let r = s.dispatch_raw("{not json").unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    #[test]
    fn a_batch_is_answered_as_a_batch() {
        let (_d, s) = server();
        let r = s
            .dispatch_raw(
                r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},
                    {"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_batch_of_only_notifications_gets_no_reply() {
        let (_d, s) = server();
        assert!(s
            .dispatch_raw(r#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#)
            .is_none());
    }

    #[test]
    fn a_request_that_is_not_an_object_is_rejected_cleanly() {
        let (_d, s) = server();
        let r = s.dispatch_raw("42").unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["error"]["code"], crate::protocol::INVALID_REQUEST);
    }
}
