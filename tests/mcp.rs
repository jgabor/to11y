//! The built binary over stdio: driven by rmcp's client in each lifecycle,
//! and by raw transcripts for what that client doesn't check.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use common::Sandbox;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::{Value, json};

async fn connect(
    sandbox: &Sandbox,
    client: &str,
    lifecycle: ClientLifecycleMode,
) -> RunningService<RoleClient, ClientConfig> {
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new(client, "1.0"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_11_25);
    let (transport, _) = TokioChildProcess::builder(sandbox.tokio_command())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    config
        .serve_with_lifecycle(transport, lifecycle)
        .await
        .unwrap()
}

/// The JSON a tool result carries in its text block.
fn text(result: &CallToolResult) -> Value {
    let result = serde_json::to_value(result).unwrap();
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn call(name: &'static str, arguments: Value) -> CallToolRequestParams {
    let params = CallToolRequestParams::new(name);
    match arguments {
        Value::Object(map) => params.with_arguments(map),
        _ => params,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rmcp_completes_the_legacy_handshake() {
    let sandbox = Sandbox::new("legacy");
    let client = connect(&sandbox, "claude-code", ClientLifecycleMode::Initialize).await;
    let info = client.peer_info().unwrap();
    assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
    assert_eq!(info.server_info.as_ref().unwrap().name, "to11y");
    assert_eq!(client.list_all_tools().await.unwrap().len(), 17);
    // No harness argument: the client's name says it is Claude Code.
    let result = client
        .call_tool(call("usage_limits", Value::Null))
        .await
        .unwrap();
    let out = text(&result);
    assert_eq!(out["harness"], "claude-code");
    assert_eq!(out["windows"][0]["used_percent"], 6);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rmcp_discovers_the_2026_lifecycle() {
    let sandbox = Sandbox::new("discover");
    let lifecycle = ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    };
    let client = connect(&sandbox, "codex-mcp-client", lifecycle).await;
    assert_eq!(
        client.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    assert_eq!(client.list_all_tools().await.unwrap().len(), 17);
    let result = client
        .call_tool(call("subscription_plan", Value::Null))
        .await
        .unwrap();
    let out = text(&result);
    assert_eq!(
        (out["harness"].as_str(), out["plan"].as_str()),
        (Some("codex"), Some("free"))
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rmcp_in_auto_mode_picks_the_2026_lifecycle() {
    let sandbox = Sandbox::new("auto");
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    };
    let client = connect(&sandbox, "some-editor", lifecycle).await;
    assert_eq!(
        client.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    let result = client
        .call_tool(call("usage_limits_all_harnesses", Value::Null))
        .await
        .unwrap();
    assert_eq!(text(&result)["harnesses"].as_array().unwrap().len(), 4);
    // An unknown client and no argument: the tool asks for a harness.
    let result = client
        .call_tool(call("usage_pace", Value::Null))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let result = client
        .call_tool(call("usage_pace", json!({"harness": "droid"})))
        .await
        .unwrap();
    assert_eq!(text(&result)["harness"], "droid");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn token_tools_read_the_logs_of_the_calling_session() {
    let sandbox = Sandbox::new("tokens").with_logs("planted-session-claude");
    let client = connect(&sandbox, "claude-code", ClientLifecycleMode::Initialize).await;
    let result = client
        .call_tool(call("token_usage_session", Value::Null))
        .await
        .unwrap();
    let out = text(&result);
    assert_eq!(out["harness"], "claude-code");
    assert_eq!(out["tokens"]["requests"], 4);
    let result = client
        .call_tool(call("token_usage_recent", json!({"period": "1y"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let result = client
        .call_tool(call("token_usage_all_harnesses", json!({"period": "7d"})))
        .await
        .unwrap();
    assert_eq!(text(&result)["harnesses"].as_array().unwrap().len(), 4);
    client.cancel().await.unwrap();
}

/// The binary driven line by line. Every line it writes must be JSON.
struct Raw {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Raw {
    fn start(sandbox: &Sandbox) -> Self {
        let mut child = sandbox
            .std_command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    return;
                }
            }
        });
        Raw {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next message, or `None` if none comes within `wait`.
    fn next(&self, wait: Duration) -> Option<Value> {
        match self.lines.recv_timeout(wait) {
            Ok(line) => Some(
                serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("not JSON on stdout: {line}")),
            ),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    fn initialize(&mut self, version: &str) -> Value {
        self.send(
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": version, "capabilities": {},
            "clientInfo": {"name": "raw-test", "version": "1"}}}),
        );
        let reply = self.next(Duration::from_secs(5)).unwrap();
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        reply
    }

    /// Closes stdin and waits for the process to exit.
    fn close(mut self, within: Duration) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if started.elapsed() > within {
                let _ = self.child.kill();
                panic!("still running {within:?} after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn an_initialize_asking_for_2026_is_answered_with_2025_11_25() {
    let sandbox = Sandbox::new("raw-init");
    let mut raw = Raw::start(&sandbox);
    let reply = raw.initialize("2026-07-28");
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    raw.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "usage_reset_times", "arguments": {"harness": "codex"}}}));
    let reply = raw.next(5 * SECOND).unwrap();
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["isError"], false);
    assert!(raw.close(5 * SECOND).success());
}

#[test]
fn modern_requests_are_checked_end_to_end() {
    let sandbox = Sandbox::new("raw-modern");
    let mut raw = Raw::start(&sandbox);
    let meta = |version: &str| {
        json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    };
    raw.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta("2026-07-28")}}));
    let reply = raw.next(5 * SECOND).unwrap();
    assert_eq!(reply["result"]["resultType"], "complete");
    assert!(reply["result"]["ttlMs"].is_u64());
    assert_eq!(reply["result"]["cacheScope"], "public");
    raw.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta("2099-01-01")}}));
    assert_eq!(raw.next(5 * SECOND).unwrap()["error"]["code"], -32022);
    raw.send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
    assert_eq!(raw.next(5 * SECOND).unwrap()["error"]["code"], -32602);
    raw.send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
        "_meta": meta("2026-07-28"), "name": "rate_limit_status", "arguments": {"harness": "antigravity"}}}));
    let reply = raw.next(5 * SECOND).unwrap();
    assert_eq!(reply["result"]["resultType"], "complete");
    assert!(raw.close(5 * SECOND).success());
}

