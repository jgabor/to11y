//! Codex: ChatGPT's usage endpoint, with the sign-in Codex stores in
//! `auth.json`.

use std::cmp::Reverse;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

use crate::creds;
use crate::env::Env;
use crate::model::{Credits, Harness, Money, Origin, Snapshot, Unavailable, Window};
use crate::pace::ALL;
use crate::providers::{Context, Failure, length_id, length_label, request, slug, window};
use crate::{time, tokens};

const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// How many of the newest rollouts are searched for a saved reading.
const SAVED_FILES: usize = 5;

/// How much of the end of each rollout is read. A reading comes with every
/// response, so the newest one is near the end.
const TAIL_BYTES: u64 = 256 * 1024;

pub fn fetch(cx: &Context, now: i64) -> Result<Snapshot, Failure> {
    let login = creds::codex(&cx.env)?;
    if login.expires_within(now, 0) {
        return Err(Unavailable::SignInExpired.into());
    }
    let auth = format!("Bearer {}", login.token.expose());
    let mut headers = vec![
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];
    if let Some(account) = &login.account {
        headers.push(("ChatGPT-Account-Id", account.expose()));
    }
    let body = request(cx, Harness::Codex, URL, &headers, None)?;
    Ok(parse(&body, now)?)
}

/// The newest reading Codex saved in its rollouts, in the usage response's
/// shape. It lacks the additional limits, so it never replaces a call.
pub fn saved(env: &Env, now: i64) -> Option<Snapshot> {
    creds::codex(env).ok()?;
    let mut files = Vec::new();
    let root = env.codex_dir()?.join("sessions");
    tokens::walk(&root, 4, &|path| tokens::named(path, ".jsonl"), &mut files);
    files.sort_by_key(|file| Reverse(file.mtime));
    let (at, limits) = files
        .iter()
        .take(SAVED_FILES)
        .filter_map(|file| latest_limits(&file.path, now))
        .max_by_key(|(at, _)| *at)?;
    let mut snapshot = parse(&as_usage(&limits), at).ok()?;
    snapshot.origin = Origin::Saved;
    Some(snapshot)
}

/// The `rate_limits` of the newest `token_count` line, not after `now`, in
/// the end of a rollout.
fn latest_limits(path: &Path, now: i64) -> Option<(i64, Value)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut read = BufReader::new(file.take(TAIL_BYTES));
    if start > 0 {
        // The first line is cut, so it is skipped.
        read.read_until(b'\n', &mut Vec::new()).ok()?;
    }
    let mut best: Option<(i64, Value)> = None;
    tokens::lines(&mut read, |line| {
        if !tokens::has(line, b"\"rate_limits\"") || !tokens::has(line, b"token_count") {
            return;
        }
        let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let payload = &value["payload"];
        let limits = &payload["rate_limits"];
        let codex = limits["limit_id"].is_null() || limits["limit_id"] == "codex";
        if payload["type"] != "token_count" || !codex || !limits["primary"].is_object() {
            return;
        }
        let Some(at) = value["timestamp"].as_str().and_then(time::parse) else {
            return;
        };
        if at <= now && best.as_ref().is_none_or(|(best, _)| at >= *best) {
            best = Some((at, value["payload"]["rate_limits"].take()));
        }
    });
    best
}

/// A rollout's `rate_limits` rewritten in the usage response's shape, so
/// its windows get the same ids.
fn as_usage(limits: &Value) -> Value {
    let window = |w: &Value| {
        if !w.is_object() {
            return Value::Null;
        }
        json!({
            "used_percent": w["used_percent"],
            "limit_window_seconds": w["window_minutes"].as_u64().map(|m| m * 60),
            "reset_at": w["resets_at"],
        })
    };
    json!({
        "rate_limit": {
            "primary_window": window(&limits["primary"]),
            "secondary_window": window(&limits["secondary"]),
        },
        "credits": limits["credits"],
        "plan_type": limits["plan_type"],
    })
}

/// Windows from the usage response. A window's length comes from
/// `limit_window_seconds`, never from whether it is primary or secondary.
pub fn parse(body: &Value, now: i64) -> Result<Snapshot, Unavailable> {
    let rate_limit = &body["rate_limit"];
    if !rate_limit.is_object() {
        return Err(Unavailable::Failed(
            "OpenAI sent an unexpected usage response.".to_owned(),
        ));
    }
    let mut windows = windows(rate_limit, ALL, "all models", now);
    for extra in body["additional_rate_limits"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let Some(name) = extra["limit_name"]
            .as_str()
            .or_else(|| extra["metered_feature"].as_str())
        else {
            continue;
        };
        windows.extend(windows_named(&extra["rate_limit"], name, now));
    }
    let credits = &body["credits"];
    Ok(Snapshot {
        harness: Harness::Codex,
        plan: body["plan_type"].as_str().map(str::to_owned),
        tier: None,
        windows,
        credits: credits.is_object().then(|| Credits {
            enabled: credits["has_credits"].as_bool(),
            unlimited: credits["unlimited"].as_bool(),
            balance: number(&credits["balance"]).map(|amount| Money {
                amount,
                currency: "credits".to_owned(),
            }),
            ..Credits::default()
        }),
        fetched_at: now,
        origin: Origin::Provider,
    })
}

