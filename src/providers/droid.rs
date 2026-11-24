//! Droid: Factory's billing limits endpoint, with the sign-in Droid keeps
//! encrypted in `~/.factory/auth.v2.keyring` and the key to it in the
//! system keyring.

use serde_json::Value;

use crate::creds;
use crate::model::{Credits, Harness, Money, Snapshot, Unavailable};
use crate::providers::{Context, Failure, request, window};
use crate::time;

const URL: &str = "https://api.factory.ai/api/billing/limits";

pub fn fetch(cx: &Context, now: i64) -> Result<Snapshot, Failure> {
    let login = creds::droid(&cx.env, &*cx.keyring)?;
    if login.expires_within(now, 0) {
        return Err(Unavailable::SignInExpired.into());
    }
    let auth = format!("Bearer {}", login.token.expose());
    let headers = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];
    let body = request(cx, Harness::Droid, URL, &headers, None)?;
    Ok(parse(&body, now)?)
}

/// Factory's two pools. Standard is spent first, then Droid Core.
const POOLS: [(&str, &str); 2] = [("standard", "Standard"), ("core", "Droid Core")];
const WINDOWS: [(&str, &str, u32, &str); 3] = [
    ("fiveHour", "5h", 300, "5-hour"),
    ("weekly", "weekly", 10_080, "Weekly"),
    ("monthly", "monthly", 43_200, "Monthly"),
];

/// Windows from the limits response. A window with no `windowEnd` hasn't
/// started.
pub fn parse(body: &Value, now: i64) -> Result<Snapshot, Unavailable> {
    if body["usesTokenRateLimitsBilling"] == false {
        return Err(Unavailable::Unsupported(
            "This Factory plan uses allowance billing, which to11y doesn't read yet.".to_owned(),
        ));
    }
    if body["hasActiveSubscription"] == false {
        return Err(Unavailable::Unsupported(
            "This Factory account has no active subscription.".to_owned(),
        ));
    }
    let limits = &body["limits"];
    if !limits.is_object() {
        return Err(Unavailable::Failed(
            "Factory sent an unexpected limits response.".to_owned(),
        ));
    }
    let mut windows = Vec::new();
    for (pool, pool_label) in POOLS {
        for (key, id, minutes, length) in WINDOWS {
            let w = &limits[pool][key];
            let Some(used) = w["usedPercent"].as_f64() else {
                continue;
            };
            windows.push(window(
                format!("{pool}:{id}"),
                format!("{pool_label}, {length}"),
                pool,
                Some(minutes),
                used,
                w["windowEnd"].as_str().and_then(time::parse),
            ));
        }
    }
    let balance = body["extraUsageBalanceCents"].as_f64();
    Ok(Snapshot {
        harness: Harness::Droid,
        plan: None,
        tier: None,
        windows,
        credits: Some(Credits {
            enabled: extra_usage_on(body),
            balance: balance.map(|cents| Money {
                amount: cents / 100.0,
                currency: "USD".to_owned(),
            }),
            note: body["overagePreference"].as_str().map(str::to_owned),
            ..Credits::default()
        }),
        fetched_at: now,
        origin: crate::model::Origin::Provider,
    })
}

/// Whether extra usage is switched on. `extraUsageAllowed` only says the
/// account may use it, and is false on a free trial. Droid counts it as on
/// when the account's `overagePreference` is `extraUsage` and it is allowed.
/// With no preference to go by, to11y doesn't guess.
fn extra_usage_on(body: &Value) -> Option<bool> {
    let allowed = body["extraUsageAllowed"].as_bool();
    match (body["overagePreference"].as_str(), allowed) {
        (_, Some(false)) | (Some("droidCore"), _) => Some(false),
        (Some("extraUsage"), allowed) => allowed,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowState;
    use serde_json::json;

    const NOW: i64 = 1_795_444_270; // 2026-11-23T14:31:10Z

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/droid.json")).unwrap()
    }

    #[test]
    fn the_fixture_has_three_windows_in_each_pool() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "standard:5h",
                "standard:weekly",
                "standard:monthly",
                "core:5h",
                "core:weekly",
                "core:monthly"
            ]
        );
        let weekly = &snapshot.windows[1];
        assert_eq!(weekly.label, "Standard, Weekly");
        assert_eq!(weekly.used_percent, 100.0);
        assert_eq!(weekly.resets_at, time::parse("2026-11-23T16:03:17Z"));
    }

    #[test]
    fn a_window_with_no_end_has_not_started() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        assert_eq!(snapshot.windows[0].state, WindowState::NotStarted);
        assert_eq!(snapshot.windows[0].resets_at, None);
    }

    #[test]
    fn a_spent_weekly_window_blocks_its_pool_until_it_resets() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        let (assessed, scopes) = crate::pace::assess(&snapshot.windows, NOW);
        let standard = scopes.iter().find(|s| s.name == "standard").unwrap();
        assert!(standard.blocked);
        assert_eq!(standard.blocked_until, time::parse("2026-11-23T16:03:17Z"));
        let monthly = &assessed[2];
        assert_eq!(monthly.pace.verdict, crate::pace::Verdict::Blocked);
    }

    #[test]
    fn the_balance_is_in_dollars() {
        let body = json!({"limits": {}, "extraUsageBalanceCents": 1250});
        let credits = parse(&body, NOW).unwrap().credits.unwrap();
        assert_eq!(credits.balance.unwrap().amount, 12.5);
    }

    #[test]
    fn extra_usage_is_on_only_when_it_is_chosen_and_allowed() {
        // The fixture's account may use extra usage but chose Droid Core.
        let credits = parse(&fixture(), NOW).unwrap().credits.unwrap();
        assert_eq!(credits.enabled, Some(false));
        assert_eq!(credits.note.as_deref(), Some("droidCore"));

        let enabled = |body: Value| parse(&body, NOW).unwrap().credits.unwrap().enabled;
        assert_eq!(
            enabled(
                json!({"limits": {}, "overagePreference": "extraUsage", "extraUsageAllowed": true})
            ),
            Some(true)
        );
        assert_eq!(
            enabled(
                json!({"limits": {}, "overagePreference": "extraUsage", "extraUsageAllowed": false})
            ),
            Some(false)
        );
        assert_eq!(
            enabled(json!({"limits": {}, "extraUsageAllowed": false})),
            Some(false)
        );
        // Allowed, but nothing says which way the account chose.
        assert_eq!(
            enabled(json!({"limits": {}, "extraUsageAllowed": true})),
            None
        );
    }

    #[test]
    fn allowance_billing_and_no_subscription_are_unsupported() {
        for body in [
            json!({"usesTokenRateLimitsBilling": false, "limits": {}}),
            json!({"hasActiveSubscription": false, "limits": {}}),
        ] {
            assert!(matches!(
                parse(&body, NOW),
                Err(Unavailable::Unsupported(_))
            ));
        }
    }
}
