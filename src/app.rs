//! The operations: save the current login, read usage, swap.
//!
//! Three rules hold throughout.
//!
//! - A login is copied into an account's slot only when it is known to belong to
//!   that account. Storing someone else's login there would destroy the slot's own.
//! - Renewing a stored login spends its refresh token, so it happens under bytt's
//!   lock and the successor is stored before anything else.
//! - The login Claude Code is using is never renewed by bytt. Claude Code does that
//!   itself, and bytt picks the result up from the Keychain.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};

use crate::api::{Api, Identity, RefreshError};
use crate::claude::{self, Claude};
use crate::credential::Credential;
use crate::keychain::Secrets;
use crate::store::{Account, AccountId, AccountState, Config, Locked, Registry, State, Store};
use crate::usage::{Decision, Reading};

/// The Keychain service bytt keeps each account's login under.
pub const SLOTS: &str = "bytt";

/// A candidate's reading may be this old and still count.
const CANDIDATE_MAX_AGE: TimeDelta = TimeDelta::minutes(30);
/// The active account's reading may be this old and still count.
const ACTIVE_MAX_AGE: TimeDelta = TimeDelta::minutes(15);
/// Within this many points of a line, the other accounts are read more often.
const CLOSE_TO_LINE: f64 = 15.0;

pub struct App {
    pub store: Store,
    pub claude: Claude,
    pub secrets: Box<dyn Secrets>,
    pub api: Box<dyn Api>,
    pub config: Config,
}

/// Who Claude Code is logged in as, from bytt's point of view.
#[derive(Debug, Clone, PartialEq)]
pub enum Live {
    LoggedOut,
    /// A bytt account, and its slot holds the current login.
    Managed(AccountId),
    /// Claude Code's config names this account, but the login could not be
    /// confirmed as theirs, so it has not been saved.
    Unverified(AccountId),
    /// A login bytt has no account for.
    Unmanaged,
}

impl Live {
    pub fn account(&self) -> Option<&AccountId> {
        match self {
            Self::Managed(id) | Self::Unverified(id) => Some(id),
            Self::LoggedOut | Self::Unmanaged => None,
        }
    }
}

/// How fresh readings need to be for a [`App::collect`].
pub struct Scope {
    pub active_ttl: TimeDelta,
    pub others_ttl: TimeDelta,
    /// Read disabled accounts too.
    pub disabled: bool,
}

/// Everything known after a collect.
pub struct Snapshot {
    pub registry: Registry,
    pub state: State,
    pub live: Live,
    pub now: DateTime<Utc>,
}

impl Snapshot {
    pub fn state_of(&self, id: &AccountId) -> Option<&AccountState> {
        self.state.accounts.get(&id.key())
    }
}

#[derive(Debug, PartialEq)]
pub enum Added {
    New(Account),
    Updated(Account),
}

#[derive(Debug, PartialEq)]
pub enum Switched {
    AlreadyActive,
    Done,
}

/// What one automatic check came to.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Held(String),
    Switched { to: String, why: String },
    WouldSwitch { to: String, why: String },
}

/// A switch was refused because it would throw away a login bytt has not saved.
/// Passing `force` accepts that.
#[derive(Debug)]
pub struct WouldDiscard(pub String);

impl std::fmt::Display for WouldDiscard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WouldDiscard {}

impl Identity {
    fn is(&self, id: &AccountId) -> bool {
        self.account_uuid == id.account_uuid
            && self.org_uuid.as_ref().is_none_or(|org| *org == id.org_uuid)
    }
}

impl App {
    fn live_raw(&self) -> Result<Option<String>> {
        self.secrets.get(claude::SERVICE, &self.claude.user)
    }

    fn slot(&self, id: &AccountId) -> Result<Option<Credential>> {
        let raw = self.secrets.get(SLOTS, &id.key())?;
        Ok(raw.and_then(|raw| Credential::parse(&raw).ok()))
    }

