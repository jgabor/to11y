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
use crate::tokens::{Period, report};

/// What a tool takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Args {
    None,
    Harness,
    HarnessPeriod,
    Period,
}

impl Args {
    fn harness(self) -> bool {
        matches!(self, Args::Harness | Args::HarnessPeriod)
    }

    fn period(self) -> bool {
        matches!(self, Args::HarnessPeriod | Args::Period)
    }
}

struct Tool {
    name: &'static str,
    description: &'static str,
    args: Args,
    /// Reads only local files, with no call to a provider.
    local: bool,
}

const TOOLS: [Tool; 17] = [
    Tool {
        name: "usage_limits",
        description: "Show a coding agent's subscription usage limits: percent used, time to reset, and pace for each 5-hour, weekly, or monthly window.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_pace",
        description: "Check whether subscription quota is being used too fast: percent used against percent of the window elapsed, with a safe hourly rate.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_forecast",
        description: "Forecast whether a subscription usage limit will run out before it resets, and when, at the current rate of use.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_reset_times",
        description: "List when each subscription usage limit window resets, as a timestamp and as time remaining.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_remaining_quota",
        description: "Show how much subscription quota is left in each usage window, and the tightest limit for each model group or pool.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "rate_limit_status",
        description: "Check whether a coding agent is rate limited right now because a usage limit is used up, and until when.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_extra_credits",
        description: "Show extra usage, overage, or credit balance beyond the limits included in a subscription.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "subscription_plan",
        description: "Show which subscription plan and tier a coding agent's account is on.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "usage_limits_all_harnesses",
        description: "Compare subscription usage limits across Claude Code, Codex, Droid, and Antigravity to find which coding agent has headroom left.",
        args: Args::None,
        local: false,
    },
    Tool {
        name: "usage_login_status",
        description: "Explain why subscription usage data is missing or stale: sign-in, keyring, and last check for a coding agent.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "token_usage_session",
        description: "Show how many tokens this coding agent session has used so far: input, cache reads and writes, output, and reasoning, by model.",
        args: Args::Harness,
        local: true,
    },
    Tool {
        name: "token_usage_recent",
        description: "Show the tokens a coding agent used in the last 5 hours, 24 hours, 7 days, or 30 days, from its local logs.",
        args: Args::HarnessPeriod,
        local: true,
    },
    Tool {
        name: "token_usage_by_model",
        description: "Break down token usage by model over a recent period, with the share of input read from the prompt cache.",
        args: Args::HarnessPeriod,
        local: true,
    },
    Tool {
        name: "token_usage_by_project",
        description: "Break down token usage by project, meaning working directory, over a recent period to see which projects use the most.",
        args: Args::HarnessPeriod,
        local: true,
    },
    Tool {
        name: "token_usage_limit_windows",
        description: "Show how many tokens went into each current subscription usage limit window, next to the percent of the limit used.",
        args: Args::Harness,
        local: false,
    },
    Tool {
        name: "token_usage_all_harnesses",
        description: "Compare token usage across Claude Code, Codex, Droid, and Antigravity over a recent period.",
        args: Args::Period,
        local: true,
    },
    Tool {
        name: "token_usage_sources",
        description: "Explain why token usage data is missing: where a coding agent's logs are, how many files there are, and how recent.",
        args: Args::Harness,
        local: true,
    },
];

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn exists(name: &str) -> bool {
    TOOLS.iter().any(|tool| tool.name == name)
}

