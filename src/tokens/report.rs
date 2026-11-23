//! The JSON each token tool returns. Results never name a file, a session
//! id, or an account: projects are shown as masked working directories.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{
    Event, Period, Scan, Tokens, antigravity, claude, codex, droid, list, scan, scan_files,
};
use crate::env::Env;
use crate::model::{Harness, Snapshot, WindowState};
use crate::providers::slug;
use crate::time;

/// How many projects `by_project` lists before folding the rest.
const TOP_PROJECTS: usize = 10;

/// A tool's answer, and whether it is an error.
pub type Answer = (Value, bool);

pub fn recent(harness: Harness, env: &Env, now: i64, period: Period) -> Answer {
    with_scan(harness, env, now, period, |out, scan| {
        out.insert("tokens".to_owned(), totals(harness, scan.events.iter()));
    })
}

pub fn by_model(harness: Harness, env: &Env, now: i64, period: Period) -> Answer {
    with_scan(harness, env, now, period, |out, scan| {
        let rows = grouped(harness, &scan.events, |e| e.model.clone(), "model");
        out.insert("models".to_owned(), rows.into());
    })
}

pub fn by_project(harness: Harness, env: &Env, now: i64, period: Period) -> Answer {
    with_scan(harness, env, now, period, |out, scan| {
        let mut rows = grouped(
            harness,
            &scan.events,
            |e| {
                e.project
                    .as_deref()
                    .map_or("unknown".to_owned(), |p| project_name(p, env))
            },
            "project",
        );
        if rows.len() > TOP_PROJECTS {
            let rest: Vec<Value> = rows.split_off(TOP_PROJECTS);
            let mut other = Tokens::default();
            let mut requests = 0;
            for row in &rest {
                other.add(&from_json(row));
                requests += row["requests"].as_u64().unwrap_or(0) as usize;
            }
            let mut row = tokens_json(harness, &other, requests);
            row.insert(
                "project".to_owned(),
                json!(format!("{} others", rest.len())),
            );
            rows.push(Value::Object(row));
        }
        out.insert("projects".to_owned(), rows.into());
        if harness == Harness::Antigravity {
            out.insert(
                "note".to_owned(),
                json!("Antigravity's transcripts don't record the working directory."),
            );
        }
    })
}

pub fn all_harnesses(env: &Env, now: i64, period: Period) -> Answer {
    let rows: Vec<Value> = Harness::ALL
        .iter()
        .map(|&harness| {
            let (answer, _) = recent(harness, env, now, period);
            let mut row = Map::new();
            row.insert("harness".to_owned(), json!(harness.name()));
            for key in ["tokens", "unavailable"] {
                if let Some(value) = answer.get(key) {
                    row.insert(key.to_owned(), value.clone());
                }
            }
            Value::Object(row)
        })
        .collect();
    (json!({ "period": period.name(), "harnesses": rows }), false)
}

