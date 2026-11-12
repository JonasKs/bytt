//! `bytt` with no arguments: one screen that shows where every account stands, what
//! automatic switching is doing about it, and lets you change either with the arrow
//! keys (or hjkl).
//!
//! Anything that touches the Keychain, the network or `brew` runs on a worker thread,
//! one job at a time, so the screen never freezes and two actions never interleave.
//! Settings are written straight to `~/.bytt/config.toml`, which `bytt auto` re-reads.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::app::{App, Live, Snapshot};
use crate::store::{AccountId, Config, Store};
use crate::ui::{self, Gauge, Level, Row};
use crate::usage::Policy;
use crate::worker::{self, Finished, Job, Note, Switcher};

/// How often the screen asks for new readings. Each account is still read at most
/// once a minute, whatever this says.
const REFRESH_EVERY: Duration = Duration::from_secs(30);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR: usize = 20;
const BAR_F: f64 = 20.0;

pub fn run(app: App) -> Result<()> {
    let mut screen = Screen::new(&app);
    let (jobs, done) = worker::spawn(app);
    screen.send(&jobs, Job::Refresh, "Reading usage");
    let mut terminal = ratatui::init();
    let result = screen.run(&mut terminal, &jobs, &done);
    ratatui::restore();
    result
}

/// What the cursor can rest on.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    /// The auto-switch line at the top; → opens its settings.
    AutoSwitch,
    Account(AccountId),
    Add,
    Setting(Setting),
}

#[derive(Debug, Clone, PartialEq)]
enum Setting {
    Auto,
    FiveHour,
    Weekly,
    Model(String),
    Margin,
    Cooldown,
    Interval,
    Notify,
}

/// The actions under an account, opened with ⏎ or →.
struct Menu {
    id: AccountId,
    prompt: Option<(String, Level)>,
    entries: Vec<(String, Action)>,
    cursor: usize,
}

#[derive(Clone, Copy)]
enum Action {
    Switch { force: bool },
    SetEnabled(bool),
    AskRemove,
    Remove,
    Close,
}

struct Screen {
    store: Store,
    config: Config,
    snapshot: Option<Snapshot>,
    cursor: Option<Item>,
    /// The auto-switch settings page is open; `back` is where the cursor returns.
    settings: bool,
    back: Option<Item>,
    menu: Option<Menu>,
    /// Jobs sent and not yet finished.
    pending: usize,
    /// What the user is waiting for; empty for background refreshes.
    busy: String,
    note: Option<Note>,
    switcher: Switcher,
    quitting: bool,
    tick: usize,
}

impl Screen {
    fn new(app: &App) -> Self {
        Self {
            config: app.config.clone(),
            store: app.store.clone(),
            snapshot: None,
            cursor: None,
            settings: false,
            back: None,
            menu: None,
            pending: 0,
            busy: String::new(),
            note: None,
            switcher: Switcher::now(),
            quitting: false,
            tick: 0,
        }
    }

    /// Whether anything is switching automatically right now.
    fn auto(&self) -> bool {
        (self.config.auto && self.switcher.app) || self.switcher.cli
    }

