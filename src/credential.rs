//! A Claude Code login, as it sits in the Keychain.
//!
//! The blob is a JSON object. `claudeAiOauth` is the login itself; a few sibling keys
//! (MCP server logins, plugin secrets) belong to the machine, not to an account.
//! Everything bytt does not understand is carried along untouched.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};

use crate::api::TokenGrant;

/// Keys next to `claudeAiOauth` that are shared by every account on this machine.
/// On a swap they are taken from the live login, so signing in to an MCP server
/// under one account is not undone by switching to another.
const SHARED_KEYS: [&str; 5] = [
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

/// Claude Code refreshes a token this long before it expires; so does bytt.
const EXPIRY_MARGIN: Duration = Duration::minutes(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Credential(Map<String, Value>);

impl Credential {
    /// Parse a Keychain blob. Fails for anything that is not a subscription login
    /// with a usable token (an API key, or a login Claude Code has emptied).
    pub fn parse(raw: &str) -> Result<Self> {
        let Value::Object(blob) = serde_json::from_str(raw).context("login is not JSON")? else {
            bail!("login is not a JSON object");
        };
        let credential = Self(blob);
        if credential.access_token().is_none() {
            bail!("login holds no access token");
        }
        Ok(credential)
    }

    pub fn to_json(&self) -> String {
        Value::Object(self.0.clone()).to_string()
    }

    fn oauth_str(&self, key: &str) -> Option<&str> {
        self.0
            .get("claudeAiOauth")?
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
    }

    pub fn access_token(&self) -> Option<&str> {
        self.oauth_str("accessToken")
    }

    /// The refresh token names the login's lineage: it survives access-token
    /// renewals and changes only when the login itself is renewed.
    pub fn refresh_token(&self) -> Option<&str> {
        self.oauth_str("refreshToken")
    }

    /// Whether the access token is expired, or close enough that Claude Code would
    /// renew it. A login without an expiry is taken to be valid.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.0
            .get("claudeAiOauth")
            .and_then(|o| o.get("expiresAt"))
            .and_then(Value::as_f64)
            .is_some_and(|expires_ms| {
                #[allow(clippy::cast_precision_loss)]
                let deadline = (now + EXPIRY_MARGIN).timestamp_millis() as f64;
                deadline >= expires_ms
            })
    }

    /// Fold a token endpoint response into the login.
    pub fn apply(&mut self, grant: TokenGrant, now: DateTime<Utc>) {
        let Some(Value::Object(oauth)) = self.0.get_mut("claudeAiOauth") else {
            return;
        };
        oauth.insert("accessToken".into(), json!(grant.access_token));
        let expires_in = i64::try_from(grant.expires_in).unwrap_or(i64::MAX / 1000);
        oauth.insert(
            "expiresAt".into(),
            json!(
                now.timestamp_millis()
                    .saturating_add(expires_in.saturating_mul(1000))
            ),
        );
        if let Some(refresh_token) = grant.refresh_token {
            oauth.insert("refreshToken".into(), json!(refresh_token));
        }
        if let Some(scope) = grant.scope {
            oauth.insert(
                "scopes".into(),
                json!(scope.split_whitespace().collect::<Vec<_>>()),
            );
        }
    }

    /// This login, carrying the machine-shared keys of whatever is live right now.
    pub fn with_shared_from(&self, live: Option<&str>) -> Self {
        let live = live
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            });
        let Some(mut live) = live else {
            return self.clone();
        };
        let mut blob = self.0.clone();
        for key in SHARED_KEYS {
            // shift_remove keeps the remaining keys in their original order.
            blob.shift_remove(key);
            if let Some(value) = live.shift_remove(key) {
                blob.insert(key.into(), value);
            }
        }
        Self(blob)
    }
}

#[cfg(test)]
pub mod testing {
    use super::Credential;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    pub fn raw_login(access: &str, refresh: &str, expires: DateTime<Utc>) -> String {
        json!({"claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires.timestamp_millis(),
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
        }})
        .to_string()
    }

    pub fn login(access: &str, refresh: &str, expires: DateTime<Utc>) -> Credential {
        Credential::parse(&raw_login(access, refresh, expires)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::login;
    use super::*;

    #[test]
    fn rejects_logins_without_a_token() {
        assert!(Credential::parse("sk-ant-api03-abc").is_err());
        assert!(Credential::parse(r#"{"mcpOAuth":{}}"#).is_err());
        assert!(Credential::parse(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_err());
    }

    #[test]
    fn expiry_uses_a_margin() {
        let now = Utc::now();
        assert!(!login("a", "r", now + Duration::minutes(10)).is_expired(now));
        assert!(login("a", "r", now + Duration::minutes(4)).is_expired(now));
        assert!(login("a", "r", now - Duration::minutes(1)).is_expired(now));
    }

    #[test]
    fn apply_rotates_tokens_and_keeps_the_rest() {
        let now = Utc::now();
        let mut credential = login("old", "old-refresh", now);
        credential.apply(
            TokenGrant {
                access_token: "new".into(),
                refresh_token: Some("new-refresh".into()),
                expires_in: 3600,
                scope: None,
            },
            now,
        );
        assert_eq!(credential.access_token(), Some("new"));
        assert_eq!(credential.refresh_token(), Some("new-refresh"));
        assert!(!credential.is_expired(now));
        assert!(credential.to_json().contains(r#""subscriptionType":"max""#));
    }

    #[test]
    fn shared_keys_follow_the_live_login() {
        let now = Utc::now();
        let mut stored = login("a", "r", now);
        stored.0.insert("mcpOAuth".into(), json!({"stale": true}));
        stored
            .0
            .insert("pluginSecrets".into(), json!({"gone": true}));
        stored.0.insert("trustedDeviceToken".into(), json!("mine"));
        let live = json!({"claudeAiOauth": {"accessToken": "x"}, "mcpOAuth": {"fresh": true}});

        let merged = stored.with_shared_from(Some(&live.to_string()));
        assert_eq!(merged.0["mcpOAuth"], json!({"fresh": true}));
        assert!(!merged.0.contains_key("pluginSecrets"));
        assert_eq!(merged.0["trustedDeviceToken"], json!("mine"));
        assert_eq!(merged.access_token(), Some("a"));

        assert_eq!(stored.with_shared_from(None), stored);
    }
}
