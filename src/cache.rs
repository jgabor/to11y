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
    ///
    /// `saved` is the reading the harness itself saved on disk, if any. It
    /// takes the place of an older snapshot after a failure, whether or not
    /// a call is made, so a held failure can still show a newer reading. For
    /// Claude Code, whose saved reading is the provider's whole answer, it
    /// also replaces an older good one, and one younger than the hold is
    /// used without a call.
    pub fn get(
        &self,
        harness: Harness,
        now: i64,
        saved: impl FnOnce() -> Option<Snapshot>,
        fetch: impl FnOnce() -> Result<Snapshot, Failure>,
    ) -> Reading {
        let mut slot = self.slot(harness);
        let last = newest(self.load(harness), slot.take());
        let saved = saved().filter(|s| s.harness == harness && s.fetched_at <= now);
        let fresh = fresh_for(harness);
        if let Some(reading) = &last {
            let hold = match reading.failure {
                None => fresh,
                Some(_) => reading.retry_after.unwrap_or(0).max(fresh),
            };
            if (0..hold).contains(&(now - reading.checked_at)) {
                let mut reading = reading.clone();
                let replaceable = reading.failure.is_some() || uses_saved_without_a_call(harness);
                if let Some(newer) = newer_than(saved, reading.snapshot.as_ref())
                    && replaceable
                {
                    reading.snapshot = Some(newer);
                    self.store(harness, &reading);
                }
                *slot = Some(reading.clone());
                return reading;
            }
        }
        if let Some(snapshot) = saved
            .as_ref()
            .filter(|s| uses_saved_without_a_call(harness) && now - s.fetched_at < fresh)
        {
            // Held from when the harness asked, not from now, so it is never
            // reused for longer than a reading to11y fetched itself.
            let reading = Reading {
                snapshot: Some(snapshot.clone()),
                failure: None,
                checked_at: snapshot.fetched_at,
                retry_after: None,
            };
            self.store(harness, &reading);
            *slot = Some(reading.clone());
            return reading;
        }
        let reading = match fetch() {
            Ok(snapshot) => Reading {
                snapshot: Some(snapshot),
                failure: None,
                checked_at: now,
                retry_after: None,
            },
            Err(failure) => {
                let last = last.and_then(|reading| reading.snapshot);
                Reading {
                    snapshot: newer_than(saved, last.as_ref()).or(last),
                    failure: Some(failure.why),
                    checked_at: now,
                    retry_after: failure.retry_after,
                }
            }
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

/// Whether a fresh saved reading stands in for a call. Only Claude Code:
/// Anthropic limits how often its usage endpoint is asked, and Claude Code
/// asks it too. Codex's saved reading lacks some of the provider's windows.
fn uses_saved_without_a_call(harness: Harness) -> bool {
    harness == Harness::ClaudeCode
}

/// `saved`, when it is newer than `current` or there is no `current`.
fn newer_than(saved: Option<Snapshot>, current: Option<&Snapshot>) -> Option<Snapshot> {
    saved.filter(|s| current.is_none_or(|c| s.fetched_at > c.fetched_at))
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
            origin: crate::model::Origin::Provider,
        }
    }

    fn saved_copy(harness: Harness, used: f64, at: i64) -> Snapshot {
        Snapshot {
            harness,
            origin: crate::model::Origin::Saved,
            ..snapshot(used, at)
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
        let first = cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        let again = cache.get(
            Harness::Codex,
            NOW + 59,
            || None,
            || panic!("fetched while fresh"),
        );
        assert_eq!(again, first);
        let later = cache.get(
            Harness::Codex,
            NOW + 60,
            || None,
            || Ok(snapshot(20.0, NOW + 60)),
        );
        assert_eq!(later.snapshot.unwrap().windows[0].used_percent, 20.0);
    }

    #[test]
    fn a_claude_reading_or_failure_is_reused_for_five_minutes() {
        let dir = TempDir::new("cache-fresh-claude");
        let cache = cache(&dir);
        cache.get(
            Harness::ClaudeCode,
            NOW,
            || None,
            || Ok(snapshot(10.0, NOW)),
        );
        cache.get(
            Harness::ClaudeCode,
            NOW + 299,
            || None,
            || panic!("fetched while fresh"),
        );
        cache.get(
            Harness::ClaudeCode,
            NOW + 300,
            || None,
            || Err(failed(Some(30))),
        );
        cache.get(
            Harness::ClaudeCode,
            NOW + 599,
            || None,
            || panic!("fetched during the hold"),
        );
        let reading = cache.get(
            Harness::ClaudeCode,
            NOW + 600,
            || None,
            || Ok(snapshot(20.0, NOW + 600)),
        );
        assert_eq!(reading.failure, None);
    }

    #[test]
    fn another_process_reads_the_same_file() {
        let dir = TempDir::new("cache-shared");
        cache(&dir).get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        let other = cache(&dir);
        let reading = other.get(
            Harness::Codex,
            NOW + 30,
            || None,
            || panic!("fetched while fresh"),
        );
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
    }

    #[test]
    fn a_failure_keeps_the_last_good_snapshot_as_stale() {
        let dir = TempDir::new("cache-stale");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        let reading = cache.get(Harness::Codex, NOW + 120, || None, || Err(failed(None)));
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
        assert!(matches!(reading.failure, Some(Unavailable::Failed(_))));
        assert_eq!(reading.checked_at, NOW + 120);
    }

    #[test]
    fn a_failure_is_held_for_a_minute_or_for_retry_after() {
        let dir = TempDir::new("cache-hold");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || None, || Err(failed(None)));
        cache.get(
            Harness::Codex,
            NOW + 59,
            || None,
            || panic!("fetched during the hold"),
        );
        cache.get(Harness::Codex, NOW + 60, || None, || Err(failed(Some(300))));
        cache.get(
            Harness::Codex,
            NOW + 359,
            || None,
            || panic!("fetched before Retry-After"),
        );
        let reading = cache.get(
            Harness::Codex,
            NOW + 360,
            || None,
            || Ok(snapshot(5.0, NOW + 360)),
        );
        assert_eq!(reading.failure, None);
    }

    #[test]
    fn a_reading_from_the_future_is_not_trusted() {
        let dir = TempDir::new("cache-future");
        let cache = cache(&dir);
        cache.get(
            Harness::Codex,
            NOW + 1000,
            || None,
            || Ok(snapshot(10.0, NOW + 1000)),
        );
        let reading = cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(20.0, NOW)));
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
                    cache.get(
                        Harness::Codex,
                        NOW,
                        || None,
                        || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            Ok(snapshot(10.0, NOW))
                        },
                    )
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn files_are_private_to_their_owner() {
        let dir = TempDir::new("cache-modes");
        cache(&dir).get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
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
    fn a_fresh_saved_claude_reading_makes_no_call() {
        let dir = TempDir::new("cache-saved-fresh");
        let cache = cache(&dir);
        let copy = saved_copy(Harness::ClaudeCode, 30.0, NOW - 100);
        let reading = cache.get(
            Harness::ClaudeCode,
            NOW,
            || Some(copy.clone()),
            || panic!("fetched with a fresh saved reading"),
        );
        assert_eq!((reading.snapshot, reading.failure), (Some(copy), None));
        cache.get(
            Harness::ClaudeCode,
            NOW + 199,
            || None,
            || panic!("fetched while fresh"),
        );
        let later = cache.get(
            Harness::ClaudeCode,
            NOW + 200,
            || None,
            || Ok(snapshot(40.0, NOW + 200)),
        );
        assert_eq!(later.snapshot.unwrap().windows[0].used_percent, 40.0);
    }

    #[test]
    fn an_old_saved_claude_reading_or_any_codex_one_still_calls() {
        let dir = TempDir::new("cache-saved-calls");
        let cache = cache(&dir);
        let calls = AtomicUsize::new(0);
        let fetch = |harness| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Snapshot {
                harness,
                ..snapshot(50.0, NOW)
            })
        };
        let old = saved_copy(Harness::ClaudeCode, 30.0, NOW - 300);
        let claude = cache.get(
            Harness::ClaudeCode,
            NOW,
            || Some(old),
            || fetch(Harness::ClaudeCode),
        );
        let fresh = saved_copy(Harness::Codex, 30.0, NOW - 1);
        let codex = cache.get(
            Harness::Codex,
            NOW,
            || Some(fresh),
            || fetch(Harness::Codex),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for reading in [claude, codex] {
            let snapshot = reading.snapshot.unwrap();
            assert_eq!(snapshot.origin, crate::model::Origin::Provider);
        }
    }

    #[test]
    fn a_held_failure_shows_a_newer_saved_reading() {
        let dir = TempDir::new("cache-saved-held");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || None, || Err(failed(Some(3600))));
        let copy = saved_copy(Harness::Codex, 30.0, NOW + 100);
        let reading = cache.get(
            Harness::Codex,
            NOW + 200,
            || Some(copy.clone()),
            || panic!("fetched before Retry-After"),
        );
        assert_eq!(reading.snapshot, Some(copy.clone()));
        assert!(reading.failure.is_some());
        let older = saved_copy(Harness::Codex, 20.0, NOW + 50);
        let again = cache.get(
            Harness::Codex,
            NOW + 300,
            || Some(older),
            || panic!("fetched before Retry-After"),
        );
        assert_eq!(again.snapshot, Some(copy));
    }

    #[test]
    fn a_good_codex_reading_is_not_replaced_by_its_saved_copy() {
        let dir = TempDir::new("cache-saved-good");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        let reading = cache.get(
            Harness::Codex,
            NOW + 30,
            || Some(saved_copy(Harness::Codex, 30.0, NOW + 20)),
            || panic!("fetched while fresh"),
        );
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
    }

    #[test]
    fn a_failed_fetch_falls_back_to_a_newer_saved_reading() {
        let dir = TempDir::new("cache-saved-failed");
        let cache = cache(&dir);
        cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        let older = saved_copy(Harness::Codex, 5.0, NOW - 10);
        let reading = cache.get(
            Harness::Codex,
            NOW + 120,
            || Some(older),
            || Err(failed(None)),
        );
        assert_eq!(reading.snapshot, Some(snapshot(10.0, NOW)));
        let newer = saved_copy(Harness::Codex, 30.0, NOW + 150);
        let reading = cache.get(
            Harness::Codex,
            NOW + 240,
            || Some(newer.clone()),
            || Err(failed(None)),
        );
        assert_eq!(reading.snapshot, Some(newer));
        assert!(reading.failure.is_some());
    }

    #[test]
    fn a_saved_reading_for_another_harness_or_from_the_future_is_ignored() {
        let dir = TempDir::new("cache-saved-ignored");
        let cache = cache(&dir);
        let reading = cache.get(
            Harness::Codex,
            NOW,
            || Some(saved_copy(Harness::Droid, 30.0, NOW - 5)),
            || Err(failed(None)),
        );
        assert_eq!(reading.snapshot, None);
        let reading = cache.get(
            Harness::ClaudeCode,
            NOW,
            || Some(saved_copy(Harness::ClaudeCode, 30.0, NOW + 5)),
            || Err(failed(None)),
        );
        assert_eq!(reading.snapshot, None);
    }

    #[test]
    fn a_saved_reading_stays_saved_in_the_file() {
        let dir = TempDir::new("cache-saved-file");
        let copy = saved_copy(Harness::ClaudeCode, 30.0, NOW - 10);
        cache(&dir).get(
            Harness::ClaudeCode,
            NOW,
            || Some(copy.clone()),
            || panic!("fetched with a fresh saved reading"),
        );
        let reading = cache(&dir).peek(Harness::ClaudeCode).unwrap();
        assert_eq!(reading.snapshot, Some(copy));
        assert_eq!(reading.checked_at, NOW - 10);
    }

    #[test]
    fn without_a_directory_readings_stay_in_memory() {
        let cache = Cache::new(None);
        cache.get(Harness::Codex, NOW, || None, || Ok(snapshot(10.0, NOW)));
        cache.get(
            Harness::Codex,
            NOW + 10,
            || None,
            || panic!("fetched while fresh"),
        );
        assert!(cache.peek(Harness::Codex).is_some());
        assert_eq!(cache.peek(Harness::Droid), None);
    }
}
