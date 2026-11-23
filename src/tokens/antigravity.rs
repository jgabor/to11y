//! Antigravity CLI transcripts:
//! `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/`,
//! and the same under each folder `Env::antigravity_cli_dirs` names, such as
//! T3 Code's `antigravity-acp`.
//!
//! `transcript_full.jsonl` and its `chunks/transcript_full/*.jsonl` hold the
//! same steps, so steps are deduplicated by conversation and step index.
//! Only model steps carry usage, without a cache-write or reasoning split
//! and without a working directory. Some machines have older conversations
//! only in encrypted or SQLite files, which to11y doesn't read.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Event, FileInfo, Listing, Parsed, Tokens, count, has, key, lines, walk};
use crate::env::Env;
use crate::time;

pub fn files(env: &Env) -> Listing {
    let mut listing = Listing::default();
    for root in env.antigravity_cli_dirs() {
        let root = root.join("brain");
        let Ok(conversations) = std::fs::read_dir(&root) else {
            continue;
        };
        for conversation in conversations.flatten() {
            listing
                .files
                .extend(conversation_files(&conversation.path()));
        }
        listing.roots.push(root);
    }
    listing
}

/// The transcript files of one conversation folder.
pub fn conversation_files(dir: &Path) -> Vec<FileInfo> {
    let logs = dir.join(".system_generated/logs");
    let mut files = Vec::new();
    if let Some(info) = super::info(logs.join("transcript_full.jsonl")) {
        files.push(info);
    }
    walk(
        &logs.join("chunks/transcript_full"),
        0,
        &|path| super::named(path, ".jsonl"),
        &mut files,
    );
    files
}

/// The folder under `brain/` that a transcript file belongs to.
fn conversation_dir(path: &Path) -> Option<&Path> {
    // A main file is 3 levels below its conversation, a chunk 5.
    let up = if super::named(path, "transcript_full.jsonl") {
        3
    } else {
        5
    };
    path.ancestors().nth(up)
}

/// The conversation a transcript file belongs to: its folder's name.
fn conversation(path: &Path) -> Option<&str> {
    conversation_dir(path)?.file_name()?.to_str()
}

/// The conversation folder holding the most recently written transcript,
/// in any of the CLI's folders.
pub fn latest_conversation(env: &Env) -> Option<PathBuf> {
    let listing = files(env);
    let latest = listing.files.iter().max_by_key(|file| file.mtime)?;
    conversation_dir(&latest.path).map(Path::to_path_buf)
}

pub fn parse(read: &mut dyn BufRead, path: &Path) -> Parsed {
    let conversation = conversation(path).unwrap_or("").to_owned();
    let mut parsed = Parsed::default();
    lines(read, |line| {
        if !has(line, b"\"input_tokens\"") {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(value) => {
                if let Some(event) = read_step(&value, &conversation) {
                    parsed.events.push(event);
                } else {
                    parsed.skipped += 1;
                }
            }
            Err(_) => parsed.skipped += 1,
        }
    });
    parsed
}

fn read_step(step: &Value, conversation: &str) -> Option<Event> {
    let at = time::parse(step["created_at"].as_str()?)?;
    let index = step["step_index"].as_u64()?.to_string();
    Some(Event {
        at,
        model: step["model"].as_str().unwrap_or("unknown").to_owned(),
        project: None,
        session: conversation.to_owned(),
        key: Some(key(&[conversation, &index])),
        tokens: Tokens {
            input: count(&step["input_tokens"]),
            cache_read: count(&step["cache_read_tokens"]),
            cache_write: 0,
            output: count(&step["output_tokens"]),
            reasoning: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Harness;
    use crate::testing::TempDir;
    use crate::tokens::{scan_files, tests::NOW};

    fn step(index: u64, input: u64) -> String {
        format!(
            r#"{{"step_index":{index},"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-23T14:31:10Z","model":"claude-opus-5-5-low","content":"hi","input_tokens":{input},"cache_read_tokens":17243,"output_tokens":312}}"#
        )
    }

    #[test]
    fn chunks_and_the_main_transcript_count_each_step_once() {
        let dir = TempDir::new("agy-chunks");
        let logs = "home/.gemini/antigravity-cli/brain/c1/.system_generated/logs";
        let user = r#"{"step_index":0,"source":"USER","type":"USER_INPUT","created_at":"2026-11-23T14:31:00Z","content":"hi"}"#;
        dir.write(
            &format!("{logs}/transcript_full.jsonl"),
            &format!("{user}\n{}\n{}\n", step(1, 885), step(3, 900)),
        );
        dir.write(
            &format!("{logs}/chunks/transcript_full/00000000.jsonl"),
            &step(1, 885),
        );
        dir.write(&format!("{logs}/transcript.jsonl"), &step(1, 885));
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        let listing = files(&env);
        assert_eq!(listing.files.len(), 2);
        let scan = scan_files(Harness::Antigravity, &listing.files);
        assert_eq!(scan.events.len(), 2);
        assert!(scan.events.iter().all(|e| e.session == "c1" && e.at == NOW));
        assert_eq!(scan.events[0].tokens.cache_read, 17_243);
        assert_eq!(latest_conversation(&env), Some(listing.roots[0].join("c1")));
    }

    #[test]
    fn t3_codes_conversations_under_gemini_home_count_too() {
        let dir = TempDir::new("agy-t3-logs");
        let logs = |root: &str, id: &str| format!("{root}/brain/{id}/.system_generated/logs");
        let plain = dir.write(
            &format!(
                "{}/transcript_full.jsonl",
                logs("home/.gemini/antigravity-cli", "c1")
            ),
            &step(1, 100),
        );
        let t3 = dir.write(
            &format!(
                "{}/transcript_full.jsonl",
                logs("t3/antigravity-acp", "t3c")
            ),
            &step(1, 200),
        );
        crate::tokens::tests::touch(&plain, NOW - 60);
        crate::tokens::tests::touch(&t3, NOW);
        let env = Env {
            home: Some(dir.path().join("home")),
            gemini_home: Some(dir.path().join("t3")),
            ..Env::default()
        };
        let listing = files(&env);
        assert_eq!((listing.roots.len(), listing.files.len()), (2, 2));
        let scan = scan_files(Harness::Antigravity, &listing.files);
        let mut sessions: Vec<&str> = scan.events.iter().map(|e| e.session.as_str()).collect();
        sessions.sort();
        assert_eq!(sessions, ["c1", "t3c"]);
        assert_eq!(
            latest_conversation(&env),
            Some(dir.path().join("t3/antigravity-acp/brain/t3c"))
        );
        let (sources, error) = crate::tokens::report::sources(Harness::Antigravity, &env, NOW);
        assert!(!error, "{sources}");
        assert_eq!(sources["folders"].as_array().unwrap().len(), 2);
        assert_eq!(sources["files"], 2);
    }

    #[test]
    fn a_chunk_knows_its_conversation() {
        let path =
            Path::new("/h/brain/c9/.system_generated/logs/chunks/transcript_full/00000001.jsonl");
        assert_eq!(conversation(path), Some("c9"));
        let main = Path::new("/h/brain/c9/.system_generated/logs/transcript_full.jsonl");
        assert_eq!(conversation(main), Some("c9"));
    }
}
