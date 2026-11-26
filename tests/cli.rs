//! The built binary run as a command: one tool, one JSON document on
//! stdout, and an exit status.

mod common;

use std::io::Write;
use std::process::{Output, Stdio};

use common::Sandbox;
use serde_json::{Value, json};

fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    sandbox.std_command().args(args).output().unwrap()
}

/// The one JSON line on stdout, after checking that stderr is empty.
fn json_line(out: &Output) -> Value {
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout.matches('\n').count(), 1, "{stdout}");
    assert!(stdout.ends_with('\n'));
    serde_json::from_str(stdout).unwrap()
}

#[test]
fn a_tool_prints_one_json_line_and_exits_0() {
    let sandbox = Sandbox::new("cli-ok");
    let out = run(&sandbox, &["usage_limits", "--harness", "codex"]);
    assert_eq!(out.status.code(), Some(0));
    let value = json_line(&out);
    assert_eq!(value["harness"], "codex");
    assert_eq!(value["windows"][0]["window"], "monthly");
}

#[test]
fn pretty_output_is_the_same_document_indented() {
    let sandbox = Sandbox::new("cli-pretty");
    let compact = json_line(&run(&sandbox, &["subscription_plan", "--harness=codex"]));
    let out = run(
        &sandbox,
        &["subscription_plan", "--harness=codex", "--pretty"],
    );
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.lines().count() > 1);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), compact);
}

