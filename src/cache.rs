//! The last reading of each harness, in `$XDG_CACHE_HOME/to11y`. The files
//! are disposable: a missing or unreadable one just means a fresh fetch.

use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use crate::model::{Harness, Snapshot, Unavailable};
use crate::providers::Failure;

/// How long a reading, or a failure, is reused before asking again, in
/// seconds. Anthropic answered 429 to a refresh one minute after the last,
/// perhaps because Claude Code also checks with the same sign-in, so Claude
/// readings are reused for longer.
fn fresh_for(harness: Harness) -> i64 {
    match harness {
        Harness::ClaudeCode => 300,
        Harness::Codex | Harness::Droid | Harness::Antigravity => 60,
    }
}

const VERSION: i64 = 1;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// What is known about a harness.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// The latest good snapshot, however old.
    pub snapshot: Option<Snapshot>,
    /// Why the latest attempt failed, if it did. With a snapshot, the
    /// snapshot is stale.
    pub failure: Option<Unavailable>,
    /// When the latest attempt was made, in Unix seconds.
    pub checked_at: i64,
    retry_after: Option<i64>,
}

pub struct Cache {
    dir: Option<PathBuf>,
    /// One per harness, held across read, fetch, and write, so parallel
    /// calls in one process cost one request. Each also keeps the latest
    /// reading, for when there is no cache directory.
    slots: [Mutex<Option<Reading>>; 4],
}

impl Cache {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Cache {
            dir,
            slots: Default::default(),
        }
    }

    /// The reading for `harness`, calling `fetch` only when the last
    /// attempt is older than its hold: `fresh_for(harness)`, or longer when
    /// a failing provider sent `Retry-After`.
    pub fn get(
        &self,
        harness: Harness,
        now: i64,
        fetch: impl FnOnce() -> Result<Snapshot, Failure>,
    ) -> Reading {
        let mut slot = self.slot(harness);
        let last = newest(self.load(harness), slot.take());
        if let Some(reading) = &last {
            let fresh = fresh_for(harness);
            let hold = match reading.failure {
                None => fresh,
                Some(_) => reading.retry_after.unwrap_or(0).max(fresh),
            };
            if (0..hold).contains(&(now - reading.checked_at)) {
                *slot = last.clone();
                return last.expect("checked above");
            }
        }
        let reading = match fetch() {
            Ok(snapshot) => Reading {
                snapshot: Some(snapshot),
                failure: None,
                checked_at: now,
                retry_after: None,
            },
            Err(failure) => Reading {
                snapshot: last.and_then(|reading| reading.snapshot),
                failure: Some(failure.why),
                checked_at: now,
                retry_after: failure.retry_after,
            },
        };
        self.store(harness, &reading);
        *slot = Some(reading.clone());
        reading
    }

    /// The reading for `harness` without fetching, if there is one.
    pub fn peek(&self, harness: Harness) -> Option<Reading> {
        let slot = self.slot(harness);
        newest(self.load(harness), slot.clone())
    }

    fn slot(&self, harness: Harness) -> std::sync::MutexGuard<'_, Option<Reading>> {
        let index = Harness::ALL
            .iter()
            .position(|h| *h == harness)
            .expect("listed");
        self.slots[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path(&self, harness: Harness) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{}.json", harness.name())))
    }

    fn load(&self, harness: Harness) -> Option<Reading> {
        let mut bytes = Vec::new();
        fs::File::open(self.path(harness)?)
            .ok()?
            .take(MAX_FILE_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        let json: Value = serde_json::from_slice(&bytes).ok()?;
        if json["version"].as_i64() != Some(VERSION) {
            return None;
        }
        let snapshot = match &json["snapshot"] {
            Value::Null => None,
            value => Some(Snapshot::from_json(value).filter(|s| s.harness == harness)?),
        };
        let failure = match &json["failure"] {
            Value::Null => None,
            value => Some(Unavailable::from_json(value)?),
        };
        Some(Reading {
            snapshot,
            failure,
            checked_at: json["checked_at"].as_i64()?,
            retry_after: json["retry_after"].as_i64(),
        })
    }

    /// Writes through a private temporary file and a rename, so a reader
    /// never sees half a file. Failures are ignored: the cache is optional.
    fn store(&self, harness: Harness, reading: &Reading) {
        let (Some(dir), Some(path)) = (&self.dir, self.path(harness)) else {
            return;
        };
        let json = json!({
            "version": VERSION,
            "checked_at": reading.checked_at,
            "failure": reading.failure.as_ref().map(Unavailable::to_json),
            "retry_after": reading.retry_after,
            "snapshot": reading.snapshot.as_ref().map(Snapshot::to_json),
        });
        let temporary = dir.join(format!(".{}.{}.tmp", harness.name(), std::process::id()));
        let written = make_dir(dir).and_then(|()| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.set_permissions(Permissions::from_mode(0o600))?;
            file.write_all(json.to_string().as_bytes())?;
            fs::rename(&temporary, &path)
        });
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }
}

