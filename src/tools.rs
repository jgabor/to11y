//! The ten tools. Each answers one question, so an agent searching for a
//! tool finds the one that answers it. Results are compact JSON in a text
//! block, with relative times so the agent does no date math.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::cache::{Cache, Reading};
use crate::creds::{self, Keyring, SecretService};
use crate::env::Env;
use crate::http;
use crate::model::{Harness, Snapshot, Unavailable};
use crate::pace::{self, Assessed, Scope, Verdict};
use crate::providers::{self, Context};
use crate::time;

/// Name, description, and whether the tool takes a `harness`.
const TOOLS: [(&str, &str, bool); 10] = [
    (
        "usage_limits",
        "Show a coding agent's subscription usage limits: percent used, time to reset, and pace for each 5-hour, weekly, or monthly window.",
        true,
    ),
    (
        "usage_pace",
        "Check whether subscription quota is being used too fast: percent used against percent of the window elapsed, with a safe hourly rate.",
        true,
    ),
    (
        "usage_forecast",
        "Forecast whether a subscription usage limit will run out before it resets, and when, at the current rate of use.",
        true,
    ),
    (
        "usage_reset_times",
        "List when each subscription usage limit window resets, as a timestamp and as time remaining.",
        true,
    ),
    (
        "usage_remaining_quota",
        "Show how much subscription quota is left in each usage window, and the tightest limit for each model group or pool.",
        true,
    ),
    (
        "rate_limit_status",
        "Check whether a coding agent is rate limited right now because a usage limit is used up, and until when.",
        true,
    ),
    (
        "usage_extra_credits",
        "Show extra usage, overage, or credit balance beyond the limits included in a subscription.",
        true,
    ),
    (
        "subscription_plan",
        "Show which subscription plan and tier a coding agent's account is on.",
        true,
    ),
    (
        "usage_limits_all_harnesses",
        "Compare subscription usage limits across Claude Code, Codex, Droid, and Antigravity to find which coding agent has headroom left.",
        false,
    ),
    (
        "usage_login_status",
        "Explain why subscription usage data is missing or stale: sign-in, keyring, and last check for a coding agent.",
        true,
    ),
];

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn exists(name: &str) -> bool {
    TOOLS.iter().any(|(tool, _, _)| *tool == name)
}

/// The `tools/list` result.
pub fn list() -> Value {
    let names: Vec<&str> = Harness::ALL.iter().map(|h| h.name()).collect();
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|(name, description, takes_harness)| {
            let properties = if *takes_harness {
                json!({ "harness": {
                    "type": "string",
                    "enum": names,
                    "description": "Coding agent to check. Defaults to the one calling.",
                }})
            } else {
                json!({})
            };
            json!({
                "name": name,
                "description": description,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "additionalProperties": false,
                },
                "annotations": { "readOnlyHint": true, "openWorldHint": true },
            })
        })
        .collect();
    json!({ "tools": tools })
}

pub struct Tools {
    env: Arc<Env>,
    cache: Cache,
    /// Built on first use, so startup does no work.
    context: OnceLock<Context>,
}

impl Tools {
    pub fn new(env: Env) -> Self {
        Tools {
            cache: Cache::new(env.cache_dir()),
            env: Arc::new(env),
            context: OnceLock::new(),
        }
    }

    fn context(&self) -> &Context {
        self.context.get_or_init(|| Context {
            env: Arc::clone(&self.env),
            keyring: Arc::new(SecretService {
                address: self.env.dbus_address.clone(),
            }),
            client: http::Client::new(HTTP_TIMEOUT),
        })
    }

