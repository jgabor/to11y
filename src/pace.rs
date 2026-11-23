//! Pacing: how a window's use compares with the time it has run, measured
//! against spending evenly over the whole window.

use crate::model::{Window, WindowState};

/// How far use may stray from even spending, in percentage points, and
/// still count as on pace.
pub const TOLERANCE: f64 = 5.0;
/// Forecasts divide by elapsed time, so they wait until this much of the
/// window has passed.
const MIN_ELAPSED_FOR_FORECAST: f64 = 0.05;
/// A safe rate in the last minutes before a reset is meaningless.
const MIN_SECONDS_FOR_SAFE_RATE: i64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Exhausted,
    /// Another window in the same scope is exhausted, so this one can't be
    /// used until that one resets.
    Blocked,
    OverPace,
    OnPace,
    UnderPace,
    NotStarted,
    Disabled,
    /// The provider gave no reset time or window length.
    Unknown,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Exhausted => "exhausted",
            Verdict::Blocked => "blocked",
            Verdict::OverPace => "over_pace",
            Verdict::OnPace => "on_pace",
            Verdict::UnderPace => "under_pace",
            Verdict::NotStarted => "not_started",
            Verdict::Disabled => "disabled",
            Verdict::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pace {
    pub verdict: Verdict,
    pub remaining_percent: f64,
    pub seconds_until_reset: Option<i64>,
    pub elapsed_percent: Option<f64>,
    /// Use minus elapsed time, in percentage points. Positive is ahead of
    /// even spending.
    pub pace_delta: Option<f64>,
    pub projected_percent_at_reset: Option<f64>,
    /// Unix seconds, when the projection crosses 100% before the reset.
    pub exhausts_at: Option<i64>,
    /// The most that can be used per hour and still last to the reset.
    pub safe_percent_per_hour: Option<f64>,
}

/// Paces one window as it stands at `now`.
pub fn pace(window: &Window, now: i64) -> Pace {
    let window = window.at(now);
    let used = window.used_percent.max(0.0);
    let remaining = (100.0 - used).max(0.0);
    let until = window.resets_at.map(|reset| (reset - now).max(0));
    let mut pace = Pace {
        verdict: Verdict::Unknown,
        remaining_percent: remaining,
        seconds_until_reset: until,
        elapsed_percent: None,
        pace_delta: None,
        projected_percent_at_reset: None,
        exhausts_at: None,
        safe_percent_per_hour: None,
    };
    match window.state {
        WindowState::Disabled => {
            pace.verdict = Verdict::Disabled;
            return pace;
        }
        WindowState::NotStarted => {
            pace.verdict = Verdict::NotStarted;
            return pace;
        }
        WindowState::Running => {}
    }
    if used >= 100.0 {
        pace.verdict = Verdict::Exhausted;
        return pace;
    }
    let (Some(until), Some(minutes)) = (until, window.window_minutes) else {
        return pace;
    };
    let length = i64::from(minutes) * 60;
    if length == 0 {
        return pace;
    }
    let elapsed = ((length - until) as f64 / length as f64).clamp(0.0, 1.0);
    let delta = used - elapsed * 100.0;
    pace.elapsed_percent = Some(elapsed * 100.0);
    pace.pace_delta = Some(delta);
    pace.verdict = if delta > TOLERANCE {
        Verdict::OverPace
    } else if delta < -TOLERANCE {
        Verdict::UnderPace
    } else {
        Verdict::OnPace
    };
    if elapsed >= MIN_ELAPSED_FOR_FORECAST {
        let projected = used / elapsed;
        pace.projected_percent_at_reset = Some(projected);
        if projected > 100.0 && used > 0.0 {
            let elapsed_seconds = elapsed * length as f64;
            let seconds_to_full = (100.0 - used) * elapsed_seconds / used;
            pace.exhausts_at = Some(now + seconds_to_full.floor() as i64);
        }
    }
    if until > MIN_SECONDS_FOR_SAFE_RATE {
        pace.safe_percent_per_hour = Some(remaining / (until as f64 / 3600.0));
    }
    pace
}

/// A window with its pace.
#[derive(Clone, Debug, PartialEq)]
pub struct Assessed {
    pub window: Window,
    pub pace: Pace,
}

/// A pool or model group, and whether anything in it can be used now.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub name: String,
    pub blocked: bool,
    /// When the last exhausted window that blocks it resets, if known.
    pub blocked_until: Option<i64>,
    /// The index in the assessed windows of the one with the least
    /// headroom, counting windows of scope `all`, which limit every scope.
    pub tightest: Option<usize>,
}

pub const ALL: &str = "all";

