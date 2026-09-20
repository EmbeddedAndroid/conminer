//! JSON-RPC dispatch: `initialize`, `tools/list`, `tools/call`, `ping`.
//!
//! One rule matters more than the rest here: a tool that fails returns a
//! *successful* JSON-RPC response whose result carries `isError: true` and the
//! structured `{code, message, hint, detail}` payload of §14.6. Protocol-level
//! errors are reserved for protocol-level problems (bad method, malformed
//! params) — an agent should never have to distinguish "the device is ambiguous"
//! from "the server is broken" by parsing a string.

use crate::protocol::*;
use crate::state::Context;
use crate::tools;
use conminer_core::error::ToolError;
use serde_json::{json, Map, Value};

pub struct Handler {
    ctx: Context,
    server_name: String,
    server_version: String,
}

impl Handler {
    pub fn new(ctx: Context) -> Self {
        Self {
            ctx,
            server_name: "conminer".into(),
            server_version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// Handle one request. Returns `None` for notifications, which take no reply.
    pub fn handle(&self, req: Request) -> Option<Response> {
        if !req.jsonrpc.is_empty() && req.jsonrpc != JSONRPC {
            return req
                .id
                .clone()
                .map(|id| Response::err(id, INVALID_REQUEST, "jsonrpc must be \"2.0\"", None));
        }
        if req.is_notification() {
            // `notifications/initialized` and friends: acknowledged by silence,
            // which is what JSON-RPC requires.
            return None;
        }
        let id = req.id.clone().unwrap_or(Value::Null);

        let result: Value = match req.method.as_str() {
            "initialize" => self.initialize(),
            "ping" => json!({}),
            "tools/list" => tools::advertise_profile(self.ctx.config().api.full_toolset),
            "tools/call" => return Some(self.call_tool(id, &req.params)),
            "resources/list" => json!({"resources": self.resources()}),
            "prompts/list" => json!({"prompts": []}),
            other => {
                return Some(Response::err(
                    id,
                    METHOD_NOT_FOUND,
                    format!("unknown method {other:?}"),
                    None,
                ))
            }
        };
        Some(Response::ok(id, result))
    }

    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {"listChanged": false},
                // Novel-template and stage-transition events are published as
                // resource-updated notifications (§8).
                "resources": {"subscribe": true, "listChanged": true},
            },
            "serverInfo": {"name": self.server_name, "version": self.server_version},
            // §F8. THE PROCEDURAL KNOWLEDGE, not the schema.
            //
            // Everything expensive an agent learns on this rig is procedural:
            // mark before actuating, epoch-boundary lag, verify power with
            // diagnose rather than the dashboard. Four rounds of findings were
            // produced by an agent that had to rediscover each one. Schemas are
            // already self-describing; this is the part that is not.
            //
            // Kept terse on purpose -- it is prepended to every client's
            // context, so it earns its space or it goes.
            "instructions":
                "conminer mines consoles into templates, epochs and stages so you never read raw \
                 logs.\n\
                 LOOP: acquire → mark (or target_mark / power {target} on multi-console boards) → \
                 power/boot_mode → follow {until: stage|pattern|prompt|quiet|watch} → boot_report \
                 → list_templates {vs_baseline or min_severity} → get_records/get_context ONLY for \
                 suspects → annotate_template so the next session starts warm.\n\
                 RULES THAT SAVE HOURS:\n\
                 - Multi-console boards: actuate by TARGET. Epochs are per-device and the boot \
                 evidence lands on the console that talks, not the one you actuated; \
                 boot_report.sibling_epochs points at the others.\n\
                 - The first seconds of a boot may land in the PREVIOUS epoch (boundary lag): for \
                 firmware-stage evidence query the epoch before, or filter by stage.\n\
                 - Verify power with `diagnose` (its own probe + EDL + usb_zombies). NEVER the \
                 dashboard power field (cached, seconds stale) and never bare lsusb (cached \
                 descriptors: a dead gadget still lists).\n\
                 - A silent console is off OR in EDL OR idle at a prompt. diagnose separates all \
                 three; console_state names the prompt and says why it is not commandable.\n\
                 - A board that returns to EDL after EVERY power cycle is usually a LATCHED \
                 OVERRIDE, not dead firmware: a strap-latching controller holds a boot-mode line \
                 through power cycles, and `power` deliberately releases nothing (a flash \
                 depends on that). `diagnose` and `boot_overrides` read what is held back from \
                 the controller, separately from the observed `edl`; unknown is never clear. \
                 `normal_boot` releases, PROVES it by readback, then cycles, and aborts before \
                 cycling (NORMAL_BOOT_ABORTED) if it cannot prove the release.\n\
                 - Page with `next_offset` from the response, never by the limit you asked for.\n\
                 - Template `count` is frequency, not chronology. list_templates is a table of \
                 contents.\n\
                 - Costs: run_command ~7 s minimum (paced, echo-verified TX -- batch with `;`); \
                 power off spends up to 8 s excluding EDL before it answers; pull_file is for \
                 small files.\n\
                 - `dry_run: true` on power/boot_mode/normal_boot/flash shows the exact hook argv \
                 and changes nothing.\n\
                 - Errors are structured: branch on `code`; `hint` and `detail.accepted` are \
                 accurate. `freshness.boot_id` tells you which epoch you are reading.\n\
                 - THIS MCP IS THE ONLY WAY TO TOUCH A CONSOLE. Never telnet, nc, socat or open \
                 the ser2net port yourself, and never write to /dev/tty* directly. Every tool \
                 here takes a lease, paces and echo-verifies TX, attributes bytes to an epoch, \
                 and hands the tty back afterwards; a raw connection does none of that. It has \
                 cost this bench real consoles -- a moment of tty contention leaves ser2net \
                 serving \"Device open failure\" to everyone until it is restarted, and bytes you \
                 send that way are invisible to every later question you ask about the boot. Use \
                 run_command, send, follow and get_recent.\n\
                 - FOUND A BUG IN CONMINER? Report it: list_reports {query} first, then \
                 confirm_report {id} if it is already known, or report_issue {title, expected, \
                 observed, tool, args} if it is new. Device, epoch, cursor and build are attached \
                 for you. Say what you EXPECTED and what you OBSERVED -- that pair is what turns \
                 your report into a regression test. Reporting beats working around it silently: \
                 a workaround costs every later agent the same hour.",
        })
    }

    fn resources(&self) -> Vec<Value> {
        self.ctx
            .registry()
            .all_devices()
            .unwrap_or_default()
            .iter()
            .map(|d| {
                json!({
                    "uri": format!("conminer://device/{}", d.display_name()),
                    "name": d.display_name(),
                    "description": "Serial console: novel templates and stage transitions",
                    "mimeType": "application/json",
                })
            })
            .collect()
    }

    fn call_tool(&self, id: Value, params: &Value) -> Response {
        let name = match params.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => return Response::err(id, INVALID_PARAMS, "tools/call requires `name`", None),
        };
        let Some(tool) = tools::find(name) else {
            return Response::err(
                id,
                METHOD_NOT_FOUND,
                format!("unknown tool {name:?}"),
                Some(json!({
                    "available": tools::registry().iter().map(|t| t.name).collect::<Vec<_>>()
                })),
            );
        };
        let empty = Map::new();
        let mut args = params
            .get("arguments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or(empty);

        // UNIVERSAL ARGUMENTS, handled here rather than in sixty schemas: they
        // are about the shape of the response, not about what the tool does.
        // Taken out of `args` before validation, so a tool never has to know
        // they exist.
        let opts = tools::CallOpts {
            path: Vec::new(),
            // Preserved by set_call_opts: the origin arrives with the request,
            // not with the arguments.
            origin: String::new(),
            envelope: args
                .remove("freshness")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            strip_ansi: !matches!(
                args.remove("ansi").as_ref().and_then(Value::as_str),
                Some("keep")
            ),
        };
        tools::set_call_opts(opts);
        // A RENAME IS NOT A REASON TO BREAK A CALLER.
        //
        // The lease arguments were `owner` and `ttl` and are now `holder` and
        // `ttl_s`; the strict check below turned every call written against the
        // old names into INVALID_ARGUMENT, with no hint that the argument still
        // exists under another name. Reported by an agent mid-flash. The old
        // spellings are accepted, mapped, and named in the reply so the caller
        // can move on rather than guess.
        let mut args = args;
        let renamed = apply_arg_aliases(tool, &mut args);

        // Unknown arguments are an error, not a silent no-op: a typo'd `sesion`
        // would otherwise quietly widen the query to the whole device.
        if let Err(e) = check_arguments(tool, &args) {
            return Response::ok(id, tool_result(error_payload(&e), true));
        }
        let args = &args;

        // ESCAPE SEQUENCES LEAVE HERE OR NOWHERE.
        //
        // Stripping them field by field was tried and does not hold: there are
        // ~40 places that put console bytes into a response (record text,
        // template examples, diffs, follow, timeline, target_context...), a
        // round-4 pass through eight of them still shipped `[0;1;31m` in fields
        // nobody had thought of, and every new tool is another chance to forget.
        // One transform at the boundary covers all of them, including tools not
        // written yet, and costs nothing on the common path -- `strip_ansi`
        // returns immediately for text with no ESC in it.
        let opts = tools::call_opts();
        let clean = |mut v: Value| {
            if opts.strip_ansi {
                strip_ansi_deep(&mut v);
            }
            v
        };
        // §P1. FEDERATION LIVES HERE, not in eighty tools. If the selector names
        // a device another node owns, the call goes there and its answer comes
        // back verbatim.
        match crate::route::route_or_call(&self.ctx, tool, args) {
            Ok(v) => Response::ok(id, tool_result(clean(note_renames(v, &renamed)), false)),
            Err(e) => Response::ok(id, tool_result(clean(error_payload(&e)), true)),
        }
    }
}

