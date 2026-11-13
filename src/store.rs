//! What bytt keeps on disk, in `~/.bytt`.
//!
//! - `accounts.json`: the accounts and who they are. Their logins are in the Keychain.
//! - `state.json`: the last usage readings and when to try again. Safe to delete.
//! - `config.toml`: hand-edited preferences.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::api::FetchError;
use crate::claude::OauthAccount;
use crate::fsx::write_atomic;
use crate::lock::FileLock;
use crate::usage::{Policy, Reading};

/// An account is a person in an organization: the same email can hold a personal
/// plan and a seat on a team, and those are separate logins with separate limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountId {
    pub account_uuid: String,
    pub org_uuid: String,
}

impl AccountId {
    pub fn from_oauth_account(account: &OauthAccount) -> Option<Self> {
        let field = |key: &str| {
            account
                .get(key)?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            account_uuid: field("accountUuid")?,
            org_uuid: field("organizationUuid")?,
        })
    }

    /// The name of this account's Keychain item and of its entry in the state file.
    pub fn key(&self) -> String {
        format!("{}:{}", self.account_uuid, self.org_uuid)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    #[serde(flatten)]
    pub id: AccountId,
    /// Held out of automatic switching when false.
    pub enabled: bool,
    pub added: DateTime<Utc>,
    /// The `oauthAccount` section Claude Code wrote for this login, restored
    /// verbatim when the account becomes active.
    pub oauth_account: OauthAccount,
}

impl Account {
    fn field(&self, key: &str) -> Option<&str> {
        self.oauth_account
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
    }

    pub fn email(&self) -> &str {
        self.field("emailAddress").unwrap_or("unknown")
    }

    pub fn organization(&self) -> Option<&str> {
        self.field("organizationName")
    }

    pub fn plan(&self) -> Option<&str> {
        self.field("planDisplayName")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub accounts: Vec<Account>,
}

impl Registry {
    pub fn find(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn find_mut(&mut self, id: &AccountId) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| &a.id == id)
    }

    /// Find an account by its number or by (part of) its email.
    pub fn resolve(&self, target: &str) -> Result<&Account> {
        if let Ok(number) = target.parse::<usize>() {
            return number
                .checked_sub(1)
                .and_then(|i| self.accounts.get(i))
                .with_context(|| format!("there is no account {number}"));
        }
        let needle = target.to_lowercase();
        let by_email = |exact: bool| -> Vec<&Account> {
            self.accounts
                .iter()
                .filter(|a| {
                    let email = a.email().to_lowercase();
                    if exact {
                        email == needle
                    } else {
                        email.contains(&needle)
                    }
                })
                .collect()
        };
        let exact = by_email(true);
        let matches = if exact.is_empty() {
            by_email(false)
        } else {
            exact
        };
        match matches.as_slice() {
            [account] => Ok(account),
            [] => bail!("no account matches \"{target}\""),
            many => bail!(
                "\"{target}\" matches {} accounts; use a number from `bytt list` instead",
                many.len()
            ),
        }
    }
}

/// What bytt last learned about one account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    pub reading: Option<Reading>,
    pub fetched_at: Option<DateTime<Utc>>,
    /// Do not ask again before this time (backoff after a failure).
    pub retry_at: Option<DateTime<Utc>>,
    pub failures: u32,
    /// The stored login can no longer be renewed; cleared by `bytt add`.
    pub needs_login: bool,
    /// The stored access token was refused although not yet due; renew it next time.
    pub force_refresh: bool,
    /// Why the latest attempt produced no reading, for `bytt list`.
    pub note: Option<String>,
}

impl AccountState {
    /// Whether it is time to take a new reading, given how old one may get.
    pub fn due(&self, ttl: TimeDelta, now: DateTime<Utc>) -> bool {
        if self.retry_at.is_some_and(|at| at > now) {
            return false;
        }
        match (&self.reading, self.fetched_at) {
            (Some(reading), Some(at)) => now - at >= ttl || reading.has_reset_since(at, now),
            _ => true,
        }
    }

