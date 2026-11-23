//! Claude Code: Anthropic's OAuth usage endpoint, with the sign-in Claude
//! Code stores in `.credentials.json`.

use std::fmt;
use std::io::{BufReader, Read};

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

use crate::creds;
use crate::env::Env;
use crate::model::{Credits, Harness, Money, Origin, Snapshot, Unavailable};
use crate::pace::ALL;
use crate::providers::{Context, Failure, request, slug, window};
use crate::time;

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// `.claude.json` grows with project history, so the cap is generous. The
/// file is streamed and only two of its fields are kept.
const MAX_SETTINGS_BYTES: u64 = 32 * 1024 * 1024;

pub fn fetch(cx: &Context, now: i64) -> Result<Snapshot, Failure> {
    let login = creds::claude(&cx.env)?;
    if login.expires_within(now, 0) {
        return Err(Unavailable::SignInExpired.into());
    }
    let auth = format!("Bearer {}", login.token.expose());
    let headers = [
        ("Authorization", auth.as_str()),
        ("anthropic-beta", "oauth-2025-04-20"),
        ("Accept", "application/json"),
    ];
    let body = request(cx, Harness::ClaudeCode, URL, &headers, None)?;
    let mut snapshot = parse(&body, now)?;
    snapshot.plan = login.plan;
    snapshot.tier = login.tier;
    Ok(snapshot)
}

/// The usage answer Claude Code saved in `.claude.json` the last time it
/// asked, as Claude Code's own `cachedUsageUtilization`. It counts only when
/// it names the account signed in now, Claude Code is signed in, and it
/// isn't from the future. The account ids are compared, then dropped.
pub fn saved(env: &Env, now: i64) -> Option<Snapshot> {
    let login = creds::claude(env).ok()?;
    let file = std::fs::File::open(env.claude_json()?).ok()?;
    let reader = BufReader::new(file.take(MAX_SETTINGS_BYTES));
    let settings =
        Settings::deserialize(&mut serde_json::Deserializer::from_reader(reader)).ok()?;
    let saved = settings.saved?;
    let signed_in = settings.account?;
    if saved["accountUuid"].as_str() != Some(signed_in.as_str()) {
        return None;
    }
    let fetched_at = saved["fetchedAtMs"].as_i64()?.div_euclid(1000);
    if fetched_at > now {
        return None;
    }
    let mut snapshot = parse(&saved["utilization"], fetched_at).ok()?;
    snapshot.plan = login.plan;
    snapshot.tier = login.tier;
    snapshot.origin = Origin::Saved;
    Some(snapshot)
}

/// The two parts of `.claude.json` to11y reads. Everything else, such as
/// the user's name and email, is skipped without being kept.
#[derive(Default)]
struct Settings {
    saved: Option<Value>,
    account: Option<String>,
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Top;
        impl<'de> Visitor<'de> for Top {
            type Value = Settings;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Settings, A::Error> {
                let mut settings = Settings::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "cachedUsageUtilization" => settings.saved = Some(map.next_value()?),
                        "oauthAccount" => settings.account = map.next_value::<Account>()?.0,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(settings)
            }
        }
        deserializer.deserialize_map(Top)
    }
}

/// `oauthAccount.accountUuid`, and nothing else of `oauthAccount`.
struct Account(Option<String>);

impl<'de> Deserialize<'de> for Account {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Account;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object or null")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Account, E> {
                Ok(Account(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Account, A::Error> {
                let mut id = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "accountUuid" {
                        id = map.next_value::<Option<String>>()?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Account(id))
            }
        }
        deserializer.deserialize_any(Fields)
    }
}

/// Windows from the usage response. `five_hour` and `seven_day` limit
/// everything. Model-scoped weekly windows come from `limits[]` entries of
/// kind `weekly_scoped`; the `weekly_all` entry repeats `seven_day`.
pub fn parse(body: &Value, now: i64) -> Result<Snapshot, Unavailable> {
    if body.get("five_hour").is_none() && body.get("seven_day").is_none() {
        return Err(Unavailable::Failed(
            "Anthropic sent an unexpected usage response.".to_owned(),
        ));
    }
    let mut windows = Vec::new();
    for (key, id, label, minutes) in [
        ("five_hour", "5h", "5-hour, all models", 300),
        ("seven_day", "weekly", "Weekly, all models", 10_080),
    ] {
        let Some(used) = body[key]["utilization"].as_f64() else {
            continue;
        };
        let reset = body[key]["resets_at"].as_str().and_then(time::parse);
        windows.push(window(
            id.to_owned(),
            label.to_owned(),
            ALL,
            Some(minutes),
            used,
            reset,
        ));
    }
    let scoped = body["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            if limit["kind"] != "weekly_scoped" {
                return None;
            }
            let model = limit["scope"]["model"]["display_name"].as_str()?;
            Some((model.to_owned(), &limit["percent"], &limit["resets_at"]))
        });
    let named = [("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")]
        .into_iter()
        .map(|(key, model)| {
            (
                model.to_owned(),
                &body[key]["utilization"],
                &body[key]["resets_at"],
            )
        });
    for (model, used, reset) in scoped.chain(named) {
        let scope = slug(&model);
        let Some(used) = used.as_f64() else { continue };
        if windows.iter().any(|w| w.scope == scope) {
            continue;
        }
        windows.push(window(
            format!("weekly:{scope}"),
            format!("Weekly, {model}"),
            &scope,
            Some(10_080),
            used,
            reset.as_str().and_then(time::parse),
        ));
    }
    Ok(Snapshot {
        harness: Harness::ClaudeCode,
        plan: None,
        tier: None,
        windows,
        credits: credits(body),
        fetched_at: now,
        origin: crate::model::Origin::Provider,
    })
}

