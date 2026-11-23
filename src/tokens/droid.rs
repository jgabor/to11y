//! Droid's debug log, `~/.factory/logs/droid-log-single.log*`, rotated
//! daily and kept for about 45 days. Each response is a line like
//! `[2026-11-22T23:16:02.113Z] INFO: [Agent] Streaming result | Context:
//! {json}`.
//!
//! The JSON's `tags` also carry installation, host, and instance ids. Only
//! `modelId` and `sessionId` are read from them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Event, Listing, Parsed, Tokens, count, has, key, lines, maybe_count, walk};
use crate::env::Env;
use crate::time;

const MARKER: &[u8] = b"] INFO: [Agent] Streaming result | Context: ";

pub fn files(env: &Env) -> Listing {
    let Some(root) = env.factory_dir().map(|dir| dir.join("logs")) else {
        return Listing::default();
    };
    let mut files = Vec::new();
    walk(&root, 0, &is_log, &mut files);
    Listing {
        root: root.is_dir().then_some(root),
        files,
        unsupported: 0,
    }
}

fn is_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("droid-log-single.log"))
}

pub fn parse(read: &mut dyn BufRead, _path: &Path) -> Parsed {
    let mut parsed = Parsed::default();
    lines(read, |line| {
        if !has(line, b"Streaming result") {
            return;
        }
        match read_line(line) {
            Some(event) => parsed.events.push(event),
            None => parsed.skipped += 1,
        }
    });
    parsed
}

fn read_line(line: &[u8]) -> Option<Event> {
    let start = memchr::memmem::find(line, MARKER)?;
    let stamp = std::str::from_utf8(line.get(1..start)?).ok()?;
    if line.first() != Some(&b'[') {
        return None;
    }
    let at = time::parse(stamp)?;
    let value: Value = serde_json::from_slice(&line[start + MARKER.len()..]).ok()?;
    let tags = &value["tags"];
    Some(Event {
        at,
        model: tags["modelId"].as_str().unwrap_or("unknown").to_owned(),
        project: None,
        session: tags["sessionId"].as_str().unwrap_or("").to_owned(),
        key: value["upstreamResponseId"].as_str().map(|id| key(&[id])),
        tokens: Tokens {
            input: count(&value["inputTokens"]),
            cache_read: count(&value["cacheReadInputTokens"]),
            cache_write: count(&value["cachedTokensWritten"]),
            output: count(&value["outputTokens"]),
            reasoning: maybe_count(&value["reasoningTokens"]),
        },
    })
}

/// Fills in each event's working directory from its session's transcript,
/// `~/.factory/sessions/<dir>/<session>.jsonl`, whose first line is a
/// `session_start` with the `cwd`.
pub fn add_projects(env: &Env, events: &mut [Event]) {
    let wanted: HashSet<&str> = events.iter().map(|e| e.session.as_str()).collect();
    let mut cwds: HashMap<String, String> = HashMap::new();
    for (session, path) in transcripts(env) {
        if !wanted.contains(session.as_str()) || cwds.contains_key(&session) {
            continue;
        }
        if let Some(cwd) = first_line(&path).and_then(|v| v["cwd"].as_str().map(str::to_owned)) {
            cwds.insert(session, cwd);
        }
    }
    for event in events {
        event.project = cwds.get(&event.session).cloned();
    }
}

/// Every session transcript, by session id.
fn transcripts(env: &Env) -> Vec<(String, PathBuf)> {
    let Some(root) = env.factory_dir().map(|dir| dir.join("sessions")) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    walk(&root, 1, &|path| super::named(path, ".jsonl"), &mut files);
    files
        .into_iter()
        .filter_map(|file| {
            let name = file.path.file_name()?.to_str()?;
            Some((name.strip_suffix(".jsonl")?.to_owned(), file.path))
        })
        .collect()
}

fn first_line(path: &Path) -> Option<Value> {
    let mut line = Vec::new();
    BufReader::new(fs::File::open(path).ok()?.take(1 << 20))
        .read_until(b'\n', &mut line)
        .ok()?;
    serde_json::from_slice(&line).ok()
}

/// A Droid session's own running total, from `<session>.settings.json`.
#[derive(Debug, PartialEq)]
pub struct SessionTotal {
    pub model: Option<String>,
    pub tokens: Tokens,
    /// When the file was last written, in Unix seconds.
    pub updated: i64,
}

