//! MCP over stdio: one JSON-RPC message per line, in both lifecycles.
//!
//! The legacy lifecycle starts with `initialize`. In 2026-07-28 there is no
//! handshake: each request names its protocol version and the client's
//! capabilities in `params._meta`, and `server/discover` describes the
//! server. Only tools are offered.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};

use crate::tools::{self, Tools};

/// The version without a handshake.
pub const MODERN: &str = "2026-07-28";
/// Versions with an `initialize` handshake, newest first. 2025-03-26 isn't
/// offered, because it alone requires JSON-RPC batches.
pub const LEGACY: [&str; 3] = ["2025-11-25", "2025-06-18", "2024-11-05"];
pub const SUPPORTED: [&str; 4] = [MODERN, LEGACY[0], LEGACY[1], LEGACY[2]];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";
/// How long a client may reuse `tools/list` and `server/discover`. The
/// tools never change while the server runs.
const TTL_MS: u64 = 60 * 60 * 1000;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const UNSUPPORTED_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "Subscription usage limits, pacing, reset times, and token \
usage from local logs for Claude Code, Codex, Droid, and Antigravity. Each tool takes \
an optional harness; the default is the agent calling the tool.";

pub struct Server<W> {
    out: Mutex<W>,
    session: Mutex<Session>,
    tools: Tools,
}

#[derive(Default)]
struct Session {
    /// The version agreed by `initialize`, in the legacy lifecycle.
    legacy: Option<String>,
    /// The client's name, from `initialize` or from request metadata.
    client: Option<String>,
    /// Tool calls still running, by id as JSON text.
    running: HashSet<String>,
    /// Running calls the client has cancelled. They get no reply.
    cancelled: HashSet<String>,
}

/// Why a request was refused before it ran.
struct Refusal {
    code: i64,
    message: String,
    data: Option<Value>,
}

/// How a request is answered.
#[derive(Clone, Copy, PartialEq)]
enum Era {
    Legacy,
    /// Results carry `"resultType": "complete"`.
    Modern,
}