    /// A `tools/call` result. `client` is the calling harness's MCP client
    /// name, if known.
    pub fn call(&self, name: &str, arguments: &Value, client: Option<&str>) -> Value {
        let now = self.env.now();
        if name == "usage_limits_all_harnesses" {
            return match arguments {
                Value::Null => self.all_harnesses(now),
                Value::Object(map) if map.is_empty() => self.all_harnesses(now),
                _ => failure("usage_limits_all_harnesses takes no arguments."),
            };
        }
        let harness = match self.harness(arguments, client) {
            Ok(harness) => harness,
            Err(message) => return failure(&message),
        };
        if name == "usage_login_status" {
            return result(self.login_status(harness, now), false);
        }
        let reading = self.reading(harness, now);
        let Some(snapshot) = &reading.snapshot else {
            let why = reading.failure.unwrap_or(Unavailable::NotSignedIn);
            return unavailable(harness, &why);
        };
        let (assessed, scopes) = pace::assess(&snapshot.windows, now);
        let mut out = Map::new();
        out.insert("harness".to_owned(), json!(harness.name()));
        if let Some(why) = &reading.failure {
            out.insert(
                "stale".to_owned(),
                json!({
                    "age": time::duration(now - snapshot.fetched_at),
                    "reason": why.kind(),
                    "message": why.message(harness),
                }),
            );
        }
        let windows = |f: &dyn Fn(&Assessed) -> Value| -> Value {
            assessed.iter().map(f).collect::<Vec<_>>().into()
        };
        match name {
            "usage_limits" => {
                put(&mut out, "plan", json!(snapshot.plan));
                out.insert("windows".to_owned(), windows(&limit));
            }
            "usage_pace" => {
                out.insert("baseline".to_owned(), json!("even use over each window"));
                out.insert("windows".to_owned(), windows(&pacing));
            }
            "usage_forecast" => {
                out.insert("windows".to_owned(), windows(&|a| forecast(a, now)));
            }
            "usage_reset_times" => {
                out.insert("windows".to_owned(), windows(&reset));
            }
            "usage_remaining_quota" => {
                out.insert("windows".to_owned(), windows(&remaining));
                out.insert("scopes".to_owned(), tightest(&assessed, &scopes));
            }
            "rate_limit_status" => blocked(&mut out, &scopes, now),
            "usage_extra_credits" => credits(&mut out, snapshot),
            "subscription_plan" => plan(&mut out, harness, snapshot),
            _ => return failure(&format!("Unknown tool: {name}")),
        }
        result(Value::Object(out), false)
    }

