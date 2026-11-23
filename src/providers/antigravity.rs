//! Antigravity: Google's quota summary, with the sign-in Antigravity keeps
//! in the system keyring. When that sign-in has expired, Antigravity's own
//! `agy -p /usage` gives the same groups, and renews it.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::creds::{self, Login};
use crate::model::{Harness, Snapshot, Unavailable, WindowState};
use crate::providers::{Context, Failure, length_label, request, slug, window};
use crate::time;

const HOSTS: [&str; 2] = [
    "daily-cloudcode-pa.googleapis.com",
    "cloudcode-pa.googleapis.com",
];
/// Google answers 403 to any other user agent.
const USER_AGENT: &str =
    "antigravity/hub/2.24.0 (aidev_client; os_type=linux; arch=x64; cl=963137146)";
const AGY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_AGY_BYTES: u64 = 1024 * 1024;

/// The direct call while the stored token is good for another minute.
/// Otherwise, or when Google turns it down, Antigravity's own `agy`, which
/// renews the stored sign-in as a side effect.
pub fn fetch(cx: &Context, now: i64) -> Result<Snapshot, Failure> {
    let login = creds::antigravity(&cx.env, &*cx.keyring)?;
    if !login.expires_within(now, 60) {
        match direct(cx, &login, now) {
            Err(failure) if failure.why == Unavailable::SignInExpired => {}
            other => return other,
        }
    }
    if cx.env.no_agy {
        return Err(Unavailable::SignInExpired.into());
    }
    let (Some(program), Some(dir)) = (cx.env.find_program("agy"), cx.env.cache_dir()) else {
        return Err(Unavailable::SignInExpired.into());
    };
    crate::cache::make_dir(&dir).map_err(|_| {
        Unavailable::Failed("to11y couldn't create its cache directory.".to_owned())
    })?;
    let out = run_agy(&program, &dir, cx.env.path.as_deref(), AGY_TIMEOUT)?;
    let json: Value = serde_json::from_slice(&out)
        .map_err(|_| Unavailable::Failed("agy's /usage answer isn't JSON.".to_owned()))?;
    Ok(parse(&json, now)?)
}

fn direct(cx: &Context, login: &Login, now: i64) -> Result<Snapshot, Failure> {
    let auth = format!("Bearer {}", login.token.expose());
    let headers = [
        ("Authorization", auth.as_str()),
        ("User-Agent", USER_AGENT),
        ("Accept", "application/json"),
    ];
    let mut last = None;
    for host in HOSTS {
        let url = format!("https://{host}/v1internal:retrieveUserQuotaSummary");
        match request(cx, Harness::Antigravity, &url, &headers, Some(b"{}")) {
            Ok(body) => return Ok(parse(&body, now)?),
            Err(failure) if failure.why == Unavailable::SignInExpired => return Err(failure),
            Err(failure) => last = Some(failure),
        }
    }
    Err(last.expect("HOSTS is not empty"))
}

/// How many `agy` that to11y started are still running.
static RUNNING_AGY: AtomicUsize = AtomicUsize::new(0);

/// Counts one running `agy` until dropped.
struct Running;

impl Running {
    fn start() -> Self {
        RUNNING_AGY.fetch_add(1, Ordering::SeqCst);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING_AGY.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Waits until no `agy` that to11y started is running, for at most agy's
/// timeout, so a command line run that gave up on agy doesn't exit while
/// agy renews the sign-in.
pub fn wait_for_agy() {
    let started = Instant::now();
    while RUNNING_AGY.load(Ordering::SeqCst) > 0
        && started.elapsed() < AGY_TIMEOUT + Duration::from_secs(1)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `agy -p /usage --output-format json`, with nothing on its stdin, its
/// errors dropped, and `dir` as its working directory. Killed after
/// `timeout`.
///
/// It can't update itself or open a browser: a sign-in that can't renew
/// silently would otherwise open one. `path` is the `PATH` to give it,
/// after the folder that holds the browser stub.
fn run_agy(
    program: &Path,
    dir: &Path,
    path: Option<&OsStr>,
    timeout: Duration,
) -> Result<Vec<u8>, Unavailable> {
    let no_browser = no_browser(dir).map_err(|_| {
        Unavailable::Failed("to11y couldn't keep agy from opening a browser.".to_owned())
    })?;
    let mut search = OsString::from(dir.join(NO_BROWSER));
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        search.push(":");
        search.push(path);
    }
    let spawn = || {
        Command::new(program)
            .args(["-p", "/usage", "--output-format", "json"])
            .current_dir(dir)
            .env("TO11Y_NO_AGY", "1")
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .env("PATH", &search)
            .env("BROWSER", &no_browser)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    // A program that is still open for writing, as while agy updates
    // itself, can't be run for a moment ("text file busy").
    const TEXT_FILE_BUSY: i32 = 26;
    let mut attempt = spawn();
    for _ in 0..5 {
        match &attempt {
            Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                std::thread::sleep(Duration::from_millis(20));
                attempt = spawn();
            }
            _ => break,
        }
    }
    let mut child = attempt
        .map_err(|error| Unavailable::Failed(format!("agy couldn't be started: {error}.")))?;
    let _running = Running::start();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(MAX_AGY_BYTES).read_to_end(&mut out);
        let _ = send.send(out);
    });
    let started = Instant::now();
    let too_slow = || {
        Unavailable::Failed(format!(
            "agy didn't answer within {} seconds.",
            timeout.as_secs()
        ))
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(too_slow());
            }
        }
    };
    if !status.success() {
        return Err(Unavailable::Failed(format!("agy failed with {status}.")));
    }
    // A process agy left behind could hold its output open, so the wait
    // for the rest of it is bounded too.
    let left = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(1));
    receive.recv_timeout(left).map_err(|_| too_slow())
}