impl<W: Write + Send + 'static> Server<W> {
    pub fn new(tools: Tools, out: W) -> Arc<Self> {
        Arc::new(Server {
            out: Mutex::new(out),
            session: Mutex::new(Session::default()),
            tools,
        })
    }

    /// Reads messages until the input ends. Tool calls run on threads of
    /// their own and may still be running when this returns.
    pub fn serve(self: &Arc<Self>, mut input: impl BufRead) {
        let mut line = Vec::new();
        loop {
            line.clear();
            match input.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(message) => self.handle(message),
                Err(_) => self.send(&error(&Value::Null, PARSE_ERROR, "Parse error", None)),
            }
        }
    }

    fn handle(self: &Arc<Self>, message: Value) {
        let Some(object) = message.as_object() else {
            let text = if message.is_array() {
                "Batches aren't supported"
            } else {
                "Invalid request"
            };
            return self.send(&error(&Value::Null, INVALID_REQUEST, text, None));
        };
        let id = object.get("id").cloned();
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let id = id.unwrap_or(Value::Null);
            return self.send(&error(&id, INVALID_REQUEST, "Invalid request", None));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // A response from the client. This server sends no requests.
            return;
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        match id {
            None => self.notify(method, &params),
            Some(id @ (Value::String(_) | Value::Number(_))) => self.request(id, method, params),
            Some(_) => self.send(&error(
                &Value::Null,
                INVALID_REQUEST,
                "Invalid request id",
                None,
            )),
        }
    }

    fn notify(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            let key = params["requestId"].to_string();
            let mut session = self.lock();
            if session.running.contains(&key) {
                session.cancelled.insert(key);
            }
        }
    }

    fn request(self: &Arc<Self>, id: Value, method: &str, params: Value) {
        if method == "initialize" {
            return self.initialize(&id, &params);
        }
        let era = match self.era(&params) {
            Ok(era) => era,
            Err(refusal) => {
                return self.send(&error(&id, refusal.code, &refusal.message, refusal.data));
            }
        };
        let reply = match method {
            "ping" => Ok(json!({})),
            "server/discover" => Ok(discover()),
            "tools/list" => Ok(with_cache_hints(tools::list())),
            "tools/call" => return self.call(id, era, params),
            _ => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
        };
        match reply {
            Ok(result) => self.send(&success(&id, era, result)),
            Err((code, message)) => self.send(&error(&id, code, &message, None)),
        }
    }

    /// The legacy handshake. A client asking for a version without one, or
    /// one this server doesn't know, is offered the newest legacy version.
    fn initialize(&self, id: &Value, params: &Value) {
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let version = LEGACY
            .into_iter()
            .find(|v| *v == asked)
            .unwrap_or(LEGACY[0]);
        {
            let mut session = self.lock();
            session.legacy = Some(version.to_owned());
            note_client(&mut session, &params["clientInfo"], version);
        }
        let result = json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": server_info(),
            "instructions": INSTRUCTIONS,
        });
        self.send(&success(id, Era::Legacy, result));
    }

    /// Which era a request belongs to, from its metadata. A request with
    /// no version is legacy once `initialize` has run, and refused before.
    fn era(&self, params: &Value) -> Result<Era, Refusal> {
        let meta = &params["_meta"];
        let Some(version) = meta.get(META_VERSION) else {
            if self.lock().legacy.is_some() {
                return Ok(Era::Legacy);
            }
            return Err(Refusal {
                code: INVALID_PARAMS,
                message: format!("Missing _meta[\"{META_VERSION}\"]; call initialize or send it."),
                data: None,
            });
        };
        let Some(version) = version.as_str().filter(|v| SUPPORTED.contains(v)) else {
            return Err(Refusal {
                code: UNSUPPORTED_VERSION,
                message: "Unsupported protocol version".to_owned(),
                data: Some(json!({ "requested": version, "supported": SUPPORTED })),
            });
        };
        if version != MODERN {
            return Ok(Era::Legacy);
        }
        if !meta.get(META_CAPABILITIES).is_some_and(Value::is_object) {
            return Err(Refusal {
                code: INVALID_PARAMS,
                message: format!("Missing _meta[\"{META_CAPABILITIES}\"]."),
                data: None,
            });
        }
        let mut session = self.lock();
        if session.client.is_none() {
            note_client(&mut session, &meta[META_CLIENT], MODERN);
        }
        Ok(Era::Modern)
    }

    /// Runs a tool on its own thread. A cancelled call gets no reply.
    fn call(self: &Arc<Self>, id: Value, era: Era, params: Value) {
        let Some(name) = params["name"].as_str() else {
            return self.send(&error(&id, INVALID_PARAMS, "Missing tool name", None));
        };
        if !tools::exists(name) {
            let message = format!("Unknown tool: {name}");
            return self.send(&error(&id, INVALID_PARAMS, &message, None));
        }
        let key = id.to_string();
        let client = {
            let mut session = self.lock();
            session.running.insert(key.clone());
            session.client.clone()
        };
        let server = Arc::clone(self);
        let name = name.to_owned();
        let (thread_id, thread_key) = (id.clone(), key.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("call-{name}"))
            .spawn(move || {
                let (id, key) = (thread_id, thread_key);
                let arguments = params.get("arguments").unwrap_or(&Value::Null);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    server.tools.call(&name, arguments, client.as_deref())
                }));
                let reply = match outcome {
                    Ok(result) => success(&id, era, result),
                    Err(_) => error(&id, INTERNAL_ERROR, "The tool failed unexpectedly", None),
                };
                let cancelled = {
                    let mut session = server.lock();
                    session.running.remove(&key);
                    session.cancelled.remove(&key)
                };
                if !cancelled {
                    server.send(&reply);
                }
            });
        if spawned.is_err() {
            self.lock().running.remove(&key);
            self.send(&error(&id, INTERNAL_ERROR, "Couldn't start the tool", None));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes one message as one line. A closed stdout is ignored; the end
    /// of stdin ends the server.
    fn send(&self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    }
}

/// Logs the client once, to stderr, so a harness's name can be matched.
fn note_client(session: &mut Session, info: &Value, version: &str) {
    if let Some(name) = info["name"].as_str() {
        session.client = Some(name.to_owned());
        let client_version = info["version"].as_str().unwrap_or("?");
        eprintln!("to11y: client {name} {client_version}, protocol {version}");
    }
}

fn server_info() -> Value {
    json!({ "name": "to11y", "version": env!("CARGO_PKG_VERSION") })
}

fn discover() -> Value {
    with_cache_hints(json!({
        "supportedVersions": SUPPORTED,
        "capabilities": { "tools": {} },
        "instructions": INSTRUCTIONS,
        "_meta": { META_SERVER: server_info() },
    }))
}

fn with_cache_hints(mut result: Value) -> Value {
    let object = result.as_object_mut().expect("results are objects");
    object.insert("ttlMs".to_owned(), json!(TTL_MS));
    object.insert("cacheScope".to_owned(), json!("public"));
    result
}

