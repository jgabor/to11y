//! Claude Code transcripts: `projects/<encoded cwd>/<session>.jsonl`, with
//! subagents under `<session>/subagents/`.
//!
//! Usage is on `type:"assistant"` lines, in `message.usage`. Claude Code
//! writes one line per content block, so the same `message.id` and
//! `requestId` repeat with the same usage. Advisor calls are left out of the
//! top-level usage and listed in `usage.iterations`.

use std::io::BufRead;
use std::path::Path;

use serde_json::Value;

use super::{Event, FileInfo, Listing, Parsed, Tokens, count, has, key, lines, maybe_count, walk};
use crate::env::Env;
use crate::time;

pub fn files(env: &Env) -> Listing {
    let Some(root) = env.claude_dir().map(|dir| dir.join("projects")) else {
        return Listing::default();
    };
    let mut files = Vec::new();
    walk(&root, 6, &is_transcript, &mut files);
    Listing {
        roots: root.is_dir().then_some(root).into_iter().collect(),
        files,
        unsupported: 0,
    }
}

/// The files of one session: its transcript and its subagents'.
pub fn session_files(env: &Env, session: &str) -> Vec<FileInfo> {
    let Some(root) = env.claude_dir().map(|dir| dir.join("projects")) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let Ok(projects) = std::fs::read_dir(&root) else {
        return files;
    };
    for project in projects.flatten() {
        let dir = project.path();
        if let Some(info) = super::info(dir.join(format!("{session}.jsonl"))) {
            files.push(info);
        }
        walk(&dir.join(session), 5, &is_transcript, &mut files);
    }
    files
}

/// The session the Claude Code that started to11y is in now. Claude Code
/// keeps it in `sessions/<pid>.json` and changes it on `/clear`, while the
/// `CLAUDE_CODE_SESSION_ID` it gave to11y at start stays the same. A file
/// counts only when its `procStart` matches the process, since files of
/// processes that have ended stay behind.
pub fn current_session(env: &Env) -> Option<String> {
    let sessions = env.claude_dir()?.join("sessions");
    let processes = env.proc_dir.as_ref()?;
    let mut pid = env.pid?;
    // Claude Code may start its MCP servers through a shell or a wrapper.
    for _ in 0..16 {
        let (parent, _) = process(processes, pid)?;
        if parent <= 1 {
            return None;
        }
        pid = parent;
        let Ok(text) = std::fs::read(sessions.join(format!("{pid}.json"))) else {
            continue;
        };
        let file: Value = serde_json::from_slice(&text).ok()?;
        let (_, started) = process(processes, pid)?;
        let ours = file["pid"].as_u64() == Some(u64::from(pid))
            && file["procStart"].as_str() == Some(started.as_str());
        return ours
            .then(|| file["sessionId"].as_str().map(str::to_owned))
            .flatten();
    }
    None
}

/// A process's parent and start time, from `<proc>/<pid>/stat`. The start
/// time stays text, as Claude Code writes it.
fn process(processes: &Path, pid: u32) -> Option<(u32, String)> {
    let stat = std::fs::read_to_string(processes.join(pid.to_string()).join("stat")).ok()?;
    // The command name is in parentheses and may hold anything, so the
    // fields are counted from the last `)`. The parent is field 4, the start
    // time field 22.
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let started = fields.nth(17)?.to_owned();
    Some((parent, started))
}

fn is_transcript(path: &Path) -> bool {
    super::named(path, ".jsonl")
}

pub fn parse(read: &mut dyn BufRead, _path: &Path) -> Parsed {
    let mut parsed = Parsed::default();
    lines(read, |line| {
        if !has(line, br#""type":"assistant""#) {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(value) => read_line(&value, &mut parsed.events),
            Err(_) => parsed.skipped += 1,
        }
    });
    parsed
}

