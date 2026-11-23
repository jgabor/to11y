//! Codex rollouts: `sessions/YYYY/MM/DD/rollout-*.jsonl`, and
//! `archived_sessions/`.
//!
//! A rollout counts tokens cumulatively in `token_count` events. Newer
//! versions also write one `token_usage_record` per response, and from the
//! first one in a file those are the source. A forked or spawned rollout
//! starts with a copy of its parent's history, stamped in a burst, which
//! isn't counted again.

use std::io::BufRead;
use std::path::Path;

use serde_json::Value;

use super::{Event, Listing, Parsed, Tokens, count, has, key, lines, maybe_count, walk};
use crate::env::Env;
use crate::time;

/// Usage lines closer together than this, right after a fork, are the
/// parent's history being copied.
const BURST_MS: i64 = 1000;

pub fn files(env: &Env) -> Listing {
    let Some(home) = env.codex_dir() else {
        return Listing::default();
    };
    let root = home.join("sessions");
    let mut files = Vec::new();
    walk(&root, 4, &|path| super::named(path, ".jsonl"), &mut files);
    walk(
        &home.join("archived_sessions"),
        4,
        &|path| super::named(path, ".jsonl"),
        &mut files,
    );
    let mut compressed = Vec::new();
    walk(
        &root,
        4,
        &|path| super::named(path, ".jsonl.zst"),
        &mut compressed,
    );
    Listing {
        roots: root.is_dir().then_some(root).into_iter().collect(),
        files,
        unsupported: compressed.len(),
    }
}

/// What the first `session_meta` of a rollout says.
#[derive(Debug, Default, PartialEq)]
pub struct Meta {
    pub id: String,
    pub cwd: Option<String>,
    /// The thread that spawned this one, for a subagent.
    pub parent: Option<String>,
    /// Unix seconds.
    pub started: i64,
    forked: bool,
}