/// The tokens of the session that's calling.
pub fn session(harness: Harness, env: &Env, now: i64) -> Answer {
    let mut out = header(harness);
    let (events, matched_by) = match harness {
        Harness::ClaudeCode => {
            let found = claude::current_session(env)
                .map(|id| (id, "the session Claude Code is in now"))
                .or_else(|| {
                    let id = env.claude_session_id.clone()?;
                    Some((id, "the session id Claude Code passed at start"))
                });
            let Some((id, matched_by)) = found else {
                return no_session(harness, "Claude Code didn't pass a session id to to11y.");
            };
            let files = claude::session_files(env, &id);
            (scan_files(harness, &files).events, matched_by)
        }
        Harness::Codex => match codex_session(env) {
            Some(events) => (
                events,
                "the Codex session in this directory that started to11y",
            ),
            None => {
                return no_session(harness, "No Codex session in this directory started to11y.");
            }
        },
        Harness::Droid => {
            let Some(total) = droid::latest_session(env) else {
                return no_session(harness, "No Droid session total was found.");
            };
            out.insert("matched_by".to_owned(), json!("the latest Droid session"));
            out.insert(
                "updated".to_owned(),
                json!(format!("{} ago", time::duration(now - total.updated))),
            );
            put(&mut out, "model", json!(total.model));
            let mut tokens = tokens_json(harness, &total.tokens, 0);
            tokens.remove("requests");
            out.insert("tokens".to_owned(), Value::Object(tokens));
            return (Value::Object(out), false);
        }
        Harness::Antigravity => {
            let Some(dir) = antigravity::latest_conversation(env) else {
                return no_session(harness, "No Antigravity conversation was found.");
            };
            let files = antigravity::conversation_files(&dir);
            (
                scan_files(harness, &files).events,
                "the latest Antigravity conversation",
            )
        }
    };
    out.insert("matched_by".to_owned(), json!(matched_by));
    out.insert("tokens".to_owned(), totals(harness, events.iter()));
    out.insert(
        "models".to_owned(),
        grouped(harness, &events, |e| e.model.clone(), "model").into(),
    );
    if let Some(first) = events.iter().map(|e| e.at).min() {
        out.insert(
            "started".to_owned(),
            json!(format!("{} ago", time::duration(now - first))),
        );
    }
    if harness == Harness::ClaudeCode {
        out.insert(
            "note".to_owned(),
            json!("A resumed session includes the history it copied."),
        );
    }
    (Value::Object(out), false)
}

/// Codex starts an MCP server in the session's directory, right after
/// the session starts, and passes no session id. The match is the latest
/// session in this directory that started before to11y did.
fn codex_session(env: &Env) -> Option<Vec<Event>> {
    let cwd = env.cwd.as_ref()?.to_str()?;
    let started = env.started();
    let listing = codex::files(env);
    // The caller's rollout is still being written, so it changed after
    // to11y started.
    let candidates: Vec<_> = listing
        .files
        .iter()
        .filter(|f| f.mtime >= started - 300)
        .collect();
    let meta = candidates
        .iter()
        .filter_map(|file| {
            let mut reader = std::io::BufReader::new(std::fs::File::open(&file.path).ok()?);
            codex::meta(&mut reader)
        })
        .filter(|m| {
            m.parent.is_none() && m.cwd.as_deref() == Some(cwd) && m.started <= started + 60
        })
        .max_by_key(|m| m.started)?;
    let files: Vec<_> = listing
        .files
        .into_iter()
        .filter(|f| f.mtime >= meta.started)
        .collect();
    let mut events = scan_files(Harness::Codex, &files).events;
    events.retain(|e| e.session == meta.id);
    Some(events)
}

/// Tokens spent in each running limit window: since `resets_at` minus the
/// window's length, by the models the window limits.
pub fn limit_windows(
    harness: Harness,
    env: &Env,
    now: i64,
    snapshot: &Snapshot,
) -> Map<String, Value> {
    let mut out = header(harness);
    let windows: Vec<_> = snapshot.windows.iter().map(|w| w.at(now)).collect();
    let start = |w: &crate::model::Window| -> Option<i64> {
        (w.state == WindowState::Running)
            .then(|| Some(w.resets_at? - i64::from(w.window_minutes?) * 60))
            .flatten()
    };
    let since = windows.iter().filter_map(start).min().unwrap_or(now);
    let found = scan(harness, env, since, now);
    let scopes: Vec<&str> = windows
        .iter()
        .map(|w| w.scope.as_str())
        .filter(|s| *s != "all")
        .collect();
    let rows: Vec<Value> = windows
        .iter()
        .map(|window| {
            let mut row = Map::new();
            row.insert("window".to_owned(), json!(window.id));
            row.insert("label".to_owned(), json!(window.label));
            row.insert(
                "used_percent".to_owned(),
                crate::tools::percent(window.used_percent),
            );
            let Some(begin) = start(window) else {
                row.insert("state".to_owned(), json!(window.state.name()));
                return Value::Object(row);
            };
            row.insert(
                "started".to_owned(),
                json!(format!("{} ago", time::duration(now - begin))),
            );
            let in_scope = |e: &&Event| {
                e.at >= begin
                    && (window.scope == "all"
                        || best_scope(&e.model, &scopes) == Some(window.scope.as_str()))
            };
            let events: Vec<&Event> = found.events.iter().filter(in_scope).collect();
            if window.scope != "all" && events.is_empty() {
                row.insert(
                    "note".to_owned(),
                    json!("No model in the logs matched this limit's scope."),
                );
            } else {
                row.insert("tokens".to_owned(), totals(harness, events.into_iter()));
            }
            Value::Object(row)
        })
        .collect();
    out.insert("windows".to_owned(), rows.into());
    out.insert(
        "note".to_owned(),
        json!("Tokens are from this machine's logs only. The provider's percent also counts other devices and the web."),
    );
    out
}