    fn policy(&self) -> Policy {
        self.config.policy()
    }

    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        jobs: &Sender<Job>,
        done: &Receiver<Finished>,
    ) -> Result<()> {
        let mut last_refresh = Instant::now();
        loop {
            while let Ok(finished) = done.try_recv() {
                self.finish(finished);
            }
            // A switch must not be cut off halfway, so quitting waits for the worker.
            if self.quitting && self.pending == 0 {
                return Ok(());
            }
            if self.pending == 0 && last_refresh.elapsed() >= REFRESH_EVERY {
                self.send(jobs, Job::Refresh, "");
                last_refresh = Instant::now();
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.quitting = true;
                } else {
                    self.key(key.code, jobs);
                }
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    fn send(&mut self, jobs: &Sender<Job>, job: Job, busy: &str) {
        if jobs.send(job).is_ok() {
            self.pending += 1;
            if !busy.is_empty() {
                self.busy = busy.to_string();
                self.note = None;
            }
        }
    }

    fn finish(&mut self, finished: Finished) {
        self.pending = self.pending.saturating_sub(1);
        if self.pending == 0 {
            self.busy.clear();
        }
        self.switcher = finished.switcher;
        match finished.snapshot {
            Ok(snapshot) => {
                let first = self.cursor.is_none();
                self.snapshot = Some(snapshot);
                let items = self.items();
                if first || !self.cursor.as_ref().is_some_and(|c| items.contains(c)) {
                    let live = self.snapshot.as_ref().and_then(|s| s.live.account());
                    self.cursor = items
                        .iter()
                        .find(|item| matches!(item, Item::Account(id) if Some(id) == live))
                        .or_else(|| items.first())
                        .cloned();
                }
                if let Some(menu) = &self.menu
                    && !items.contains(&Item::Account(menu.id.clone()))
                {
                    self.menu = None;
                }
            }
            Err(error) => self.note = Some(Note::Error(error)),
        }
        match finished.note {
            Some(Note::Discard { id, why }) => {
                self.settings = false;
                self.cursor = Some(Item::Account(id.clone()));
                self.menu = Some(Menu {
                    id,
                    prompt: Some((format!("{why} Switching discards it."), Level::Near)),
                    entries: vec![
                        ("Cancel".into(), Action::Close),
                        (
                            "Switch anyway and discard it".into(),
                            Action::Switch { force: true },
                        ),
                    ],
                    cursor: 0,
                });
            }
            Some(Note::Checked(_)) | None => {}
            Some(note) => self.note = Some(note),
        }
    }

    /// What the cursor can move over on the page that is open.
    fn items(&self) -> Vec<Item> {
        if !self.settings {
            let mut items = vec![Item::AutoSwitch];
            items.extend(
                self.snapshot
                    .iter()
                    .flat_map(|s| s.registry.accounts.iter())
                    .map(|a| Item::Account(a.id.clone())),
            );
            items.push(Item::Add);
            return items;
        }
        let mut items = vec![Item::Setting(Setting::Auto)];
        items.push(Item::Setting(Setting::FiveHour));
        items.push(Item::Setting(Setting::Weekly));
        items.extend(
            self.models()
                .into_iter()
                .map(|m| Item::Setting(Setting::Model(m))),
        );
        items.extend(
            [
                Setting::Margin,
                Setting::Cooldown,
                Setting::Interval,
                Setting::Notify,
            ]
            .map(Item::Setting),
        );
        items
    }

    /// Every model with a limit of its own: those in readings, and those watched.
    fn models(&self) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        let read = self
            .snapshot
            .iter()
            .flat_map(|s| s.state.accounts.values())
            .filter_map(|s| s.reading.as_ref())
            .flat_map(|r| r.models.iter().map(|m| m.name.clone()));
        for name in read.chain(self.config.models.iter().cloned()) {
            if !models.iter().any(|m| m.eq_ignore_ascii_case(&name)) {
                models.push(name);
            }
        }
        models
    }

    fn email(&self, id: &AccountId) -> String {
        self.snapshot
            .as_ref()
            .and_then(|s| s.registry.find(id))
            .map_or_else(|| "that account".into(), |a| a.email().to_string())
    }

    fn move_cursor(&mut self, by: isize) {
        let items = self.items();
        let at = self
            .cursor
            .as_ref()
            .and_then(|c| items.iter().position(|i| i == c))
            .unwrap_or(0);
        let to = std::cmp::min(at.saturating_add_signed(by), items.len().saturating_sub(1));
        self.cursor = items.get(to).cloned();
    }

    fn key(&mut self, code: KeyCode, jobs: &Sender<Job>) {
        if self.menu.is_some() {
            self.menu_key(code, jobs);
            return;
        }
        let (up, down, left, right, enter) = (
            matches!(code, KeyCode::Up | KeyCode::Char('k')),
            matches!(code, KeyCode::Down | KeyCode::Char('j')),
            matches!(code, KeyCode::Left | KeyCode::Char('h')),
            matches!(code, KeyCode::Right | KeyCode::Char('l')),
            matches!(code, KeyCode::Enter | KeyCode::Char(' ')),
        );
        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            if self.settings {
                self.settings = false;
                self.cursor = self.back.take();
                let items = self.items();
                if !self.cursor.as_ref().is_some_and(|c| items.contains(c)) {
                    self.cursor = items.first().cloned();
                }
            } else {
                self.quitting = true;
            }
        } else if up {
            self.move_cursor(-1);
        } else if down {
            self.move_cursor(1);
        } else if matches!(code, KeyCode::Home | KeyCode::Char('g')) {
            self.cursor = self.items().first().cloned();
        } else if matches!(code, KeyCode::End | KeyCode::Char('G')) {
            self.cursor = self.items().last().cloned();
        } else if left || right || enter {
            let step = if left { -1 } else { i8::from(right) };
            match self.cursor.clone() {
                Some(Item::AutoSwitch) if !left => {
                    self.back = self.cursor.take();
                    self.settings = true;
                    self.cursor = self.items().first().cloned();
                }
                Some(Item::Account(id)) if !left => self.open_menu(id),
                Some(Item::Add) if !left => self.add(jobs),
                Some(Item::Setting(setting)) => self.change(&setting, step),
                _ => {}
            }
        }
    }

    fn open_menu(&mut self, id: AccountId) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let active = snapshot.live.account() == Some(&id);
        let enabled = snapshot.registry.find(&id).is_some_and(|a| a.enabled);
        let mut entries = Vec::new();
        if !active {
            entries.push((
                "Switch Claude Code to it".into(),
                Action::Switch { force: false },
            ));
        }
        entries.push(if enabled {
            (
                "Leave it out of auto-switch".into(),
                Action::SetEnabled(false),
            )
        } else {
            ("Include it in auto-switch".into(), Action::SetEnabled(true))
        });
        entries.push(("Remove it…".into(), Action::AskRemove));
        let prompt = active.then(|| ("Claude Code is using this account.".into(), Level::Dim));
        self.menu = Some(Menu {
            id,
            prompt,
            entries,
            cursor: 0,
        });
    }

    fn menu_key(&mut self, code: KeyCode, jobs: &Sender<Job>) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        match code {
            KeyCode::Up | KeyCode::Char('k') => menu.cursor = menu.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                menu.cursor = std::cmp::min(menu.cursor + 1, menu.entries.len() - 1);
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h' | 'q') => self.menu = None,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l' | ' ') => {
                let id = menu.id.clone();
                let action = menu.entries[menu.cursor].1;
                self.act(id, action, jobs);
            }
            _ => {}
        }
    }

    fn act(&mut self, id: AccountId, action: Action, jobs: &Sender<Job>) {
        let email = self.email(&id);
        if !self.busy.is_empty() && !matches!(action, Action::Close | Action::AskRemove) {
            return; // one action at a time
        }
        self.menu = None;
        match action {
            Action::Close => {}
            Action::Switch { force } => {
                self.send(
                    jobs,
                    Job::Switch { id, force },
                    &format!("Switching to {email}"),
                );
            }
            Action::SetEnabled(enabled) => self.send(jobs, Job::SetEnabled(id, enabled), "Saving"),
            Action::AskRemove => {
                self.menu = Some(Menu {
                    id,
                    prompt: Some((
                        "Forget this account and delete its saved login?".into(),
                        Level::Near,
                    )),
                    entries: vec![
                        ("Keep it".into(), Action::Close),
                        ("Remove it".into(), Action::Remove),
                    ],
                    cursor: 0,
                });
            }
            Action::Remove => self.send(jobs, Job::Remove(id), &format!("Removing {email}")),
        }
    }

    fn add(&mut self, jobs: &Sender<Job>) {
        if !self.busy.is_empty() {
            return;
        }
        match self.snapshot.as_ref().map(|s| &s.live) {
            Some(Live::Managed(id)) => {
                self.note = Some(Note::Info(format!(
                    "Claude Code is logged in as {}, which is saved. Run /login in Claude Code \
                     with the next account (not /logout, that revokes the saved login), then \
                     come back here.",
                    self.email(id)
                )));
            }
            Some(Live::LoggedOut) => {
                self.note = Some(Note::Error(
                    "Claude Code is not logged in. Run /login in Claude Code first.".into(),
                ));
            }
            _ => self.send(jobs, Job::Add, "Saving the current login"),
        }
    }

    /// ← is -1, → is +1, ⏎ is 0 (toggle where that means something).
    fn change(&mut self, setting: &Setting, step: i8) {
        let flip = |on: bool| match step {
            1 => true,
            -1 => false,
            _ => !on,
        };
        let percent =
            |value: f64| (((value / 5.0).round() + f64::from(step)) * 5.0).clamp(50.0, 100.0);
        let before = self.config.clone();
        let config = &mut self.config;
        match setting {
            Setting::Auto => config.auto = flip(config.auto),
            Setting::FiveHour => config.threshold = percent(config.threshold),
            Setting::Weekly => {
                config.weekly_threshold =
                    Some(percent(config.weekly_threshold.unwrap_or(config.threshold)));
            }
            Setting::Model(name) => {
                let watched = config.models.iter().any(|m| m.eq_ignore_ascii_case(name));
                if flip(watched) {
                    if !watched {
                        config.models.push(name.clone());
                    }
                } else {
                    config.models.retain(|m| !m.eq_ignore_ascii_case(name));
                }
            }
            // Shown as the percentage a target must be under (line minus margin), so →
            // raises that percentage by lowering the margin.
            Setting::Margin => {
                config.margin = (config.margin - 5.0 * f64::from(step)).clamp(0.0, 50.0);
            }
            Setting::Cooldown => {
                config.cooldown = config
                    .cooldown
                    .saturating_add_signed(60 * i64::from(step))
                    .clamp(0, 3600);
            }
            Setting::Interval => {
                config.interval = config
                    .interval
                    .saturating_add_signed(60 * i64::from(step))
                    .clamp(60, 3600);
            }
            Setting::Notify if step == 0 => {
                crate::notify::send(
                    "Test notification. bytt shows one like this when it switches account.",
                );
                self.note = Some(Note::Info(
                    "Sent a test notification. Nothing showed up? Allow notifications for \
                     Script Editor in System Settings → Notifications."
                        .into(),
                ));
                return;
            }
            Setting::Notify => config.notify = flip(config.notify),
        }
        if *config != before
            && let Err(error) = self.store.save_config(config)
        {
            self.note = Some(Note::Error(format!(
                "Could not save the settings: {error:#}"
            )));
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(2, 1));
        let status = self.status_block();
        let [top, _, body, _, footer, help] = Layout::vertical([
            Constraint::Length(u16::try_from(status.len()).unwrap_or(4)),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(status), top);

        let (lines, at) = self.body();
        // Keep the cursor in view, with a little context below it.
        let height = usize::from(body.height);
        let scroll = (at + 4).saturating_sub(height);
        frame.render_widget(
            Paragraph::new(lines).scroll((u16::try_from(scroll).unwrap_or(0), 0)),
            body,
        );
        frame.render_widget(
            Paragraph::new(self.footer()).wrap(Wrap { trim: true }),
            footer,
        );
        frame.render_widget(Paragraph::new(self.help()).dim(), help);
    }

    /// The top of the screen: is auto-switch on, and what does it make of things now.
    fn status_block(&self) -> Vec<Line<'static>> {
        let label = |text: &str| Span::from(format!("{text:<13}")).dim();
        let mut auto = vec![Span::from(format!("{:<13}", "Auto-switch"))];
        if self.auto() {
            auto.push(Span::from("● on").green().bold());
            auto.push(Span::from(format!("   {}", self.lines_summary())).dim());
        } else if self.config.auto {
            auto.push(Span::from("○ off").yellow().bold());
            auto.push(Span::from("   open the bytt app, it does the switching").dim());
        } else {
            auto.push(Span::from("○ off").yellow().bold());
        }
        if !self.settings {
            auto.push(Span::from("   → settings").cyan());
        }
        let mut lines = vec![
            Line::from(vec![Span::from("   "), Span::from("bytt").bold().cyan()]),
            Line::default(),
            self.item_line(&Item::AutoSwitch, auto),
        ];
        let rest = match &self.snapshot {
            None => vec![vec![label("Now"), Span::from("reading usage…").dim()]],
            Some(snapshot) => {
                let (now, next) = self.outlook(snapshot);
                vec![
                    [vec![label("Now")], now].concat(),
                    [vec![label("Next up")], next].concat(),
                ]
            }
        };
        lines.extend(
            rest.into_iter()
                .map(|spans| Line::from([vec![Span::from("   ")], spans].concat())),
        );
        lines
    }

    fn lines_summary(&self) -> String {
        let policy = self.policy();
        let mut text = if (policy.weekly_threshold - policy.threshold).abs() < f64::EPSILON {
            format!("switches at {:.0}% usage", policy.threshold)
        } else {
            format!(
                "switches at {:.0}% 5-hour or {:.0}% weekly usage",
                policy.threshold, policy.weekly_threshold
            )
        };
        if !policy.models.is_empty() {
            text = format!("{text} · counting {}", policy.models.join(", "));
        }
        text
    }

    /// "Now" and "Next up", styled.
    fn outlook(&self, snapshot: &Snapshot) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let style = |says: ui::Says| {
            let head = Span::from(says.text);
            vec![
                if says.warn { head.yellow() } else { head },
                Span::from(if says.detail.is_empty() {
                    String::new()
                } else {
                    format!("   {}", says.detail)
                })
                .dim(),
            ]
        };
        let (now, next) = ui::outlook(snapshot, &self.config, self.auto());
        (style(now), style(next))
    }

    /// The scrolling part, and the line the cursor is on.
    fn body(&self) -> (Vec<Line<'static>>, usize) {
        let mut at = 0;
        let mark = |lines: &[Line], item: &Item, at: &mut usize| {
            if self.cursor.as_ref() == Some(item) {
                *at = lines.len();
            }
        };
        if self.settings {
            let mut lines = vec![section("Auto-switch settings")];
            for item in self.items() {
                if let Item::Setting(setting) = &item {
                    mark(&lines, &item, &mut at);
                    lines.push(self.setting_line(&item, setting));
                }
            }
            return (lines, at);
        }

        let mut lines = vec![section("Accounts")];

        if let Some(snapshot) = &self.snapshot {
            let rows = ui::rows(snapshot);
            let label_width = rows
                .iter()
                .filter_map(|r| r.usage)
                .flat_map(|u| ui::gauges(u, &self.policy(), snapshot.now))
                .map(|g| g.label.chars().count())
                .max()
                .unwrap_or(2);
            for row in &rows {
                let item = Item::Account(row.id.clone());
                mark(&lines, &item, &mut at);
                self.account(&mut lines, row, snapshot, label_width);
                lines.push(Line::default());
            }
        } else {
            lines.push(Line::from("  Reading usage…").dim());
            lines.push(Line::default());
        }

        mark(&lines, &Item::Add, &mut at);
        let unsaved = self
            .snapshot
            .as_ref()
            .is_some_and(|s| matches!(s.live, Live::Unmanaged | Live::Unverified(_)));
        let add = if unsaved {
            Span::from("+ Save the login Claude Code is using now").yellow()
        } else {
            Span::from("+ Add an account")
        };
        lines.push(self.item_line(&Item::Add, vec![add]));
        (lines, at)
    }

    /// `›` and emphasis on the cursor's line.
    fn item_line(&self, item: &Item, spans: Vec<Span<'static>>) -> Line<'static> {
        let here = self.cursor.as_ref() == Some(item) && self.menu.is_none();
        let marker = if here {
            Span::from(" › ").cyan().bold()
        } else {
            Span::from("   ")
        };
        let mut line = Line::from([vec![marker], spans].concat());
        if here {
            line = line.bold();
        }
        line
    }

    fn account(
        &self,
        lines: &mut Vec<Line<'static>>,
        row: &Row<'_>,
        snapshot: &Snapshot,
        label_width: usize,
    ) {
        let item = Item::Account(row.id.clone());
        let mut head = vec![if row.active {
            Span::from("● ").green()
        } else {
            Span::from("  ")
        }];
        head.push(Span::from(row.email.to_string()));
        if row.active {
            head.push(Span::from("  in use").green());
        }
        let org = row
            .organization
            .filter(|org| *org != format!("{}'s Organization", row.email));
        let who: Vec<&str> = org.into_iter().chain(row.plan).collect();
        if !who.is_empty() {
            head.push(Span::from(format!("  {}", who.join(" · "))).dim());
        }
        if !row.enabled {
            head.push(Span::from("  left out of auto-switch").yellow());
        }
        lines.push(self.item_line(&item, head));

        let indent = || Span::from("       ");
        if let Some(reading) = row.usage {
            for gauge in ui::gauges(reading, &self.policy(), snapshot.now) {
                let mut spans = vec![
                    indent(),
                    Span::from(format!("{:<label_width$}  ", gauge.label)).dim(),
                ];
                spans.extend(bar(&gauge));
                spans.push(Span::styled(
                    format!(" {}", gauge.figure()),
                    level_style(gauge.level),
                ));
                if let (Some(reset), Some(at)) = (&gauge.reset, gauge.resets_at) {
                    let local = at.with_timezone(&Local).format("%a %H:%M");
                    spans.push(Span::from(format!("   resets in {reset}, {local}")).dim());
                }
                lines.push(Line::from(spans));
            }
        }
        if let Some((text, level)) = row.notice() {
            lines.push(Line::from(vec![
                indent(),
                Span::styled(text, level_style(level)),
            ]));
        }
        if let Some(age) = row.age(snapshot.now) {
            lines.push(Line::from(vec![indent(), Span::from(age).dim()]));
        }

        if let Some(menu) = self.menu.as_ref().filter(|m| &m.id == row.id) {
            if let Some((prompt, level)) = &menu.prompt {
                lines.push(Line::from(vec![
                    indent(),
                    Span::styled(prompt.clone(), level_style(*level)),
                ]));
            }
            for (index, (text, _)) in menu.entries.iter().enumerate() {
                let line = if index == menu.cursor {
                    Line::from(vec![
                        indent(),
                        Span::from("› ").cyan(),
                        Span::from(text.clone()),
                    ])
                    .bold()
                } else {
                    Line::from(vec![
                        indent(),
                        Span::from("  "),
                        Span::from(text.clone()).dim(),
                    ])
                };
                lines.push(line);
            }
        }
    }

    fn setting_line(&self, item: &Item, setting: &Setting) -> Line<'static> {
        let config = &self.config;
        let minutes = |seconds: u64| match seconds {
            0 => "off".to_string(),
            s if s % 60 == 0 => format!("{} min", s / 60),
            s => format!("{s} s"),
        };
        let (name, value, style) = match setting {
            Setting::Auto => (
                "Auto-switch".to_string(),
                if self.config.auto { "on" } else { "off" }.to_string(),
                if self.config.auto {
                    Style::new().green()
                } else {
                    Style::new().yellow()
                },
            ),
            Setting::FiveHour => (
                "Switch when 5-hour usage hits".into(),
                format!("{:.0}%", config.threshold),
                Style::new(),
            ),
            Setting::Weekly => (
                "Switch when weekly usage hits".into(),
                format!(
                    "{:.0}%",
                    config.weekly_threshold.unwrap_or(config.threshold)
                ),
                Style::new(),
            ),
            Setting::Model(model) => {
                let watched = self.policy().watches(model);
                (
                    format!("Switch on {model} usage too"),
                    if watched { "yes" } else { "no" }.to_string(),
                    if watched {
                        Style::new()
                    } else {
                        Style::new().dim()
                    },
                )
            }
            Setting::Margin => (
                "Only switch to accounts under".into(),
                {
                    let policy = self.policy();
                    let five = policy.threshold - policy.margin;
                    let weekly = policy.weekly_threshold - policy.margin;
                    if (five - weekly).abs() < f64::EPSILON {
                        format!("{five:.0}%")
                    } else {
                        format!("{five:.0}% / {weekly:.0}%")
                    }
                },
                Style::new(),
            ),
            Setting::Cooldown => (
                "Wait between switches".into(),
                minutes(config.cooldown),
                Style::new(),
            ),
            Setting::Interval => (
                "Check usage every".into(),
                minutes(config.interval),
                Style::new(),
            ),
            Setting::Notify => (
                "Notification on switch".into(),
                if config.notify { "on" } else { "off" }.to_string(),
                Style::new(),
            ),
        };
        let here = self.cursor.as_ref() == Some(item);
        let value = if here {
            Span::styled(format!("‹ {value} ›"), style.cyan())
        } else {
            Span::styled(format!("  {value}  "), style)
        };
        self.item_line(item, vec![Span::from(format!("{name:<32}")), value])
    }

    /// What just happened, what is happening, or what the cursor's item does.
    fn footer(&self) -> Line<'static> {
        let spin = SPINNER[self.tick / 2 % 10];
        if self.quitting {
            return Line::from(format!("{spin} Finishing up…")).dim();
        }
        if !self.busy.is_empty() {
            return Line::from(format!("{spin} {}…", self.busy));
        }
        match &self.note {
            Some(Note::Info(text)) => return Line::from(text.clone()).green(),
            Some(Note::Error(text)) => return Line::from(text.clone()).red(),
            Some(Note::Discard { .. } | Note::Checked(_)) | None => {}
        }
        let hint = match &self.cursor {
            _ if self.menu.is_some() => "",
            Some(Item::AutoSwitch) => {
                "→ to turn auto-switch on or off, and choose when it switches."
            }
            Some(Item::Account(_)) => {
                "⏎ for this account's actions: switch to it, leave it out of auto-switch, remove it."
            }
            Some(Item::Add) => {
                "Saves the account Claude Code is logged in as, so bytt can switch to it."
            }
            Some(Item::Setting(setting)) => match setting {
                Setting::Auto => {
                    "Switch accounts automatically when usage gets high. The bytt menu bar app does the switching while it runs."
                }
                Setting::FiveHour => {
                    "Move to another account when the current one has used this much of its 5-hour limit."
                }
                Setting::Weekly => {
                    "Move to another account when the current one has used this much of its weekly limit."
                }
                Setting::Model(_) => {
                    "This model has its own weekly limit. Yes: also switch when it reaches the weekly percentage above."
                }
                Setting::Margin => {
                    "Never switch to an account that is already this full, so bytt doesn't hop to one about to run out too."
                }
                Setting::Cooldown => {
                    "After switching, stay at least this long before switching again (unless the account runs out)."
                }
                Setting::Interval => "How often the background switcher checks usage.",
                Setting::Notify => {
                    "A macOS notification each time bytt switches account. ⏎ sends a test one."
                }
            },
            None => "",
        };
        Line::from(hint).dim()
    }

    fn help(&self) -> Line<'static> {
        let text = match &self.cursor {
            _ if self.menu.is_some() => "↑↓ move   ⏎ choose   ← back",
            Some(Item::Setting(Setting::Notify)) => "↑↓ move   ←→ off/on   ⏎ test   esc back",
            Some(Item::Setting(Setting::Auto | Setting::Model(_))) => {
                "↑↓ move   ←→ off/on   esc back"
            }
            Some(Item::Setting(_)) => "↑↓ move   ←→ change   esc back",
            Some(Item::AutoSwitch) => "↑↓ move   → settings   q quit",
            _ => "↑↓ move   ⏎ open   q quit",
        };
        Line::from(text)
    }
}

fn section(title: &str) -> Line<'static> {
    Line::from(format!("   {}", title.to_uppercase()))
        .dim()
        .bold()
}

/// `██████░░░░░░`, coloured by how close the window is to its switch point.
fn bar(gauge: &Gauge) -> Vec<Span<'static>> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (gauge.pct / 100.0 * BAR_F).round().clamp(0.0, BAR_F) as usize;
    vec![
        Span::styled("█".repeat(filled), level_style(gauge.level)),
        Span::from("░".repeat(BAR - filled)).dark_gray(),
    ]
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Ok => Style::new().fg(Color::Green),
        Level::Near => Style::new().fg(Color::Yellow),
        Level::Over => Style::new().fg(Color::Red),
        Level::Dim => Style::new().dim(),
    }
}