/// The folder, inside the cache, whose `xdg-open` always fails.
const NO_BROWSER: &str = "no-browser";

/// Writes the `xdg-open` that always fails, unless it is already there,
/// and returns its path.
fn no_browser(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    const STUB: &[u8] = b"#!/bin/sh\nexit 1\n";
    let folder = dir.join(NO_BROWSER);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let stub = folder.join("xdg-open");
    if std::fs::read(&stub).ok().as_deref() != Some(STUB) {
        let partial = folder.join("xdg-open.partial");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(&partial)?;
        std::io::Write::write_all(&mut file, STUB)?;
        drop(file);
        std::fs::rename(&partial, &stub)?;
    }
    Ok(stub)
}

/// Windows from either answer: `retrieveUserQuotaSummary`, or the JSON of
/// `agy -p /usage --output-format json`, whose field names are snake case.
pub fn parse(body: &Value, now: i64) -> Result<Snapshot, Unavailable> {
    let groups = match (
        body["groups"].as_array(),
        body["command"]["data"]["groups"].as_array(),
    ) {
        (Some(groups), _) => groups,
        (None, Some(groups)) if body["status"] == "SUCCESS" => groups,
        _ => {
            return Err(Unavailable::Failed(
                "Google sent an unexpected quota response.".to_owned(),
            ));
        }
    };
    let field = |value: &'_ Value, camel: &str, snake: &str| -> Value {
        value
            .get(camel)
            .or_else(|| value.get(snake))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut windows = Vec::new();
    for group in groups {
        let Some(name) = field(group, "displayName", "name")
            .as_str()
            .map(str::to_owned)
        else {
            continue;
        };
        let scope = scope(&name);
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let Some(remaining) = field(bucket, "remainingFraction", "remaining_fraction").as_f64()
            else {
                continue;
            };
            let length = bucket["window"].as_str().unwrap_or_default();
            let minutes = match length {
                "5h" => Some(300),
                "weekly" => Some(10_080),
                _ => None,
            };
            let reset = field(bucket, "resetTime", "reset_time")
                .as_str()
                .and_then(time::parse);
            let id_length = if length.is_empty() { "window" } else { length };
            let mut w = window(
                format!("{scope}:{id_length}"),
                format!("{name}, {}", length_label(minutes).to_lowercase()),
                &scope,
                minutes,
                (1.0 - remaining.clamp(0.0, 1.0)) * 100.0,
                reset,
            );
            // An untouched bucket reports a reset of now plus its length.
            let untouched = remaining >= 1.0
                && bucket.get("description").is_none()
                && match (reset, minutes) {
                    (Some(reset), Some(minutes)) => {
                        (reset - now - i64::from(minutes) * 60).abs() <= 60
                    }
                    _ => false,
                };
            if bucket["disabled"] == true {
                // A window that doesn't apply has no reset worth reporting.
                w.state = WindowState::Disabled;
                w.resets_at = None;
            } else if untouched {
                w.state = WindowState::NotStarted;
                w.resets_at = None;
            }
            windows.push(w);
        }
    }
    Ok(Snapshot {
        harness: Harness::Antigravity,
        plan: None,
        tier: None,
        windows,
        credits: None,
        fetched_at: now,
        origin: crate::model::Origin::Provider,
    })
}