/// Paces every window, then rolls them up by scope. A window whose scope
/// is blocked by an exhausted window, or whose harness is blocked by an
/// exhausted `all` window, is `Blocked` and has no pace of its own.
pub fn assess(windows: &[Window], now: i64) -> (Vec<Assessed>, Vec<Scope>) {
    let mut assessed: Vec<Assessed> = windows
        .iter()
        .map(|w| Assessed {
            window: w.at(now),
            pace: pace(w, now),
        })
        .collect();
    let mut names: Vec<&str> = Vec::new();
    for item in &assessed {
        if !names.contains(&item.window.scope.as_str()) {
            names.push(&item.window.scope);
        }
    }
    let applies =
        |scope: &str, item: &Assessed| item.window.scope == scope || item.window.scope == ALL;
    let mut scopes: Vec<Scope> = names
        .iter()
        .map(|name| {
            let exhausted: Vec<&Assessed> = assessed
                .iter()
                .filter(|item| applies(name, item) && item.pace.verdict == Verdict::Exhausted)
                .collect();
            let blocked_until = exhausted
                .iter()
                .map(|item| item.window.resets_at)
                .collect::<Option<Vec<i64>>>()
                .and_then(|resets| resets.into_iter().max());
            Scope {
                name: (*name).to_owned(),
                blocked: !exhausted.is_empty(),
                blocked_until,
                tightest: None,
            }
        })
        .collect();
    for item in &mut assessed {
        let blocked = scopes
            .iter()
            .any(|scope| scope.blocked && scope.name == item.window.scope);
        let pacing = !matches!(
            item.pace.verdict,
            Verdict::Exhausted | Verdict::Disabled | Verdict::NotStarted
        );
        if blocked && pacing {
            item.pace = Pace {
                verdict: Verdict::Blocked,
                elapsed_percent: None,
                pace_delta: None,
                projected_percent_at_reset: None,
                exhausts_at: None,
                safe_percent_per_hour: None,
                ..item.pace.clone()
            };
        }
    }
    for scope in &mut scopes {
        scope.tightest = assessed
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                applies(&scope.name, item) && item.window.state == WindowState::Running
            })
            .min_by(|(_, a), (_, b)| {
                a.pace
                    .remaining_percent
                    .total_cmp(&b.pace.remaining_percent)
            })
            .map(|(index, _)| index);
    }
    (assessed, scopes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowKind;

    const NOW: i64 = 1_800_000_000;
    const HOUR: i64 = 3600;
    const WEEK_MINUTES: u32 = 10_080;

    fn window(scope: &str, used: f64, until: i64, minutes: u32) -> Window {
        Window {
            id: format!("{scope}:{minutes}"),
            label: String::new(),
            scope: scope.to_owned(),
            kind: WindowKind::from_minutes(minutes),
            state: WindowState::Running,
            used_percent: used,
            resets_at: Some(NOW + until),
            window_minutes: Some(minutes),
        }
    }

    /// A five-hour window with `elapsed` hours gone.
    fn five_hour(used: f64, elapsed: f64) -> Window {
        window("all", used, ((5.0 - elapsed) * HOUR as f64) as i64, 300)
    }

    #[test]
    fn elapsed_time_is_clamped_to_the_window() {
        let early = window("all", 10.0, 6 * HOUR, 300);
        assert_eq!(pace(&early, NOW).elapsed_percent, Some(0.0));
    }

    #[test]
    fn the_verdict_follows_use_against_elapsed_time() {
        assert_eq!(pace(&five_hour(50.0, 2.5), NOW).verdict, Verdict::OnPace);
        assert_eq!(pace(&five_hour(54.0, 2.5), NOW).verdict, Verdict::OnPace);
        assert_eq!(pace(&five_hour(56.0, 2.5), NOW).verdict, Verdict::OverPace);
        assert_eq!(pace(&five_hour(44.0, 2.5), NOW).verdict, Verdict::UnderPace);
        assert_eq!(pace(&five_hour(56.0, 2.5), NOW).pace_delta, Some(6.0));
    }

    #[test]
    fn a_window_with_no_length_or_reset_has_an_unknown_pace() {
        let no_length = Window {
            window_minutes: None,
            ..five_hour(30.0, 1.0)
        };
        let no_reset = Window {
            resets_at: None,
            ..five_hour(30.0, 1.0)
        };
        for w in [no_length, no_reset] {
            let p = pace(&w, NOW);
            assert_eq!(p.verdict, Verdict::Unknown);
            assert_eq!(p.remaining_percent, 70.0);
            assert_eq!(p.pace_delta, None);
        }
    }

    #[test]
    fn a_forecast_waits_for_five_percent_of_the_window() {
        // 30% used in the first ten minutes: a real signal, but too early
        // to divide by.
        let early = pace(&five_hour(30.0, 10.0 / 60.0), NOW);
        assert_eq!(early.verdict, Verdict::OverPace);
        assert_eq!(early.projected_percent_at_reset, None);
        assert_eq!(early.exhausts_at, None);
        let later = pace(&five_hour(30.0, 0.5), NOW);
        assert_eq!(later.projected_percent_at_reset, Some(300.0));
    }

    #[test]
    fn a_projection_past_full_says_when_it_runs_out() {
        // 60% in 2 hours is 30% an hour, so the last 40% lasts 80 minutes.
        let p = pace(&five_hour(60.0, 2.0), NOW);
        assert_eq!(p.projected_percent_at_reset, Some(150.0));
        assert_eq!(p.exhausts_at, Some(NOW + 80 * 60));
        let slow = pace(&five_hour(20.0, 2.0), NOW);
        assert_eq!(slow.projected_percent_at_reset, Some(50.0));
        assert_eq!(slow.exhausts_at, None);
    }

    #[test]
    fn the_safe_rate_spreads_what_is_left_over_the_time_left() {
        let p = pace(&five_hour(40.0, 3.0), NOW);
        assert_eq!(p.safe_percent_per_hour, Some(30.0));
        let last_minutes = pace(&window("all", 40.0, 10 * 60, 300), NOW);
        assert_eq!(last_minutes.safe_percent_per_hour, None);
    }

    #[test]
    fn a_full_window_is_exhausted() {
        let p = pace(&five_hour(100.0, 1.0), NOW);
        assert_eq!(p.verdict, Verdict::Exhausted);
        assert_eq!(p.remaining_percent, 0.0);
        assert_eq!(p.seconds_until_reset, Some(4 * HOUR));
    }

    #[test]
    fn not_started_and_disabled_windows_keep_their_state() {
        let not_started = Window {
            state: WindowState::NotStarted,
            resets_at: None,
            used_percent: 0.0,
            ..five_hour(0.0, 0.0)
        };
        let disabled = Window {
            state: WindowState::Disabled,
            ..five_hour(0.0, 1.0)
        };
        assert_eq!(pace(&not_started, NOW).verdict, Verdict::NotStarted);
        assert_eq!(pace(&disabled, NOW).verdict, Verdict::Disabled);
    }

    #[test]
    fn a_window_whose_reset_passed_has_started_over() {
        let stale = window("all", 100.0, -60, 300);
        assert_eq!(pace(&stale, NOW).verdict, Verdict::NotStarted);
        assert_eq!(pace(&stale, NOW).remaining_percent, 100.0);
    }

    #[test]
    fn an_exhausted_window_blocks_the_others_in_its_scope() {
        let windows = [
            window("gemini", 100.0, 5 * 24 * HOUR, WEEK_MINUTES),
            window("gemini", 10.0, 2 * HOUR, 300),
            window("claude-gpt", 10.0, 2 * HOUR, 300),
        ];
        let (assessed, scopes) = assess(&windows, NOW);
        assert_eq!(assessed[0].pace.verdict, Verdict::Exhausted);
        assert_eq!(assessed[1].pace.verdict, Verdict::Blocked);
        assert_eq!(assessed[1].pace.pace_delta, None);
        assert_eq!(assessed[1].pace.remaining_percent, 90.0);
        assert_eq!(assessed[2].pace.verdict, Verdict::UnderPace);
        assert_eq!(scopes[0].name, "gemini");
        assert!(scopes[0].blocked);
        assert_eq!(scopes[0].blocked_until, Some(NOW + 5 * 24 * HOUR));
        assert!(!scopes[1].blocked);
        assert_eq!(scopes[1].blocked_until, None);
    }

    #[test]
    fn a_blocked_scope_waits_for_its_last_exhausted_window() {
        let windows = [
            window("standard", 100.0, 2 * HOUR, 300),
            window("standard", 100.0, 3 * 24 * HOUR, WEEK_MINUTES),
        ];
        let (_, scopes) = assess(&windows, NOW);
        assert_eq!(scopes[0].blocked_until, Some(NOW + 3 * 24 * HOUR));
    }

    #[test]
    fn an_exhausted_all_window_blocks_every_scope_but_not_the_reverse() {
        let windows = [
            window("all", 100.0, 24 * HOUR, WEEK_MINUTES),
            window("fable", 20.0, 24 * HOUR, WEEK_MINUTES),
        ];
        let (assessed, scopes) = assess(&windows, NOW);
        assert!(scopes.iter().all(|s| s.blocked));
        assert_eq!(assessed[1].pace.verdict, Verdict::Blocked);

        let windows = [
            window("all", 20.0, 24 * HOUR, WEEK_MINUTES),
            window("fable", 100.0, 24 * HOUR, WEEK_MINUTES),
        ];
        let (assessed, scopes) = assess(&windows, NOW);
        assert!(!scopes[0].blocked);
        assert!(scopes[1].blocked);
        assert_ne!(assessed[0].pace.verdict, Verdict::Blocked);
    }

    #[test]
    fn the_tightest_window_counts_all_scope_windows() {
        let windows = [
            window("all", 70.0, 24 * HOUR, WEEK_MINUTES),
            window("fable", 40.0, 24 * HOUR, WEEK_MINUTES),
            Window {
                state: WindowState::Disabled,
                ..window("fable", 95.0, 24 * HOUR, 300)
            },
        ];
        let (_, scopes) = assess(&windows, NOW);
        assert_eq!(scopes[0].tightest, Some(0));
        assert_eq!(scopes[1].tightest, Some(0));
    }
}