/// The `tools/list` result.
pub fn list() -> Value {
    let names: Vec<&str> = Harness::ALL.iter().map(|h| h.name()).collect();
    let periods: Vec<&str> = Period::ALL.iter().map(|p| p.name()).collect();
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|tool| {
            let mut properties = Map::new();
            if tool.args.harness() {
                properties.insert(
                    "harness".to_owned(),
                    json!({
                        "type": "string",
                        "enum": names,
                        "description": "Coding agent to check. Defaults to the one calling.",
                    }),
                );
            }
            if tool.args.period() {
                properties.insert(
                    "period".to_owned(),
                    json!({
                        "type": "string",
                        "enum": periods,
                        "description": "How far back to look. Defaults to 24h.",
                    }),
                );
            }
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "additionalProperties": false,
                },
                "annotations": { "readOnlyHint": true, "openWorldHint": !tool.local },
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
        let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
            return failure(&format!("Unknown tool: {name}"));
        };
        let (named, period) = match parse_arguments(tool, arguments) {
            Ok(parsed) => parsed,
            Err(message) => return failure(&message),
        };
        let env = &*self.env;
        match name {
            "usage_limits_all_harnesses" => return self.all_harnesses(now),
            "token_usage_all_harnesses" => return answer(report::all_harnesses(env, now, period)),
            _ => {}
        }
        let harness = match self.harness(named, client) {
            Ok(harness) => harness,
            Err(message) => return failure(&message),
        };
        match name {
            "usage_login_status" => return result(self.login_status(harness, now), false),
            "token_usage_session" => return answer(report::session(harness, env, now)),
            "token_usage_recent" => return answer(report::recent(harness, env, now, period)),
            "token_usage_by_model" => return answer(report::by_model(harness, env, now, period)),
            "token_usage_by_project" => {
                return answer(report::by_project(harness, env, now, period));
            }
            "token_usage_sources" => return answer(report::sources(harness, env, now)),
            _ => {}
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
        if name == "token_usage_limit_windows" {
            out.extend(report::limit_windows(harness, env, now, snapshot));
            return result(Value::Object(out), false);
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
    fn harness(&self, named: Option<Harness>, client: Option<&str>) -> Result<Harness, String> {
        named
            .or_else(|| self.env.harness.as_deref().and_then(Harness::parse))
            .or_else(|| detect(client?))
            .ok_or_else(|| {
                format!(
                    "Couldn't tell which coding agent is calling. Pass harness, one of: {}.",
                    harness_names()
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

fn harness_names() -> String {
    let names: Vec<&str> = Harness::ALL.iter().map(|h| h.name()).collect();
    names.join(", ")
}

/// The harness and period in a tool's arguments. Missing ones are `None`
/// and the default period.
fn parse_arguments(tool: &Tool, arguments: &Value) -> Result<(Option<Harness>, Period), String> {
    let map = match arguments {
        Value::Null => return Ok((None, Period::Day)),
        Value::Object(map) => map,
        _ => return Err("Arguments must be an object.".to_owned()),
    };
    let mut allowed = Vec::new();
    if tool.args.harness() {
        allowed.push("harness");
    }
    if tool.args.period() {
        allowed.push("period");
    }
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(if allowed.is_empty() {
            format!("{} takes no arguments.", tool.name)
        } else {
            format!(
                "Unknown argument {key}. {} takes: {}.",
                tool.name,
                allowed.join(", ")
            )
        });
    }
    let harness =
        match map.get("harness") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => Some(Harness::parse(name).ok_or_else(|| {
                format!("Unknown harness {name}. Use one of: {}.", harness_names())
            })?),
            Some(_) => return Err(format!("harness must be one of: {}.", harness_names())),
        };
    let periods = || {
        let names: Vec<&str> = Period::ALL.iter().map(|p| p.name()).collect();
        names.join(", ")
    };
    let period = match map.get("period") {
        None | Some(Value::Null) => Period::Day,
        Some(Value::String(name)) => Period::parse(name)
            .ok_or_else(|| format!("Unknown period {name}. Use one of: {}.", periods()))?,
        Some(_) => return Err(format!("period must be one of: {}.", periods())),
    };
    Ok((harness, period))
}

fn answer((value, is_error): report::Answer) -> Value {
    result(value, is_error)
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
pub(crate) fn percent(value: f64) -> Value {
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
        assert_eq!(tools.len(), 17);
        for tool in tools {
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert!(tool["description"].as_str().unwrap().len() < 160);
        }
        // About four bytes to a token: 2,200 tokens is about 8,800 bytes.
        assert!(list.to_string().len() < 8_800, "{}", list.to_string().len());
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

    /// A copy of the fixture home with logs for all four harnesses, and
    /// tools that read it as Claude Code's session `planted-session-claude`
    /// started in `/work/app`.
    fn log_tools() -> (crate::testing::TempDir, Tools) {
        let dir = crate::testing::TempDir::new(&format!("tools-logs-{}", next_id()));
        let home = dir.copy(
            std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/logs/home"
            )),
            "home",
        );
        let tools = Tools::new(Env {
            home: Some(home),
            now: Some(NOW),
            started: Some(NOW - 4800),
            fixtures: Some(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures").into()),
            claude_session_id: Some("planted-session-claude".to_owned()),
            cwd: Some("/work/app".into()),
            ..Env::default()
        });
        (dir, tools)
    }

    #[test]
    fn after_clear_the_session_is_the_one_claude_code_is_in_now() {
        let (dir, _) = log_tools();
        // to11y (pid 40) was started by Claude Code (pid 20), which has
        // moved on from the session it passed at start.
        dir.write(
            "proc/40/stat",
            "40 (to11y) S 20 40 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 900 0",
        );
        dir.write(
            "proc/20/stat",
            "20 (claude) S 1 20 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 700 0",
        );
        dir.write(
            "home/.claude/sessions/20.json",
            r#"{"pid":20,"sessionId":"other-session","procStart":"700"}"#,
        );
        let tools = Tools::new(Env {
            home: Some(dir.path().join("home")),
            now: Some(NOW),
            claude_session_id: Some("planted-session-claude".to_owned()),
            pid: Some(40),
            proc_dir: Some(dir.path().join("proc")),
            ..Env::default()
        });
        let (out, error) = call(
            &tools,
            "token_usage_session",
            json!({"harness": "claude-code"}),
        );
        assert!(!error, "{out}");
        assert_eq!(out["matched_by"], "the session Claude Code is in now");
        assert_ne!(out["tokens"]["requests"], 4);
        assert!(!out.to_string().contains("other-session"));
    }

    fn next_id() -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn recent_tokens_add_up_each_channel() {
        let (_dir, tools) = log_tools();
        let (out, error) = call(
            &tools,
            "token_usage_recent",
            json!({"harness": "claude-code"}),
        );
        assert!(!error, "{out}");
        assert_eq!(out["period"], "24h");
        assert_eq!(
            out["tokens"],
            json!({"total": 256_974, "input": 70_020, "cache_read": 176_932, "cache_write": 3199,
                   "output": 6823, "reasoning": 100, "requests": 5, "cache_read_percent": 70.7})
        );
        let (out, _) = call(&tools, "token_usage_recent", json!({"harness": "codex"}));
        assert_eq!(
            out["tokens"],
            json!({"total": 47_750, "input": 11_000, "cache_read": 35_000, "cache_write": 1000,
                   "output": 750, "reasoning": 260, "requests": 3, "cache_read_percent": 74.5})
        );
        let (out, _) = call(&tools, "token_usage_recent", json!({"harness": "droid"}));
        assert_eq!(out["tokens"]["total"], 21_600);
        assert_eq!(out["tokens"]["requests"], 2);
        let (out, _) = call(
            &tools,
            "token_usage_recent",
            json!({"harness": "antigravity"}),
        );
        assert_eq!(out["tokens"]["total"], 36_096);
        assert!(out["tokens"].get("cache_write").is_none());
    }

    #[test]
    fn a_longer_period_reaches_older_events() {
        let (_dir, tools) = log_tools();
        let (day, _) = call(
            &tools,
            "token_usage_recent",
            json!({"harness": "claude-code"}),
        );
        let (week, _) = call(
            &tools,
            "token_usage_recent",
            json!({"harness": "claude-code", "period": "7d"}),
        );
        assert_eq!(
            week["tokens"]["requests"],
            day["tokens"]["requests"].as_u64().unwrap() + 1
        );
        let (hours, _) = call(
            &tools,
            "token_usage_recent",
            json!({"harness": "claude-code", "period": "5h"}),
        );
        assert_eq!(hours["tokens"]["requests"], 5);
    }

    #[test]
    fn models_and_projects_are_ranked_by_total() {
        let (_dir, tools) = log_tools();
        let (out, _) = call(
            &tools,
            "token_usage_by_model",
            json!({"harness": "claude-code"}),
        );
        let models: Vec<&str> = out["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["model"].as_str().unwrap())
            .collect();
        assert_eq!(models, ["claude-opus-5-5", "claude-fable-5-2"]);
        assert_eq!(out["models"][1]["total"], 78_205);
        let (out, _) = call(
            &tools,
            "token_usage_by_project",
            json!({"harness": "claude-code"}),
        );
        let projects: Vec<&str> = out["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["project"].as_str().unwrap())
            .collect();
        assert_eq!(projects, ["/work/app", "temporary directories"]);
        let (out, _) = call(
            &tools,
            "token_usage_by_project",
            json!({"harness": "droid"}),
        );
        assert_eq!(out["projects"][0]["project"], "/work/site");
    }

    #[test]
    fn the_session_is_the_callers_own() {
        let (_dir, tools) = log_tools();
        let (out, error) = call(
            &tools,
            "token_usage_session",
            json!({"harness": "claude-code"}),
        );
        assert!(!error, "{out}");
        assert_eq!(
            out["matched_by"],
            "the session id Claude Code passed at start"
        );
        assert_eq!(out["tokens"]["requests"], 4);
        assert_eq!(out["started"], "1h 31m ago");
        let (out, error) = call(&tools, "token_usage_session", json!({"harness": "codex"}));
        assert!(!error, "{out}");
        assert_eq!(out["tokens"]["requests"], 3);
        assert_eq!(out["models"][0]["model"], "gpt-6.1-sol");
        let (out, _) = call(&tools, "token_usage_session", json!({"harness": "droid"}));
        assert_eq!(out["matched_by"], "the latest Droid session");
        assert_eq!(out["tokens"]["total"], 21_600);
        assert_eq!(out["model"], "gpt-6.1-sol");
        let (out, _) = call(
            &tools,
            "token_usage_session",
            json!({"harness": "antigravity"}),
        );
        assert_eq!(out["tokens"]["requests"], 2);
    }

    #[test]
    fn a_codex_session_from_another_directory_is_not_the_caller() {
        let (dir, _) = log_tools();
        let tools = Tools::new(Env {
            home: Some(dir.path().join("home")),
            now: Some(NOW),
            started: Some(NOW - 4800),
            cwd: Some("/elsewhere".into()),
            ..Env::default()
        });
        let (out, error) = call(&tools, "token_usage_session", json!({"harness": "codex"}));
        assert!(error);
        assert_eq!(out["unavailable"], "no_session");
    }

    #[test]
    fn limit_windows_count_the_models_they_limit() {
        let (_dir, tools) = log_tools();
        let (out, error) = call(
            &tools,
            "token_usage_limit_windows",
            json!({"harness": "claude-code"}),
        );
        assert!(!error, "{out}");
        let windows = out["windows"].as_array().unwrap();
        let weekly = windows.iter().find(|w| w["window"] == "weekly").unwrap();
        assert_eq!(weekly["tokens"]["total"], 256_974);
        let fable = windows
            .iter()
            .find(|w| w["window"] == "weekly:fable")
            .unwrap();
        assert_eq!(fable["tokens"]["total"], 78_205);
        assert!(out["note"].as_str().unwrap().contains("this machine"));
        let (limits, _) = call(&tools, "usage_limits", json!({"harness": "claude-code"}));
        let limit = limits["windows"].as_array().unwrap();
        let limit = limit.iter().find(|w| w["window"] == "weekly").unwrap();
        assert_eq!(weekly["used_percent"], limit["used_percent"]);
    }

    #[test]
    fn all_harnesses_compare_their_totals() {
        let (_dir, tools) = log_tools();
        let (out, _) = call(
            &tools,
            "token_usage_all_harnesses",
            json!({"period": "24h"}),
        );
        let totals: Vec<u64> = out["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["tokens"]["total"].as_u64().unwrap())
            .collect();
        assert_eq!(totals, [256_974, 47_750, 21_600, 36_096]);
    }

    #[test]
    fn sources_say_where_the_logs_are() {
        let (_dir, with_logs) = log_tools();
        let (out, error) = call(
            &with_logs,
            "token_usage_sources",
            json!({"harness": "claude-code"}),
        );
        assert!(!error);
        assert_eq!(out["folder"], "~/.claude/projects");
        assert_eq!(out["files"], 3);
        let (out, error) = call(&tools(), "token_usage_sources", json!({"harness": "codex"}));
        assert!(error);
        assert_eq!(out["unavailable"], "no_logs");
    }

    #[test]
    fn bad_arguments_name_what_the_tool_takes() {
        let (out, error) = call(&tools(), "token_usage_recent", json!({"period": "1y"}));
        assert!(error);
        assert!(out["error"].as_str().unwrap().contains("5h, 24h, 7d, 30d"));
        let (out, _) = call(&tools(), "usage_limits", json!({"period": "7d"}));
        assert_eq!(
            out["error"],
            "Unknown argument period. usage_limits takes: harness."
        );
        let (out, _) = call(
            &tools(),
            "usage_limits_all_harnesses",
            json!({"harness": "codex"}),
        );
        assert_eq!(
            out["error"],
            "usage_limits_all_harnesses takes no arguments."
        );
    }

    #[test]
    fn no_fixture_identifier_reaches_any_tool() {
        let (_dir, tools) = log_tools();
        for tool in &TOOLS {
            let harnesses: Vec<Value> = if tool.args.harness() {
                Harness::ALL
                    .iter()
                    .map(|h| json!({"harness": h.name()}))
                    .collect()
            } else {
                vec![Value::Null]
            };
            for arguments in harnesses {
                let text = tools.call(tool.name, &arguments, None).to_string();
                for secret in [
                    "user-fixture",
                    "00000000-0000-4000-8000",
                    "example.invalid",
                    "planted",
                    "0f3a9c1e2b7d44aa",
                ] {
                    assert!(
                        !text.contains(secret),
                        "{} leaked {secret}: {text}",
                        tool.name
                    );
                }
            }
        }
    }
}