/// The later of two readings.
fn newest(a: Option<Reading>, b: Option<Reading>) -> Option<Reading> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.checked_at > a.checked_at { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Creates `dir` readable only by its owner.
pub fn make_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Window, WindowKind, WindowState};
    use crate::testing::TempDir;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: i64 = 1_795_444_270;

    fn cache(dir: &TempDir) -> Cache {
        Cache::new(Some(dir.path().join("to11y")))
    }

    fn snapshot(used: f64, at: i64) -> Snapshot {
        Snapshot {
            harness: Harness::Codex,
            plan: Some("plus".to_owned()),
            tier: None,
            windows: vec![Window {
                id: "5h".to_owned(),
                label: "5-hour, all models".to_owned(),
                scope: "all".to_owned(),
                kind: WindowKind::FiveHour,
                state: WindowState::Running,
                used_percent: used,
                resets_at: Some(at + 3600),
                window_minutes: Some(300),
            }],
            credits: None,
            fetched_at: at,
        }
    }

    fn failed(retry_after: Option<i64>) -> Failure {
        Failure {
            why: Unavailable::Failed("OpenAI answered the usage request with HTTP 500.".to_owned()),
            retry_after,
        }
    }

    #[test]
    fn a_reading_is_reused_for_a_minute() {
        let dir = TempDir::new("cache-fresh");
        let cache = cache(&dir);
        let first = cache.get(Harness::Codex, NOW, || Ok(snapshot(10.0, NOW)));
        let again = cache.get(Harness::Codex, NOW + 59, || panic!("fetched while fresh"));
        assert_eq!(again, first);
        let later = cache.get(Harness::Codex, NOW + 60, || Ok(snapshot(20.0, NOW + 60)));
        assert_eq!(later.snapshot.unwrap().windows[0].used_percent, 20.0);
    }

    #[test]
    fn a_claude_reading_or_failure_is_reused_for_five_minutes() {
        let dir = TempDir::new("cache-fresh-claude");
        let cache = cache(&dir);
        cache.get(Harness::ClaudeCode, NOW, || Ok(snapshot(10.0, NOW)));
        cache.get(Harness::ClaudeCode, NOW + 299, || {
            panic!("fetched while fresh")
        });
        cache.get(Harness::ClaudeCode, NOW + 300, || Err(failed(Some(30))));
        cache.get(Harness::ClaudeCode, NOW + 599, || {
            panic!("fetched during the hold")
        });
        let reading = cache.get(Harness::ClaudeCode, NOW + 600, || {
            Ok(snapshot(20.0, NOW + 600))
        });
        assert_eq!(reading.failure, None);
    }

    #[test]
    fn another_process_reads_the_same_file() {
        let dir = TempDir::new("cache-shared");
        cache(&dir).get(Harness::Codex, NOW, || Ok(snapshot(10.0, NOW)));
        let other = cache(&dir);
        let reading = other.get(Harness::Codex, NOW + 30, || panic!("fetched while fresh"));
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
    }

    #[test]
    fn a_failure_keeps_the_last_good_snapshot_as_stale() {
        let dir = TempDir::new("cache-stale");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || Ok(snapshot(10.0, NOW)));
        let reading = cache.get(Harness::Codex, NOW + 120, || Err(failed(None)));
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
        assert!(matches!(reading.failure, Some(Unavailable::Failed(_))));
        assert_eq!(reading.checked_at, NOW + 120);
    }

    #[test]
    fn a_failure_is_held_for_a_minute_or_for_retry_after() {
        let dir = TempDir::new("cache-hold");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || Err(failed(None)));
        cache.get(Harness::Codex, NOW + 59, || {
            panic!("fetched during the hold")
        });
        cache.get(Harness::Codex, NOW + 60, || Err(failed(Some(300))));
        cache.get(Harness::Codex, NOW + 359, || {
            panic!("fetched before Retry-After")
        });
        let reading = cache.get(Harness::Codex, NOW + 360, || Ok(snapshot(5.0, NOW + 360)));
        assert_eq!(reading.failure, None);
    }

    #[test]
    fn a_reading_from_the_future_is_not_trusted() {
        let dir = TempDir::new("cache-future");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW + 1000, || {
            Ok(snapshot(10.0, NOW + 1000))
        });
        let reading = cache.get(Harness::Codex, NOW, || Ok(snapshot(20.0, NOW)));
        assert_eq!(reading.snapshot.unwrap().windows[0].used_percent, 20.0);
    }

    #[test]
    fn parallel_calls_make_one_request() {
        let dir = TempDir::new("cache-flight");
        let cache = cache(&dir);
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..10 {
                scope.spawn(|| {
                    cache.get(Harness::Codex, NOW, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        Ok(snapshot(10.0, NOW))
                    })
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn files_are_private_to_their_owner() {
        let dir = TempDir::new("cache-modes");
        cache(&dir).get(Harness::Codex, NOW, || Ok(snapshot(10.0, NOW)));
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("to11y")), 0o700);
        assert_eq!(mode(&dir.path().join("to11y/codex.json")), 0o600);
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("to11y")).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn a_damaged_or_foreign_file_is_ignored() {
        let dir = TempDir::new("cache-damaged");
        let cache = cache(&dir);
        make_dir(&dir.path().join("to11y")).unwrap();
        let path = dir.path().join("to11y/codex.json");
        fs::write(&path, "{").unwrap();
        assert_eq!(cache.peek(Harness::Codex), None);
        let mut other = snapshot(10.0, NOW);
        other.harness = Harness::Droid;
        let foreign = json!({"version": 1, "checked_at": NOW, "snapshot": other.to_json()});
        fs::write(&path, foreign.to_string()).unwrap();
        assert_eq!(
            Cache::new(Some(dir.path().join("to11y"))).peek(Harness::Codex),
            None
        );
    }

    #[test]
    fn without_a_directory_readings_stay_in_memory() {
        let cache = Cache::new(None);
        cache.get(Harness::Codex, NOW, || Ok(snapshot(10.0, NOW)));
        cache.get(Harness::Codex, NOW + 10, || panic!("fetched while fresh"));
        assert!(cache.peek(Harness::Codex).is_some());
        assert_eq!(cache.peek(Harness::Droid), None);
    }
}