#[test]
fn an_error_result_exits_1_and_a_stale_one_exits_3() {
    let sandbox = Sandbox::new("cli-errors").own_fixtures();
    sandbox.fixture(
        "droid.json",
        r#"{"to11y_unavailable": {"kind": "keyring_locked"}}"#,
    );
    let out = run(&sandbox, &["usage_limits", "--harness", "droid"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_line(&out)["unavailable"], "keyring_locked");

    run(&sandbox, &["usage_limits", "--harness", "codex"]);
    sandbox.fixture(
        "codex.json",
        r#"{"to11y_unavailable": {"kind": "failed", "detail": "HTTP 500."}}"#,
    );
    let sandbox = sandbox.at("2026-11-23T14:33:10Z");
    let out = run(&sandbox, &["usage_limits", "--harness", "codex"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(json_line(&out)["stale"]["reason"], "failed");
}

/// Twenty hours after the fixtures were captured.
const LATER: &str = "2026-11-24T10:31:10Z";

/// A sandbox in which Droid's last good reading is from `NOW`, with its
/// `core` scope blocked, and every check since fails with `why`. It runs at
/// `LATER`.
fn stale_droid(name: &str, why: &str) -> Sandbox {
    let sandbox = Sandbox::new(name).own_fixtures();
    let out = run(&sandbox, &["rate_limit_status", "--harness", "droid"]);
    assert_eq!(json_line(&out)["blocked"], "all");
    let why: Value = serde_json::from_str(why).unwrap();
    sandbox.fixture(
        "droid.json",
        &json!({ "to11y_unavailable": why }).to_string(),
    );
    sandbox.at(LATER)
}

/// The tools that answer from a reading of the provider.
const LIMIT_TOOLS: [&str; 9] = [
    "usage_limits",
    "usage_pace",
    "usage_forecast",
    "usage_reset_times",
    "usage_remaining_quota",
    "rate_limit_status",
    "usage_extra_credits",
    "subscription_plan",
    "token_usage_limit_windows",
];

#[test]
fn every_limit_tool_puts_a_stale_reading_under_last_known() {
    let sandbox = stale_droid(
        "cli-stale-all",
        r#"{"kind": "failed", "detail": "HTTP 500."}"#,
    );
    for tool in LIMIT_TOOLS {
        let out = run(&sandbox, &[tool, "--harness", "droid"]);
        assert_eq!(out.status.code(), Some(3), "{tool}");
        let value = json_line(&out);
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        let expected: &[&str] = if tool == "rate_limit_status" {
            &["blocked", "harness", "last_known", "scopes", "stale"]
        } else {
            &["harness", "last_known", "stale"]
        };
        assert_eq!(keys, expected, "{tool}: {value}");
        assert_eq!(value["stale"]["age"], "20h", "{tool}");
        assert_eq!(value["stale"]["message"], "HTTP 500.", "{tool}");
        assert_eq!(value["last_known"]["age"], "20h", "{tool}");
        assert!(
            value["last_known"].as_object().unwrap().len() > 1,
            "{tool}: {value}"
        );
    }
}

#[test]
fn rate_limit_status_on_a_stale_reading_says_blocks_are_unknown() {
    let sandbox = stale_droid(
        "cli-stale-blocked",
        r#"{"kind": "failed", "detail": "HTTP 500."}"#,
    );
    let value = json_line(&run(&sandbox, &["rate_limit_status", "--harness", "droid"]));
    assert_eq!(value["blocked"], "unknown");
    assert_eq!(
        value["scopes"],
        json!([
            {"scope": "standard", "blocked": null},
            {"scope": "core", "blocked": null},
        ])
    );
    // The block on `core` lasts past `LATER`; `standard`'s windows reset.
    assert_eq!(value["last_known"]["blocked"], "some");
    assert_eq!(value["last_known"]["scopes"][1]["scope"], "core");
    assert_eq!(value["last_known"]["scopes"][1]["blocked"], true);
    assert_eq!(
        value["last_known"]["scopes"][1]["until"],
        "2026-11-29T01:13:07Z"
    );
    assert_eq!(value["last_known"]["scopes"][0]["blocked"], false);
}

#[test]
fn all_harnesses_says_a_stale_harness_has_unknown_blocks() {
    let sandbox = stale_droid(
        "cli-stale-harnesses",
        r#"{"kind": "failed", "detail": "HTTP 500."}"#,
    );
    let out = run(&sandbox, &["usage_limits_all_harnesses"]);
    assert_eq!(out.status.code(), Some(3));
    let value = json_line(&out);
    let droid = value["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "droid")
        .unwrap();
    assert_eq!(droid["stale"], json!({"age": "20h", "reason": "failed"}));
    assert_eq!(
        droid["scopes"],
        json!([
            {"scope": "standard", "blocked": null},
            {"scope": "core", "blocked": null},
        ])
    );
    assert_eq!(droid["last_known"]["scopes"][1]["blocked"], true);
    let codex = &value["harnesses"][1];
    assert_eq!(codex["harness"], "codex");
    assert!(codex.get("stale").is_none() && codex.get("last_known").is_none());
}

#[test]
fn a_refused_sign_in_is_its_own_stale_reason() {
    let sandbox = stale_droid(
        "cli-stale-refused",
        r#"{"kind": "sign_in_refused", "detail": "Factory refused the stored sign-in (HTTP 401)."}"#,
    );
    let out = run(&sandbox, &["rate_limit_status", "--harness", "droid"]);
    assert_eq!(out.status.code(), Some(3));
    let stale = &json_line(&out)["stale"];
    assert_eq!(stale["reason"], "sign_in_refused");
    assert_eq!(
        stale["message"],
        "Factory refused the stored sign-in (HTTP 401). Sign in to droid again; to11y never refreshes logins."
    );
}

#[test]
fn a_bad_command_line_exits_2_with_the_reason_on_stderr() {
    let sandbox = Sandbox::new("cli-usage");
    for args in [
        &["usage_limits"][..],
        &["usage_limits", "--period", "7d"],
        &["usage_limits", "--harness"],
        &["usage_limits", "--harness", "cursor"],
        &["token_usage_recent", "--harness", "codex", "--period", "1y"],
        &["cursor_usage"],
        &["--nope"],
    ] {
        let out = run(&sandbox, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.starts_with("to11y: "), "{args:?}: {stderr}");
        assert!(
            stderr.ends_with("Run to11y --help.\n"),
            "{args:?}: {stderr}"
        );
    }
    let stderr = String::from_utf8(run(&sandbox, &["usage_limits"]).stderr).unwrap();
    assert!(stderr.contains("claude-code, codex, droid, antigravity"));
}

#[test]
fn the_harness_can_come_from_to11y_harness() {
    let sandbox = Sandbox::new("cli-env-harness");
    let out = sandbox
        .std_command()
        .arg("usage_limits")
        .env("TO11Y_HARNESS", "codex")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_line(&out)["harness"], "codex");
    let out = sandbox
        .std_command()
        .arg("usage_limits_all_harnesses")
        .env("TO11Y_HARNESS", "codex")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_line(&out)["harnesses"].as_array().unwrap().len(), 4);
}

#[test]
fn token_tools_read_the_logs() {
    let sandbox = Sandbox::new("cli-tokens").with_logs("planted-session-claude");
    let out = run(
        &sandbox,
        &[
            "token_usage_recent",
            "--harness",
            "claude-code",
            "--period",
            "7d",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let value = json_line(&out);
    assert_eq!(value["period"], "7d");
    assert!(value["tokens"]["input"].as_u64().unwrap() > 0, "{value}");
    assert!(!value.to_string().contains("planted"), "{value}");
}

#[test]
fn the_tool_list_matches_tools_list_over_mcp() {
    let sandbox = Sandbox::new("cli-tools");
    let listed = json_line(&run(&sandbox, &["tools"]));
    let mut child = sandbox
        .std_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    let mut stdin = child.stdin.take().unwrap();
    for message in messages {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let served = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .unwrap();
    assert_eq!(listed["tools"], served["result"]["tools"]);
    assert_eq!(listed["tools"].as_array().unwrap().len(), 17);
}

#[test]
fn a_closed_stdout_does_not_panic() {
    let sandbox = Sandbox::new("cli-pipe");
    let mut child = sandbox
        .std_command()
        .args(["tools", "--pretty"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn help_lists_the_commands() {
    let sandbox = Sandbox::new("cli-help");
    for args in [&["--help"][..], &["usage_limits", "--help"]] {
        let out = run(&sandbox, args);
        assert_eq!(out.status.code(), Some(0));
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("to11y <tool> [options]"), "{text}");
    }
}