fn windows_named(rate_limit: &Value, name: &str, now: i64) -> Vec<Window> {
    windows(rate_limit, &slug(name), name, now)
}

fn windows(rate_limit: &Value, scope: &str, scope_label: &str, now: i64) -> Vec<Window> {
    ["primary_window", "secondary_window"]
        .into_iter()
        .filter_map(|key| {
            let w = &rate_limit[key];
            let used = number(&w["used_percent"])?;
            let minutes = w["limit_window_seconds"]
                .as_u64()
                .and_then(|seconds| u32::try_from(seconds / 60).ok());
            let reset = w["reset_at"]
                .as_i64()
                .or_else(|| w["reset_after_seconds"].as_i64().map(|after| now + after));
            let id = if scope == ALL {
                length_id(minutes)
            } else {
                format!("{scope}:{}", length_id(minutes))
            };
            Some(window(
                id,
                format!("{}, {scope_label}", length_label(minutes)),
                scope,
                minutes,
                used,
                reset,
            ))
        })
        .collect()
}

/// A number, or a string that holds one.
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowKind;
    use crate::testing::TempDir;
    use crate::tokens::tests::touch;

    const NOW: i64 = 1_795_444_270; // 2026-11-23T14:31:10Z

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/codex.json")).unwrap()
    }

    #[test]
    fn the_fixture_has_one_monthly_window_on_the_free_plan() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("free"));
        assert_eq!(snapshot.windows.len(), 1);
        let w = &snapshot.windows[0];
        assert_eq!((w.id.as_str(), w.kind), ("monthly", WindowKind::Monthly));
        assert_eq!(w.label, "Monthly, all models");
        assert_eq!((w.used_percent, w.resets_at), (10.0, Some(1_797_458_405)));
        assert_eq!(w.window_minutes, Some(43_200));
    }

    #[test]
    fn account_identifiers_never_reach_the_snapshot() {
        let text = parse(&fixture(), NOW).unwrap().to_json().to_string();
        for secret in [
            "user-fixture0000000000000000",
            "00000000-0000-4000-8000-000000000000",
            "fixture-user@example.invalid",
        ] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }

    fn limits_line(at: i64, limits: Value) -> String {
        json!({
            "timestamp": time::format(at),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": null, "rate_limits": limits},
        })
        .to_string()
    }

    fn monthly(used: f64) -> Value {
        json!({
            "limit_id": "codex", "limit_name": null,
            "primary": {"used_percent": used, "window_minutes": 43200, "resets_at": 1_797_458_405},
            "secondary": null,
            "credits": {"has_credits": false, "unlimited": false, "balance": null},
            "plan_type": "free",
        })
    }

    /// A Codex home signed in with ChatGPT, with `rollouts` as (name, mtime,
    /// lines).
    fn rollouts(name: &str, files: &[(&str, i64, Vec<String>)]) -> (TempDir, Env) {
        let dir = TempDir::new(name);
        dir.write(
            "home/.codex/auth.json",
            r#"{"tokens": {"access_token": "secret-token", "account_id": "secret-account"}}"#,
        );
        for (file, mtime, lines) in files {
            let path = dir.write(
                &format!("home/.codex/sessions/2026/11/23/{file}"),
                &(lines.join("\n") + "\n"),
            );
            touch(&path, *mtime);
        }
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        (dir, env)
    }

    #[test]
    fn a_rollout_reading_reads_like_the_provider_answer() {
        let (_dir, env) = rollouts(
            "codex-saved-reads",
            &[(
                "rollout-a.jsonl",
                NOW,
                vec![limits_line(NOW - 90, monthly(10.0))],
            )],
        );
        let snapshot = saved(&env, NOW).unwrap();
        let answer = parse(&fixture(), NOW - 90).unwrap();
        assert_eq!(snapshot.windows, answer.windows);
        assert_eq!(snapshot.plan, answer.plan);
        assert_eq!(snapshot.credits, answer.credits);
        assert_eq!(
            (snapshot.fetched_at, snapshot.origin),
            (NOW - 90, Origin::Saved)
        );
        let text = snapshot.to_json().to_string();
        assert!(!text.contains("secret"), "a sign-in leaked");
    }

    #[test]
    fn lines_without_a_codex_reading_are_skipped() {
        let mut other = monthly(50.0);
        other["limit_id"] = json!("codex_other");
        let mut no_primary = monthly(60.0);
        no_primary["primary"] = Value::Null;
        let mut unnamed = monthly(20.0);
        unnamed["limit_id"] = Value::Null;
        let (_dir, env) = rollouts(
            "codex-saved-skips",
            &[(
                "rollout-a.jsonl",
                NOW,
                vec![
                    limits_line(NOW - 50, unnamed),
                    limits_line(NOW - 40, json!({"plan_type": "free"})),
                    limits_line(NOW - 30, other),
                    limits_line(NOW - 20, no_primary),
                    "not json, but token_count and \"rate_limits\"".to_owned(),
                ],
            )],
        );
        let snapshot = saved(&env, NOW).unwrap();
        assert_eq!(snapshot.fetched_at, NOW - 50);
        assert_eq!(snapshot.windows[0].used_percent, 20.0);
    }

    #[test]
    fn when_the_newest_rollout_has_no_reading_the_next_one_is_used() {
        let (_dir, env) = rollouts(
            "codex-saved-next",
            &[
                (
                    "rollout-new.jsonl",
                    NOW,
                    vec![r#"{"type":"session_meta"}"#.to_owned()],
                ),
                (
                    "rollout-old.jsonl",
                    NOW - 600,
                    vec![limits_line(NOW - 700, monthly(30.0))],
                ),
            ],
        );
        assert_eq!(saved(&env, NOW).unwrap().fetched_at, NOW - 700);
    }

    #[test]
    fn with_equal_mtimes_the_newest_line_wins() {
        let (_dir, env) = rollouts(
            "codex-saved-equal",
            &[
                (
                    "rollout-a.jsonl",
                    NOW,
                    vec![limits_line(NOW - 10, monthly(40.0))],
                ),
                (
                    "rollout-b.jsonl",
                    NOW,
                    vec![limits_line(NOW - 99, monthly(41.0))],
                ),
            ],
        );
        assert_eq!(saved(&env, NOW).unwrap().windows[0].used_percent, 40.0);
    }

    #[test]
    fn a_reading_from_the_future_is_ignored() {
        let (_dir, env) = rollouts(
            "codex-saved-future",
            &[(
                "rollout-a.jsonl",
                NOW,
                vec![
                    limits_line(NOW - 10, monthly(1.0)),
                    limits_line(NOW + 10, monthly(2.0)),
                ],
            )],
        );
        assert_eq!(saved(&env, NOW).unwrap().windows[0].used_percent, 1.0);
    }

    #[test]
    fn only_the_end_of_a_long_rollout_is_read() {
        let padding = format!(r#"{{"pad":"{}"}}"#, "x".repeat(TAIL_BYTES as usize));
        let (_dir, env) = rollouts(
            "codex-saved-tail",
            &[(
                "rollout-a.jsonl",
                NOW,
                vec![
                    limits_line(NOW - 20, monthly(5.0)),
                    padding,
                    limits_line(NOW - 30, monthly(6.0)),
                ],
            )],
        );
        // The newer line sits before the last 256 KiB, so it isn't seen.
        assert_eq!(saved(&env, NOW).unwrap().windows[0].used_percent, 6.0);
    }

    #[test]
    fn a_saved_reading_needs_codex_signed_in() {
        let (dir, env) = rollouts(
            "codex-saved-signin",
            &[(
                "rollout-a.jsonl",
                NOW,
                vec![limits_line(NOW - 10, monthly(1.0))],
            )],
        );
        std::fs::remove_file(dir.path().join("home/.codex/auth.json")).unwrap();
        assert!(saved(&env, NOW).is_none());
    }

    #[test]
    fn a_primary_window_can_be_weekly() {
        let body = json!({"rate_limit": {
            "primary_window": {"used_percent": 25, "limit_window_seconds": 604800, "reset_after_seconds": 100},
            "secondary_window": {"used_percent": 5, "limit_window_seconds": 18000, "reset_at": NOW + 50},
        }});
        let snapshot = parse(&body, NOW).unwrap();
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["weekly", "5h"]);
        assert_eq!(snapshot.windows[0].resets_at, Some(NOW + 100));
    }

    #[test]
    fn additional_limits_get_a_scope_of_their_own() {
        let body = json!({"rate_limit": {}, "additional_rate_limits": [{
            "limit_name": "GPT-5.5-Codex-Spark",
            "rate_limit": {"primary_window": {"used_percent": 60, "limit_window_seconds": 18000, "reset_at": NOW + 60}},
        }]});
        let snapshot = parse(&body, NOW).unwrap();
        assert_eq!(snapshot.windows[0].id, "gpt-5-5-codex-spark:5h");
        assert_eq!(snapshot.windows[0].scope, "gpt-5-5-codex-spark");
        assert_eq!(snapshot.windows[0].label, "5-hour, GPT-5.5-Codex-Spark");
    }

    #[test]
    fn credits_balance_may_be_a_string() {
        let body = json!({"rate_limit": {}, "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"}});
        let credits = parse(&body, NOW).unwrap().credits.unwrap();
        assert_eq!(credits.balance.unwrap().amount, 12.5);
        assert_eq!(credits.enabled, Some(true));
    }
}