/// A group's scope: its name as a slug, without a trailing `models`.
fn scope(name: &str) -> String {
    let slug = slug(name);
    slug.strip_suffix("-models").unwrap_or(&slug).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemoryKeyring;
    use crate::env::Env;
    use crate::testing::TempDir;
    use std::sync::Arc;

    const NOW: i64 = 1_795_444_270; // 2026-11-23T14:31:10Z
    const AGY_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/antigravity-agy.json"
    );

    /// A context whose home, cache, and `PATH` are all inside `dir`.
    fn context(dir: &TempDir, keyring: impl crate::creds::Keyring + 'static) -> Context {
        Context {
            env: Arc::new(Env {
                home: Some(dir.path().join("home")),
                xdg_cache_home: Some(dir.path().join("cache")),
                path: Some(dir.path().join("bin").into_os_string()),
                ..Env::default()
            }),
            keyring: Arc::new(keyring),
            client: crate::http::Client::new(Duration::from_secs(1)),
        }
    }

    /// A stored sign-in whose token expired half an hour before `NOW`.
    fn expired() -> MemoryKeyring {
        MemoryKeyring::default().with(
            &[("service", "gemini"), ("username", "antigravity")],
            br#"{"token":{"access_token":"ya29","expiry":"2026-11-23T14:00:00Z"}}"#,
        )
    }

    #[test]
    fn an_expired_sign_in_asks_agy_instead() {
        let dir = TempDir::new("agy-fallback");
        // The stub answers only when to11y runs it the way it should: no
        // updates, and no browser either way agy might open one.
        let cache = dir.path().canonicalize().unwrap().join("cache/to11y");
        let stub = cache.join("no-browser/xdg-open");
        dir.script(
            "bin/agy",
            &format!(
                r#"[ "$TO11Y_NO_AGY" = 1 ] && [ "$(pwd -P)" = "{cache}" ] && [ "$*" = "-p /usage --output-format json" ] && [ "$AGY_CLI_DISABLE_AUTO_UPDATE" = true ] && [ "$(command -v xdg-open)" = "{stub}" ] && [ "$BROWSER" = "{stub}" ] && ! xdg-open https://example.invalid && PATH=/usr/bin:/bin exec cat "{AGY_FIXTURE}"; exit 3"#,
                cache = cache.display(),
                stub = stub.display(),
            ),
        );
        let snapshot = fetch(&context(&dir, expired()), NOW).unwrap();
        assert_eq!(snapshot.windows.len(), 5);
    }

    #[test]
    fn a_locked_keyring_never_runs_agy() {
        let dir = TempDir::new("agy-locked");
        let marker = dir.path().join("agy-ran");
        dir.script(
            "bin/agy",
            &format!(
                r#"PATH=/usr/bin:/bin; touch "{}"; exec cat "{AGY_FIXTURE}""#,
                marker.display()
            ),
        );
        dir.write(
            "home/.gemini/antigravity-cli/antigravity-oauth-token",
            r#"{"token":{"access_token":"ya29-file","expiry":"2026-11-23T14:00:00Z"}}"#,
        );
        let locked = MemoryKeyring {
            locked: true,
            ..expired()
        };
        let failure = fetch(&context(&dir, locked), NOW).unwrap_err();
        assert_eq!(failure.why, Unavailable::KeyringLocked);
        assert!(!marker.exists(), "agy ran with the keyring locked");
    }

    #[test]
    fn an_expired_token_file_without_a_keyring_asks_agy() {
        let dir = TempDir::new("agy-file-expired");
        dir.script(
            "bin/agy",
            &format!(r#"PATH=/usr/bin:/bin exec cat "{AGY_FIXTURE}""#),
        );
        dir.write(
            "home/.gemini/antigravity-cli/antigravity-oauth-token",
            r#"{"token":{"access_token":"ya29-file","expiry":"2026-11-23T14:00:00Z"}}"#,
        );
        let none = crate::creds::SecretService { address: None };
        let snapshot = fetch(&context(&dir, none), NOW).unwrap();
        assert_eq!(snapshot.windows.len(), 5);
    }

    #[test]
    fn without_agy_an_expired_sign_in_stays_expired() {
        let dir = TempDir::new("agy-missing");
        let failure = fetch(&context(&dir, expired()), NOW).unwrap_err();
        assert_eq!(failure.why, Unavailable::SignInExpired);
        dir.script("bin/agy", &format!(r#"exec cat "{AGY_FIXTURE}""#));
        let mut cx = context(&dir, expired());
        Arc::make_mut(&mut cx.env).no_agy = true;
        assert_eq!(fetch(&cx, NOW).unwrap_err().why, Unavailable::SignInExpired);
    }

    #[test]
    fn an_agy_that_hangs_is_killed_at_the_timeout() {
        let dir = TempDir::new("agy-hang");
        let program = dir.script("bin/agy", "exec sleep 5");
        let started = Instant::now();
        let path = std::env::var_os("PATH");
        let timeout = Duration::from_millis(300);
        let error = run_agy(&program, dir.path(), path.as_deref(), timeout).unwrap_err();
        assert!(matches!(error, Unavailable::Failed(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn an_agy_that_fails_is_a_failure() {
        let dir = TempDir::new("agy-fails");
        let program = dir.script("bin/agy", "echo '{}'; exit 4");
        let path = std::env::var_os("PATH");
        let error = run_agy(&program, dir.path(), path.as_deref(), AGY_TIMEOUT).unwrap_err();
        assert_eq!(
            error,
            Unavailable::Failed("agy failed with exit status: 4.".to_owned())
        );
    }

    #[test]
    fn a_harness_that_takes_too_long_is_given_up_on() {
        let dir = TempDir::new("agy-deadline");
        dir.script("bin/agy", "exec sleep 5");
        let cx = context(&dir, expired());
        let started = Instant::now();
        let failure = crate::providers::fetch_within(
            Harness::Antigravity,
            &cx,
            NOW,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(matches!(failure.why, Unavailable::Failed(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "direct" => include_str!("../../tests/fixtures/antigravity.json"),
            _ => include_str!("../../tests/fixtures/antigravity-agy.json"),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn both_answers_give_the_same_windows() {
        let direct = parse(&fixture("direct"), NOW).unwrap();
        let agy = parse(&fixture("agy"), NOW).unwrap();
        assert_eq!(direct.windows.len(), 5);
        // agy sends fractions as 32-bit floats, so compare to a hundredth.
        let summary = |s: &Snapshot| {
            s.windows
                .iter()
                .map(|w| {
                    let used = (w.used_percent * 100.0).round() as i64;
                    (w.id.clone(), w.state, used, w.resets_at)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(summary(&direct), summary(&agy));
    }

    #[test]
    fn groups_become_scopes() {
        let snapshot = parse(&fixture("direct"), NOW).unwrap();
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini:weekly",
                "gemini:5h",
                "claude-and-gpt:weekly",
                "claude-and-gpt:5h",
                "gemini-4-argon:weekly"
            ]
        );
        assert_eq!(snapshot.windows[0].label, "Gemini Models, weekly");
        assert_eq!(snapshot.windows[0].used_percent, 100.0);
    }

    #[test]
    fn a_disabled_bucket_is_disabled() {
        let snapshot = parse(&fixture("direct"), NOW).unwrap();
        assert_eq!(snapshot.windows[1].state, WindowState::Disabled);
    }

    #[test]
    fn an_untouched_bucket_has_not_started_and_loses_its_fake_reset() {
        let snapshot = parse(&fixture("agy"), NOW).unwrap();
        let argon = &snapshot.windows[4];
        assert_eq!(argon.state, WindowState::NotStarted);
        assert_eq!(argon.resets_at, None);
        // The same bucket an hour later is a real reset, not a fake one.
        let later = parse(&fixture("agy"), NOW - 3600).unwrap();
        assert_eq!(later.windows[4].state, WindowState::Running);
    }

    #[test]
    fn a_spent_weekly_window_blocks_only_its_own_group() {
        let snapshot = parse(&fixture("direct"), NOW).unwrap();
        let (assessed, scopes) = crate::pace::assess(&snapshot.windows, NOW);
        let gemini = scopes.iter().find(|s| s.name == "gemini").unwrap();
        assert!(gemini.blocked);
        assert_eq!(gemini.blocked_until, time::parse("2026-11-28T15:34:08Z"));
        let other = scopes.iter().find(|s| s.name == "claude-and-gpt").unwrap();
        assert!(!other.blocked);
        let tightest = &assessed[other.tightest.unwrap()].window;
        assert_eq!(tightest.id, "claude-and-gpt:5h");
    }

    #[test]
    fn a_failed_agy_answer_is_a_failure() {
        let body = serde_json::json!({"status": "ERROR", "command": {"data": {"groups": []}}});
        assert!(matches!(parse(&body, NOW), Err(Unavailable::Failed(_))));
    }
}
