//! Usage readings, and the policy that turns them into a decision.
//!
//! Everything here is a pure function of readings and the clock.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One rate-limit window: how full it is and when it empties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

impl Window {
    /// The percentage as of `now`: a window whose reset has passed is empty,
    /// whatever the last reading said.
    pub fn pct_at(&self, now: DateTime<Utc>) -> f64 {
        match self.resets_at {
            Some(reset) if reset <= now => 0.0,
            _ => self.pct,
        }
    }
}

/// A weekly window that counts one model only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelWindow {
    pub name: String,
    #[serde(flatten)]
    pub window: Window,
}

/// What the usage endpoint reported for one account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelWindow>,
}

impl Reading {
    /// Whether a window has reset since this reading was taken at `taken`, which
    /// makes it worth taking a new one.
    pub fn has_reset_since(&self, taken: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        self.five_hour
            .iter()
            .chain(&self.seven_day)
            .chain(self.models.iter().map(|m| &m.window))
            .any(|w| {
                w.resets_at
                    .is_some_and(|reset| taken < reset && reset <= now)
            })
    }
}

/// When to leave an account, and what counts as a better one.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Leave when the 5-hour window reaches this percentage.
    pub threshold: f64,
    /// Leave when the 7-day window (or a watched model's) reaches this percentage.
    pub weekly_threshold: f64,
    /// A target needs this many points left under each of its lines.
    pub margin: f64,
    /// Models whose own weekly window counts, by display name.
    pub models: Vec<String>,
}

/// Where an account stands against the policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Standing {
    /// The window nearest its line.
    pub window: String,
    pub pct: f64,
    pub line: f64,
    /// Points left before that line; zero or less means it is time to leave.
    pub room: f64,
    /// Points left before any window is completely full.
    pub headroom: f64,
}

impl Standing {
    pub fn exhausted(&self) -> bool {
        self.headroom < 1.0
    }
}

impl Policy {
    /// Whether a model's own weekly limit counts.
    pub fn watches(&self, model: &str) -> bool {
        self.models
            .iter()
            .any(|name| name.eq_ignore_ascii_case(model))
    }

    /// `None` when the reading has no window to judge by.
    pub fn standing(&self, reading: &Reading, now: DateTime<Utc>) -> Option<Standing> {
        let five_hour = reading.five_hour.iter().map(|w| ("5h", w, self.threshold));
        let seven_day = reading
            .seven_day
            .iter()
            .map(|w| ("7d", w, self.weekly_threshold));
        let models = reading
            .models
            .iter()
            .filter(|m| self.watches(&m.name))
            .map(|m| (m.name.as_str(), &m.window, self.weekly_threshold));

        let mut standing: Option<Standing> = None;
        for (label, window, line) in five_hour.chain(seven_day).chain(models) {
            let pct = window.pct_at(now);
            let (room, headroom) = (line - pct, 100.0 - pct);
            match &mut standing {
                Some(s) => {
                    s.headroom = f64::min(s.headroom, headroom);
                    if room < s.room {
                        (s.window, s.pct, s.line, s.room) = (label.to_string(), pct, line, room);
                    }
                }
                None => {
                    standing = Some(Standing {
                        window: label.to_string(),
                        pct,
                        line,
                        room,
                        headroom,
                    });
                }
            }
        }
        standing
    }

    /// Decide whether to leave the active account, and for which candidate.
    ///
    /// `cooling_down` is true shortly after a switch; it holds everything except
    /// leaving an account that is completely out of quota.
    pub fn decide<'a, K>(
        &self,
        active: Option<&Reading>,
        candidates: &'a [(K, Reading)],
        cooling_down: bool,
        now: DateTime<Utc>,
    ) -> Decision<'a, K> {
        let Some(active) = active.and_then(|r| self.standing(r, now)) else {
            return Decision::Stay("no usage reading for the active account".into());
        };
        let at = format!("{} {:.0}%", active.window, active.pct);
        if active.room > 0.0 {
            return Decision::Stay(format!("{at}, switches at {:.0}%", active.line));
        }
        if cooling_down && !active.exhausted() {
            return Decision::Stay(format!("{at}, but switched moments ago"));
        }

        let ranked: Vec<(&K, Standing)> = candidates
            .iter()
            .filter_map(|(key, reading)| Some((key, self.standing(reading, now)?)))
            .collect();

        // First choice: the account furthest under its lines, if it has room to spare.
        let roomiest = ranked.iter().max_by(|a, b| a.1.room.total_cmp(&b.1.room));
        if let Some((key, standing)) = roomiest.filter(|(_, s)| s.room >= self.margin) {
            return Decision::Switch {
                to: key,
                why: format!("{at}; target at {} {:.0}%", standing.window, standing.pct),
            };
        }

        // Every account is near or over a line. Moving is still worth it when
        // another one has clearly more quota left, and any quota beats none.
        let needed = if active.exhausted() { 3.0 } else { self.margin };
        let fullest = ranked
            .iter()
            .max_by(|a, b| a.1.headroom.total_cmp(&b.1.headroom));
        if let Some((key, standing)) =
            fullest.filter(|(_, s)| s.headroom >= active.headroom + needed)
        {
            return Decision::Switch {
                to: key,
                why: format!(
                    "{at}; every account is near its limit, target has {:.0}% left",
                    standing.headroom
                ),
            };
        }
        Decision::Stay(format!("{at}, and no other account has more room"))
    }
}