    /// Losing a renewed login means logging in again, so a failed write is retried.
    fn save_slot(&self, id: &AccountId, raw: &str) -> Result<()> {
        let mut attempt = 0;
        loop {
            match self.secrets.set(SLOTS, &id.key(), raw) {
                Ok(()) => return Ok(()),
                Err(e) if attempt == 2 => return Err(e.context("saving a login to the Keychain")),
                Err(_) => attempt += 1,
            }
        }
    }

    /// Work out who is logged in, and bring that account's slot up to date.
    ///
    /// Claude Code renews its login as it runs. A renewal that keeps the refresh
    /// token is plainly the same login. One that replaces it could also be a new
    /// `/login` as someone else, so Anthropic is asked who the token belongs to
    /// before it is stored.
    fn capture_live(
        &self,
        lock: &Locked,
        registry: &mut Registry,
    ) -> Result<(Live, Option<String>)> {
        let raw = self.live_raw()?;
        let Some(live) = raw.as_deref().and_then(|raw| Credential::parse(raw).ok()) else {
            return Ok((Live::LoggedOut, raw));
        };
        let named = self.claude.oauth_account()?.and_then(|oauth_account| {
            Some((
                AccountId::from_oauth_account(&oauth_account)?,
                oauth_account,
            ))
        });
        let Some((id, oauth_account)) = named else {
            return Ok((Live::Unmanaged, raw));
        };
        if registry.find(&id).is_none() {
            return Ok((Live::Unmanaged, raw));
        }

        let slot_raw = self.secrets.get(SLOTS, &id.key())?;
        let slot = slot_raw
            .as_deref()
            .and_then(|raw| Credential::parse(raw).ok());
        let same_login = slot
            .as_ref()
            .and_then(Credential::refresh_token)
            .is_some_and(|token| Some(token) == live.refresh_token());
        if !same_login {
            let token = live
                .access_token()
                .context("live login has no access token")?;
            match self.api.profile(token) {
                Ok(who) if who.is(&id) => {}
                Ok(_) => return Ok((Live::Unmanaged, raw)),
                Err(_) => return Ok((Live::Unverified(id), raw)),
            }
        }
        if slot_raw != raw {
            self.save_slot(&id, raw.as_deref().unwrap_or_default())?;
        }
        if let Some(account) = registry.find_mut(&id)
            && account.oauth_account != oauth_account
        {
            account.oauth_account = oauth_account;
            lock.save_registry(registry)?;
        }
        Ok((Live::Managed(id), raw))
    }

    /// An access token for an account that is not live, renewing the stored login
    /// if it has expired. `None` means no token for now; `state` says why.
    fn slot_token(
        &self,
        _lock: &Locked,
        id: &AccountId,
        state: &mut AccountState,
        now: DateTime<Utc>,
    ) -> Result<Option<String>> {
        if state.needs_login {
            return Ok(None);
        }
        let Some(mut login) = self.slot(id)? else {
            state.needs_login = true;
            return Ok(None);
        };
        if !login.is_expired(now) && !state.force_refresh {
            return Ok(login.access_token().map(str::to_string));
        }
        let Some(refresh_token) = login.refresh_token().map(str::to_string) else {
            state.needs_login = true;
            return Ok(None);
        };
        match self.api.refresh(&refresh_token) {
            Ok(grant) => {
                login.apply(grant, now);
                self.save_slot(id, &login.to_json())?;
                state.force_refresh = false;
                Ok(login.access_token().map(str::to_string))
            }
            Err(RefreshError::Dead) => {
                state.needs_login = true;
                Ok(None)
            }
            Err(RefreshError::Transient(detail)) => {
                state.note = Some(format!("could not renew the login: {detail}"));
                state.retry_at = Some(now + TimeDelta::minutes(1));
                Ok(None)
            }
        }
    }