#[test]
fn a_cancelled_call_gets_no_reply() {
    let sandbox = Sandbox::new("raw-cancel").own_fixtures();
    sandbox.fixture("droid.json", r#"{"to11y_delay_ms": 800, "limits": {}}"#);
    let mut raw = Raw::start(&sandbox);
    raw.initialize("2025-11-25");
    raw.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "usage_limits", "arguments": {"harness": "droid"}}}));
    raw.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "test"}}),
    );
    raw.send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    assert_eq!(raw.next(5 * SECOND).unwrap()["id"], 3);
    assert_eq!(raw.next(2 * SECOND), None, "the cancelled call replied");
    assert!(raw.close(5 * SECOND).success());
}

#[test]
fn the_server_exits_when_stdin_ends_even_mid_call() {
    let sandbox = Sandbox::new("raw-eof").own_fixtures();
    sandbox.fixture("droid.json", r#"{"to11y_delay_ms": 10000, "limits": {}}"#);
    let mut raw = Raw::start(&sandbox);
    raw.initialize("2025-11-25");
    raw.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "usage_limits", "arguments": {"harness": "droid"}}}));
    std::thread::sleep(Duration::from_millis(100));
    assert!(raw.close(SECOND).success());
}

#[test]
fn version_and_help_print_without_serving() {
    let sandbox = Sandbox::new("args");
    let out = sandbox.std_command().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().starts_with("to11y "));
    let out = sandbox.std_command().arg("--nope").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
