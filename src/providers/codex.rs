//! Codex: ChatGPT's usage endpoint, with the sign-in Codex stores in
//! `auth.json`.

use serde_json::Value;

use crate::creds;
use crate::model::{Credits, Harness, Money, Snapshot, Unavailable, Window};
use crate::pace::ALL;
use crate::providers::{Context, Failure, length_id, length_label, request, slug, window};

const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

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
    use serde_json::json;

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
