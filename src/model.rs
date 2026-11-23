//! What to11y knows about a harness's subscription limits, independent of
//! the provider it came from.

use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Droid,
    Antigravity,
}

impl Harness {
    pub const ALL: [Harness; 4] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Droid,
        Harness::Antigravity,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Droid => "droid",
            Harness::Antigravity => "antigravity",
        }
    }

    pub fn parse(name: &str) -> Option<Harness> {
        Harness::ALL.into_iter().find(|h| h.name() == name)
    }

    /// The company whose subscription the harness uses.
    pub fn provider(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Anthropic",
            Harness::Codex => "OpenAI",
            Harness::Droid => "Factory",
            Harness::Antigravity => "Google",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowKind {
    FiveHour,
    Weekly,
    Monthly,
    Other,
}

impl WindowKind {
    pub fn from_minutes(minutes: u32) -> Self {
        match minutes {
            300 => WindowKind::FiveHour,
            10_080 => WindowKind::Weekly,
            40_320..=44_640 => WindowKind::Monthly,
            _ => WindowKind::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            WindowKind::FiveHour => "5h",
            WindowKind::Weekly => "weekly",
            WindowKind::Monthly => "monthly",
            WindowKind::Other => "other",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            WindowKind::FiveHour,
            WindowKind::Weekly,
            WindowKind::Monthly,
            WindowKind::Other,
        ]
        .into_iter()
        .find(|kind| kind.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowState {
    /// Counting toward its reset.
    Running,
    /// A rolling window with nothing used in it yet, so no reset time.
    NotStarted,
    /// The provider says it doesn't apply right now.
    Disabled,
}

impl WindowState {
    pub fn name(self) -> &'static str {
        match self {
            WindowState::Running => "running",
            WindowState::NotStarted => "not_started",
            WindowState::Disabled => "disabled",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            WindowState::Running,
            WindowState::NotStarted,
            WindowState::Disabled,
        ]
        .into_iter()
        .find(|state| state.name() == name)
    }
}

/// One usage limit: a share of a quota that resets on a schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// Stable within a harness, as in `weekly`, `weekly:fable`, `standard:5h`.
    pub id: String,
    pub label: String,
    /// The pool or model group the window limits. `all` when it limits
    /// everything the harness does.
    pub scope: String,
    pub kind: WindowKind,
    pub state: WindowState,
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    pub window_minutes: Option<u32>,
}

impl Window {
    /// The window as it stands at `now`: one whose reset has passed has
    /// started over.
    pub fn at(&self, now: i64) -> Window {
        match self.resets_at {
            Some(reset) if reset <= now => Window {
                state: WindowState::NotStarted,
                used_percent: 0.0,
                resets_at: None,
                ..self.clone()
            },
            _ => self.clone(),
        }
    }
}

/// An amount in a currency's major unit, or in provider credits.
#[derive(Clone, Debug, PartialEq)]
pub struct Money {
    pub amount: f64,
    /// An ISO 4217 code, or `credits`.
    pub currency: String,
}

/// Paid usage beyond the plan's limits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Credits {
    pub enabled: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<Money>,
    pub used: Option<Money>,
    pub limit: Option<Money>,
    /// A provider's reason or setting, as in `out_of_credits`.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub harness: Harness,
    pub plan: Option<String>,
    pub tier: Option<String>,
    pub windows: Vec<Window>,
    pub credits: Option<Credits>,
    /// Unix seconds.
    pub fetched_at: i64,
}

/// Why a harness has no current reading.
#[derive(Clone, Debug, PartialEq)]
pub enum Unavailable {
    NotInstalled,
    NotSignedIn,
    SignInExpired,
    KeyringLocked,
    /// No Secret Service, or no session bus to reach one.
    KeyringMissing,
    Unsupported(String),
    /// A request or parse failed. The text is a kind, never a response body.
    Failed(String),
}

impl Unavailable {
    pub fn kind(&self) -> &str {
        match self {
            Unavailable::NotInstalled => "not_installed",
            Unavailable::NotSignedIn => "not_signed_in",
            Unavailable::SignInExpired => "sign_in_expired",
            Unavailable::KeyringLocked => "keyring_locked",
            Unavailable::KeyringMissing => "keyring_missing",
            Unavailable::Unsupported(_) => "unsupported",
            Unavailable::Failed(_) => "failed",
        }
    }

    /// One sentence an agent can pass on to the user.
    pub fn message(&self, harness: Harness) -> String {
        let name = harness.name();
        match self {
            Unavailable::NotInstalled => format!("{name} isn't set up on this machine."),
            Unavailable::NotSignedIn => format!("{name} has no stored sign-in."),
            Unavailable::SignInExpired => format!(
                "{name}'s stored sign-in has expired. Using {name} renews it; to11y never refreshes logins."
            ),
            Unavailable::KeyringLocked => {
                "The system keyring is locked. Unlock it, then ask again.".to_owned()
            }
            Unavailable::KeyringMissing => {
                "No Secret Service is running, so the keyring can't be read.".to_owned()
            }
            Unavailable::Unsupported(why) | Unavailable::Failed(why) => why.clone(),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Unavailable::Unsupported(why) | Unavailable::Failed(why) => {
                json!({ "kind": self.kind(), "detail": why })
            }
            _ => json!({ "kind": self.kind() }),
        }
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let detail = || value["detail"].as_str().unwrap_or_default().to_owned();
        Some(match value["kind"].as_str()? {
            "not_installed" => Unavailable::NotInstalled,
            "not_signed_in" => Unavailable::NotSignedIn,
            "sign_in_expired" => Unavailable::SignInExpired,
            "keyring_locked" => Unavailable::KeyringLocked,
            "keyring_missing" => Unavailable::KeyringMissing,
            "unsupported" => Unavailable::Unsupported(detail()),
            "failed" => Unavailable::Failed(detail()),
            _ => return None,
        })
    }
}

fn money_json(money: &Option<Money>) -> Value {
    money.as_ref().map_or(
        Value::Null,
        |m| json!({ "amount": m.amount, "currency": m.currency }),
    )
}

fn money_from(value: &Value) -> Option<Money> {
    Some(Money {
        amount: value["amount"].as_f64()?,
        currency: value["currency"].as_str()?.to_owned(),
    })
}

fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !value.is_null() {
        map.insert(key.to_owned(), value);
    }
}

