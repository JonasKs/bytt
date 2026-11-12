//! The one thread that touches the Keychain, the network and Claude Code's files on
//! behalf of the TUI and the menu bar app. It runs one job at a time, so two actions
//! never interleave and the screen or menu never freezes.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use chrono::TimeDelta;

use crate::app::{Added, App, Outcome, Scope, Snapshot, Switched, WouldDiscard};
use crate::notify;
use crate::store::{AccountId, Config};

pub enum Job {
    Refresh,
    Switch {
        id: AccountId,
        force: bool,
    },
    Add,
    SetEnabled(AccountId, bool),
    Remove(AccountId),
    /// One automatic check with these settings; switches if it is time.
    Check(Box<Config>),
}

pub enum Note {
    Info(String),
    Error(String),
    /// The switch would throw away an unsaved login; ask first.
    Discard {
        id: AccountId,
        why: String,
    },
    /// What an automatic check came to, when it did not switch.
    Checked(String),
}

pub struct Finished {
    pub note: Option<Note>,
    pub snapshot: Result<Snapshot, String>,
    pub switcher: Switcher,
}

/// What is switching automatically on this Mac right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Switcher {
    /// The menu bar app is running (it switches when `auto` is on in the config).
    pub app: bool,
    /// `bytt auto` runs in a terminal (it switches regardless of the config).
    pub cli: bool,
}

impl Switcher {
    pub fn now() -> Self {
        let running = |pattern: &str| {
            Command::new("/usr/bin/pgrep")
                .args(["-qf", pattern])
                .status()
                .is_ok_and(|status| status.success())
        };
        Self {
            app: running(r"Bytt\.app/Contents/MacOS/bytt$"),
            cli: running("bytt auto"),
        }
    }
}

pub fn spawn(app: App) -> (Sender<Job>, Receiver<Finished>) {
    let (jobs, inbox) = mpsc::channel();
    let (outbox, done) = mpsc::channel();
    thread::spawn(move || run(app, &inbox, &outbox));
    (jobs, done)
}

fn run(mut app: App, inbox: &Receiver<Job>, outbox: &Sender<Finished>) {
    let ttl = TimeDelta::seconds(60);
    let scope = Scope {
        active_ttl: ttl,
        others_ttl: ttl,
        disabled: true,
    };
    for job in inbox {
        let note = work(&mut app, job);
        let snapshot = app.collect(&scope).map_err(|e| format!("{e:#}"));
        let finished = Finished {
            note,
            snapshot,
            switcher: Switcher::now(),
        };
        if outbox.send(finished).is_err() {
            return;
        }
    }
}

fn work(app: &mut App, job: Job) -> Option<Note> {
    let email = |app: &App, id: &AccountId| {
        app.store
            .registry()
            .ok()
            .and_then(|r| r.find(id).map(|a| a.email().to_string()))
            .unwrap_or_else(|| "that account".into())
    };
    let result = match job {
        Job::Refresh => return None,
        Job::Check(config) => {
            app.config = *config;
            return Some(match app.check(false) {
                Ok(Outcome::Switched { to, why }) => {
                    if app.config.notify {
                        notify::send(&format!("Switched to {to}"));
                    }
                    Note::Info(format!("Switched to {to} ({why})."))
                }
                Ok(Outcome::Held(why)) => Note::Checked(format!("holding, {why}")),
                Ok(Outcome::WouldSwitch { to, why }) => {
                    Note::Checked(format!("would switch to {to} ({why})"))
                }
                Err(e) => Note::Checked(format!("check failed: {e:#}")),
            });
        }
        Job::Switch { id, force } => match app.switch_to(&id, force) {
            Ok(Switched::AlreadyActive) => Ok(format!("Already on {}.", email(app, &id))),
            Ok(Switched::Done) => Ok(format!(
                "Switched to {}. Running Claude Code sessions follow within about 30 seconds.",
                email(app, &id)
            )),
            Err(e) if e.is::<WouldDiscard>() => {
                return Some(Note::Discard {
                    id,
                    why: e.to_string(),
                });
            }
            Err(e) => Err(e),
        },
        Job::Add => app.add().map(|added| match added {
            Added::New(a) => format!("Added {}.", a.email()),
            Added::Updated(a) => format!("Updated the saved login for {}.", a.email()),
        }),
        Job::SetEnabled(id, enabled) => app.set_enabled(&id, enabled).map(|a| {
            if enabled {
                format!("{} takes part in automatic switching again.", a.email())
            } else {
                format!("{} is left out of automatic switching.", a.email())
            }
        }),
        Job::Remove(id) => app
            .remove(&id)
            .map(|a| format!("Removed {} and its saved login.", a.email())),
    };
    Some(match result {
        Ok(message) => Note::Info(message),
        Err(e) => Note::Error(format!("{e:#}")),
    })
}