#[derive(Debug, PartialEq)]
pub enum Decision<'a, K> {
    Stay(String),
    Switch { to: &'a K, why: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn policy() -> Policy {
        Policy {
            threshold: 90.0,
            weekly_threshold: 95.0,
            margin: 10.0,
            models: vec![],
        }
    }

    fn reading(five_hour: f64, seven_day: f64) -> Reading {
        let later = Some(Utc::now() + Duration::hours(1));
        Reading {
            five_hour: Some(Window {
                pct: five_hour,
                resets_at: later,
            }),
            seven_day: Some(Window {
                pct: seven_day,
                resets_at: later,
            }),
            models: vec![],
        }
    }

    fn target<'a>(decision: &Decision<'a, &'a str>) -> Option<&'a str> {
        match decision {
            Decision::Switch { to, .. } => Some(**to),
            Decision::Stay(_) => None,
        }
    }

    #[test]
    fn standing_picks_the_window_nearest_its_line() {
        let s = policy().standing(&reading(80.0, 93.0), Utc::now()).unwrap();
        assert_eq!((s.window.as_str(), s.room, s.headroom), ("7d", 2.0, 7.0));
        let s = policy().standing(&reading(89.0, 10.0), Utc::now()).unwrap();
        assert_eq!((s.window.as_str(), s.room), ("5h", 1.0));
        assert_eq!(policy().standing(&Reading::default(), Utc::now()), None);
    }

    #[test]
    fn a_window_past_its_reset_counts_as_empty() {
        let mut r = reading(100.0, 10.0);
        r.five_hour.as_mut().unwrap().resets_at = Some(Utc::now() - Duration::minutes(1));
        assert!(r.has_reset_since(Utc::now() - Duration::hours(1), Utc::now()));
        assert!(!r.has_reset_since(Utc::now(), Utc::now()));
        let s = policy().standing(&r, Utc::now()).unwrap();
        assert_eq!((s.window.as_str(), s.pct), ("7d", 10.0));
    }

    #[test]
    fn watched_models_count_against_the_weekly_line() {
        let mut r = reading(10.0, 10.0);
        r.models.push(ModelWindow {
            name: "Fable".into(),
            window: Window {
                pct: 99.0,
                resets_at: None,
            },
        });
        assert!(policy().standing(&r, Utc::now()).unwrap().room > 0.0);
        let watching = Policy {
            models: vec!["fable".into()],
            ..policy()
        };
        let s = watching.standing(&r, Utc::now()).unwrap();
        assert_eq!((s.window.as_str(), s.room), ("Fable", -4.0));
    }

    #[test]
    fn stays_while_under_the_line() {
        let others = [("b", reading(0.0, 0.0))];
        let d = policy().decide(Some(&reading(89.0, 50.0)), &others, false, Utc::now());
        assert_eq!(target(&d), None);
    }

    #[test]
    fn stays_without_a_reading() {
        let others = [("b", reading(0.0, 0.0))];
        assert_eq!(
            target(&policy().decide(None, &others, false, Utc::now())),
            None
        );
    }

    #[test]
    fn switches_to_the_account_with_most_room() {
        let others = [
            ("b", reading(60.0, 20.0)),
            ("c", reading(5.0, 30.0)),
            ("d", reading(0.0, 94.0)),
        ];
        let d = policy().decide(Some(&reading(91.0, 50.0)), &others, false, Utc::now());
        assert_eq!(target(&d), Some("c"));
    }

    #[test]
    fn does_not_ping_pong_between_accounts_near_the_line() {
        let others = [("b", reading(85.0, 50.0))];
        let d = policy().decide(Some(&reading(91.0, 50.0)), &others, false, Utc::now());
        assert_eq!(target(&d), None);
    }

    #[test]
    fn leaves_an_exhausted_account_for_any_real_quota() {
        let others = [("b", reading(95.0, 50.0)), ("c", reading(99.0, 50.0))];
        let d = policy().decide(Some(&reading(100.0, 50.0)), &others, false, Utc::now());
        assert_eq!(target(&d), Some("b"));
        let others = [("c", reading(99.0, 50.0))];
        let d = policy().decide(Some(&reading(100.0, 50.0)), &others, false, Utc::now());
        assert_eq!(target(&d), None);
    }

    #[test]
    fn cooldown_holds_unless_exhausted() {
        let others = [("b", reading(0.0, 0.0))];
        let d = policy().decide(Some(&reading(92.0, 50.0)), &others, true, Utc::now());
        assert_eq!(target(&d), None);
        let d = policy().decide(Some(&reading(100.0, 50.0)), &others, true, Utc::now());
        assert_eq!(target(&d), Some("b"));
    }
}