/// Say which arguments were accepted under an old name.
///
/// Silently accepting a rename is how a caller keeps using a spelling that will
/// eventually be dropped, and never finds out. The call works; the answer tells
/// them what to write next time.
fn note_renames(mut v: Value, renamed: &[(String, String)]) -> Value {
    if renamed.is_empty() {
        return v;
    }
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "renamed_arguments".into(),
            json!(renamed
                .iter()
                .map(|(old, new)| json!({"you_sent": old, "now_called": new}))
                .collect::<Vec<_>>()),
        );
    }
    v
}

/// Strip ANSI/VT escapes from every string in a response, in place.
///
/// Keys are left alone: they are ours, not the board's.
fn strip_ansi_deep(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.contains('\x1b') {
                *s = conminer_core::strip_ansi(s);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_ansi_deep),
        Value::Object(o) => o.values_mut().for_each(strip_ansi_deep),
        _ => {}
    }
}

/// Argument spellings an earlier surface used, and what they are called now.
///
/// Applied only when the tool actually HAS the new argument, so a rename can
/// never invent a parameter for a tool that never had one. Kept deliberately
/// short: this is a compatibility shim for names that shipped, not a synonym
/// dictionary.
const ARG_ALIASES: &[(&str, &str)] = &[("owner", "holder"), ("ttl", "ttl_s")];