fn read_line(line: &Value, events: &mut Vec<Event>) {
    // `user` lines carry a subagent's totals in `toolUseResult.usage`.
    // The subagent's own file counts those.
    if line["type"] != "assistant" {
        return;
    }
    let message = &line["message"];
    let usage = &message["usage"];
    let Some(model) = message["model"].as_str() else {
        return;
    };
    if model == "<synthetic>" || !usage.is_object() {
        return;
    }
    let Some(at) = line["timestamp"].as_str().and_then(time::parse) else {
        return;
    };
    let id = message["id"].as_str();
    let request = line["requestId"].as_str().unwrap_or("");
    let event = |model: &str, key: Option<u64>, tokens: Tokens| Event {
        at,
        model: model.to_owned(),
        project: line["cwd"].as_str().map(str::to_owned),
        session: line["sessionId"].as_str().unwrap_or("").to_owned(),
        key,
        tokens,
    };
    events.push(event(
        model,
        id.map(|id| key(&[id, request])),
        Tokens {
            reasoning: maybe_count(&usage["output_tokens_details"]["thinking_tokens"]),
            ..tokens(usage)
        },
    ));
    let iterations = usage["iterations"].as_array().into_iter().flatten();
    for (n, iteration) in iterations.enumerate() {
        if iteration["type"] != "advisor_message" {
            continue;
        }
        let advisor = iteration["model"]
            .as_str()
            .or_else(|| line["advisorModel"].as_str())
            .unwrap_or(model);
        let n = n.to_string();
        events.push(event(
            advisor,
            id.map(|id| key(&[id, request, "advisor", &n])),
            tokens(iteration),
        ));
    }
}

