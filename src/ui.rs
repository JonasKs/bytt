//! What an account row says, shared by `bytt list` (table, JSON) and the TUI.

use std::fmt::Write as _;
use std::io::IsTerminal;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

use crate::app::{App, Live, Snapshot};
use crate::store::{AccountId, Config};
use crate::usage::{Decision, Policy, Reading, Window};

#[derive(Serialize)]
pub struct Row<'a> {
    #[serde(skip)]
    pub id: &'a AccountId,
    pub number: usize,
    pub email: &'a str,
    pub organization: Option<&'a str>,
    pub plan: Option<&'a str>,
    pub active: bool,
    pub enabled: bool,
    pub needs_login: bool,
    pub note: Option<&'a str>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub usage: Option<&'a Reading>,
}

impl Row<'_> {
    /// "Org · Plan", or whichever of the two is known.
    pub fn who(&self) -> String {
        match (self.organization, self.plan) {
            (Some(org), Some(plan)) => format!("{org} · {plan}"),
            (Some(one), None) | (None, Some(one)) => one.to_string(),
            (None, None) => String::new(),
        }
    }

    /// Why the row has no current reading, if there is something to say.
    pub fn notice(&self) -> Option<(String, Level)> {
        if self.needs_login {
            Some((
                "needs a new login: log in with it in Claude Code, then add it".into(),
                Level::Over,
            ))
        } else if let Some(note) = self.note {
            Some((note.to_string(), Level::Near))
        } else if self.usage.is_none() {
            Some(("no usage reading yet".into(), Level::Dim))
        } else {
            None
        }
    }

    /// "read 12m ago", once a reading is old enough to mention.
    pub fn age(&self, now: DateTime<Utc>) -> Option<String> {
        let age = now - self.fetched_at?;
        (age > TimeDelta::minutes(3)).then(|| format!("read {} ago", countdown(now + age, now)))
    }
}

pub fn rows(snapshot: &Snapshot) -> Vec<Row<'_>> {
    snapshot
        .registry
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let state = snapshot.state_of(&account.id);
            Row {
                id: &account.id,
                number: index + 1,
                email: account.email(),
                organization: account.organization(),
                plan: account.plan(),
                active: snapshot.live.account() == Some(&account.id),
                enabled: account.enabled,
                needs_login: state.is_some_and(|s| s.needs_login),
                note: state.and_then(|s| s.note.as_deref()),
                fetched_at: state.and_then(|s| s.fetched_at),
                usage: state.and_then(|s| s.reading.as_ref()),
            }
        })
        .collect()
}

pub fn json(snapshot: &Snapshot) -> Result<String> {
    Ok(serde_json::to_string_pretty(&rows(snapshot))?)
}

/// What Claude Code's login looks like from here, when it is not a saved account.
pub fn live_notice(live: &Live) -> Option<&'static str> {
    match live {
        Live::Managed(_) => None,
        Live::LoggedOut => Some("Claude Code is not logged in."),
        Live::Unmanaged => Some("Claude Code is logged in as an account that is not saved here."),
        Live::Unverified(_) => Some(
            "The current login could not be confirmed with Anthropic, so it has not been saved yet.",
        ),
    }
}

/// One status line: what to say, a quieter detail, and whether it needs attention.
pub struct Says {
    pub text: String,
    pub detail: String,
    pub warn: bool,
}

/// "Now" and "Next up": what an automatic check would decide, and whom it would pick.
pub fn outlook(snapshot: &Snapshot, config: &Config, auto: bool) -> (Says, Says) {
    let policy = config.policy();
    let email = |id: &AccountId| {
        snapshot
            .registry
            .find(id)
            .map_or_else(|| "unknown".into(), |a| a.email().to_string())
    };
    let says = |text: String, detail: String, warn: bool| Says { text, detail, warn };
    let candidates = App::candidates(snapshot);
    let best = candidates
        .iter()
        .filter_map(|(id, r)| Some((id, policy.standing(r, snapshot.now)?)))
        .max_by(|a, b| a.1.room.total_cmp(&b.1.room));
    let next = match best {
        None if snapshot.registry.accounts.len() < 2 => {
            says("no other account saved".into(), String::new(), false)
        }
        None => says("no other account to switch to".into(), String::new(), false),
        Some((id, s)) if s.room >= policy.margin => {
            says(email(id), format!("{} {:.0}%", s.window, s.pct), false)
        }
        Some((id, s)) => says(
            "no account to switch to".into(),
            format!("least used: {} at {} {:.0}%", email(id), s.window, s.pct),
            true,
        ),
    };

    let now = match &snapshot.live {
        Live::Managed(id) => {
            let reading = snapshot.state_of(id).and_then(|s| s.reading.as_ref());
            let cooldown = TimeDelta::seconds(i64::try_from(config.cooldown).unwrap_or(0));
            let cooling = snapshot
                .state
                .last_switch
                .is_some_and(|at| snapshot.now - at < cooldown);
            match policy.decide(reading, &candidates, cooling, snapshot.now) {
                Decision::Stay(why) => says(format!("on {}", email(id)), why, false),
                Decision::Switch { to, why } => says(
                    format!(
                        "{} {}",
                        if auto {
                            "switching to"
                        } else {
                            "would switch to"
                        },
                        email(to)
                    ),
                    why,
                    true,
                ),
            }
        }
        live => says(
            live_notice(live).unwrap_or_default().to_string(),
            String::new(),
            true,
        ),
    };
    (now, next)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Near,
    Over,
    Dim,
}