/// The most specific scope among `scopes` whose words appear in `model`.
fn best_scope<'a>(model: &str, scopes: &[&'a str]) -> Option<&'a str> {
    let model = slug(model);
    scopes
        .iter()
        .filter_map(|scope| {
            let longest = scope
                .split("-and-")
                .filter(|word| !word.is_empty() && model.contains(word))
                .map(str::len)
                .max()?;
            Some((longest, *scope))
        })
        .max_by_key(|(longest, _)| *longest)
        .map(|(_, scope)| scope)
}

/// Where each harness keeps its logs, and what's there, from file times
/// only.
pub fn sources(harness: Harness, env: &Env, now: i64) -> Answer {
    let listing = list(harness, env);
    let mut out = header(harness);
    let Some(root) = &listing.root else {
        return unavailable(harness, env);
    };
    out.insert("folder".to_owned(), json!(display_path(root, env)));
    out.insert("files".to_owned(), json!(listing.files.len()));
    let ago = |at: i64| json!(format!("{} ago", time::duration(now - at)));
    if let Some(newest) = listing.files.iter().map(|f| f.mtime).max() {
        out.insert("newest_file".to_owned(), ago(newest));
    }
    if let Some(oldest) = listing.files.iter().map(|f| f.mtime).min() {
        out.insert("oldest_file".to_owned(), ago(oldest));
    }
    if listing.unsupported > 0 {
        out.insert("unsupported_files".to_owned(), json!(listing.unsupported));
    }
    if let Some(note) = coverage(harness) {
        out.insert("note".to_owned(), json!(note));
    }
    (Value::Object(out), false)
}

fn coverage(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::ClaudeCode | Harness::Codex => None,
        Harness::Droid => Some("Droid's debug log is kept for about 45 days."),
        Harness::Antigravity => Some(
            "Only Antigravity CLI conversations since Nov 21, 2026 record tokens in a form to11y reads.",
        ),
    }
}

/// Runs a period scan and fills in the common fields, or says why there's
/// nothing to scan.
fn with_scan(
    harness: Harness,
    env: &Env,
    now: i64,
    period: Period,
    fill: impl FnOnce(&mut Map<String, Value>, &Scan),
) -> Answer {
    if !root(harness, env).is_some_and(|root| root.is_dir()) {
        return unavailable(harness, env);
    }
    let found = scan(harness, env, now - period.seconds(), now);
    let mut out = header(harness);
    out.insert("period".to_owned(), json!(period.name()));
    fill(&mut out, &found);
    out.insert(
        "scanned".to_owned(),
        json!(format!(
            "{} files, {:.0} MiB",
            found.files,
            found.bytes as f64 / (1 << 20) as f64
        )),
    );
    if found.skipped_lines > 0 || found.unreadable > 0 {
        out.insert(
            "unread".to_owned(),
            json!({ "lines": found.skipped_lines, "files": found.unreadable }),
        );
    }
    (Value::Object(out), false)
}

