//! Claude Code's own state: where its login lives and how to change it safely.
//!
//! The login is two things. The credential is a Keychain item; who it belongs to is
//! the `oauthAccount` section of `~/.claude.json`. A swap replaces both, under the
//! locks Claude Code itself takes, and leaves the rest of the file alone.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::fsx::write_atomic;
use crate::lock::DirLock;

/// The Keychain service holding the login of the default Claude Code profile.
pub const SERVICE: &str = "Claude Code-credentials";

/// Claude Code considers a credential lock dead after 60 seconds without a touch,
/// and a config lock after 10.
const CREDENTIAL_LOCK_STALE: Duration = Duration::from_secs(60);
const CONFIG_LOCK_STALE: Duration = Duration::from_secs(10);

pub type OauthAccount = Map<String, Value>;

/// The default Claude Code profile of one user. `CLAUDE_CONFIG_DIR` profiles are
/// deliberately out of scope: the background service would not see the variable.
pub struct Claude {
    home: PathBuf,
    /// The Keychain account name Claude Code stores its login under.
    pub user: String,
}

/// Both of Claude Code's credential locks, held until dropped.
pub struct CredentialLocks(#[allow(dead_code)] DirLock, #[allow(dead_code)] DirLock);

impl Claude {
    pub fn new(home: PathBuf, user: String) -> Self {
        Self { home, user }
    }

    pub fn for_current_user() -> Result<Self> {
        let home = std::env::home_dir().context("could not find your home directory")?;
        Ok(Self::new(home, current_user()))
    }

    /// Claude Code keeps its global config in `~/.claude.json`, unless the legacy
    /// `~/.claude/.config.json` exists and is in use.
    fn config_path(&self) -> PathBuf {
        let modern = self.home.join(".claude.json");
        let legacy = self.home.join(".claude").join(".config.json");
        let legacy_in_use = fs::metadata(&legacy).is_ok_and(|meta| {
            meta.is_file()
                && (meta.len() > 0 || (meta.permissions().mode() & 0o222 != 0 && !modern.exists()))
        });
        if legacy_in_use { legacy } else { modern }
    }

    fn read_config(path: &Path) -> Result<Option<Map<String, Value>>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        match serde_json::from_str(&text) {
            Ok(Value::Object(config)) => Ok(Some(config)),
            Ok(_) => bail!("{} is not a JSON object", path.display()),
            Err(e) => Err(e).with_context(|| format!("parsing {}", path.display())),
        }
    }

    /// Who Claude Code believes is logged in.
    pub fn oauth_account(&self) -> Result<Option<OauthAccount>> {
        let config = Self::read_config(&self.config_path())?;
        Ok(config.and_then(|mut c| match c.remove("oauthAccount") {
            Some(Value::Object(account)) => Some(account),
            _ => None,
        }))
    }

    /// Point Claude Code's config at `account`, touching nothing else in the file.
    pub fn set_oauth_account(&self, account: &OauthAccount) -> Result<()> {
        let path = self.config_path();
        let lock_path = path.with_file_name(format!(
            "{}.lock",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(".claude.json")
        ));
        let _lock = DirLock::acquire(lock_path, CONFIG_LOCK_STALE)?;
        // An unreadable file is an error rather than something to overwrite: it
        // holds far more than the login.
        let mut config = Self::read_config(&path)?.unwrap_or_default();
        config.insert("oauthAccount".into(), Value::Object(account.clone()));
        let text = serde_json::to_string_pretty(&Value::Object(config))?;
        write_atomic(&path, text.as_bytes())
    }

    /// Take the locks Claude Code holds while it refreshes a token, in its order:
    /// `~/.claude/.oauth_refresh.lock`, then the older `~/.claude.lock`.
    pub fn lock_credentials(&self) -> Result<CredentialLocks> {
        let refresh = DirLock::acquire(
            self.home.join(".claude").join(".oauth_refresh.lock"),
            CREDENTIAL_LOCK_STALE,
        )?;
        let legacy = DirLock::acquire(self.home.join(".claude.lock"), CREDENTIAL_LOCK_STALE)?;
        Ok(CredentialLocks(refresh, legacy))
    }
}

/// The name Claude Code files its Keychain item under: `$USER`, else the login name.
fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| {
            let out = Command::new("/usr/bin/id").arg("-un").output().ok()?;
            let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .unwrap_or_else(|| "claude-code-user".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude(home: &Path) -> Claude {
        Claude::new(home.to_path_buf(), "tester".into())
    }

    #[test]
    fn splices_the_account_and_leaves_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let original = "{\n  \"numStartups\": 7,\n  \"oauthAccount\": {\n    \"emailAddress\": \"a@x.no\"\n  },\n  \"projects\": {\n    \"/tmp\": {\n      \"cost\": 1.25\n    }\n  }\n}";
        fs::write(home.path().join(".claude.json"), original).unwrap();
        let claude = claude(home.path());
        assert_eq!(
            claude.oauth_account().unwrap().unwrap()["emailAddress"],
            "a@x.no"
        );

        let next = json!({"emailAddress": "b@x.no"});
        claude.set_oauth_account(next.as_object().unwrap()).unwrap();

        let written = fs::read_to_string(home.path().join(".claude.json")).unwrap();
        assert_eq!(written, original.replace("a@x.no", "b@x.no"));
        assert!(!home.path().join(".claude.json.lock").exists());
    }

    #[test]
    fn refuses_to_overwrite_a_config_it_cannot_read() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".claude.json"), "{ torn").unwrap();
        assert!(claude(home.path()).set_oauth_account(&Map::new()).is_err());
        assert_eq!(
            fs::read_to_string(home.path().join(".claude.json")).unwrap(),
            "{ torn"
        );
    }

    #[test]
    fn a_missing_config_means_nobody_is_logged_in() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(claude(home.path()).oauth_account().unwrap(), None);
    }

    #[test]
    fn credential_locks_are_released_on_drop() {
        let home = tempfile::tempdir().unwrap();
        let claude = claude(home.path());
        let locks = claude.lock_credentials().unwrap();
        assert!(home.path().join(".claude.lock").is_dir());
        assert!(home.path().join(".claude/.oauth_refresh.lock").is_dir());
        drop(locks);
        assert!(!home.path().join(".claude.lock").exists());
    }
}