    /// Save the login Claude Code is using right now as an account.
    pub fn add(&self) -> Result<Added> {
        let lock = self.store.lock()?;
        let mut registry = lock.registry()?;
        let raw = self
            .live_raw()?
            .context("Claude Code is not logged in. Run `claude`, log in, then try again.")?;
        let login = Credential::parse(&raw)
            .context("Claude Code is not logged in with a Claude subscription")?;
        let oauth_account = self.claude.oauth_account()?.context(
            "~/.claude.json does not say who is logged in. Open Claude Code once, then try again.",
        )?;
        let id = AccountId::from_oauth_account(&oauth_account)
            .context("~/.claude.json does not identify the logged-in account")?;

        let token = login
            .access_token()
            .context("live login has no access token")?;
        match self.api.profile(token) {
            Ok(who) if who.is(&id) => {}
            Ok(_) => bail!(
                "Claude Code's login and ~/.claude.json disagree about who is logged in. \
                 Restart Claude Code and try again."
            ),
            Err(crate::api::FetchError::Unauthorized) => bail!(
                "The current login has expired. Send a message in Claude Code so it renews, \
                 then run `bytt add` again."
            ),
            Err(e) => bail!("Could not confirm the login with Anthropic: {e}"),
        }

        self.save_slot(&id, &raw)?;
        let added = if let Some(account) = registry.find_mut(&id) {
            account.oauth_account = oauth_account;
            Added::Updated(account.clone())
        } else {
            let account = Account {
                id: id.clone(),
                enabled: true,
                added: Utc::now(),
                oauth_account,
            };
            registry.accounts.push(account.clone());
            Added::New(account)
        };
        lock.save_registry(&registry)?;

        // A fresh login clears whatever was wrong with the old one; the last usage
        // reading still stands.
        let mut state = lock.state();
        if let Some(entry) = state.accounts.get_mut(&id.key()) {
            *entry = AccountState {
                reading: entry.reading.take(),
                fetched_at: entry.fetched_at,
                ..AccountState::default()
            };
        }
        lock.save_state(&state)?;
        Ok(added)
    }

    pub fn remove(&self, id: &AccountId) -> Result<Account> {
        let lock = self.store.lock()?;
        let mut registry = lock.registry()?;
        let account = registry.find(id).context("no such account")?.clone();
        registry.accounts.retain(|a| a.id != account.id);
        lock.save_registry(&registry)?;
        let mut state = lock.state();
        state.accounts.remove(&account.id.key());
        lock.save_state(&state)?;
        self.secrets.delete(SLOTS, &account.id.key())?;
        Ok(account)
    }

    pub fn set_enabled(&self, id: &AccountId, enabled: bool) -> Result<Account> {
        let lock = self.store.lock()?;
        let mut registry = lock.registry()?;
        let account = registry.find_mut(id).context("no such account")?;
        account.enabled = enabled;
        let account = account.clone();
        lock.save_registry(&registry)?;
        Ok(account)
    }

    /// Read usage for every account whose reading is older than `scope` allows.
    ///
    /// Tokens are gathered under bytt's lock, the requests are made without it, and
    /// the results are merged under it again, so a slow network never blocks
    /// another bytt.
    pub fn collect(&self, scope: &Scope) -> Result<Snapshot> {
        let now = Utc::now();
        let (live, jobs) = {
            let lock = self.store.lock()?;
            let mut registry = lock.registry()?;
            let mut state = lock.state();
            let (live, raw) = self.capture_live(&lock, &mut registry)?;
            let live_login = raw.as_deref().and_then(|raw| Credential::parse(raw).ok());

            let mut jobs = Vec::new();
            for account in &registry.accounts {
                let entry = state.accounts.entry(account.id.key()).or_default();
                let is_live = live.account() == Some(&account.id);
                if matches!(&live, Live::Managed(id) if *id == account.id) {
                    entry.needs_login = false;
                }
                let wanted = is_live || account.enabled || scope.disabled;
                let ttl = if is_live {
                    scope.active_ttl
                } else {
                    scope.others_ttl
                };
                if !wanted || !entry.due(ttl, now) {
                    continue;
                }
                let token = if is_live {
                    match live_login.as_ref().filter(|login| !login.is_expired(now)) {
                        Some(login) => login.access_token().map(str::to_string),
                        None => {
                            entry.note = Some(
                                "idle: Claude Code renews this login on your next message".into(),
                            );
                            None
                        }
                    }
                } else {
                    self.slot_token(&lock, &account.id, entry, now)?
                };
                if let Some(token) = token {
                    jobs.push((account.id.key(), token));
                }
            }
            lock.save_state(&state)?;
            (live, jobs)
        };

        let results: Vec<_> = jobs
            .into_iter()
            .map(|(key, token)| (key, self.api.usage(&token)))
            .collect();

        let lock = self.store.lock()?;
        let registry = lock.registry()?;
        let mut state = lock.state();
        for (key, result) in results {
            state.accounts.entry(key).or_default().record(result, now);
        }
        lock.save_state(&state)?;
        Ok(Snapshot {
            registry,
            state,
            live,
            now,
        })
    }