    /// The harness named in the arguments, else `TO11Y_HARNESS`, else the
    /// calling harness.
    fn harness(&self, arguments: &Value, client: Option<&str>) -> Result<Harness, String> {
        let valid = || {
            let names: Vec<&str> = Harness::ALL.iter().map(|h| h.name()).collect();
            names.join(", ")
        };
        let named = match arguments {
            Value::Null => None,
            Value::Object(map) => {
                if let Some(key) = map.keys().find(|key| *key != "harness") {
                    return Err(format!("Unknown argument {key}. The only one is harness."));
                }
                match map.get("harness") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(name)) => Some(Harness::parse(name).ok_or_else(|| {
                        format!("Unknown harness {name}. Use one of: {}.", valid())
                    })?),
                    Some(_) => return Err(format!("harness must be one of: {}.", valid())),
                }
            }
            _ => return Err("Arguments must be an object.".to_owned()),
        };
        named
            .or_else(|| self.env.harness.as_deref().and_then(Harness::parse))
            .or_else(|| detect(client?))
            .ok_or_else(|| {
                format!(
                    "Couldn't tell which coding agent is calling. Pass harness, one of: {}.",
                    valid()
                )
            })
    }

    fn reading(&self, harness: Harness, now: i64) -> Reading {
        let cx = self.context();
        self.cache.get(harness, now, || {
            providers::fetch_within(harness, cx, now, providers::DEADLINE)
        })
    }

    fn all_harnesses(&self, now: i64) -> Value {
        let readings: Vec<(Harness, Reading)> = std::thread::scope(|scope| {
            let handles: Vec<_> = Harness::ALL
                .map(|harness| scope.spawn(move || (harness, self.reading(harness, now))))
                .into_iter()
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a reading doesn't panic"))
                .collect()
        });
        let harnesses: Vec<Value> = readings
            .iter()
            .map(|(harness, reading)| {
                let mut out = Map::new();
                out.insert("harness".to_owned(), json!(harness.name()));
                match (&reading.snapshot, &reading.failure) {
                    (None, why) => {
                        let why = why.clone().unwrap_or(Unavailable::NotSignedIn);
                        out.insert("unavailable".to_owned(), json!(why.kind()));
                    }
                    (Some(snapshot), why) => {
                        if let Some(why) = why {
                            out.insert(
                                "stale".to_owned(),
                                json!({ "age": time::duration(now - snapshot.fetched_at), "reason": why.kind() }),
                            );
                        }
                        let (assessed, scopes) = pace::assess(&snapshot.windows, now);
                        let rows: Vec<Value> = scopes
                            .iter()
                            .map(|scope| summary(&assessed, scope, now))
                            .collect();
                        out.insert("scopes".to_owned(), rows.into());
                    }
                }
                Value::Object(out)
            })
            .collect();
        result(json!({ "harnesses": harnesses }), false)
    }

    /// What to11y can tell about a harness without asking its provider.
    fn login_status(&self, harness: Harness, now: i64) -> Value {
        let keyring: &dyn Keyring = &*self.context().keyring;
        let mut out = Map::new();
        out.insert("harness".to_owned(), json!(harness.name()));
        match creds::login(harness, &self.env, keyring) {
            Ok(login) => {
                out.insert("signed_in".to_owned(), json!(true));
                out.insert("source".to_owned(), json!(login.source.name()));
                match login.expires_at {
                    Some(at) if at <= now => {
                        out.insert(
                            "expired".to_owned(),
                            json!(time::duration(now - at) + " ago"),
                        );
                        let renewal = if harness == Harness::Antigravity
                            && !self.env.no_agy
                            && self.env.find_program("agy").is_some()
                        {
                            "to11y runs agy on the next check, which renews it.".to_owned()
                        } else {
                            format!("Using {} renews it.", harness.name())
                        };
                        out.insert("renewal".to_owned(), json!(renewal));
                    }
                    Some(at) => {
                        out.insert("expires_in".to_owned(), json!(time::duration(at - now)));
                    }
                    None => {}
                }
            }
            Err(why) => {
                out.insert("signed_in".to_owned(), json!(false));
                out.insert("reason".to_owned(), json!(why.kind()));
                out.insert("message".to_owned(), json!(why.message(harness)));
            }
        }
        if let Some(reading) = self.cache.peek(harness) {
            let mut last = Map::new();
            last.insert(
                "age".to_owned(),
                json!(time::duration(now - reading.checked_at)),
            );
            match &reading.failure {
                None => {
                    last.insert("result".to_owned(), json!("ok"));
                }
                Some(why) => {
                    last.insert("result".to_owned(), json!(why.kind()));
                    last.insert("message".to_owned(), json!(why.message(harness)));
                }
            }
            if let (Some(snapshot), Some(_)) = (&reading.snapshot, &reading.failure) {
                last.insert(
                    "last_good_reading_age".to_owned(),
                    json!(time::duration(now - snapshot.fetched_at)),
                );
            }
            out.insert("last_check".to_owned(), Value::Object(last));
        }
        Value::Object(out)
    }
}

/// The harness an MCP client name belongs to.
fn detect(client: &str) -> Option<Harness> {
    let name = client.to_ascii_lowercase();
    [
        ("claude", Harness::ClaudeCode),
        ("codex", Harness::Codex),
        ("droid", Harness::Droid),
        ("factory", Harness::Droid),
        ("antigravity", Harness::Antigravity),
        ("agy", Harness::Antigravity),
    ]
    .into_iter()
    .find(|(part, _)| name.contains(part))
    .map(|(_, harness)| harness)
}

fn result(value: Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": is_error,
    })
}

fn failure(message: &str) -> Value {
    result(json!({ "error": message }), true)
}

fn unavailable(harness: Harness, why: &Unavailable) -> Value {
    result(
        json!({
            "harness": harness.name(),
            "unavailable": why.kind(),
            "message": why.message(harness),
        }),
        true,
    )
}

fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !value.is_null() {
        map.insert(key.to_owned(), value);
    }
}

/// A percentage to one decimal place, without a trailing `.0`.
fn percent(value: f64) -> Value {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

fn row(a: &Assessed) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("window".to_owned(), json!(a.window.id));
    row
}

fn verdict(row: &mut Map<String, Value>, a: &Assessed) {
    row.insert("verdict".to_owned(), json!(a.pace.verdict.name()));
}

