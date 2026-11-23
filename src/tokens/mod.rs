//! Token usage read from the logs each harness writes on disk. Every call
//! rescans the files in its range: there is no index.
//!
//! Each harness module has `files`, which lists the logs, and a pure
//! `parse`, which turns one file into events. Parsers keep only usage
//! fields, a model name, a session id, and a working directory. Message
//! text never outlives the line it was read from.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::env::Env;
use crate::model::Harness;

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod droid;
pub mod report;

/// The most threads one scan uses.
const MAX_THREADS: usize = 8;

/// Token counts in four channels that add up to the total. `input` is
/// always the uncached share. `output` includes `reasoning`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: Option<u64>,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning = match (self.reasoning, other.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
    }

    /// The larger of each channel, for two copies of one response.
    fn merge_max(&mut self, other: &Tokens) {
        self.input = self.input.max(other.input);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
        self.output = self.output.max(other.output);
        self.reasoning = match (self.reasoning, other.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).max(b.unwrap_or(0))),
        };
    }
}

/// One model response, or for Codex one turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Unix seconds.
    pub at: i64,
    pub model: String,
    /// The working directory, as the log recorded it.
    pub project: Option<String>,
    /// Used to pick out one session. Never shown in a result.
    pub session: String,
    /// A hash of the harness's dedupe key, when it has one.
    pub key: Option<u64>,
    pub tokens: Tokens,
}

/// What one file held.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub events: Vec<Event>,
    /// Lines that looked like usage but couldn't be read.
    pub skipped: usize,
}

/// A log file and what the file system says about it.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: PathBuf,
    /// Unix seconds.
    pub mtime: i64,
    pub size: u64,
}

/// The log files of one harness.
#[derive(Debug, Default)]
pub struct Listing {
    /// The folders the logs live in that exist. Antigravity may have more
    /// than one.
    pub roots: Vec<PathBuf>,
    pub files: Vec<FileInfo>,
    /// Files that look like logs but can't be read, such as compressed
    /// Codex rollouts.
    pub unsupported: usize,
}

/// The events of a range, and what it took to find them.
#[derive(Debug, Default)]
pub struct Scan {
    pub events: Vec<Event>,
    pub files: usize,
    pub bytes: u64,
    pub skipped_lines: usize,
    pub unreadable: usize,
    pub unsupported: usize,
}

/// A rolling range ending now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    FiveHours,
    Day,
    Week,
    Month,
}

impl Period {
    pub const ALL: [Period; 4] = [Period::FiveHours, Period::Day, Period::Week, Period::Month];

    pub fn name(self) -> &'static str {
        match self {
            Period::FiveHours => "5h",
            Period::Day => "24h",
            Period::Week => "7d",
            Period::Month => "30d",
        }
    }

    pub fn parse(name: &str) -> Option<Period> {
        Period::ALL.into_iter().find(|p| p.name() == name)
    }

    pub fn seconds(self) -> i64 {
        match self {
            Period::FiveHours => 5 * 3600,
            Period::Day => 86_400,
            Period::Week => 7 * 86_400,
            Period::Month => 30 * 86_400,
        }
    }
}

/// The log files of `harness`.
pub fn list(harness: Harness, env: &Env) -> Listing {
    match harness {
        Harness::ClaudeCode => claude::files(env),
        Harness::Codex => codex::files(env),
        Harness::Droid => droid::files(env),
        Harness::Antigravity => antigravity::files(env),
    }
}

fn parser(harness: Harness) -> fn(&mut dyn BufRead, &Path) -> Parsed {
    match harness {
        Harness::ClaudeCode => claude::parse,
        Harness::Codex => codex::parse,
        Harness::Droid => droid::parse,
        Harness::Antigravity => antigravity::parse,
    }
}

/// The events of `harness` from `since` to `until`, in Unix seconds.
pub fn scan(harness: Harness, env: &Env, since: i64, until: i64) -> Scan {
    let listing = list(harness, env);
    let files: Vec<FileInfo> = listing
        .files
        .into_iter()
        // A log is only appended to, so a file last changed before the
        // range can't hold an event in it.
        .filter(|file| file.mtime >= since)
        .collect();
    let mut scan = scan_files(harness, &files);
    scan.unsupported = listing.unsupported;
    scan.events
        .retain(|event| (since..=until).contains(&event.at));
    if harness == Harness::Droid {
        droid::add_projects(env, &mut scan.events);
    }
    scan
}