    /// Make `target` the account Claude Code is logged in as.
    ///
    /// The outgoing login is saved first. If it cannot be attributed to an account
    /// the switch is refused, because overwriting it would lose it; `force` accepts
    /// that loss.
    pub fn switch_to(&self, target: &AccountId, force: bool) -> Result<Switched> {
        let lock = self.store.lock()?;
        let mut registry = lock.registry()?;
        let mut state = lock.state();
        let now = Utc::now();

        let (live, raw) = self.capture_live(&lock, &mut registry)?;
        let account = registry.find(target).context("no such account")?.clone();
        match &live {
            Live::Managed(id) if id == target => return Ok(Switched::AlreadyActive),
            Live::Unmanaged if !force => {
                return Err(WouldDiscard(
                    "Claude Code is logged in as an account bytt has not saved, and switching \
                     would discard that login. Save it first."
                        .into(),
                )
                .into());
            }
            Live::Unverified(id) if !force => {
                return Err(WouldDiscard(format!(
                    "Could not confirm that the current login belongs to {}, so it has not been \
                     saved and switching would discard it. Send a message in Claude Code so it \
                     renews the login and try again.",
                    registry
                        .find(id)
                        .map_or("the active account", Account::email)
                ))
                .into());
            }
            _ => {}
        }

        // Renew the target's login now, outside Claude Code's locks: a dead one is
        // found before anything is changed.
        let entry = state.accounts.entry(target.key()).or_default();
        let token = self.slot_token(&lock, target, entry, now)?;
        let problem = match (&token, entry.needs_login) {
            (Some(_), _) => None,
            (None, true) => Some(format!(
                "{} needs a new login. Log in with it in Claude Code, then run `bytt add`.",
                account.email()
            )),
            (None, false) => Some(format!(
                "{}: {}",
                account.email(),
                entry.note.as_deref().unwrap_or("its login is unavailable")
            )),
        };
        lock.save_state(&state)?;
        if let Some(problem) = problem {
            bail!(problem);
        }
        let login = self
            .slot(target)?
            .context("the account's login disappeared")?;

        {
            let _locks = self.claude.lock_credentials()?;
            let current = self.live_raw()?;
            if current != raw {
                bail!("Claude Code changed its login while bytt was switching. Try again.");
            }
            let next = login.with_shared_from(current.as_deref()).to_json();
            self.secrets
                .set(claude::SERVICE, &self.claude.user, &next)
                .context("writing the login Claude Code uses")?;
            if let Err(error) = self.claude.set_oauth_account(&account.oauth_account) {
                let restored = match &current {
                    Some(raw) => self.secrets.set(claude::SERVICE, &self.claude.user, raw),
                    None => self.secrets.delete(claude::SERVICE, &self.claude.user),
                };
                return Err(error.context(if restored.is_ok() {
                    "could not update ~/.claude.json; the previous login was put back"
                } else {
                    "could not update ~/.claude.json, and putting the previous login back failed too"
                }));
            }
        }

        state.last_switch = Some(now);
        lock.save_state(&state)?;
        Ok(Switched::Done)
    }

