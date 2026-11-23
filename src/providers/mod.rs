//! One module per harness: where its sign-in lives, the call that returns
//! its limits, and the parser for the answer. Parsers are pure and take
//! the time as `now`.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod droid;

use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::creds::Keyring;
use crate::env::Env;
use crate::http;
use crate::model::{Harness, Snapshot, Unavailable, Window, WindowKind, WindowState};

/// How long one harness may take, end to end.
pub const DEADLINE: Duration = Duration::from_secs(12);
/// The longest `Retry-After` honoured, so a bad header can't stall a
/// harness for days.
const MAX_RETRY_AFTER: i64 = 3600;

/// What a fetch needs. Cheap to clone, so a fetch can run on its own
/// thread.
#[derive(Clone)]
pub struct Context {
    pub env: Arc<Env>,
    pub keyring: Arc<dyn Keyring>,
    pub client: http::Client,
}

/// Why a fetch failed, and how long the provider asked us to wait.
#[derive(Debug, PartialEq)]
pub struct Failure {
    pub why: Unavailable,
    /// Seconds.
    pub retry_after: Option<i64>,
}

impl From<Unavailable> for Failure {
    fn from(why: Unavailable) -> Self {
        Failure {
            why,
            retry_after: None,
        }
    }
}

/// The harness's current limits, from its provider.
pub fn fetch(harness: Harness, cx: &Context, now: i64) -> Result<Snapshot, Failure> {
    #[cfg(debug_assertions)]
    if let Some(dir) = &cx.env.fixtures {
        return fixture(harness, dir, now);
    }
    match harness {
        Harness::ClaudeCode => claude::fetch(cx, now),
        Harness::Codex => codex::fetch(cx, now),
        Harness::Droid => droid::fetch(cx, now),
        Harness::Antigravity => antigravity::fetch(cx, now),
    }
}

/// `fetch`, given up on after `deadline`. A fetch that is given up on
/// finishes on its own thread, within its own timeouts, and is ignored.
pub fn fetch_within(
    harness: Harness,
    cx: &Context,
    now: i64,
    deadline: Duration,
) -> Result<Snapshot, Failure> {
    let (send, receive) = mpsc::channel();
    let cx = cx.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("fetch-{}", harness.name()))
        .spawn(move || {
            let _ = send.send(fetch(harness, &cx, now));
        });
    if spawned.is_err() {
        return Err(Unavailable::Failed("to11y couldn't start a thread.".to_owned()).into());
    }
    receive.recv_timeout(deadline).unwrap_or_else(|_| {
        Err(Unavailable::Failed(format!(
            "{} didn't answer within {} seconds.",
            harness.provider(),
            deadline.as_secs()
        ))
        .into())
    })
}

/// One request whose answer is JSON. A 401 means the stored sign-in no
/// longer works. Failures name the status, never the body.
pub fn request(
    cx: &Context,
    harness: Harness,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<Value, Failure> {
    let provider = harness.provider();
    let response = cx.client.send(url, headers, body).map_err(|error| {
        Failure::from(Unavailable::Failed(format!(
            "Couldn't reach {provider}: {error}."
        )))
    })?;
    let retry_after = response
        .header("retry-after")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .map(|seconds| seconds.clamp(0, MAX_RETRY_AFTER));
    let failed = |text: String| Failure {
        why: Unavailable::Failed(text),
        retry_after,
    };
    match response.status {
        200 => serde_json::from_slice(&response.body)
            .map_err(|_| failed(format!("{provider} sent a usage answer that isn't JSON."))),
        401 => Err(Failure {
            why: Unavailable::SignInExpired,
            retry_after,
        }),
        429 => Err(failed(format!(
            "{provider} is limiting how often usage can be checked (HTTP 429)."
        ))),
        status => Err(failed(format!(
            "{provider} answered the usage request with HTTP {status}."
        ))),
    }
}

/// `<dir>/<harness>.json` parsed as the provider's answer. A missing file
/// is a harness that isn't installed, and `{"to11y_unavailable": ...}`
/// stands for any other reason. `"to11y_delay_ms"` makes the answer slow,
/// so tests can cancel a call while it runs.
#[cfg(debug_assertions)]
fn fixture(harness: Harness, dir: &std::path::Path, now: i64) -> Result<Snapshot, Failure> {
    let path = dir.join(format!("{}.json", harness.name()));
    let bad = || {
        Failure::from(Unavailable::Failed(format!(
            "{} is a bad fixture.",
            path.display()
        )))
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Unavailable::NotInstalled.into());
        }
        Err(_) => return Err(bad()),
    };
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if let Some(delay) = json["to11y_delay_ms"].as_u64() {
        std::thread::sleep(Duration::from_millis(delay));
    }
    if let Some(why) = json.get("to11y_unavailable") {
        return Err(Unavailable::from_json(why).map_or_else(bad, Failure::from));
    }
    let parsed = match harness {
        Harness::ClaudeCode => claude::parse(&json, now),
        Harness::Codex => codex::parse(&json, now),
        Harness::Droid => droid::parse(&json, now),
        Harness::Antigravity => antigravity::parse(&json, now),
    };
    parsed.map_err(Failure::from)
}