fn limit(a: &Assessed) -> Value {
    let mut row = row(a);
    row.insert("label".to_owned(), json!(a.window.label));
    row.insert("used_percent".to_owned(), percent(a.window.used_percent));
    if let Some(until) = a.pace.seconds_until_reset {
        row.insert("resets_in".to_owned(), json!(time::duration(until)));
    }
    verdict(&mut row, a);
    Value::Object(row)
}

fn pacing(a: &Assessed) -> Value {
    let mut row = row(a);
    row.insert("used_percent".to_owned(), percent(a.window.used_percent));
    put(
        &mut row,
        "elapsed_percent",
        a.pace.elapsed_percent.map_or(Value::Null, percent),
    );
    put(
        &mut row,
        "pace_delta",
        a.pace.pace_delta.map_or(Value::Null, percent),
    );
    verdict(&mut row, a);
    put(
        &mut row,
        "safe_percent_per_hour",
        a.pace.safe_percent_per_hour.map_or(Value::Null, percent),
    );
    Value::Object(row)
}

fn forecast(a: &Assessed, now: i64) -> Value {
    let mut row = row(a);
    match a.pace.projected_percent_at_reset {
        Some(projected) => {
            row.insert("projected_percent_at_reset".to_owned(), percent(projected));
            row.insert("runs_out".to_owned(), json!(a.pace.exhausts_at.is_some()));
            if let Some(at) = a.pace.exhausts_at {
                row.insert("runs_out_at".to_owned(), json!(time::format(at)));
                row.insert("runs_out_in".to_owned(), json!(time::duration(at - now)));
            }
        }
        None => {
            let why = match a.pace.verdict {
                Verdict::OverPace | Verdict::OnPace | Verdict::UnderPace => "too_early_to_forecast",
                other => other.name(),
            };
            row.insert("no_forecast".to_owned(), json!(why));
        }
    }
    if let Some(until) = a.pace.seconds_until_reset {
        row.insert("resets_in".to_owned(), json!(time::duration(until)));
    }
    Value::Object(row)
}

fn reset(a: &Assessed) -> Value {
    let mut row = row(a);
    match (a.window.resets_at, a.pace.seconds_until_reset) {
        (Some(at), Some(until)) => {
            row.insert("resets_at".to_owned(), json!(time::format(at)));
            row.insert("resets_in".to_owned(), json!(time::duration(until)));
        }
        _ => {
            let why = match a.window.state {
                crate::model::WindowState::NotStarted => "starts when first used",
                crate::model::WindowState::Disabled => "doesn't apply right now",
                crate::model::WindowState::Running => "unknown",
            };
            row.insert("resets".to_owned(), json!(why));
        }
    }
    Value::Object(row)
}

fn remaining(a: &Assessed) -> Value {
    let mut row = row(a);
    row.insert(
        "remaining_percent".to_owned(),
        percent(a.pace.remaining_percent),
    );
    Value::Object(row)
}

fn tightest(assessed: &[Assessed], scopes: &[Scope]) -> Value {
    scopes
        .iter()
        .map(|scope| {
            let mut row = Map::new();
            row.insert("scope".to_owned(), json!(scope.name));
            if let Some(a) = scope.tightest.map(|i| &assessed[i]) {
                row.insert("tightest_window".to_owned(), json!(a.window.id));
                row.insert(
                    "remaining_percent".to_owned(),
                    percent(a.pace.remaining_percent),
                );
            }
            Value::Object(row)
        })
        .collect::<Vec<_>>()
        .into()
}

fn blocked(out: &mut Map<String, Value>, scopes: &[Scope], now: i64) {
    let count = scopes.iter().filter(|s| s.blocked).count();
    let all = scopes.iter().any(|s| s.blocked && s.name == pace::ALL);
    let state = if count == 0 {
        "none"
    } else if all || count == scopes.len() {
        "all"
    } else {
        "some"
    };
    out.insert("blocked".to_owned(), json!(state));
    let rows: Vec<Value> = scopes
        .iter()
        .map(|scope| {
            let mut row = Map::new();
            row.insert("scope".to_owned(), json!(scope.name));
            row.insert("blocked".to_owned(), json!(scope.blocked));
            if let Some(until) = scope.blocked_until {
                row.insert("until".to_owned(), json!(time::format(until)));
                row.insert("for".to_owned(), json!(time::duration(until - now)));
            }
            Value::Object(row)
        })
        .collect();
    out.insert("scopes".to_owned(), rows.into());
}