/// Every event in `files`, deduplicated across them.
pub fn scan_files(harness: Harness, files: &[FileInfo]) -> Scan {
    let parse = parser(harness);
    let mut order: Vec<usize> = (0..files.len()).collect();
    // Largest first, so one big file doesn't run alone at the end.
    order.sort_by_key(|&i| std::cmp::Reverse(files[i].size));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Option<Parsed>)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, MAX_THREADS)
        .min(files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&index) = order.get(at) else { break };
                    let path = &files[index].path;
                    let parsed = fs::File::open(path).ok().map(|file| {
                        let mut reader = BufReader::with_capacity(1 << 18, file);
                        let mut parsed = parse(&mut reader, path);
                        // Each file's repeats go first, so the merge holds
                        // fewer events at once.
                        let mut own = Vec::new();
                        fold(&mut own, &mut HashMap::new(), parsed.events);
                        parsed.events = own;
                        parsed
                    });
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((index, parsed));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    // File order, so the merge doesn't depend on thread timing.
    results.sort_by_key(|(index, _)| *index);
    let mut scan = Scan {
        files: files.len(),
        bytes: files.iter().map(|f| f.size).sum(),
        ..Scan::default()
    };
    let mut seen: HashMap<u64, usize> = HashMap::new();
    for (_, parsed) in results {
        let Some(parsed) = parsed else {
            scan.unreadable += 1;
            continue;
        };
        scan.skipped_lines += parsed.skipped;
        fold(&mut scan.events, &mut seen, parsed.events);
    }
    scan
}

/// Adds `events` to `into`, merging each into an earlier event with the
/// same key, channel by channel, keeping the larger value.
fn fold(into: &mut Vec<Event>, seen: &mut HashMap<u64, usize>, events: Vec<Event>) {
    for event in events {
        match event.key {
            Some(key) => match seen.get(&key) {
                Some(&at) => into[at].tokens.merge_max(&event.tokens),
                None => {
                    seen.insert(key, into.len());
                    into.push(event);
                }
            },
            None => into.push(event),
        }
    }
}

/// A 64-bit hash of a dedupe key. `DefaultHasher::new` uses fixed keys, so
/// the hash is the same in every thread.
pub fn key(parts: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

/// Calls `line` with each line of `read`, without its line ending. Reading
/// stops at the first I/O error.
pub fn lines(read: &mut dyn BufRead, mut line: impl FnMut(&[u8])) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match read.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let mut end = buffer.len();
                while end > 0 && matches!(buffer[end - 1], b'\n' | b'\r') {
                    end -= 1;
                }
                if end > 0 {
                    line(&buffer[..end]);
                }
            }
        }
    }
}

/// Whether `line` contains `needle`, checked before any JSON parse.
pub fn has(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}

/// A count field, missing or negative as zero.
pub fn count(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|v| *v > 0.0).map(|v| v as u64))
        .unwrap_or(0)
}

/// A count field that may be missing.
pub fn maybe_count(value: &Value) -> Option<u64> {
    (!value.is_null()).then(|| count(value))
}

/// Files under `dir` for which `wanted` says yes, down to `depth` levels of
/// folders. Unreadable folders are skipped.
pub fn walk(dir: &Path, depth: usize, wanted: &dyn Fn(&Path) -> bool, out: &mut Vec<FileInfo>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth > 0 {
                walk(&path, depth - 1, wanted, out);
            }
        } else if kind.is_file()
            && wanted(&path)
            && let Some(info) = info(path)
        {
            out.push(info);
        }
    }
}