/// The folder a harness's logs live in, whether or not it exists.
fn root(harness: Harness, env: &Env) -> Option<std::path::PathBuf> {
    match harness {
        Harness::ClaudeCode => env.claude_dir().map(|d| d.join("projects")),
        Harness::Codex => env.codex_dir().map(|d| d.join("sessions")),
        Harness::Droid => env.factory_dir().map(|d| d.join("logs")),
        Harness::Antigravity => env.antigravity_cli_dir().map(|d| d.join("brain")),
    }
}

fn header(harness: Harness) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("harness".to_owned(), json!(harness.name()));
    out
}

fn unavailable(harness: Harness, env: &Env) -> Answer {
    let folder = root(harness, env).map(|root| display_path(&root, env));
    let mut out = header(harness);
    out.insert("unavailable".to_owned(), json!("no_logs"));
    out.insert(
        "message".to_owned(),
        json!(match folder {
            Some(folder) => format!("No {} logs in {folder}.", label(harness)),
            None => format!("No {} logs: the home directory isn't set.", label(harness)),
        }),
    );
    (Value::Object(out), true)
}

fn no_session(harness: Harness, message: &str) -> Answer {
    let mut out = header(harness);
    out.insert("unavailable".to_owned(), json!("no_session"));
    out.insert("message".to_owned(), json!(message));
    (Value::Object(out), true)
}

fn label(harness: Harness) -> &'static str {
    match harness {
        Harness::ClaudeCode => "Claude Code",
        Harness::Codex => "Codex",
        Harness::Droid => "Droid",
        Harness::Antigravity => "Antigravity",
    }
}

/// Totals of `events`, with the channels a harness doesn't record left
/// out.
fn totals<'a>(harness: Harness, events: impl Iterator<Item = &'a Event>) -> Value {
    let mut sum = Tokens::default();
    let mut requests = 0;
    for event in events {
        sum.add(&event.tokens);
        requests += 1;
    }
    Value::Object(tokens_json(harness, &sum, requests))
}

fn tokens_json(harness: Harness, t: &Tokens, requests: usize) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("total".to_owned(), json!(t.total()));
    out.insert("input".to_owned(), json!(t.input));
    out.insert("cache_read".to_owned(), json!(t.cache_read));
    if harness != Harness::Antigravity {
        out.insert("cache_write".to_owned(), json!(t.cache_write));
    }
    out.insert("output".to_owned(), json!(t.output));
    if let Some(reasoning) = t.reasoning {
        out.insert("reasoning".to_owned(), json!(reasoning));
    }
    out.insert("requests".to_owned(), json!(requests));
    let input = t.input + t.cache_read + t.cache_write;
    if input > 0 {
        let share = t.cache_read as f64 * 100.0 / input as f64;
        out.insert(
            "cache_read_percent".to_owned(),
            json!((share * 10.0).round() / 10.0),
        );
    }
    out
}

fn from_json(row: &Value) -> Tokens {
    let n = |key: &str| row[key].as_u64().unwrap_or(0);
    Tokens {
        input: n("input"),
        cache_read: n("cache_read"),
        cache_write: n("cache_write"),
        output: n("output"),
        reasoning: row["reasoning"].as_u64(),
    }
}

/// Totals per group, largest first.
fn grouped(
    harness: Harness,
    events: &[Event],
    group: impl Fn(&Event) -> String,
    name: &str,
) -> Vec<Value> {
    let mut groups: HashMap<String, (Tokens, usize)> = HashMap::new();
    for event in events {
        let entry = groups.entry(group(event)).or_default();
        entry.0.add(&event.tokens);
        entry.1 += 1;
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| {
        b.1.0
            .total()
            .cmp(&a.1.0.total())
            .then_with(|| a.0.cmp(&b.0))
    });
    groups
        .into_iter()
        .map(|(key, (tokens, requests))| {
            let mut row = Map::new();
            row.insert(name.to_owned(), json!(key));
            row.extend(tokens_json(harness, &tokens, requests));
            Value::Object(row)
        })
        .collect()
}

fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !value.is_null() {
        map.insert(key.to_owned(), value);
    }
}