fn credits(out: &mut Map<String, Value>, snapshot: &Snapshot) {
    match &snapshot.credits {
        Some(credits) => {
            out.insert("credits".to_owned(), credits.to_json());
        }
        None => {
            out.insert(
                "credits".to_owned(),
                json!("The provider reports no extra usage or credits."),
            );
        }
    }
}

fn plan(out: &mut Map<String, Value>, harness: Harness, snapshot: &Snapshot) {
    out.insert("provider".to_owned(), json!(harness.provider()));
    put(out, "plan", json!(snapshot.plan));
    put(out, "tier", json!(snapshot.tier));
    if snapshot.plan.is_none() && snapshot.tier.is_none() {
        out.insert(
            "note".to_owned(),
            json!("The provider doesn't report a plan name."),
        );
    }
}

/// One scope in the all-harnesses comparison: blocked until when, or its
/// tightest window.
fn summary(assessed: &[Assessed], scope: &Scope, now: i64) -> Value {
    let mut row = Map::new();
    row.insert("scope".to_owned(), json!(scope.name));
    if scope.blocked {
        row.insert("blocked".to_owned(), json!(true));
        if let Some(until) = scope.blocked_until {
            row.insert("for".to_owned(), json!(time::duration(until - now)));
        }
        return Value::Object(row);
    }
    match scope.tightest.map(|i| &assessed[i]) {
        Some(a) => {
            row.insert("tightest_window".to_owned(), json!(a.window.id));
            row.insert("used_percent".to_owned(), percent(a.window.used_percent));
            if let Some(until) = a.pace.seconds_until_reset {
                row.insert("resets_in".to_owned(), json!(time::duration(until)));
            }
            verdict(&mut row, a);
        }
        None => {
            row.insert("verdict".to_owned(), json!("not_started"));
        }
    }
    Value::Object(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_444_270; // 2026-11-23T14:31:10Z

    /// Tools that read the committed fixtures at the fixture time, with no
    /// home, cache directory, or keyring.
    fn tools() -> Tools {
        Tools::new(Env {
            now: Some(NOW),
            fixtures: Some(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures").into()),
            ..Env::default()
        })
    }

    /// The JSON inside a result's text block, and whether it is an error.
    fn call(tools: &Tools, name: &str, arguments: Value) -> (Value, bool) {
        let result = tools.call(name, &arguments, None);
        let text = result["content"][0]["text"].as_str().unwrap();
        (
            serde_json::from_str(text).unwrap(),
            result["isError"] == true,
        )
    }

    #[test]
    fn the_tool_list_is_small_and_every_tool_is_read_only() {
        let list = list();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        for tool in tools {
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert!(tool["description"].as_str().unwrap().len() < 160);
        }
        // About four bytes to a token: 1,500 tokens is about 6,000 bytes.
        assert!(list.to_string().len() < 6_000, "{}", list.to_string().len());
    }

    #[test]
    fn usage_limits_show_use_reset_and_verdict() {
        let (out, error) = call(&tools(), "usage_limits", json!({"harness": "claude-code"}));
        assert!(!error);
        assert_eq!(
            out["windows"][0],
            json!({"window": "weekly", "label": "Weekly, all models", "used_percent": 6,
                   "resets_in": "6d 19h", "verdict": "on_pace"})
        );
    }

    #[test]
    fn rate_limit_status_names_the_blocked_scope() {
        let (out, _) = call(
            &tools(),
            "rate_limit_status",
            json!({"harness": "antigravity"}),
        );
        assert_eq!(out["blocked"], "some");
        let gemini = &out["scopes"][0];
        assert_eq!(gemini["scope"], "gemini");
        assert_eq!(gemini["until"], "2026-11-28T15:34:08Z");
        assert_eq!(gemini["for"], "5d 1h");
        assert_eq!(out["scopes"][1]["blocked"], false);
    }

    #[test]
    fn forecasts_wait_for_enough_of_the_window() {
        let (out, _) = call(&tools(), "usage_forecast", json!({"harness": "codex"}));
        let row = &out["windows"][0];
        assert!(
            row.get("projected_percent_at_reset").is_some() || row["no_forecast"].is_string(),
            "{row}"
        );
    }

    #[test]
    fn reset_times_explain_windows_without_one() {
        let (out, _) = call(&tools(), "usage_reset_times", json!({"harness": "droid"}));
        assert_eq!(
            out["windows"][0],
            json!({"window": "standard:5h", "resets": "starts when first used"})
        );
        assert_eq!(out["windows"][1]["resets_at"], "2026-11-23T16:03:17Z");
    }

    #[test]
    fn all_harnesses_reports_each_one() {
        let (out, error) = call(&tools(), "usage_limits_all_harnesses", Value::Null);
        assert!(!error);
        let names: Vec<&str> = out["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["harness"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["claude-code", "codex", "droid", "antigravity"]);
        let droid = &out["harnesses"][2]["scopes"][0];
        assert_eq!(droid["scope"], "standard");
        assert_eq!(droid["blocked"], true);
    }

    #[test]
    fn a_missing_harness_is_an_error_that_lists_the_choices() {
        let (out, error) = call(&tools(), "usage_limits", Value::Null);
        assert!(error);
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("claude-code, codex, droid, antigravity")
        );
        let (out, error) = call(&tools(), "usage_limits", json!({"harness": "cursor"}));
        assert!(error, "{out}");
        let (_, error) = call(&tools(), "usage_limits", json!({"model": "x"}));
        assert!(error);
    }

    #[test]
    fn the_calling_harness_is_the_default() {
        let tools = tools();
        let result = tools.call("subscription_plan", &Value::Null, Some("codex-mcp-client"));
        let text = result["content"][0]["text"].as_str().unwrap();
        let out: Value = serde_json::from_str(text).unwrap();
        assert_eq!(out["harness"], "codex");
        assert_eq!(out["plan"], "free");
        assert_eq!(detect("some-editor"), None);
    }

    /// The `clientInfo.name` each harness sent on 2026-11-23: Claude Code
    /// 2.1.330, Codex 0.171.0, Droid 1.16.0, and agy 1.5.11.
    #[test]
    fn each_harness_is_known_by_the_name_its_client_sends() {
        assert_eq!(detect("claude-code"), Some(Harness::ClaudeCode));
        assert_eq!(detect("codex-mcp-client"), Some(Harness::Codex));
        assert_eq!(detect("factory-cli"), Some(Harness::Droid));
        assert_eq!(detect("antigravity-client"), Some(Harness::Antigravity));
    }

    #[test]
    fn an_unavailable_harness_says_why() {
        let dir = crate::testing::TempDir::new("tools-unavailable");
        dir.write(
            "droid.json",
            r#"{"to11y_unavailable": {"kind": "keyring_locked"}}"#,
        );
        let tools = Tools::new(Env {
            now: Some(NOW),
            fixtures: Some(dir.path().to_owned()),
            ..Env::default()
        });
        let (out, error) = call(&tools, "usage_limits", json!({"harness": "droid"}));
        assert!(error);
        assert_eq!(out["unavailable"], "keyring_locked");
        let (out, _) = call(&tools, "usage_limits", json!({"harness": "codex"}));
        assert_eq!(out["unavailable"], "not_installed");
    }

    #[test]
    fn percents_have_one_decimal_place_at_most() {
        assert_eq!(percent(6.0), json!(6));
        assert_eq!(percent(0.006664), json!(0));
        assert_eq!(percent(17.96), json!(18));
        assert_eq!(percent(2.94), json!(2.9));
    }

    #[test]
    fn no_fixture_identifier_reaches_any_tool() {
        let tools = tools();
        for (name, _, takes_harness) in TOOLS {
            let harnesses: Vec<Value> = if takes_harness {
                Harness::ALL
                    .iter()
                    .map(|h| json!({"harness": h.name()}))
                    .collect()
            } else {
                vec![Value::Null]
            };
            for arguments in harnesses {
                let text = tools.call(name, &arguments, None).to_string();
                for secret in ["user-fixture", "00000000-0000-4000-8000", "example.invalid"] {
                    assert!(!text.contains(secret), "{name} leaked {secret}");
                }
            }
        }
    }
}