/// The first `session_meta` of a rollout, from its first few lines.
pub fn meta(read: &mut dyn BufRead) -> Option<Meta> {
    let mut found = None;
    let mut seen = 0;
    lines(read, |line| {
        seen += 1;
        if found.is_some() || seen > 5 || !has(line, b"session_meta") {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(line) {
            found = read_meta(&value);
        }
    });
    found
}

fn read_meta(line: &Value) -> Option<Meta> {
    if line["type"] != "session_meta" {
        return None;
    }
    let payload = &line["payload"];
    let parent = payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"]
        .as_str()
        .map(str::to_owned);
    Some(Meta {
        id: payload["id"].as_str()?.to_owned(),
        cwd: payload["cwd"].as_str().map(str::to_owned),
        forked: parent.is_some() || payload["forked_from_id"].is_string(),
        parent,
        started: payload["timestamp"]
            .as_str()
            .or_else(|| line["timestamp"].as_str())
            .and_then(time::parse)
            .unwrap_or(0),
    })
}

/// One rollout's events. A spawned subagent's events belong to the thread
/// that spawned it, so a session's total includes its subagents.
pub fn parse(read: &mut dyn BufRead, _path: &Path) -> Parsed {
    let mut state = State::default();
    let mut parsed = Parsed::default();
    lines(read, |line| {
        if !(has(line, b"token_count")
            || has(line, b"token_usage_record")
            || has(line, b"turn_context")
            || has(line, b"session_meta"))
        {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(value) => state.read(&value, &mut parsed.events),
            Err(_) => parsed.skipped += 1,
        }
    });
    parsed
}

#[derive(Default)]
struct State {
    meta: Option<Meta>,
    model: Option<String>,
    total: Option<Tokens>,
    /// Set by the first `token_usage_record`.
    records: bool,
    /// True from a fork's `session_meta` until the copied burst ends.
    copying: bool,
    /// The time of the last usage line, in milliseconds.
    last_ms: i64,
}

impl State {
    fn read(&mut self, line: &Value, events: &mut Vec<Event>) {
        let at_ms = line["timestamp"].as_str().and_then(time::parse_millis);
        match line["type"].as_str() {
            Some("session_meta") => {
                if self.meta.is_none() {
                    self.meta = read_meta(line);
                    if let Some(meta) = &self.meta {
                        self.copying = meta.forked;
                    }
                    self.last_ms = at_ms.unwrap_or(0);
                }
            }
            Some("turn_context") => {
                if let Some(model) = line["payload"]["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
            }
            Some("token_usage_record") => {
                self.records = true;
                let payload = &line["payload"];
                if self.copied(at_ms) {
                    return;
                }
                let tokens = tokens(&payload["usage"]);
                let key = payload["response_id"].as_str().map(|id| key(&[id]));
                self.push(at_ms, key, tokens, events);
            }
            Some("event_msg") if line["payload"]["type"] == "token_count" => {
                let info = &line["payload"]["info"];
                if info.is_null() {
                    return;
                }
                let total = tokens(&info["total_token_usage"]);
                let delta = match self.total {
                    Some(previous) if previous == total => None,
                    previous => Some(if info["last_token_usage"].is_object() {
                        tokens(&info["last_token_usage"])
                    } else {
                        minus(&total, &previous.unwrap_or_default())
                    }),
                };
                // Always kept, so a counter that resets starts a new base.
                self.total = Some(total);
                if self.records || self.copied(at_ms) {
                    return;
                }
                if let Some(delta) = delta.filter(|d| d.total() > 0) {
                    self.push(at_ms, None, delta, events);
                }
            }
            _ => {}
        }
    }

    /// Whether a usage line at `at_ms` is part of a fork's copied burst.
    fn copied(&mut self, at_ms: Option<i64>) -> bool {
        let at_ms = at_ms.unwrap_or(self.last_ms);
        let copied = self.copying && at_ms - self.last_ms < BURST_MS;
        if !copied {
            self.copying = false;
        }
        self.last_ms = at_ms;
        copied
    }

    fn push(&self, at_ms: Option<i64>, key: Option<u64>, tokens: Tokens, events: &mut Vec<Event>) {
        let Some(at_ms) = at_ms else { return };
        let meta = self.meta.as_ref();
        events.push(Event {
            at: at_ms.div_euclid(1000),
            model: self.model.clone().unwrap_or_else(|| "unknown".to_owned()),
            project: meta.and_then(|m| m.cwd.clone()),
            session: meta
                .map(|m| m.parent.clone().unwrap_or_else(|| m.id.clone()))
                .unwrap_or_default(),
            key,
            tokens,
        });
    }
}

/// Codex counts cached input and cache writes inside `input_tokens`.
fn tokens(usage: &Value) -> Tokens {
    let input = count(&usage["input_tokens"]);
    let cache_read = count(&usage["cached_input_tokens"]).min(input);
    let cache_write = count(&usage["cache_write_input_tokens"]).min(input - cache_read);
    Tokens {
        input: input - cache_read - cache_write,
        cache_read,
        cache_write,
        output: count(&usage["output_tokens"]),
        reasoning: maybe_count(&usage["reasoning_output_tokens"]),
    }
}

fn minus(a: &Tokens, b: &Tokens) -> Tokens {
    Tokens {
        input: a.input.saturating_sub(b.input),
        cache_read: a.cache_read.saturating_sub(b.cache_read),
        cache_write: a.cache_write.saturating_sub(b.cache_write),
        output: a.output.saturating_sub(b.output),
        reasoning: match (a.reasoning, b.reasoning) {
            (Some(a), b) => Some(a.saturating_sub(b.unwrap_or(0))),
            (None, _) => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::tests::parse_text;

    fn meta_line(at: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"{at}","type":"session_meta","payload":{{"id":"t1","session_id":"t1","timestamp":"{at}","cwd":"/home/u/git/app","creator_account_id":"acct-secret","creator_user_id":"user-secret"{extra}}}}}"#
        )
    }

    fn model(at: &str, model: &str) -> String {
        format!(
            r#"{{"timestamp":"{at}","type":"turn_context","payload":{{"model":"{model}","cwd":"/x"}}}}"#
        )
    }

    fn usage(input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":1,"total_tokens":{}}}"#,
            input + output
        )
    }

    fn count_line(at: &str, total: (u64, u64, u64), last: (u64, u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{},"model_context_window":272000}},"rate_limits":null}}}}"#,
            usage(total.0, total.1, total.2),
            usage(last.0, last.1, last.2)
        )
    }

    fn record(at: &str, id: &str, u: (u64, u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"{at}","type":"token_usage_record","payload":{{"response_id":"{id}","usage":{}}}}}"#,
            usage(u.0, u.1, u.2)
        )
    }

    fn run(lines: &[String]) -> Parsed {
        parse_text(parse, &lines.join("\n"), "rollout.jsonl")
    }

    #[test]
    fn each_turn_counts_its_own_tokens_with_cached_input_split_out() {
        let parsed = run(&[
            meta_line("2026-11-23T10:00:00Z", ""),
            model("2026-11-23T10:00:01Z", "gpt-6.1-sol"),
            count_line("2026-11-23T10:00:05Z", (100, 40, 10), (100, 40, 10)),
            count_line("2026-11-23T10:01:00Z", (250, 140, 30), (150, 100, 20)),
        ]);
        assert_eq!(parsed.events.len(), 2);
        let second = &parsed.events[1];
        assert_eq!(second.model, "gpt-6.1-sol");
        assert_eq!(second.session, "t1");
        assert_eq!(second.project.as_deref(), Some("/home/u/git/app"));
        assert_eq!(
            second.tokens,
            Tokens {
                input: 50,
                cache_read: 100,
                cache_write: 0,
                output: 20,
                reasoning: Some(1),
            }
        );
    }

    #[test]
    fn a_repeated_total_counts_nothing() {
        let parsed = run(&[
            meta_line("2026-11-23T10:00:00Z", ""),
            count_line("2026-11-23T10:00:05Z", (100, 0, 10), (100, 0, 10)),
            count_line("2026-11-23T10:00:06Z", (100, 0, 10), (100, 0, 10)),
        ]);
        assert_eq!(parsed.events.len(), 1);
    }

    #[test]
    fn a_counter_that_resets_starts_a_new_base() {
        let parsed = run(&[
            meta_line("2026-11-23T10:00:00Z", ""),
            count_line("2026-11-23T10:00:05Z", (1000, 0, 100), (1000, 0, 100)),
            count_line("2026-11-23T11:00:00Z", (50, 0, 5), (50, 0, 5)),
            count_line("2026-11-23T11:00:30Z", (80, 0, 8), (30, 0, 3)),
        ]);
        let inputs: Vec<u64> = parsed.events.iter().map(|e| e.tokens.input).collect();
        assert_eq!(inputs, [1000, 50, 30]);
    }

    #[test]
    fn usage_records_take_over_from_their_first_appearance() {
        let parsed = run(&[
            meta_line("2026-11-23T10:00:00Z", ""),
            count_line("2026-11-23T10:00:05Z", (100, 0, 10), (100, 0, 10)),
            record("2026-11-23T11:00:00Z", "resp_1", (60, 0, 6)),
            count_line("2026-11-23T11:00:00.200Z", (160, 0, 16), (60, 0, 6)),
            record("2026-11-23T11:00:30Z", "resp_2", (70, 0, 7)),
        ]);
        let inputs: Vec<u64> = parsed.events.iter().map(|e| e.tokens.input).collect();
        assert_eq!(inputs, [100, 60, 70]);
        assert!(parsed.events[1].key.is_some());
        assert_eq!(parsed.events[0].key, None);
    }

    #[test]
    fn a_forks_copied_burst_is_not_counted_again() {
        let fork = r#","forked_from_id":"t0""#;
        let parsed = run(&[
            meta_line("2026-09-12T06:44:04.448Z", fork),
            meta_line("2026-09-12T06:44:04.448Z", "").replace("t1", "t0"),
            count_line("2026-09-12T06:44:04.448Z", (2000, 0, 200), (500, 0, 50)),
            count_line("2026-09-12T06:44:04.449Z", (2600, 0, 260), (600, 0, 60)),
            count_line("2026-09-12T06:45:00Z", (2700, 0, 270), (100, 0, 10)),
        ]);
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].tokens.input, 100);
        assert_eq!(parsed.events[0].project.as_deref(), Some("/home/u/git/app"));
    }

    #[test]
    fn a_fork_that_starts_with_a_carried_total_counts_only_the_new_turn() {
        let fork = r#","forked_from_id":"t0""#;
        let parsed = run(&[
            meta_line("2026-09-13T08:58:37Z", fork),
            count_line(
                "2026-09-13T09:06:25Z",
                (9_000_000, 0, 1000),
                (198_000, 0, 600),
            ),
        ]);
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].tokens.input, 198_000);
    }

    #[test]
    fn a_spawned_subagent_counts_toward_its_parent_thread() {
        let spawn = r#","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#;
        let parsed = run(&[
            meta_line("2026-11-23T10:00:00Z", spawn),
            count_line("2026-11-23T10:00:05Z", (100, 0, 10), (100, 0, 10)),
        ]);
        assert_eq!(parsed.events[0].session, "root");
    }

    #[test]
    fn the_first_session_meta_says_who_and_where() {
        let text = [
            meta_line("2026-11-23T10:00:00Z", ""),
            model("2026-11-23T10:00:01Z", "m"),
        ]
        .join("\n");
        let meta = meta(&mut text.as_bytes()).unwrap();
        assert_eq!(meta.id, "t1");
        assert_eq!(meta.cwd.as_deref(), Some("/home/u/git/app"));
        assert_eq!(meta.started, crate::tokens::tests::NOW - 16_270);
    }

    #[test]
    fn a_count_without_info_is_ignored() {
        let line = r#"{"timestamp":"2026-11-23T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        let parsed = run(&[meta_line("2026-11-23T10:00:00Z", ""), line.to_owned()]);
        assert!(parsed.events.is_empty());
    }
}