    /// The enabled accounts other than the live one that have a usable reading.
    pub fn candidates(snapshot: &Snapshot) -> Vec<(AccountId, Reading)> {
        snapshot
            .registry
            .accounts
            .iter()
            .filter(|a| a.enabled && snapshot.live.account() != Some(&a.id))
            .filter_map(|a| {
                let state = snapshot.state_of(&a.id).filter(|s| !s.needs_login)?;
                let reading = state.fresh_reading(CANDIDATE_MAX_AGE, snapshot.now)?;
                Some((a.id.clone(), reading.clone()))
            })
            .collect()
    }

    /// One automatic check: read usage, decide, and switch if it is time.
    pub fn check(&self, dry_run: bool) -> Result<Outcome> {
        let policy = self.config.policy();
        let scope = |others_minutes| Scope {
            active_ttl: TimeDelta::seconds(30),
            others_ttl: TimeDelta::minutes(others_minutes),
            disabled: false,
        };
        let mut snapshot = self.collect(&scope(15))?;

        let active = match &snapshot.live {
            Live::Managed(id) => id.clone(),
            Live::LoggedOut => return Ok(Outcome::Held("Claude Code is not logged in".into())),
            Live::Unmanaged => {
                return Ok(Outcome::Held(
                    "Claude Code is logged in as an account bytt has not saved; run `bytt add`"
                        .into(),
                ));
            }
            Live::Unverified(id) => {
                let email = snapshot
                    .registry
                    .find(id)
                    .map_or("the active account", Account::email);
                return Ok(Outcome::Held(format!(
                    "could not confirm the current login belongs to {email}; holding"
                )));
            }
        };

        let reading_of = |snapshot: &Snapshot| {
            snapshot
                .state_of(&active)
                .and_then(|s| s.fresh_reading(ACTIVE_MAX_AGE, snapshot.now))
                .cloned()
        };
        // Close to a line the decision is near, so it should rest on recent numbers.
        let close = reading_of(&snapshot)
            .and_then(|r| policy.standing(&r, snapshot.now))
            .is_some_and(|s| s.room <= CLOSE_TO_LINE);
        if close {
            snapshot = self.collect(&scope(3))?;
        }

        let email = |id: &AccountId| {
            snapshot
                .registry
                .find(id)
                .map_or("unknown", Account::email)
                .to_string()
        };
        let reading = reading_of(&snapshot);
        if reading.is_none() {
            let note = snapshot.state_of(&active).and_then(|s| s.note.clone());
            return Ok(Outcome::Held(format!(
                "{}: {}",
                email(&active),
                note.unwrap_or_else(|| "no usage reading yet".into())
            )));
        }

        let candidates = Self::candidates(&snapshot);
        let cooldown = TimeDelta::seconds(i64::try_from(self.config.cooldown).unwrap_or(i64::MAX));
        let cooling_down = snapshot
            .state
            .last_switch
            .is_some_and(|at| snapshot.now - at < cooldown);
        match policy.decide(reading.as_ref(), &candidates, cooling_down, snapshot.now) {
            Decision::Stay(why) => Ok(Outcome::Held(format!("{}: {why}", email(&active)))),
            Decision::Switch { to, why } => {
                let why = format!("{} at {why}", email(&active));
                if dry_run {
                    return Ok(Outcome::WouldSwitch { to: email(to), why });
                }
                self.switch_to(to, false)?;
                Ok(Outcome::Switched { to: email(to), why })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::FakeApi;
    use crate::api::{FetchError, TokenGrant};
    use crate::credential::testing::raw_login;
    use crate::keychain::testing::MemorySecrets;
    use crate::store::testing::account;
    use crate::usage::Window;
    use serde_json::Value;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A fake machine: a home directory, a Keychain and Anthropic.
    struct World {
        home: TempDir,
        secrets: Arc<MemorySecrets>,
        api: Arc<FakeApi>,
        app: App,
    }

    impl Secrets for Arc<MemorySecrets> {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
            (**self).get(service, account)
        }
        fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
            (**self).set(service, account, secret)
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            (**self).delete(service, account)
        }
    }

    impl Api for Arc<FakeApi> {
        fn usage(&self, token: &str) -> Result<Reading, FetchError> {
            (**self).usage(token)
        }
        fn profile(&self, token: &str) -> Result<Identity, FetchError> {
            (**self).profile(token)
        }
        fn refresh(&self, token: &str) -> Result<TokenGrant, RefreshError> {
            (**self).refresh(token)
        }
    }

    fn later() -> DateTime<Utc> {
        Utc::now() + TimeDelta::hours(4)
    }

    fn reading(five_hour: f64) -> Reading {
        Reading {
            five_hour: Some(Window {
                pct: five_hour,
                resets_at: Some(later()),
            }),
            seven_day: Some(Window {
                pct: 10.0,
                resets_at: Some(later()),
            }),
            models: vec![],
        }
    }

    impl World {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let secrets = Arc::new(MemorySecrets::default());
            let api = Arc::new(FakeApi::default());
            let app = App {
                store: Store::open(home.path().join(".bytt")).unwrap(),
                claude: Claude::new(home.path().to_path_buf(), "tester".into()),
                secrets: Box::new(secrets.clone()),
                api: Box::new(api.clone()),
                config: Config::default(),
            };
            Self {
                home,
                secrets,
                api,
                app,
            }
        }

        /// Log Claude Code in as `name`, the way `/login` would.
        fn login(&self, name: &str, access: &str, refresh: &str) {
            self.secrets
                .set(
                    claude::SERVICE,
                    "tester",
                    &raw_login(access, refresh, later()),
                )
                .unwrap();
            let config = serde_json::json!({
                "numStartups": 3,
                "oauthAccount": account(name).oauth_account,
            });
            fs::write(self.home.path().join(".claude.json"), config.to_string()).unwrap();
            self.knows(access, name);
        }

        /// Anthropic says `access` belongs to `name`.
        fn knows(&self, access: &str, name: &str) {
            let id = account(name).id;
            self.api.profiles.lock().unwrap().insert(
                access.into(),
                Identity {
                    account_uuid: id.account_uuid,
                    org_uuid: Some(id.org_uuid),
                },
            );
        }

        fn usage(&self, access: &str, five_hour: f64) {
            self.api
                .usage
                .lock()
                .unwrap()
                .insert(access.into(), Ok(reading(five_hour)));
        }

        /// Two saved accounts, with anna logged in.
        fn with_anna_and_bob() -> Self {
            let world = Self::new();
            world.login("bob", "bob-1", "bob-r1");
            world.app.add().unwrap();
            world.login("anna", "anna-1", "anna-r1");
            world.app.add().unwrap();
            world
        }

        fn live_access(&self) -> String {
            let raw = self
                .secrets
                .get(claude::SERVICE, "tester")
                .unwrap()
                .unwrap();
            Credential::parse(&raw)
                .unwrap()
                .access_token()
                .unwrap()
                .to_string()
        }

        fn slot_access(&self, name: &str) -> String {
            self.app
                .slot(&account(name).id)
                .unwrap()
                .unwrap()
                .access_token()
                .unwrap()
                .to_string()
        }

        fn config_email(&self) -> String {
            let text = fs::read_to_string(self.home.path().join(".claude.json")).unwrap();
            let config: Value = serde_json::from_str(&text).unwrap();
            config["oauthAccount"]["emailAddress"]
                .as_str()
                .unwrap()
                .to_string()
        }

        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.api.calls.lock().unwrap())
        }
    }

    #[test]
    fn add_saves_the_live_login_once_per_account() {
        let world = World::new();
        world.login("anna", "anna-1", "anna-r1");
        assert!(matches!(world.app.add().unwrap(), Added::New(_)));
        assert!(matches!(world.app.add().unwrap(), Added::Updated(_)));
        assert_eq!(world.app.store.registry().unwrap().accounts.len(), 1);
        assert_eq!(world.slot_access("anna"), "anna-1");
    }

    #[test]
    fn add_refuses_a_login_that_belongs_to_someone_else() {
        let world = World::new();
        world.login("anna", "anna-1", "anna-r1");
        world.knows("anna-1", "bob");
        assert!(world.app.add().is_err());
        assert!(world.app.store.registry().unwrap().accounts.is_empty());
    }

    #[test]
    fn switch_swaps_the_login_and_the_config() {
        let world = World::with_anna_and_bob();
        assert_eq!(
            world.app.switch_to(&account("bob").id, false).unwrap(),
            Switched::Done
        );
        assert_eq!(world.live_access(), "bob-1");
        assert_eq!(world.config_email(), "bob@example.com");
        // The rest of the config is untouched, and anna's login is still in her slot.
        let text = fs::read_to_string(world.home.path().join(".claude.json")).unwrap();
        assert!(text.contains("\"numStartups\": 3"));
        assert_eq!(world.slot_access("anna"), "anna-1");

        assert_eq!(
            world.app.switch_to(&account("bob").id, false).unwrap(),
            Switched::AlreadyActive
        );
        assert_eq!(
            world.app.switch_to(&account("anna").id, false).unwrap(),
            Switched::Done
        );
        assert_eq!(world.live_access(), "anna-1");
    }

    #[test]
    fn switch_saves_a_login_claude_code_renewed() {
        let world = World::with_anna_and_bob();
        // Claude Code renewed anna's login: new access token and new refresh token.
        world.login("anna", "anna-2", "anna-r2");
        world.app.switch_to(&account("bob").id, false).unwrap();
        assert_eq!(world.slot_access("anna"), "anna-2");
        assert_eq!(world.live_access(), "bob-1");
    }

    #[test]
    fn switch_refuses_to_discard_a_login_it_cannot_place() {
        let world = World::with_anna_and_bob();
        // A renewed login that Anthropic cannot be asked about (offline).
        world.login("anna", "anna-2", "anna-r2");
        world.api.profiles.lock().unwrap().clear();
        assert!(world.app.switch_to(&account("bob").id, false).is_err());
        assert_eq!(world.live_access(), "anna-2");
        assert_eq!(world.slot_access("anna"), "anna-1");

        // Someone bytt has never seen.
        world.login("carol", "carol-1", "carol-r1");
        assert!(world.app.switch_to(&account("bob").id, false).is_err());
        assert_eq!(world.live_access(), "carol-1");
        world.app.switch_to(&account("bob").id, true).unwrap();
        assert_eq!(world.live_access(), "bob-1");
    }

    #[test]
    fn a_login_for_another_account_never_enters_a_slot() {
        let world = World::with_anna_and_bob();
        // The config still says anna, but the Keychain already holds bob's new login.
        world
            .secrets
            .set(
                claude::SERVICE,
                "tester",
                &raw_login("bob-9", "bob-r9", later()),
            )
            .unwrap();
        world.knows("bob-9", "bob");
        let snapshot = world.app.collect(&Scope {
            active_ttl: TimeDelta::zero(),
            others_ttl: TimeDelta::zero(),
            disabled: true,
        });
        assert_eq!(snapshot.unwrap().live, Live::Unmanaged);
        assert_eq!(world.slot_access("anna"), "anna-1");
        assert_eq!(world.slot_access("bob"), "bob-1");
    }

    #[test]
    fn switch_puts_the_login_back_when_the_config_cannot_be_written() {
        let world = World::with_anna_and_bob();
        world.app.switch_to(&account("bob").id, false).unwrap();
        fs::write(world.home.path().join(".claude.json"), "{ torn").unwrap();
        // The torn config no longer names anyone, so only --force gets this far.
        assert!(world.app.switch_to(&account("anna").id, true).is_err());
        assert_eq!(world.live_access(), "bob-1");
    }

    #[test]
    fn an_expired_stored_login_is_renewed_and_the_successor_kept() {
        let world = World::with_anna_and_bob();
        let expired = raw_login("bob-1", "bob-r1", Utc::now() - TimeDelta::hours(1));
        world
            .secrets
            .set(SLOTS, &account("bob").id.key(), &expired)
            .unwrap();
        world.api.grants.lock().unwrap().insert(
            "bob-r1".into(),
            Ok(TokenGrant {
                access_token: "bob-2".into(),
                refresh_token: Some("bob-r2".into()),
                expires_in: 28800,
                scope: None,
            }),
        );
        world.app.switch_to(&account("bob").id, false).unwrap();
        assert_eq!(world.live_access(), "bob-2");
        assert_eq!(world.slot_access("bob"), "bob-2");
    }

    #[test]
    fn a_dead_stored_login_is_reported_and_not_switched_to() {
        let world = World::with_anna_and_bob();
        let expired = raw_login("bob-1", "bob-r1", Utc::now() - TimeDelta::hours(1));
        world
            .secrets
            .set(SLOTS, &account("bob").id.key(), &expired)
            .unwrap();
        let error = world.app.switch_to(&account("bob").id, false).unwrap_err();
        assert!(error.to_string().contains("needs a new login"));
        assert_eq!(world.live_access(), "anna-1");
        assert!(world.app.store.state().accounts[&account("bob").id.key()].needs_login);

        // Logging in again and adding clears it.
        world.login("bob", "bob-5", "bob-r5");
        world.app.add().unwrap();
        let state = world.app.store.state();
        assert!(!state.accounts[&account("bob").id.key()].needs_login);
    }

    #[test]
    fn collect_reads_each_account_and_then_serves_from_cache() {
        let world = World::with_anna_and_bob();
        world.usage("anna-1", 40.0);
        world.usage("bob-1", 5.0);
        world.calls();
        let scope = Scope {
            active_ttl: TimeDelta::minutes(1),
            others_ttl: TimeDelta::minutes(1),
            disabled: false,
        };
        let snapshot = world.app.collect(&scope).unwrap();
        assert_eq!(snapshot.live, Live::Managed(account("anna").id));
        assert_eq!(world.calls(), ["usage bob-1", "usage anna-1"]);
        let candidates = App::candidates(&snapshot);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, account("bob").id);
        assert_eq!(candidates[0].1.five_hour.as_ref().unwrap().pct, 5.0);

        world.app.collect(&scope).unwrap();
        assert!(world.calls().is_empty());
    }

    #[test]
    fn check_holds_under_the_line_and_switches_over_it() {
        let world = World::with_anna_and_bob();
        world.usage("anna-1", 40.0);
        world.usage("bob-1", 5.0);
        assert!(matches!(world.app.check(false).unwrap(), Outcome::Held(_)));
        assert_eq!(world.live_access(), "anna-1");

        world.usage("anna-1", 93.0);
        // Pretend the earlier reading has aged out.
        fs::remove_file(world.home.path().join(".bytt/state.json")).unwrap();
        assert!(matches!(
            world.app.check(true).unwrap(),
            Outcome::WouldSwitch { .. }
        ));
        assert_eq!(world.live_access(), "anna-1");

        let outcome = world.app.check(false).unwrap();
        assert!(matches!(&outcome, Outcome::Switched { to, .. } if to == "bob@example.com"));
        assert_eq!(world.live_access(), "bob-1");
        assert_eq!(world.config_email(), "bob@example.com");
    }

    #[test]
    fn check_skips_disabled_accounts() {
        let world = World::with_anna_and_bob();
        world.usage("anna-1", 93.0);
        world.usage("bob-1", 5.0);
        world.app.set_enabled(&account("bob").id, false).unwrap();
        assert!(matches!(world.app.check(false).unwrap(), Outcome::Held(_)));
        assert_eq!(world.live_access(), "anna-1");
    }

    #[test]
    fn check_holds_when_the_live_login_is_unknown() {
        let world = World::with_anna_and_bob();
        world.login("carol", "carol-1", "carol-r1");
        assert!(matches!(world.app.check(false).unwrap(), Outcome::Held(_)));
        assert_eq!(world.live_access(), "carol-1");
    }

    #[test]
    fn remove_forgets_the_account_and_its_login() {
        let world = World::with_anna_and_bob();
        world.app.remove(&account("bob").id).unwrap();
        assert_eq!(world.app.store.registry().unwrap().accounts.len(), 1);
        assert!(world.app.slot(&account("bob").id).unwrap().is_none());
    }
}