fn success(id: &Value, era: Era, mut result: Value) -> Value {
    if let (Era::Modern, Some(object)) = (era, result.as_object_mut()) {
        object.insert("resultType".to_owned(), json!("complete"));
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut body = Map::new();
    body.insert("code".to_owned(), json!(code));
    body.insert("message".to_owned(), json!(message));
    if let Some(data) = data {
        body.insert("data".to_owned(), data);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Feeds `lines` to a fresh server and returns its replies. Tool calls
    /// are left out: they reply from other threads.
    fn exchange(lines: &[Value]) -> Vec<Value> {
        let out = Shared::default();
        let server = Server::new(Tools::new(Env::default()), out.clone());
        let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
        server.serve(input.as_bytes());
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn modern(method: &str, id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"_meta": {
            META_VERSION: MODERN,
            META_CAPABILITIES: {},
            META_CLIENT: {"name": "test-client", "version": "1"},
        }}})
    }

    #[test]
    fn initialize_agrees_on_a_legacy_version() {
        for (asked, agreed) in [
            ("2025-06-18", "2025-06-18"),
            ("2026-07-28", "2025-11-25"),
            ("2025-03-26", "2025-11-25"),
            ("1999-01-01", "2025-11-25"),
        ] {
            let replies = exchange(&[json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": asked, "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}})]);
            assert_eq!(
                replies[0]["result"]["protocolVersion"], agreed,
                "asked {asked}"
            );
            assert_eq!(replies[0]["result"]["capabilities"], json!({"tools": {}}));
            assert_eq!(replies[0]["result"].get("resultType"), None);
        }
    }

    #[test]
    fn modern_results_say_they_are_complete() {
        let replies = exchange(&[
            modern("server/discover", 1),
            modern("tools/list", 2),
            modern("ping", 3),
        ]);
        for reply in &replies {
            assert_eq!(reply["result"]["resultType"], "complete", "{reply}");
        }
        let discover = &replies[0]["result"];
        assert_eq!(discover["supportedVersions"], json!(SUPPORTED));
        assert_eq!(discover["_meta"][META_SERVER]["name"], "to11y");
        for result in [&replies[0]["result"], &replies[1]["result"]] {
            assert_eq!(result["ttlMs"], TTL_MS);
            assert_eq!(result["cacheScope"], "public");
        }
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 17);
    }

    #[test]
    fn a_request_without_a_version_or_handshake_is_refused() {
        let replies = exchange(&[json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"})]);
        assert_eq!(replies[0]["error"]["code"], INVALID_PARAMS);
        assert_eq!(replies[0]["id"], 7);
    }

    #[test]
    fn a_modern_request_without_capabilities_is_refused() {
        let mut request = modern("tools/list", 1);
        request["params"]["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(META_CAPABILITIES);
        assert_eq!(exchange(&[request])[0]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn an_unknown_version_is_refused_with_the_supported_list() {
        let mut request = modern("tools/list", 1);
        request["params"]["_meta"][META_VERSION] = json!("2099-01-01");
        let reply = &exchange(&[request])[0];
        assert_eq!(reply["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(reply["error"]["data"]["supported"], json!(SUPPORTED));
        assert_eq!(reply["error"]["data"]["requested"], "2099-01-01");
    }

    #[test]
    fn legacy_requests_work_after_initialize() {
        let replies = exchange(&[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": "two", "method": "tools/list"}),
        ]);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["id"], "two");
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 17);
        assert_eq!(replies[1]["result"].get("resultType"), None);
    }

    #[test]
    fn bad_messages_get_json_rpc_errors() {
        let out = Shared::default();
        let server = Server::new(Tools::new(Env::default()), out.clone());
        server.serve(&b"{not json\n[1,2]\n\n{\"jsonrpc\":\"1.0\",\"id\":3,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n"[..]);
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let codes: Vec<i64> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["error"]["code"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            codes,
            [
                PARSE_ERROR,
                INVALID_REQUEST,
                INVALID_REQUEST,
                INVALID_REQUEST
            ]
        );
    }

    #[test]
    fn unknown_methods_and_tools_are_errors() {
        let mut call = modern("tools/call", 2);
        call["params"]["name"] = json!("no_such_tool");
        let replies = exchange(&[modern("resources/list", 1), call]);
        assert_eq!(replies[0]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(replies[1]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn notifications_and_client_responses_get_no_reply() {
        let replies = exchange(&[
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 9}}),
            json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
        ]);
        assert!(replies.is_empty());
    }
}