    /// The last reading, unless it is older than `max_age`.
    pub fn fresh_reading(&self, max_age: TimeDelta, now: DateTime<Utc>) -> Option<&Reading> {
        self.reading
            .as_ref()
            .filter(|_| self.fetched_at.is_some_and(|at| now - at <= max_age))
    }

    pub fn record(&mut self, result: Result<Reading, FetchError>, now: DateTime<Utc>) {
        match result {
            Ok(reading) => {
                *self = Self {
                    reading: Some(reading),
                    fetched_at: Some(now),
                    needs_login: self.needs_login,
                    ..Self::default()
                };
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                let doublings = std::cmp::min(self.failures - 1, 6);
                let backoff =
                    |base: i64, cap: i64| TimeDelta::seconds(std::cmp::min(base << doublings, cap));
                let wait = match &error {
                    FetchError::RateLimited {
                        retry_after: Some(after),
                    } => {
                        let seconds = i64::try_from(after.as_secs()).unwrap_or(3600);
                        TimeDelta::seconds(seconds.clamp(60, 3600))
                    }
                    FetchError::RateLimited { retry_after: None } => backoff(300, 1800),
                    FetchError::Unauthorized | FetchError::Other(_) => backoff(60, 900),
                };
                self.force_refresh = error == FetchError::Unauthorized;
                self.retry_at = Some(now + wait);
                self.note = Some(error.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_switch: Option<DateTime<Utc>>,
    /// Keyed by [`AccountId::key`].
    pub accounts: BTreeMap<String, AccountState>,
}

/// Hand-edited preferences, `~/.bytt/config.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Whether the menu bar app switches automatically.
    pub auto: bool,
    pub threshold: f64,
    pub weekly_threshold: Option<f64>,
    pub models: Vec<String>,
    pub margin: f64,
    pub interval: u64,
    pub cooldown: u64,
    pub notify: bool,
    pub menubar: MenuBar,
}

/// What the menu bar app shows in the menu bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuBar {
    /// The active account's usage, as a number.
    #[default]
    Usage,
    /// An icon; usage next to it only when the active account nears its switch point.
    Icon,
    /// Nothing. Opening the app again shows the icon.
    Hidden,
}

impl MenuBar {
    pub const ALL: [Self; 3] = [Self::Usage, Self::Icon, Self::Hidden];

    pub fn key(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Icon => "icon",
            Self::Hidden => "hidden",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Icon => "Icon, usage when near",
            Self::Hidden => "Hidden",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto: false,
            threshold: 90.0,
            weekly_threshold: None,
            models: Vec::new(),
            margin: 10.0,
            interval: 600,
            cooldown: 300,
            notify: true,
            menubar: MenuBar::Usage,
        }
    }
}

impl Config {
    /// The file as bytt writes it: every setting, each with what it does.
    pub fn render(&self) -> String {
        let weekly = match self.weekly_threshold {
            Some(weekly) => format!("weekly_threshold = {weekly}"),
            None => "# weekly_threshold = 95".into(),
        };
        let models = self
            .models
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"# bytt configuration. `bytt auto` re-reads this file on every check.
# Changing a setting in `bytt` rewrites this file.

# Switch automatically while the bytt menu bar app runs.
auto = {auto}

# Leave an account when its 5-hour window reaches this percentage.
threshold = {threshold}

# Leave an account when its 7-day window, or a watched model's, reaches this
# percentage. Uses `threshold` when unset.
{weekly}

# Also watch these models' own weekly limits, by the name `bytt` shows,
# e.g. ["Fable"].
models = [{models}]

# An account is only switched to if it has this many points left under its lines.
margin = {margin}

# Seconds between checks while `bytt auto` runs.
interval = {interval}

# Seconds to stay on an account after switching to it, unless it runs out completely.
cooldown = {cooldown}

# Show a macOS notification when bytt switches account.
notify = {notify}

# The menu bar: "usage" (the active account's usage), "icon" (an icon, with usage
# when near the switch point) or "hidden" (open Bytt again to show it).
menubar = "{menubar}"
"#,
            menubar = self.menubar.key(),
            auto = self.auto,
            threshold = self.threshold,
            margin = self.margin,
            interval = self.interval,
            cooldown = self.cooldown,
            notify = self.notify,
        )
    }

    fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        let percent = |name: &str, value: f64| {
            if (1.0..=100.0).contains(&value) {
                Ok(())
            } else {
                bail!("{name} must be between 1 and 100, not {value}")
            }
        };
        percent("threshold", config.threshold)?;
        if let Some(weekly) = config.weekly_threshold {
            percent("weekly_threshold", weekly)?;
        }
        if !(0.0..=50.0).contains(&config.margin) {
            bail!("margin must be between 0 and 50, not {}", config.margin);
        }
        if config.interval < 30 {
            bail!(
                "interval must be at least 30 seconds, not {}",
                config.interval
            );
        }
        Ok(config)
    }

    pub fn policy(&self) -> Policy {
        Policy {
            threshold: self.threshold,
            weekly_threshold: self.weekly_threshold.unwrap_or(self.threshold),
            margin: self.margin,
            models: self.models.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

/// Proof that this process holds bytt's lock. Everything that changes accounts,
/// stored logins or state goes through it.
pub struct Locked<'a> {
    store: &'a Store,
    _lock: FileLock,
}

impl Store {
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// Read the config, writing a commented default the first time.
    pub fn config(&self) -> Result<Config> {
        let path = self.config_path();
        match fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let config = Config::default();
                write_atomic(&path, config.render().as_bytes())?;
                Ok(config)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        write_atomic(&self.config_path(), config.render().as_bytes())
    }

    pub fn lock(&self) -> Result<Locked<'_>> {
        Ok(Locked {
            store: self,
            _lock: FileLock::acquire(&self.dir.join("lock"))?,
        })
    }

    pub fn registry(&self) -> Result<Registry> {
        let path = self.dir.join("accounts.json");
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The state file is a cache: one that is missing or unreadable starts over.
    pub fn state(&self) -> State {
        fs::read_to_string(self.dir.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

impl Locked<'_> {
    pub fn registry(&self) -> Result<Registry> {
        self.store.registry()
    }

    pub fn state(&self) -> State {
        self.store.state()
    }

    pub fn save_registry(&self, registry: &Registry) -> Result<()> {
        self.save(
            "accounts.json",
            serde_json::to_string_pretty(registry)?.as_bytes(),
        )
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        self.save(
            "state.json",
            serde_json::to_string_pretty(state)?.as_bytes(),
        )
    }

    fn save(&self, name: &str, data: &[u8]) -> Result<()> {
        write_atomic(&Path::new(&self.store.dir).join(name), data)
    }
}

#[cfg(test)]
pub mod testing {
    use super::{Account, AccountId};
    use chrono::Utc;
    use serde_json::{Value, json};

    pub fn account(name: &str) -> Account {
        let Value::Object(oauth_account) = json!({
            "accountUuid": format!("uuid-{name}"),
            "organizationUuid": format!("org-{name}"),
            "emailAddress": format!("{name}@example.com"),
            "organizationName": format!("{name} org"),
        }) else {
            unreachable!()
        };
        Account {
            id: AccountId::from_oauth_account(&oauth_account).unwrap(),
            enabled: true,
            added: Utc::now(),
            oauth_account,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::account;
    use super::*;
    use crate::usage::Window;
    use std::time::Duration;

    #[test]
    fn resolves_by_number_and_email() {
        let registry = Registry {
            accounts: vec![account("anna"), account("annabel")],
        };
        assert_eq!(
            registry.resolve("2").unwrap().email(),
            "annabel@example.com"
        );
        assert_eq!(
            registry.resolve("anna@example.com").unwrap().email(),
            "anna@example.com"
        );
        assert_eq!(
            registry.resolve("ANNAB").unwrap().email(),
            "annabel@example.com"
        );
        assert!(registry.resolve("anna").is_err());
        assert!(registry.resolve("0").is_err());
        assert!(registry.resolve("3").is_err());
        assert!(registry.resolve("bob").is_err());
    }

    #[test]
    fn config_defaults_and_validation() {
        assert_eq!(
            Config::parse(&Config::default().render()).unwrap(),
            Config::default()
        );
        let edited = Config {
            auto: true,
            threshold: 85.0,
            weekly_threshold: Some(95.0),
            models: vec!["Fable".into()],
            margin: 5.0,
            interval: 60,
            cooldown: 0,
            notify: false,
            menubar: MenuBar::Hidden,
        };
        assert_eq!(Config::parse(&edited.render()).unwrap(), edited);
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::parse("weekly_threshold = 97")
                .unwrap()
                .policy()
                .weekly_threshold,
            97.0
        );
        assert_eq!(Config::default().policy().weekly_threshold, 90.0);
        assert!(Config::parse("threshold = 120").is_err());
        assert!(Config::parse("interval = 5").is_err());
        assert!(Config::parse("treshold = 80").is_err());
    }

    #[test]
    fn readings_go_stale_by_age_and_by_reset() {
        let now = Utc::now();
        let mut state = AccountState::default();
        assert!(state.due(TimeDelta::minutes(5), now));

        let reset = now + TimeDelta::minutes(2);
        let reading = Reading {
            five_hour: Some(Window {
                pct: 50.0,
                resets_at: Some(reset),
            }),
            ..Reading::default()
        };
        state.record(Ok(reading), now);
        assert!(!state.due(TimeDelta::minutes(5), now + TimeDelta::minutes(1)));
        assert!(state.due(TimeDelta::minutes(5), now + TimeDelta::minutes(3)));
        assert!(state.due(TimeDelta::minutes(5), now + TimeDelta::minutes(6)));
        assert!(
            state
                .fresh_reading(TimeDelta::minutes(5), now + TimeDelta::minutes(3))
                .is_some()
        );
        assert!(
            state
                .fresh_reading(TimeDelta::minutes(5), now + TimeDelta::minutes(6))
                .is_none()
        );
    }

    #[test]
    fn failures_back_off_and_keep_the_last_reading() {
        let now = Utc::now();
        let mut state = AccountState::default();
        state.record(Ok(Reading::default()), now);
        state.record(Err(FetchError::Other("http 500".into())), now);
        assert_eq!(state.retry_at, Some(now + TimeDelta::seconds(60)));
        state.record(Err(FetchError::Other("http 500".into())), now);
        assert_eq!(state.retry_at, Some(now + TimeDelta::seconds(120)));
        assert!(state.reading.is_some());
        assert!(!state.due(TimeDelta::zero(), now));

        let limited = FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
        };
        state.record(Err(limited), now);
        assert_eq!(state.retry_at, Some(now + TimeDelta::seconds(60)));
        state.record(Err(FetchError::Unauthorized), now);
        assert!(state.force_refresh);

        state.record(Ok(Reading::default()), now);
        assert_eq!(
            (state.failures, state.retry_at, state.force_refresh),
            (0, None, false)
        );
    }

    #[test]
    fn registry_and_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bytt")).unwrap();
        assert_eq!(store.registry().unwrap(), Registry::default());
        assert_eq!(store.config().unwrap(), Config::default());
        assert!(store.config_path().exists());

        let registry = Registry {
            accounts: vec![account("anna")],
        };
        let mut state = State::default();
        state
            .accounts
            .entry(registry.accounts[0].id.key())
            .or_default()
            .needs_login = true;
        let lock = store.lock().unwrap();
        lock.save_registry(&registry).unwrap();
        lock.save_state(&state).unwrap();
        assert_eq!(store.registry().unwrap(), registry);
        assert_eq!(store.state(), state);
    }
}