/// One window, ready to draw: `5h ████░░░░░░  41% · 2h 10m`.
#[derive(Debug, PartialEq)]
pub struct Gauge {
    pub label: String,
    pub pct: f64,
    /// Tenths filled, 0 to 10.
    pub filled: usize,
    pub level: Level,
    pub resets_at: Option<DateTime<Utc>>,
    pub reset: Option<String>,
}

impl Gauge {
    fn new(label: &str, window: &Window, line: f64, now: DateTime<Utc>) -> Self {
        let pct = window.pct_at(now);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let filled = (pct / 10.0).round().clamp(0.0, 10.0) as usize;
        let level = if pct >= line {
            Level::Over
        } else if pct >= line - 15.0 {
            Level::Near
        } else {
            Level::Ok
        };
        let reset = window
            .resets_at
            .filter(|reset| *reset > now)
            .map(|reset| countdown(reset, now));
        Self {
            label: label.to_string(),
            pct,
            filled,
            level,
            resets_at: window.resets_at,
            reset,
        }
    }

    pub fn bar(&self) -> String {
        format!(
            "{}{}",
            "█".repeat(self.filled),
            "░".repeat(10 - self.filled)
        )
    }

    pub fn figure(&self) -> String {
        format!("{:>3.0}%", self.pct)
    }
}

/// Every window in a reading: 5h, 7d, then each model's own weekly limit.
pub fn gauges(reading: &Reading, policy: &Policy, now: DateTime<Utc>) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    if let Some(window) = &reading.five_hour {
        gauges.push(Gauge::new("5h", window, policy.threshold, now));
    }
    if let Some(window) = &reading.seven_day {
        gauges.push(Gauge::new("7d", window, policy.weekly_threshold, now));
    }
    for model in &reading.models {
        gauges.push(Gauge::new(
            &model.name,
            &model.window,
            policy.weekly_threshold,
            now,
        ));
    }
    gauges
}

pub fn countdown(until: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = std::cmp::max((until - now).num_minutes(), 0);
    let (days, hours, minutes) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

struct Paint(bool);

impl Paint {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn level(&self, level: Level, text: &str) -> String {
        let code = match level {
            Level::Ok => "32",
            Level::Near => "33",
            Level::Over => "31",
            Level::Dim => "2",
        };
        self.wrap(code, text)
    }
    fn gauge(&self, gauge: &Gauge) -> String {
        let reset = gauge
            .reset
            .as_ref()
            .map(|reset| self.wrap("2", &format!(" · {reset}")))
            .unwrap_or_default();
        format!(
            "{} {} {}{reset}",
            gauge.label,
            self.level(gauge.level, &gauge.bar()),
            gauge.figure()
        )
    }
}

pub fn table(snapshot: &Snapshot, policy: &Policy) -> String {
    let paint = Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    let rows = rows(snapshot);
    if rows.is_empty() {
        return "No accounts yet. Log in to Claude Code, then run `bytt add`.\n".into();
    }
    let width = |text: &str| text.chars().count();
    let email_width = rows.iter().map(|r| width(r.email)).max().unwrap_or(0);
    let who_width = rows.iter().map(|r| width(&r.who())).max().unwrap_or(0);

    let mut out = String::new();
    for row in &rows {
        let marker = if row.active {
            paint.wrap("32", "●")
        } else {
            " ".into()
        };
        let email = format!("{:<email_width$}", row.email);
        let email = if row.active {
            paint.wrap("1", &email)
        } else {
            email
        };
        let _ = write!(
            out,
            "{:>2} {marker} {email}  {}",
            row.number,
            paint.wrap("2", &format!("{:<who_width$}", row.who()))
        );

        let mut cells: Vec<String> = row
            .usage
            .map(|reading| gauges(reading, policy, snapshot.now))
            .unwrap_or_default()
            .iter()
            .map(|gauge| paint.gauge(gauge))
            .collect();
        if let Some((text, level)) = row.notice() {
            cells.push(paint.level(level, &text));
        }
        if let Some(age) = row.age(snapshot.now) {
            cells.push(paint.wrap("2", &age));
        }
        if !row.enabled {
            cells.push(paint.wrap("2", "(disabled)"));
        }
        let _ = writeln!(out, "  {}", cells.join("   "));
    }

    if let Some(notice) = live_notice(&snapshot.live) {
        let hint = if matches!(snapshot.live, Live::Unmanaged) {
            " Run `bytt add` to save it."
        } else {
            ""
        };
        let _ = writeln!(out, "\n{notice}{hint}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdowns_pick_two_units() {
        let now = Utc::now();
        assert_eq!(countdown(now + TimeDelta::minutes(12), now), "12m");
        assert_eq!(countdown(now + TimeDelta::minutes(130), now), "2h 10m");
        assert_eq!(countdown(now + TimeDelta::hours(76), now), "3d 4h");
        assert_eq!(countdown(now - TimeDelta::hours(1), now), "0m");
    }

    #[test]
    fn gauges_fill_by_tenths_and_colour_by_line() {
        let now = Utc::now();
        let window = Window {
            pct: 41.0,
            resets_at: Some(now + TimeDelta::minutes(130)),
        };
        let gauge = Gauge::new("5h", &window, 90.0, now);
        assert_eq!(Paint(false).gauge(&gauge), "5h ████░░░░░░  41% · 2h 10m");
        assert_eq!(gauge.level, Level::Ok);

        let full = Window {
            pct: 100.0,
            resets_at: None,
        };
        let gauge = Gauge::new("7d", &full, 90.0, now);
        assert_eq!(Paint(false).gauge(&gauge), "7d ██████████ 100%");
        assert_eq!(gauge.level, Level::Over);
        assert_eq!(
            Gauge::new(
                "7d",
                &Window {
                    pct: 80.0,
                    resets_at: None
                },
                90.0,
                now
            )
            .level,
            Level::Near
        );
    }
}