/// A working directory as a project name: `~` for home, temporary folders
/// grouped, and segments that look like ids masked.
pub fn project_name(cwd: &str, env: &Env) -> String {
    let path = Path::new(cwd);
    let temporary = [Path::new("/tmp"), Path::new("/var/tmp")]
        .into_iter()
        .chain(env.tmpdir.as_deref())
        .any(|dir| path.starts_with(dir));
    if temporary {
        return "temporary directories".to_owned();
    }
    display_path(path, env)
}

fn display_path(path: &Path, env: &Env) -> String {
    let (prefix, rest) = match env
        .home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => ("~", rest),
        None => ("", path),
    };
    let mut out = prefix.to_owned();
    for part in rest.components() {
        let part = part.as_os_str().to_string_lossy();
        if part == "/" {
            continue;
        }
        out.push('/');
        out.push_str(if looks_like_id(&part) { "…" } else { &part });
    }
    if out.is_empty() { "/".to_owned() } else { out }
}

/// A run of 12 or more letters and digits with at least 4 digits, as in a
/// UUID's last group, a hash, or a ULID.
fn looks_like_id(part: &str) -> bool {
    part.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|run| run.len() >= 12 && run.chars().filter(char::is_ascii_digit).count() >= 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            home: Some("/home/u".into()),
            tmpdir: Some("/run/user/1000/tmp".into()),
            ..Env::default()
        }
    }

    #[test]
    fn projects_show_home_as_a_tilde() {
        assert_eq!(project_name("/home/u/git/to11y", &env()), "~/git/to11y");
        assert_eq!(project_name("/srv/app", &env()), "/srv/app");
        assert_eq!(project_name("/", &env()), "/");
    }

    #[test]
    fn temporary_folders_are_one_project() {
        for cwd in ["/tmp/x", "/var/tmp/y/z", "/run/user/1000/tmp/abc"] {
            assert_eq!(project_name(cwd, &env()), "temporary directories");
        }
    }

    #[test]
    fn segments_that_look_like_ids_are_masked() {
        assert_eq!(
            project_name(
                "/home/u/.local/share/lira/01KKH5FD97BHG41K9M5WXC8ZC9/worktrees/fix-login",
                &env()
            ),
            "~/.local/share/lira/…/worktrees/fix-login"
        );
        assert_eq!(
            project_name("/home/u/w/f5736d05-2ede-4fb7-a186-32d7ee6140ca", &env()),
            "~/w/…"
        );
        assert_eq!(
            project_name("/home/u/git/x86_64-unknown-linux", &env()),
            "~/git/x86_64-unknown-linux"
        );
    }

    #[test]
    fn a_model_goes_to_its_most_specific_scope() {
        let scopes = ["gemini", "gemini-4-argon", "claude-and-gpt", "fable"];
        assert_eq!(
            best_scope("gemini-4-argon-medium", &scopes),
            Some("gemini-4-argon")
        );
        assert_eq!(best_scope("gemini-3.8-flash-high", &scopes), Some("gemini"));
        assert_eq!(
            best_scope("claude-opus-5-5-low", &scopes),
            Some("claude-and-gpt")
        );
        assert_eq!(best_scope("gpt-6.1-sol", &scopes), Some("claude-and-gpt"));
        assert_eq!(best_scope("claude-fable-5-2", &["fable"]), Some("fable"));
        assert_eq!(best_scope("claude-opus-5-5", &["standard", "core"]), None);
    }

    #[test]
    fn antigravity_totals_leave_out_cache_writes_it_doesnt_record() {
        let t = Tokens {
            input: 10,
            cache_read: 30,
            ..Tokens::default()
        };
        let out = tokens_json(Harness::Antigravity, &t, 1);
        assert!(!out.contains_key("cache_write"));
        assert!(!out.contains_key("reasoning"));
        assert_eq!(out["cache_read_percent"], json!(75.0));
        assert!(tokens_json(Harness::Codex, &t, 1).contains_key("cache_write"));
    }
}