/// What the file system says about `path`.
pub fn info(path: PathBuf) -> Option<FileInfo> {
    let meta = fs::metadata(&path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    Some(FileInfo {
        path,
        mtime,
        size: meta.len(),
    })
}

/// Whether `path`'s file name ends with `suffix`.
pub fn named(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(suffix))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::time::{Duration, SystemTime};

    pub const NOW: i64 = 1_795_444_270;

    /// Sets `path`'s mtime to `at`, in Unix seconds.
    pub fn touch(path: &Path, at: i64) {
        let file = fs::File::options().append(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at as u64))
            .unwrap();
    }

    pub fn parse_text(
        parse: fn(&mut dyn BufRead, &Path) -> Parsed,
        text: &str,
        path: &str,
    ) -> Parsed {
        parse(&mut text.as_bytes(), Path::new(path))
    }

    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input,
            output,
            ..Tokens::default()
        }
    }

    #[test]
    fn the_four_channels_add_up_to_the_total() {
        let t = Tokens {
            input: 1,
            cache_read: 20,
            cache_write: 300,
            output: 4000,
            reasoning: Some(10),
        };
        assert_eq!(t.total(), 4321);
    }

    #[test]
    fn reasoning_stays_unknown_until_some_is_known() {
        let mut sum = tokens(1, 1);
        sum.add(&tokens(1, 1));
        assert_eq!(sum.reasoning, None);
        sum.add(&Tokens {
            reasoning: Some(5),
            ..tokens(0, 5)
        });
        assert_eq!(sum.reasoning, Some(5));
    }

    #[test]
    fn periods_round_trip_their_names() {
        for period in Period::ALL {
            assert_eq!(Period::parse(period.name()), Some(period));
        }
        assert_eq!(Period::parse("1y"), None);
        assert_eq!(Period::Week.seconds(), 604_800);
    }

    #[test]
    fn lines_drop_line_endings_and_blank_lines() {
        let mut seen = Vec::new();
        lines(&mut &b"one\r\n\ntwo\nthree"[..], |line| {
            seen.push(String::from_utf8(line.to_vec()).unwrap())
        });
        assert_eq!(seen, ["one", "two", "three"]);
    }

    #[test]
    fn copies_of_one_response_count_once_with_the_larger_value() {
        let dir = TempDir::new("tokens-dedupe");
        let line = |input: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-11-23T14:00:00Z","sessionId":"s","message":{{"id":"m1","model":"claude-opus-5-5","usage":{{"input_tokens":{input},"output_tokens":2}}}},"requestId":"r1"}}"#
            )
        };
        let a = dir.write("a.jsonl", &format!("{}\n{}\n", line(5), line(5)));
        let b = dir.write("b.jsonl", &format!("{}\n", line(7)));
        let files: Vec<FileInfo> = [a, b].into_iter().filter_map(info).collect();
        let scan = scan_files(Harness::ClaudeCode, &files);
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tokens.input, 7);
        assert_eq!(scan.files, 2);
    }

    #[test]
    fn a_file_that_cant_be_opened_is_counted_as_unreadable() {
        let files = vec![FileInfo {
            path: "/nonexistent/to11y/file.jsonl".into(),
            mtime: NOW,
            size: 0,
        }];
        let scan = scan_files(Harness::ClaudeCode, &files);
        assert_eq!(scan.unreadable, 1);
        assert!(scan.events.is_empty());
    }

    #[test]
    fn files_last_changed_before_the_range_are_not_read() {
        let dir = TempDir::new("tokens-mtime");
        let line = |at: &str, id: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"{at}","sessionId":"s","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":1,"output_tokens":1}}}},"requestId":"r"}}"#
            )
        };
        let old = dir.write(
            "home/.claude/projects/p/old.jsonl",
            &line("2026-11-23T13:00:00Z", "a"),
        );
        let new = dir.write(
            "home/.claude/projects/p/new.jsonl",
            &format!(
                "{}\n{}\n",
                line("2026-11-20T00:00:00Z", "b"),
                line("2026-11-23T13:00:00Z", "c")
            ),
        );
        // The old file's event is in range, but its mtime says it can't be.
        touch(&old, NOW - 2 * 86_400);
        touch(&new, NOW - 60);
        let env = Env {
            home: Some(dir.path().join("home")),
            ..Env::default()
        };
        let scan = scan(Harness::ClaudeCode, &env, NOW - 86_400, NOW);
        assert_eq!(scan.files, 1);
        // The new file's older event is outside the range.
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].at, NOW - 5470);
    }
}