/// The most recently updated Droid session's own total. Droid gives its MCP
/// servers no session id, so this is the best match.
pub fn latest_session(env: &Env) -> Option<SessionTotal> {
    let root = env.factory_dir()?.join("sessions");
    let mut files = Vec::new();
    walk(
        &root,
        1,
        &|path| super::named(path, ".settings.json"),
        &mut files,
    );
    let latest = files.into_iter().max_by_key(|file| file.mtime)?;
    let value: Value = serde_json::from_slice(&fs::read(&latest.path).ok()?).ok()?;
    let usage = if value["inclusiveTokenUsage"].is_object() {
        &value["inclusiveTokenUsage"]
    } else {
        &value["tokenUsage"]
    };
    if !usage.is_object() {
        return None;
    }
    Some(SessionTotal {
        model: value["model"].as_str().map(str::to_owned),
        tokens: Tokens {
            input: count(&usage["inputTokens"]),
            cache_read: count(&usage["cacheReadTokens"]),
            cache_write: count(&usage["cacheCreationTokens"]),
            output: count(&usage["outputTokens"]),
            reasoning: maybe_count(&usage["thinkingTokens"]),
        },
        updated: latest.mtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use crate::tokens::tests::{NOW, parse_text, touch};

    pub fn line(at: &str, id: &str, session: &str) -> String {
        format!(
            r#"[{at}] INFO: [Agent] Streaming result | Context: {{"count":1,"inputTokens":120,"cacheReadInputTokens":3000,"cachedTokensWritten":40,"totalInputTokens":3160,"outputTokens":500,"reasoningTokens":200,"reason":"end_turn","upstreamResponseId":{id},"hasReasoningContent":true,"tags":{{"droidInstallationId":"install-secret","hostId":"host-secret","sessionId":"{session}","modelId":"claude-opus-5-5","factoryTier":"pro"}},"factory.process.instance_id":"instance-secret","process.pid":42}}"#
        )
    }

    #[test]
    fn a_streaming_result_line_gives_its_tokens() {
        let parsed = parse_text(
            parse,
            &line("2026-11-23T14:31:10.500Z", r#""resp_1""#, "s1"),
            "log",
        );
        assert_eq!(parsed.events.len(), 1);
        let event = &parsed.events[0];
        assert_eq!(event.at, NOW);
        assert_eq!(event.model, "claude-opus-5-5");
        assert_eq!(event.session, "s1");
        assert!(event.key.is_some());
        assert_eq!(
            event.tokens,
            Tokens {
                input: 120,
                cache_read: 3000,
                cache_write: 40,
                output: 500,
                reasoning: Some(200),
            }
        );
    }

    #[test]
    fn a_line_without_a_response_id_counts_without_dedupe() {
        let parsed = parse_text(parse, &line("2026-11-23T14:31:10Z", "null", "s1"), "log");
        assert_eq!(parsed.events[0].key, None);
    }

    #[test]
    fn other_log_lines_are_ignored() {
        let text = "[2026-11-23T14:31:10Z] INFO: [Agent] Streaming started | Context: {}\nnot a log line\n";
        let parsed = parse_text(parse, text, "log");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn a_broken_streaming_result_is_skipped_and_counted() {
        let text =
            "[2026-11-23T14:31:10Z] INFO: [Agent] Streaming result | Context: {\"inputTokens\":";
        assert_eq!(parse_text(parse, text, "log").skipped, 1);
    }

    #[test]
    fn projects_come_from_the_session_transcript() {
        let dir = TempDir::new("droid-projects");
        dir.write(
            "home/.factory/sessions/-home-u-git-app/s1.jsonl",
            "{\"type\":\"session_start\",\"id\":\"s1\",\"cwd\":\"/home/u/git/app\"}\n{\"type\":\"message\"}\n",
        );
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        let mut events =
            parse_text(parse, &line("2026-11-23T14:31:10Z", "null", "s1"), "log").events;
        events.extend(parse_text(parse, &line("2026-11-23T14:31:10Z", "null", "s2"), "log").events);
        add_projects(&env, &mut events);
        assert_eq!(events[0].project.as_deref(), Some("/home/u/git/app"));
        assert_eq!(events[1].project, None);
    }

    #[test]
    fn the_latest_session_total_includes_its_subagents() {
        let dir = TempDir::new("droid-latest");
        let old = dir.write(
            "home/.factory/sessions/a/s1.settings.json",
            r#"{"model":"old","tokenUsage":{"inputTokens":1}}"#,
        );
        let new = dir.write(
            "home/.factory/sessions/b/s2.settings.json",
            r#"{"model":"claude-opus-5-5","tokenUsage":{"inputTokens":10,"outputTokens":5},"inclusiveTokenUsage":{"inputTokens":20,"outputTokens":7,"cacheReadTokens":300,"cacheCreationTokens":4,"thinkingTokens":2}}"#,
        );
        touch(&old, NOW - 600);
        touch(&new, NOW - 60);
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        let total = latest_session(&env).unwrap();
        assert_eq!(total.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(total.tokens.input, 20);
        assert_eq!(total.tokens.cache_read, 300);
        assert_eq!(total.updated, NOW - 60);
    }
}