fn apply_arg_aliases(tool: &tools::Tool, args: &mut Map<String, Value>) -> Vec<(String, String)> {
    let schema = (tool.schema)();
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut renamed = Vec::new();
    for (old, new) in ARG_ALIASES {
        if !props.contains_key(*new) || props.contains_key(*old) {
            continue;
        }
        if let Some(v) = args.remove(*old) {
            // An explicit new-name argument wins: the caller who used both meant
            // the one they spelled correctly.
            args.entry((*new).to_string()).or_insert(v);
            renamed.push(((*old).to_string(), (*new).to_string()));
        }
    }
    renamed
}

fn check_arguments(tool: &tools::Tool, args: &Map<String, Value>) -> conminer_core::Result<()> {
    let schema = (tool.schema)();
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let unknown: Vec<&String> = args.keys().filter(|k| !props.contains_key(*k)).collect();
    if !unknown.is_empty() {
        return Err(ToolError::invalid_arg(format!(
            "unknown argument(s) for {}: {}",
            tool.name,
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .with_detail(json!({
            "accepted": props.keys().collect::<Vec<_>>(),
            // Universal, accepted by every tool, absent from every schema.
            "always_accepted": {
                "freshness": "false omits the freshness envelope",
                "ansi": "\"keep\" leaves ANSI escapes in console text (default: stripped)",
            },
        })));
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required {
            let Some(k) = r.as_str() else { continue };
            if !args.contains_key(k) {
                return Err(ToolError::invalid_arg(format!(
                    "{} requires argument `{k}`",
                    tool.name
                )));
            }
        }
    }
    Ok(())
}

fn error_payload(e: &ToolError) -> Value {
    json!({
        "error": {
            "code": e.code,
            "message": e.message,
            "hint": e.hint,
            "detail": e.detail,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conminer_core::config::Config;
    use conminer_core::framer::ProfileSet;
    use std::sync::Arc;

    fn handler() -> (tempfile::TempDir, Handler) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.data_dir = dir.path().to_path_buf();
        let ctx = Context::open(
            cfg,
            Arc::new(ProfileSet::builtin().unwrap()),
            Arc::new(conminer_core::clock::StepClock::default()),
        )
        .unwrap();
        (dir, Handler::new(ctx))
    }

    fn req(id: i64, method: &str, params: Value) -> Request {
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))
        .unwrap()
    }

    #[test]
    fn initialize_advertises_the_protocol_and_how_to_use_the_server() {
        let (_d, h) = handler();
        let r = h.handle(req(1, "initialize", json!({}))).unwrap();
        let v = r.result.unwrap();
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["serverInfo"]["name"], "conminer");
        assert!(v["capabilities"]["tools"].is_object());
        assert!(
            v["instructions"]
                .as_str()
                .unwrap()
                .contains("table of contents"),
            "the instructions must tell an agent not to page the log"
        );
    }

    #[test]
    fn notifications_get_no_reply() {
        let (_d, h) = handler();
        let n: Request =
            serde_json::from_value(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .unwrap();
        assert!(h.handle(n).is_none());
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let (_d, h) = handler();
        let r = h.handle(req(1, "does/not/exist", json!({}))).unwrap();
        assert_eq!(r.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn an_unknown_tool_lists_what_is_available() {
        let (_d, h) = handler();
        let r = h
            .handle(req(1, "tools/call", json!({"name": "nope"})))
            .unwrap();
        let e = r.error.unwrap();
        assert_eq!(e.code, METHOD_NOT_FOUND);
        assert!(e.data.unwrap()["available"].as_array().unwrap().len() > 10);
    }

    #[test]
    fn a_tool_failure_is_a_structured_result_not_a_protocol_error() {
        let (_d, h) = handler();
        let r = h
            .handle(req(
                1,
                "tools/call",
                json!({"name": "list_templates", "arguments": {"device": "nope"}}),
            ))
            .unwrap();
        assert!(r.error.is_none(), "the protocol layer succeeded");
        let v = r.result.unwrap();
        assert_eq!(v["isError"], true);
        assert_eq!(v["structuredContent"]["error"]["code"], "UNKNOWN_DEVICE");
        assert!(!v["structuredContent"]["error"]["hint"]
            .as_str()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_typo_in_an_argument_name_is_refused_rather_than_silently_widening_the_query() {
        let (_d, h) = handler();
        let r = h
            .handle(req(
                1,
                "tools/call",
                json!({"name": "list_templates", "arguments": {"sesion": 1}}),
            ))
            .unwrap();
        let v = r.result.unwrap();
        assert_eq!(v["isError"], true);
        assert_eq!(v["structuredContent"]["error"]["code"], "INVALID_ARGUMENT");
        assert!(v["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sesion"));
    }

    #[test]
    fn a_missing_required_argument_is_refused() {
        let (_d, h) = handler();
        let r = h
            .handle(req(
                1,
                "tools/call",
                json!({"name": "get_context", "arguments": {}}),
            ))
            .unwrap();
        let v = r.result.unwrap();
        assert_eq!(v["isError"], true);
        assert!(v["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line_id"));
    }

    #[test]
    fn tools_list_matches_the_registry() {
        let (_d, h) = handler();
        let r = h.handle(req(1, "tools/list", json!({}))).unwrap();
        let tools = r.result.unwrap()["tools"].as_array().unwrap().len();
        // tools/list serves the CORE profile by default, so it is deliberately
        // smaller than the registry. What must hold is that it is a non-empty
        // subset -- every registered tool is still callable by name, and `help`
        // indexes the rest.
        assert!(tools > 0, "tools/list must advertise something");
        assert!(
            tools <= tools::registry().len(),
            "advertised {tools} exceeds the registry"
        );
    }

    #[test]
    fn a_bad_jsonrpc_version_is_rejected() {
        let (_d, h) = handler();
        let bad: Request =
            serde_json::from_value(json!({"jsonrpc":"1.0","id":1,"method":"ping"})).unwrap();
        assert_eq!(h.handle(bad).unwrap().error.unwrap().code, INVALID_REQUEST);
    }

    #[test]
    fn ping_works() {
        let (_d, h) = handler();
        assert!(h.handle(req(1, "ping", json!({}))).unwrap().error.is_none());
    }
}