/// A lower-case name made of letters, digits, and single dashes.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// The words for a window's length, as in `5-hour` or `Weekly`.
pub fn length_label(minutes: Option<u32>) -> String {
    match minutes.map(WindowKind::from_minutes) {
        Some(WindowKind::FiveHour) => "5-hour".to_owned(),
        Some(WindowKind::Weekly) => "Weekly".to_owned(),
        Some(WindowKind::Monthly) => "Monthly".to_owned(),
        _ => match minutes {
            Some(minutes) if minutes % 1440 == 0 => format!("{}-day", minutes / 1440),
            Some(minutes) if minutes % 60 == 0 => format!("{}-hour", minutes / 60),
            Some(minutes) => format!("{minutes}-minute"),
            None => "Rolling".to_owned(),
        },
    }
}

/// The part of a window id that names its length, as in `5h` or `weekly`.
pub fn length_id(minutes: Option<u32>) -> String {
    match minutes.map(WindowKind::from_minutes) {
        Some(WindowKind::Other) | None => match minutes {
            Some(minutes) => format!("{minutes}m"),
            None => "window".to_owned(),
        },
        Some(kind) => kind.name().to_owned(),
    }
}

/// A running window, or one not started when it has no use and no reset.
pub fn window(
    id: String,
    label: String,
    scope: &str,
    minutes: Option<u32>,
    used_percent: f64,
    resets_at: Option<i64>,
) -> Window {
    let state = if resets_at.is_none() && used_percent <= 0.0 {
        WindowState::NotStarted
    } else {
        WindowState::Running
    };
    Window {
        id,
        label,
        scope: scope.to_owned(),
        kind: minutes.map_or(WindowKind::Other, WindowKind::from_minutes),
        state,
        used_percent,
        resets_at,
        window_minutes: minutes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemoryKeyring;

    /// The outcome of one request to a localhost server that sends `head`
    /// and `body`.
    fn answer(head: &str, body: &str) -> Result<Value, Failure> {
        let response = format!("{head}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let (port, server) = crate::testing::https_server(response.into_bytes());
        let cx = Context {
            env: Arc::new(Env::default()),
            keyring: Arc::new(MemoryKeyring::default()),
            client: crate::testing::https_client(),
        };
        let url = format!("https://localhost:{port}/usage");
        let outcome = request(&cx, Harness::Codex, &url, &[], None);
        server.join().unwrap();
        outcome
    }

    #[test]
    fn a_200_answer_is_its_json() {
        assert_eq!(
            answer("HTTP/1.1 200 OK", r#"{"ok":true}"#).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn a_401_means_the_sign_in_no_longer_works() {
        let failure = answer("HTTP/1.1 401 Unauthorized", r#"{"error":"token expired"}"#);
        assert_eq!(failure.unwrap_err().why, Unavailable::SignInExpired);
    }

    #[test]
    fn a_429_keeps_its_retry_after_and_never_the_body() {
        let failure = answer(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120",
            r#"{"detail":"secret-ish body text"}"#,
        )
        .unwrap_err();
        assert_eq!(failure.retry_after, Some(120));
        let Unavailable::Failed(text) = failure.why else {
            panic!("not a failure")
        };
        assert!(
            text.contains("429") && !text.contains("secret-ish"),
            "{text}"
        );
    }

    #[test]
    fn a_huge_retry_after_is_capped_at_an_hour() {
        let failure = answer("HTTP/1.1 503 Busy\r\nRetry-After: 9999999", "").unwrap_err();
        assert_eq!(failure.retry_after, Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn an_answer_that_is_not_json_is_a_failure() {
        let failure = answer("HTTP/1.1 200 OK", "<html>").unwrap_err();
        assert!(matches!(failure.why, Unavailable::Failed(_)));
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let failure = answer(
            "HTTP/1.1 302 Found\r\nLocation: https://elsewhere.invalid/",
            "",
        )
        .unwrap_err();
        assert!(matches!(failure.why, Unavailable::Failed(text) if text.contains("302")));
    }

    #[test]
    fn slugs_are_lower_case_words_joined_by_dashes() {
        assert_eq!(slug("Claude and GPT models"), "claude-and-gpt-models");
        assert_eq!(slug("  Gemini 4 Argon!"), "gemini-4-argon");
        assert_eq!(slug("GPT-5.5-Codex-Spark"), "gpt-5-5-codex-spark");
    }

    #[test]
    fn labels_and_ids_name_the_window_length() {
        assert_eq!(length_label(Some(300)), "5-hour");
        assert_eq!(length_label(Some(43_200)), "Monthly");
        assert_eq!(length_label(Some(1440)), "1-day");
        assert_eq!(length_id(Some(10_080)), "weekly");
        assert_eq!(length_id(Some(90)), "90m");
    }
}