/// Extra usage, from `spend` when it is there, else `extra_usage`.
fn credits(body: &Value) -> Option<Credits> {
    let money = |value: &Value| {
        let minor = value["amount_minor"].as_f64()?;
        let exponent = value["exponent"].as_i64().unwrap_or(2);
        Some(Money {
            amount: minor / 10f64.powi(exponent as i32),
            currency: value["currency"].as_str()?.to_owned(),
        })
    };
    let spend = &body["spend"];
    if spend.is_object() {
        return Some(Credits {
            enabled: spend["enabled"].as_bool(),
            unlimited: None,
            balance: money(&spend["balance"]),
            used: money(&spend["used"]),
            limit: money(&spend["limit"]).or_else(|| money(&spend["cap"]["money"])),
            note: spend["disabled_reason"].as_str().map(str::to_owned),
        });
    }
    let extra = &body["extra_usage"];
    if extra.is_object() {
        let places = extra["decimal_places"].as_i64().unwrap_or(2) as i32;
        let limit = extra["monthly_limit"]
            .as_f64()
            .zip(extra["currency"].as_str());
        return Some(Credits {
            enabled: extra["is_enabled"].as_bool(),
            limit: limit.map(|(amount, currency)| Money {
                amount: amount / 10f64.powi(places),
                currency: currency.to_owned(),
            }),
            note: extra["disabled_reason"].as_str().map(str::to_owned),
            ..Credits::default()
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowState;
    use serde_json::json;

    const NOW: i64 = 1_795_444_270; // 2026-11-23T14:31:10Z

    #[test]
    fn an_expired_sign_in_fails_before_any_request() {
        use crate::creds::MemoryKeyring;
        use crate::env::Env;
        use std::sync::Arc;
        let dir = crate::testing::TempDir::new("claude-expired");
        dir.write(
            "home/.claude/.credentials.json",
            r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":1795444000000}}"#,
        );
        let cx = Context {
            env: Arc::new(Env {
                home: Some(dir.path().join("home")),
                ..Env::default()
            }),
            keyring: Arc::new(MemoryKeyring::default()),
            // Nothing listens on this client's behalf; a request would fail
            // with a different reason.
            client: crate::http::Client::new(std::time::Duration::from_millis(1)),
        };
        assert_eq!(fetch(&cx, NOW).unwrap_err().why, Unavailable::SignInExpired);
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/claude-code.json")).unwrap()
    }

    #[test]
    fn the_fixture_has_a_weekly_window_and_a_fable_window() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["weekly", "weekly:fable"]);
        let weekly = &snapshot.windows[0];
        assert_eq!(weekly.used_percent, 6.0);
        assert_eq!(weekly.resets_at, time::parse("2026-11-30T09:59:59Z"));
        assert_eq!(weekly.scope, "all");
        assert_eq!(weekly.state, WindowState::Running);
        let fable = &snapshot.windows[1];
        assert_eq!((fable.scope.as_str(), fable.used_percent), ("fable", 4.0));
        assert_eq!(fable.label, "Weekly, Fable");
    }

    #[test]
    fn a_null_five_hour_window_is_left_out() {
        let snapshot = parse(&fixture(), NOW).unwrap();
        assert!(snapshot.windows.iter().all(|w| w.id != "5h"));
    }

    #[test]
    fn a_five_hour_window_is_read_when_present() {
        let body = json!({
            "five_hour": {"utilization": 42.0, "resets_at": "2026-11-23T17:00:00+00:00"},
            "seven_day": null,
        });
        let snapshot = parse(&body, NOW).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].id, "5h");
        assert_eq!(snapshot.windows[0].window_minutes, Some(300));
    }

    #[test]
    fn named_model_windows_fill_in_for_missing_scoped_limits() {
        let body = json!({
            "five_hour": null,
            "seven_day": {"utilization": 10.0, "resets_at": "2026-11-30T09:59:59Z"},
            "seven_day_opus": {"utilization": 30.0, "resets_at": "2026-11-30T09:59:59Z"},
            "limits": [{"kind": "weekly_scoped", "percent": 4, "resets_at": "2026-11-30T09:59:59Z",
                        "scope": {"model": {"display_name": "Opus"}}}],
        });
        let snapshot = parse(&body, NOW).unwrap();
        let opus: Vec<_> = snapshot
            .windows
            .iter()
            .filter(|w| w.scope == "opus")
            .collect();
        assert_eq!(opus.len(), 1);
        assert_eq!(opus[0].used_percent, 4.0);
    }

    #[test]
    fn spend_gives_the_extra_usage_credits() {
        let credits = parse(&fixture(), NOW).unwrap().credits.unwrap();
        assert_eq!(credits.enabled, Some(false));
        assert_eq!(credits.limit.unwrap().amount, 10.0);
        assert_eq!(credits.used.unwrap().currency, "EUR");
        assert_eq!(credits.note.as_deref(), Some("out_of_credits"));
    }

    /// A home where Claude Code is signed in and saved `fetched_ms` as its
    /// last reading, for account `saved_for`, while `signed_in` is signed in.
    fn saved_home(
        name: &str,
        saved_for: &str,
        signed_in: &str,
        fetched_ms: i64,
    ) -> crate::testing::TempDir {
        let dir = crate::testing::TempDir::new(name);
        dir.write(
            "home/.claude/.credentials.json",
            r#"{"claudeAiOauth":{"accessToken":"tok","subscriptionType":"max"}}"#,
        );
        let settings = json!({
            "numStartups": 12,
            "oauthAccount": {
                "accountUuid": signed_in,
                "emailAddress": "planted@example.invalid",
                "displayName": "Planted Name",
            },
            "projects": {"/home/planted/secret-project": {"history": []}},
            "cachedUsageUtilization": {
                "fetchedAtMs": fetched_ms,
                "accountUuid": saved_for,
                "utilization": fixture(),
            },
        });
        dir.write("home/.claude.json", &settings.to_string());
        dir
    }

    fn home_env(dir: &crate::testing::TempDir) -> Env {
        Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        }
    }

    #[test]
    fn claude_codes_saved_reading_reads_like_the_provider_answer() {
        let dir = saved_home(
            "claude-saved-reads",
            "acct-1",
            "acct-1",
            (NOW - 90) * 1000 + 500,
        );
        let snapshot = saved(&home_env(&dir), NOW).unwrap();
        let answer = parse(&fixture(), NOW - 90).unwrap();
        assert_eq!(snapshot.windows, answer.windows);
        assert_eq!(snapshot.fetched_at, NOW - 90);
        assert_eq!(snapshot.origin, Origin::Saved);
        assert_eq!(snapshot.plan.as_deref(), Some("max"));
        let text = snapshot.to_json().to_string();
        for planted in ["acct-1", "planted", "Planted", "secret-project"] {
            assert!(!text.contains(planted), "{planted} leaked");
        }
    }

    #[test]
    fn a_saved_reading_for_another_account_is_ignored() {
        let dir = saved_home(
            "claude-saved-account",
            "acct-old",
            "acct-new",
            (NOW - 90) * 1000,
        );
        assert_eq!(saved(&home_env(&dir), NOW), None);
    }

    #[test]
    fn a_saved_reading_from_the_future_is_ignored() {
        let dir = saved_home("claude-saved-future", "acct-1", "acct-1", (NOW + 90) * 1000);
        assert_eq!(saved(&home_env(&dir), NOW), None);
    }

    #[test]
    fn a_saved_reading_needs_claude_code_signed_in() {
        let dir = saved_home("claude-saved-signin", "acct-1", "acct-1", (NOW - 90) * 1000);
        std::fs::remove_file(dir.path().join("home/.claude/.credentials.json")).unwrap();
        assert_eq!(saved(&home_env(&dir), NOW), None);
    }

    #[test]
    fn a_damaged_or_accountless_settings_file_gives_no_reading() {
        let dir = saved_home(
            "claude-saved-damaged",
            "acct-1",
            "acct-1",
            (NOW - 90) * 1000,
        );
        dir.write(
            "home/.claude.json",
            r#"{"cachedUsageUtilization": {"fetchedAtMs": 1"#,
        );
        assert_eq!(saved(&home_env(&dir), NOW), None);
        dir.write(
            "home/.claude.json",
            r#"{"oauthAccount": null, "cachedUsageUtilization": {"fetchedAtMs": 1, "accountUuid": "a"}}"#,
        );
        assert_eq!(saved(&home_env(&dir), NOW), None);
    }

    #[test]
    fn the_settings_file_follows_claude_config_dir() {
        let dir = saved_home("claude-saved-config", "acct-1", "acct-1", (NOW - 90) * 1000);
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::rename(
            dir.path().join("home/.claude.json"),
            config.join(".claude.json"),
        )
        .unwrap();
        std::fs::rename(dir.path().join("home/.claude"), config.join("creds")).unwrap();
        std::fs::rename(
            config.join("creds/.credentials.json"),
            config.join(".credentials.json"),
        )
        .unwrap();
        let env = Env {
            claude_config_dir: Some(config),
            ..home_env(&dir)
        };
        assert!(saved(&env, NOW).is_some());
    }

    #[test]
    fn a_response_with_no_windows_at_all_is_a_failure() {
        assert!(matches!(
            parse(&json!({"error": "x"}), NOW),
            Err(Unavailable::Failed(_))
        ));
    }
}