fn tokens(usage: &Value) -> Tokens {
    Tokens {
        input: count(&usage["input_tokens"]),
        cache_read: count(&usage["cache_read_input_tokens"]),
        cache_write: count(&usage["cache_creation_input_tokens"]),
        output: count(&usage["output_tokens"]),
        reasoning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::tests::{NOW, parse_text};

    fn assistant(id: &str, usage: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"message":{{"model":"claude-opus-5-5","id":"{id}","type":"message","role":"assistant","content":[{{"type":"text","text":"hello"}}],"usage":{usage}}},"requestId":"req_1","type":"assistant","uuid":"u","timestamp":"2026-11-23T14:31:10.123Z","cwd":"/home/u/git/app","sessionId":"s1","version":"2.1.330","advisorModel":"claude-opus-5-5"}}"#
        )
    }

    #[test]
    fn an_assistant_line_gives_its_four_channels_and_thinking() {
        let line = assistant(
            "msg_1",
            r#"{"input_tokens":4,"cache_creation_input_tokens":2199,"cache_read_input_tokens":153932,"output_tokens":1114,"output_tokens_details":{"thinking_tokens":100}}"#,
        );
        let parsed = parse_text(parse, &line, "x.jsonl");
        assert_eq!(parsed.events.len(), 1);
        let event = &parsed.events[0];
        assert_eq!(event.at, NOW);
        assert_eq!(event.model, "claude-opus-5-5");
        assert_eq!(event.project.as_deref(), Some("/home/u/git/app"));
        assert_eq!(event.session, "s1");
        assert_eq!(
            event.tokens,
            Tokens {
                input: 4,
                cache_read: 153_932,
                cache_write: 2199,
                output: 1114,
                reasoning: Some(100),
            }
        );
    }

    #[test]
    fn content_blocks_of_one_response_share_a_key() {
        let line = assistant("msg_1", r#"{"input_tokens":1,"output_tokens":1}"#);
        let parsed = parse_text(parse, &format!("{line}\n{line}\n"), "x.jsonl");
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].key, parsed.events[1].key);
        assert!(parsed.events[0].key.is_some());
    }

    #[test]
    fn advisor_iterations_count_on_their_own() {
        let line = assistant(
            "msg_1",
            r#"{"input_tokens":4,"output_tokens":1114,"iterations":[{"type":"message","input_tokens":2,"output_tokens":73},{"type":"advisor_message","model":"claude-fable-5-2","input_tokens":73563,"output_tokens":5518,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},{"type":"message","input_tokens":2,"output_tokens":1041}]}"#,
        );
        let parsed = parse_text(parse, &line, "x.jsonl");
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].tokens.input, 4);
        assert_eq!(parsed.events[1].model, "claude-fable-5-2");
        assert_eq!(parsed.events[1].tokens.input, 73_563);
        assert_ne!(parsed.events[0].key, parsed.events[1].key);
    }

    #[test]
    fn subagent_totals_on_user_lines_are_not_counted() {
        let line = r#"{"type":"user","message":{"role":"user","content":[]},"toolUseResult":{"usage":{"input_tokens":500,"output_tokens":900},"type":"assistant"},"timestamp":"2026-11-23T14:31:10Z","sessionId":"s1"}"#;
        let parsed = parse_text(parse, line, "x.jsonl");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn synthetic_messages_are_skipped() {
        let line = assistant("msg_1", r#"{"input_tokens":0,"output_tokens":0}"#)
            .replace("claude-opus-5-5\",\"id", "<synthetic>\",\"id");
        assert!(parse_text(parse, &line, "x.jsonl").events.is_empty());
    }

    #[test]
    fn a_truncated_line_is_skipped_and_counted() {
        let line = assistant("msg_1", r#"{"input_tokens":1,"output_tokens":1}"#);
        let parsed = parse_text(parse, &line[..line.len() - 20], "x.jsonl");
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.skipped, 1);
    }

    /// A home with a `sessions` file, and a process list where to11y
    /// (pid 40) was started by a shell (30) that Claude Code (20) started.
    fn processes(dir: &crate::testing::TempDir, session: &str, proc_start: &str) -> Env {
        dir.write(
            "proc/40/stat",
            "40 (to11y) S 30 40 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 900 0 0",
        );
        dir.write(
            "proc/30/stat",
            "30 (sh) S 20 30 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 800 0 0",
        );
        dir.write(
            "proc/20/stat",
            "20 (claude (2.1) x) S 1 20 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 700 0 0",
        );
        dir.write(
            "home/.claude/sessions/20.json",
            &format!(r#"{{"pid":20,"sessionId":"{session}","procStart":"{proc_start}"}}"#),
        );
        Env {
            home: Some(dir.path().join("home")),
            pid: Some(40),
            proc_dir: Some(dir.path().join("proc")),
            ..Env::default()
        }
    }

    #[test]
    fn the_current_session_comes_from_the_claude_code_above_to11y() {
        let dir = crate::testing::TempDir::new("claude-current-session");
        let env = processes(&dir, "after-clear", "700");
        assert_eq!(current_session(&env).as_deref(), Some("after-clear"));
    }

    #[test]
    fn a_sessions_file_left_by_an_ended_process_is_ignored() {
        let dir = crate::testing::TempDir::new("claude-stale-session");
        let env = processes(&dir, "someone-else", "650");
        assert_eq!(current_session(&env), None);
    }

    #[test]
    fn without_a_claude_code_above_there_is_no_current_session() {
        let dir = crate::testing::TempDir::new("claude-no-session");
        let mut env = processes(&dir, "x", "700");
        std::fs::remove_file(dir.path().join("home/.claude/sessions/20.json")).unwrap();
        assert_eq!(current_session(&env), None);
        env.proc_dir = None;
        assert_eq!(current_session(&env), None);
    }

    #[test]
    fn subagent_files_are_part_of_their_session() {
        let dir = crate::testing::TempDir::new("claude-session-files");
        let base = "home/.claude/projects/-home-u-git-app";
        dir.write(&format!("{base}/s1.jsonl"), "");
        dir.write(&format!("{base}/s1/subagents/agent-a.jsonl"), "");
        dir.write(
            &format!("{base}/s1/subagents/workflows/wf_1/agent-b.jsonl"),
            "",
        );
        dir.write(&format!("{base}/s1/tool-results/x.txt"), "");
        dir.write(&format!("{base}/s2.jsonl"), "");
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        assert_eq!(session_files(&env, "s1").len(), 3);
        assert_eq!(files(&env).files.len(), 4);
    }
}