impl Credits {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        put(&mut map, "enabled", json!(self.enabled));
        put(&mut map, "unlimited", json!(self.unlimited));
        put(&mut map, "balance", money_json(&self.balance));
        put(&mut map, "used", money_json(&self.used));
        put(&mut map, "limit", money_json(&self.limit));
        put(&mut map, "note", json!(self.note));
        Value::Object(map)
    }

    fn from_json(value: &Value) -> Self {
        Credits {
            enabled: value["enabled"].as_bool(),
            unlimited: value["unlimited"].as_bool(),
            balance: money_from(&value["balance"]),
            used: money_from(&value["used"]),
            limit: money_from(&value["limit"]),
            note: value["note"].as_str().map(str::to_owned),
        }
    }
}

impl Snapshot {
    /// The form kept in the cache file. It holds no account identifiers.
    pub fn to_json(&self) -> Value {
        let windows: Vec<Value> = self
            .windows
            .iter()
            .map(|w| {
                json!({
                    "id": w.id,
                    "label": w.label,
                    "scope": w.scope,
                    "kind": w.kind.name(),
                    "state": w.state.name(),
                    "used_percent": w.used_percent,
                    "resets_at": w.resets_at,
                    "window_minutes": w.window_minutes,
                })
            })
            .collect();
        json!({
            "harness": self.harness.name(),
            "plan": self.plan,
            "tier": self.tier,
            "windows": windows,
            "credits": self.credits.as_ref().map(Credits::to_json),
            "fetched_at": self.fetched_at,
        })
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let windows = value["windows"]
            .as_array()?
            .iter()
            .map(|w| {
                Some(Window {
                    id: w["id"].as_str()?.to_owned(),
                    label: w["label"].as_str()?.to_owned(),
                    scope: w["scope"].as_str()?.to_owned(),
                    kind: WindowKind::parse(w["kind"].as_str()?)?,
                    state: WindowState::parse(w["state"].as_str()?)?,
                    used_percent: w["used_percent"].as_f64()?,
                    resets_at: w["resets_at"].as_i64(),
                    window_minutes: w["window_minutes"]
                        .as_u64()
                        .and_then(|m| u32::try_from(m).ok()),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Snapshot {
            harness: Harness::parse(value["harness"].as_str()?)?,
            plan: value["plan"].as_str().map(str::to_owned),
            tier: value["tier"].as_str().map(str::to_owned),
            windows,
            credits: value
                .get("credits")
                .filter(|c| c.is_object())
                .map(Credits::from_json),
            fetched_at: value["fetched_at"].as_i64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, used: f64, resets_at: Option<i64>) -> Window {
        Window {
            id: id.to_owned(),
            label: id.to_owned(),
            scope: "all".to_owned(),
            kind: WindowKind::Weekly,
            state: WindowState::Running,
            used_percent: used,
            resets_at,
            window_minutes: Some(10_080),
        }
    }

    #[test]
    fn a_window_past_its_reset_reads_as_not_started() {
        let w = window("weekly", 80.0, Some(1_000)).at(1_000);
        assert_eq!(w.state, WindowState::NotStarted);
        assert_eq!(w.used_percent, 0.0);
        assert_eq!(w.resets_at, None);
        assert_eq!(
            window("weekly", 80.0, Some(1_001)).at(1_000).used_percent,
            80.0
        );
    }

    #[test]
    fn window_kinds_come_from_their_length() {
        assert_eq!(WindowKind::from_minutes(300), WindowKind::FiveHour);
        assert_eq!(WindowKind::from_minutes(10_080), WindowKind::Weekly);
        assert_eq!(WindowKind::from_minutes(43_200), WindowKind::Monthly);
        assert_eq!(WindowKind::from_minutes(60), WindowKind::Other);
    }

    #[test]
    fn a_snapshot_survives_the_cache_format() {
        let snapshot = Snapshot {
            harness: Harness::Droid,
            plan: Some("max".to_owned()),
            tier: None,
            windows: vec![
                window("standard:weekly", 100.0, Some(1_800_000_000)),
                Window {
                    state: WindowState::NotStarted,
                    resets_at: None,
                    ..window("standard:5h", 0.0, None)
                },
            ],
            credits: Some(Credits {
                enabled: Some(true),
                balance: Some(Money {
                    amount: 12.5,
                    currency: "USD".to_owned(),
                }),
                note: Some("droidCore".to_owned()),
                ..Credits::default()
            }),
            fetched_at: 1_799_999_000,
        };
        let text = snapshot.to_json().to_string();
        let back = Snapshot::from_json(&serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(back, snapshot);
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::parse(harness.name()), Some(harness));
        }
        assert_eq!(Harness::parse("claude"), None);
    }
}
