//! The three Anthropic endpoints bytt talks to: usage, profile and token refresh.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::usage::{ModelWindow, Reading, Window};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's public OAuth client id; refresh tokens are bound to it.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// The access token was refused.
    Unauthorized,
    RateLimited {
        retry_after: Option<Duration>,
    },
    Other(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("token refused"),
            Self::RateLimited { .. } => f.write_str("usage endpoint rate limited"),
            Self::Other(detail) => f.write_str(detail),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefreshError {
    /// The refresh token is dead; only logging in again brings the account back.
    Dead,
    /// Network or server trouble; the token may well still be good.
    Transient(String),
}

/// A token endpoint response.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenGrant {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Who a token belongs to, according to Anthropic.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub account_uuid: String,
    pub org_uuid: Option<String>,
}

pub trait Api: Send + Sync {
    fn usage(&self, access_token: &str) -> Result<Reading, FetchError>;
    fn profile(&self, access_token: &str) -> Result<Identity, FetchError>;
    /// Spend a refresh token. On success the old token is dead and the grant holds
    /// its successor, so the caller must store it.
    fn refresh(&self, refresh_token: &str) -> Result<TokenGrant, RefreshError>;
}

pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .user_agent(concat!("bytt/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }

    fn get(&self, url: &str, access_token: &str) -> Result<Value, FetchError> {
        let mut response = self
            .agent
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", OAUTH_BETA)
            .call()
            .map_err(|e| FetchError::Other(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .body_mut()
                .read_json()
                .map_err(|e| FetchError::Other(format!("unreadable response: {e}"))),
            401 => Err(FetchError::Unauthorized),
            429 => Err(FetchError::RateLimited {
                retry_after: response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
                    .map(Duration::from_secs),
            }),
            status => Err(FetchError::Other(format!("http {status}"))),
        }
    }
}

impl Api for Http {
    fn usage(&self, access_token: &str) -> Result<Reading, FetchError> {
        Ok(parse_usage(&self.get(USAGE_URL, access_token)?))
    }

    fn profile(&self, access_token: &str) -> Result<Identity, FetchError> {
        let body = self.get(PROFILE_URL, access_token)?;
        let uuid = |object: &str| {
            body.get(object)?
                .get("uuid")?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Ok(Identity {
            account_uuid: uuid("account")
                .ok_or_else(|| FetchError::Other("profile response names no account".into()))?,
            org_uuid: uuid("organization"),
        })
    }

    fn refresh(&self, refresh_token: &str) -> Result<TokenGrant, RefreshError> {
        let mut response = self
            .agent
            .post(TOKEN_URL)
            .send_json(json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLIENT_ID,
            }))
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 200 {
            return response
                .body_mut()
                .read_json()
                .map_err(|e| RefreshError::Transient(format!("unreadable token response: {e}")));
        }
        // Only an explicit verdict from the server condemns a token. Anything
        // ambiguous costs one retry; a wrong "dead" costs the user a login.
        let verdict = response
            .body_mut()
            .read_json::<Value>()
            .ok()
            .and_then(|body| Some(body.get("error")?.as_str()?.to_string()));
        if matches!(status, 400 | 401 | 403) && verdict.as_deref() == Some("invalid_grant") {
            Err(RefreshError::Dead)
        } else {
            Err(RefreshError::Transient(format!(
                "http {status}{}",
                verdict.map_or_default(|v| format!(" ({v})"))
            )))
        }
    }
}

fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    Some(
        DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()?
            .with_timezone(&Utc),
    )
}

/// A window is only kept when it carries a usable percentage: a missing one means
/// the account has no such limit, and a null one that the number is unknown.
fn parse_window(value: Option<&Value>, pct_key: &str) -> Option<Window> {
    let value = value?;
    let pct = value
        .get(pct_key)?
        .as_f64()
        .filter(|p| p.is_finite() && *p >= 0.0)?;
    Some(Window {
        pct,
        resets_at: value.get("resets_at").and_then(parse_time),
    })
}

fn parse_usage(body: &Value) -> Reading {
    // Per-model weekly limits come in a `limits` array, named by the model's
    // display name.
    let models = body
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            let name = limit
                .get("scope")?
                .get("model")?
                .get("display_name")?
                .as_str()?;
            Some(ModelWindow {
                name: name.to_string(),
                window: parse_window(Some(limit), "percent")?,
            })
        })
        .collect();
    Reading {
        five_hour: parse_window(body.get("five_hour"), "utilization"),
        seven_day: parse_window(body.get("seven_day"), "utilization"),
        models,
    }
}

#[cfg(test)]
pub mod testing {
    use super::{Api, FetchError, Identity, Reading, RefreshError, TokenGrant};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A scripted stand-in for Anthropic, keyed by token.
    #[derive(Default)]
    pub struct FakeApi {
        pub usage: Mutex<HashMap<String, Result<Reading, FetchError>>>,
        pub profiles: Mutex<HashMap<String, Identity>>,
        pub grants: Mutex<HashMap<String, Result<TokenGrant, RefreshError>>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Api for FakeApi {
        fn usage(&self, access_token: &str) -> Result<Reading, FetchError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("usage {access_token}"));
            self.usage
                .lock()
                .unwrap()
                .get(access_token)
                .cloned()
                .unwrap_or(Err(FetchError::Unauthorized))
        }
        fn profile(&self, access_token: &str) -> Result<Identity, FetchError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("profile {access_token}"));
            self.profiles
                .lock()
                .unwrap()
                .get(access_token)
                .cloned()
                .ok_or(FetchError::Unauthorized)
        }
        fn refresh(&self, refresh_token: &str) -> Result<TokenGrant, RefreshError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("refresh {refresh_token}"));
            // A refresh token works once.
            self.grants
                .lock()
                .unwrap()
                .remove(refresh_token)
                .unwrap_or(Err(RefreshError::Dead))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_and_model_limits() {
        let body = json!({
            "five_hour": {"utilization": 41.0, "resets_at": "2026-11-12T15:00:00.123456+00:00"},
            "seven_day": {"utilization": 18, "resets_at": null},
            "limits": [
                {"scope": {"model": {"display_name": "Fable"}}, "percent": 77, "resets_at": "2026-11-15T00:00:00Z"},
                {"scope": null, "percent": 5},
            ],
        });
        let reading = parse_usage(&body);
        let five_hour = reading.five_hour.unwrap();
        assert_eq!(five_hour.pct, 41.0);
        assert_eq!(five_hour.resets_at.unwrap().timestamp(), 1_794_495_600);
        assert_eq!(
            reading.seven_day.unwrap(),
            Window {
                pct: 18.0,
                resets_at: None
            }
        );
        assert_eq!(reading.models.len(), 1);
        assert_eq!(reading.models[0].name, "Fable");
        assert_eq!(reading.models[0].window.pct, 77.0);
    }

    #[test]
    fn an_account_without_a_five_hour_window_has_none() {
        let reading = parse_usage(&json!({"five_hour": null, "seven_day": {"utilization": null}}));
        assert_eq!(reading, Reading::default());
    }
}
