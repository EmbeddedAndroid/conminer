//! A whole MCP server in a temp directory, driven through the real JSON-RPC
//! handler.
//!
//! Tool tests that call the implementation functions directly would pass while
//! the wire contract an agent actually sees was broken: a schema that rejects
//! the argument, an error that comes back as a protocol fault instead of a
//! structured `isError`, a field renamed in the projection. So everything goes
//! through `Handler::handle`, exactly as a client would.

use conminer_core::config::Config;
use conminer_core::framer::ProfileSet;
use conminer_mcp::protocol::Request;
use conminer_mcp::{Context, Handler};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct McpRig {
    pub dir: tempfile::TempDir,
    pub handler: Handler,
}

impl Default for McpRig {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRig {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(mut config: Config) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        config.paths.data_dir = dir.path().to_path_buf();
        let ctx = Context::open(
            config,
            Arc::new(ProfileSet::builtin().expect("profiles")),
            // Deterministic, so a fingerprint or a timing assertion does not
            // depend on how fast the test host is.
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .expect("context");
        Self {
            dir,
            handler: Handler::new(ctx),
        }
    }

    /// Call a tool, asserting success, and return its structured content.
    pub fn call(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(
            v["isError"],
            false,
            "{name} failed: {}",
            serde_json::to_string_pretty(&v["structuredContent"]).unwrap_or_default()
        );
        v["structuredContent"].clone()
    }

    /// Call a tool, asserting failure, and return the structured error.
    pub fn err(&self, name: &str, args: Value) -> Value {
        let v = self.raw(name, args);
        assert_eq!(v["isError"], true, "{name} unexpectedly succeeded: {v}");
        v["structuredContent"]["error"].clone()
    }

    pub fn raw(&self, name: &str, args: Value) -> Value {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))
        .expect("request");
        let resp = self.handler.handle(req).expect("a call always replies");
        assert!(resp.error.is_none(), "protocol error: {:?}", resp.error);
        resp.result.expect("result")
    }

    /// Write text to a file and mine it, returning `(device, session_id)`.
    pub fn ingest(&self, name: &str, text: &str, device: Option<&str>) -> (String, i64) {
        let path = self.dir.path().join(name);
        std::fs::write(&path, text).expect("write corpus");
        let mut args = json!({"path": path.display().to_string()});
        if let Some(d) = device {
            args["device"] = json!(d);
        }
        let r = self.call("ingest_file", args);
        (
            r["device"].as_str().expect("device").to_string(),
            r["ingest"]["session_id"].as_i64().expect("session"),
        )
    }

    /// Byte size of a tool's response — the unit the context window is actually
    /// spent in, used by the response-budget suite.
    pub fn response_bytes(&self, name: &str, args: Value) -> usize {
        serde_json::to_string(&self.call(name, args))
            .map(|s| s.len())
            .unwrap_or(0)
    }
}
