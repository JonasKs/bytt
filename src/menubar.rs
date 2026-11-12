//! The menu bar app: every account's usage, switching, adding and removing, and the
//! automatic switcher itself, which runs here while `auto` is on in the config.
//!
//! It is a plain `NSStatusItem` with an `NSMenu` that is rebuilt whenever something
//! changes. All Keychain and network work goes through the shared worker thread; a
//! timer on the main thread collects its results.
//!
//! `AppKit` is reached through `objc2`, which marks much of it `unsafe`; this module is
//! the one place bytt allows that.
#![allow(unsafe_code)]
// NSUserNotificationCenter: see `notify`.
#![allow(deprecated)]

use std::cell::RefCell;
use std::fs;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use chrono::Local;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSUserNotification,
    NSUserNotificationCenter, NSUserNotificationCenterDelegate,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::app::{App, Live, Snapshot};
use crate::notify;
use crate::store::{AccountId, Config, Store};
use crate::ui::{self, Level};
use crate::worker::{self, Finished, Job, Note};

/// How often usage is re-read while nothing else happens.
const REFRESH_EVERY: Duration = Duration::from_secs(120);
/// How often the config file is checked for edits made elsewhere (the TUI, an editor).
const CONFIG_EVERY: Duration = Duration::from_secs(3);

pub fn run(app: App) {
    let mtm = MainThreadMarker::new().expect("the menu bar app runs on the main thread");
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let store = app.store.clone();
    let config = app.config.clone();
    let (jobs, done) = worker::spawn(app);

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("arrow.left.arrow.right"),
            Some(&NSString::from_str("bytt")),
        );
        button.setImage(image.as_deref());
    }
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("bytt"));
    menu.setAutoenablesItems(false);
    item.setMenu(Some(&menu));

    let handler = Handler::new(
        mtm,
        State {
            jobs,
            done,
            config_seen: modified(&store),
            store,
            config,
            snapshot: None,
            pending: 0,
            busy: None,
            last_refresh: Instant::now(),
            last_check: None,
            last_config_look: Instant::now(),
            checked: None,
            actions: Vec::new(),
            item,
            menu,
            dirty: true,
        },
    );
    // The center holds its delegate weakly; the handler lives as long as the app.
    unsafe {
        NSUserNotificationCenter::defaultUserNotificationCenter()
            .setDelegate(Some(ProtocolObject::from_ref(&*handler)));
    }
    handler.send(Job::Refresh, Some("Reading usage"));
    first_launch_login_item();

    // Common modes, so the menu keeps updating while it is open.
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.5,
            &handler,
            sel!(tick:),
            None,
            true,
        )
    };
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    handler.tick_now();

    application.run();
}

/// What a menu item does; its tag is the index into `State::actions`.
enum Action {
    Switch(AccountId),
    SetEnabled(AccountId, bool),
    Remove(AccountId),
    Add,
    Auto(bool),
    Threshold(f64),
    Weekly(f64),
    Model(String, bool),
    Margin(f64),
    Cooldown(u64),
    Interval(u64),
    Notify(bool),
    TestNotification,
    OpenConfig,
    LoginItem(bool),
    Refresh,
    Quit,
}

struct State {
    jobs: Sender<Job>,
    done: Receiver<Finished>,
    store: Store,
    config: Config,
    config_seen: Option<SystemTime>,
    snapshot: Option<Snapshot>,
    pending: usize,
    busy: Option<String>,
    last_refresh: Instant,
    last_check: Option<Instant>,
    last_config_look: Instant,
    /// The last automatic check that did not switch: when, and what it said.
    checked: Option<(chrono::DateTime<Local>, String)>,
    actions: Vec<Action>,
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    dirty: bool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Handler has no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ByttMenuHandler"]
    #[ivars = RefCell<State>]
    struct Handler;

    impl Handler {
        #[unsafe(method(pick:))]
        fn pick(&self, sender: &NSMenuItem) {
            if let Ok(index) = usize::try_from(sender.tag()) {
                self.act(index);
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.tick_now();
        }
    }

    unsafe impl NSObjectProtocol for Handler {}

    // A running app's own notifications are only shown when its delegate says so;
    // without this, the test notification from the menu never appears.
    unsafe impl NSUserNotificationCenterDelegate for Handler {
        #[unsafe(method(userNotificationCenter:shouldPresentNotification:))]
        fn should_present(
            &self,
            _center: &NSUserNotificationCenter,
            _notification: &NSUserNotification,
        ) -> bool {
            true
        }
    }
);

impl Handler {
    fn new(mtm: MainThreadMarker, state: State) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::new(state));
        unsafe { msg_send![super(this), init] }
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::from(self)
    }

    fn send(&self, job: Job, busy: Option<&str>) {
        let mut state = self.ivars().borrow_mut();
        if state.jobs.send(job).is_ok() {
            state.pending += 1;
            if let Some(busy) = busy {
                state.busy = Some(busy.to_string());
                state.dirty = true;
            }
        }
    }

    fn tick_now(&self) {
        let mut notes = Vec::new();
        {
            let mut state = self.ivars().borrow_mut();
            while let Ok(finished) = state.done.try_recv() {
                state.pending = state.pending.saturating_sub(1);
                if state.pending == 0 {
                    state.busy = None;
                }
                match finished.snapshot {
                    Ok(snapshot) => state.snapshot = Some(snapshot),
                    Err(error) => notes.push(Note::Error(error)),
                }
                notes.extend(finished.note);
                state.dirty = true;
            }

            if state.last_config_look.elapsed() >= CONFIG_EVERY {
                state.last_config_look = Instant::now();
                let seen = modified(&state.store);
                if seen != state.config_seen {
                    state.config_seen = seen;
                    if let Ok(config) = state.store.config() {
                        state.config = config;
                        state.dirty = true;
                    }
                }
            }
        }
        for note in notes {
            self.note(note);
        }

        let job = {
            let state = self.ivars().borrow();
            let interval = Duration::from_secs(state.config.interval);
            if state.pending > 0 {
                None
            } else if state.config.auto
                && state.last_check.is_none_or(|at| at.elapsed() >= interval)
            {
                Some(Job::Check(Box::new(state.config.clone())))
            } else if state.last_refresh.elapsed() >= REFRESH_EVERY {
                Some(Job::Refresh)
            } else {
                None
            }
        };
        if let Some(job) = job {
            {
                let mut state = self.ivars().borrow_mut();
                if matches!(job, Job::Check(_)) {
                    state.last_check = Some(Instant::now());
                }
                state.last_refresh = Instant::now();
            }
            self.send(job, None);
        }

        let dirty = std::mem::take(&mut self.ivars().borrow_mut().dirty);
        if dirty {
            self.rebuild();
        }
    }

    fn note(&self, note: Note) {
        match note {
            Note::Info(_) => {}
            Note::Checked(line) => {
                self.ivars().borrow_mut().checked = Some((Local::now(), line));
            }
            Note::Error(error) => {
                alert(self.mtm(), "bytt", &error, &["OK"]);
            }
            Note::Discard { id, why } => {
                let choice = alert(
                    self.mtm(),
                    "Switch anyway?",
                    &format!("{why} Switching discards it."),
                    &["Cancel", "Switch Anyway"],
                );
                if choice == 1 {
                    self.send(Job::Switch { id, force: true }, Some("Switching"));
                }
            }
        }
    }

    fn act(&self, index: usize) {
        let Some(action) = self.ivars().borrow_mut().actions.get_mut(index).map(|a| {
            // Actions are rebuilt with the menu; take this one out and leave a no-op.
            std::mem::replace(a, Action::Refresh)
        }) else {
            return;
        };
        let mtm = self.mtm();
        match action {
            Action::Switch(id) => self.send(Job::Switch { id, force: false }, Some("Switching")),
            Action::SetEnabled(id, on) => self.send(Job::SetEnabled(id, on), Some("Saving")),
            Action::Remove(id) => {
                let email = self.email(&id);
                let choice = alert(
                    mtm,
                    &format!("Remove {email}?"),
                    "bytt forgets this account and deletes its saved login. Claude Code itself \
                     is not touched.",
                    &["Cancel", "Remove"],
                );
                if choice == 1 {
                    self.send(Job::Remove(id), Some("Removing"));
                }
            }
            Action::Add => self.add(),
            Action::Auto(on) => self.configure(|c| c.auto = on),
            Action::Threshold(pct) => self.configure(|c| c.threshold = pct),
            Action::Weekly(pct) => self.configure(|c| c.weekly_threshold = Some(pct)),
            Action::Model(name, on) => self.configure(|c| {
                c.models.retain(|m| !m.eq_ignore_ascii_case(&name));
                if on {
                    c.models.push(name.clone());
                }
            }),
            Action::Margin(margin) => self.configure(|c| c.margin = margin),
            Action::Cooldown(seconds) => self.configure(|c| c.cooldown = seconds),
            Action::Interval(seconds) => self.configure(|c| c.interval = seconds),
            Action::Notify(on) => self.configure(|c| c.notify = on),
            Action::TestNotification => {
                notify::send(
                    "Test notification. bytt shows one like this when it switches account.",
                );
            }
            Action::OpenConfig => {
                let path = self.ivars().borrow().store.config_path();
                let _ = Command::new("/usr/bin/open").arg("-t").arg(path).status();
            }
            Action::LoginItem(on) => {
                if let Err(error) = set_login_item(on) {
                    alert(mtm, "Could not change Open at Login", &error, &["OK"]);
                }
                self.ivars().borrow_mut().dirty = true;
            }
            Action::Refresh => self.send(Job::Refresh, Some("Reading usage")),
            Action::Quit => NSApplication::sharedApplication(mtm).terminate(None),
        }
        self.tick_now();
    }

    fn configure(&self, change: impl FnOnce(&mut Config)) {
        let mut state = self.ivars().borrow_mut();
        let mut config = state.config.clone();
        change(&mut config);
        if let Err(error) = state.store.save_config(&config) {
            drop(state);
            alert(
                self.mtm(),
                "Could not save the settings",
                &format!("{error:#}"),
                &["OK"],
            );
            return;
        }
        // Turning auto-switch on checks straight away.
        if config.auto && !state.config.auto {
            state.last_check = None;
        }
        state.config_seen = modified(&state.store);
        state.config = config;
        state.dirty = true;
    }

    fn add(&self) {
        let live = self
            .ivars()
            .borrow()
            .snapshot
            .as_ref()
            .map(|s| s.live.clone());
        match live {
            Some(Live::Managed(id)) => {
                let email = self.email(&id);
                alert(
                    self.mtm(),
                    "Log in with the next account first",
                    &format!(
                        "Claude Code is logged in as {email}, which bytt already has.\n\n\
                         Run /login in Claude Code with the account you want to add (not \
                         /logout, that revokes the saved login), then choose Add Account again."
                    ),
                    &["OK"],
                );
            }
            Some(Live::LoggedOut) => {
                alert(
                    self.mtm(),
                    "Claude Code is not logged in",
                    "Run /login in Claude Code with the account you want to add, then choose \
                     Add Account again.",
                    &["OK"],
                );
            }
            _ => self.send(Job::Add, Some("Saving the current login")),
        }
    }

    fn email(&self, id: &AccountId) -> String {
        self.ivars()
            .borrow()
            .snapshot
            .as_ref()
            .and_then(|s| s.registry.find(id))
            .map_or_else(|| "that account".into(), |a| a.email().to_string())
    }

    /// Lay the menu out again from the current state.
    // A menu is a long, flat list of items; splitting it further hides the layout.
    #[allow(clippy::too_many_lines)]
    fn rebuild(&self) {
        let mtm = self.mtm();
        let mut state = self.ivars().borrow_mut();
        let state = &mut *state;
        state.actions.clear();
        state.menu.removeAllItems();
        let mut menu = Builder {
            handler: self,
            mtm,
            actions: &mut state.actions,
        };
        let root = &state.menu;
        let config = &state.config;
        let policy = config.policy();

        // What auto-switch is doing.
        let auto = if config.auto {
            let lines = if (policy.weekly_threshold - policy.threshold).abs() < f64::EPSILON {
                format!("{:.0}%", policy.threshold)
            } else {
                format!(
                    "{:.0}% 5-hour, {:.0}% weekly",
                    policy.threshold, policy.weekly_threshold
                )
            };
            format!("Auto-switch on · switches at {lines}")
        } else {
            "Auto-switch off".to_string()
        };
        menu.info(root, &auto);
        if let Some(snapshot) = &state.snapshot {
            let (now, next) = ui::outlook(snapshot, config, config.auto);
            menu.info(root, &join("Now", &now));
            menu.info(root, &join("Next up", &next));
        }
        if let Some(busy) = &state.busy {
            menu.info(root, &format!("{busy}…"));
        }
        root.addItem(&NSMenuItem::separatorItem(mtm));

        // Accounts.
        let mut button_title = String::new();
        match &state.snapshot {
            None => {
                menu.info(root, "Reading usage…");
            }
            Some(snapshot) => {
                for row in ui::rows(snapshot) {
                    let gauges = row
                        .usage
                        .map(|r| ui::gauges(r, &policy, snapshot.now))
                        .unwrap_or_default();
                    if row.active
                        && let Some(top) = gauges.iter().max_by(|a, b| a.pct.total_cmp(&b.pct))
                    {
                        button_title = format!(" {:.0}%", top.pct);
                    }

                    let account = menu.item(root, row.email, None, row.active);
                    let sub =
                        NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(row.email));
                    sub.setAutoenablesItems(false);
                    for gauge in &gauges {
                        let reset = gauge
                            .resets_at
                            .filter(|_| gauge.reset.is_some())
                            .map(|at| {
                                format!(
                                    " · resets in {}, {}",
                                    gauge.reset.as_deref().unwrap_or_default(),
                                    at.with_timezone(&Local).format("%a %H:%M")
                                )
                            })
                            .unwrap_or_default();
                        menu.info(
                            &sub,
                            &format!(
                                "{} {}  {}{reset}",
                                dot(gauge.level),
                                gauge.label,
                                gauge.figure().trim()
                            ),
                        );
                    }
                    if let Some((text, _)) = row.notice() {
                        menu.info(&sub, &text);
                    }
                    if let Some(age) = row.age(snapshot.now) {
                        menu.info(&sub, &age);
                    }
                    sub.addItem(&NSMenuItem::separatorItem(mtm));
                    let switch = menu.item(
                        &sub,
                        "Switch to This Account",
                        Some(Action::Switch(row.id.clone())),
                        false,
                    );
                    switch.setEnabled(!row.active);
                    menu.item(
                        &sub,
                        "Include in Auto-Switch",
                        Some(Action::SetEnabled(row.id.clone(), !row.enabled)),
                        row.enabled,
                    );
                    menu.item(&sub, "Remove…", Some(Action::Remove(row.id.clone())), false);
                    account.setSubmenu(Some(&sub));

                    // One line under the account, so usage shows without opening it.
                    let summary = if gauges.is_empty() {
                        row.notice().map(|(text, _)| text).unwrap_or_default()
                    } else {
                        let worst = gauges
                            .iter()
                            .map(|g| g.level)
                            .max_by_key(|l| severity(*l))
                            .unwrap_or(Level::Ok);
                        let parts: Vec<String> = gauges
                            .iter()
                            .map(|g| format!("{} {}", g.label, g.figure().trim()))
                            .collect();
                        format!("{} {}", dot(worst), parts.join("  ·  "))
                    };
                    let line = menu.info(root, &summary);
                    line.setIndentationLevel(1);
                }
                if let Some(notice) = ui::live_notice(&snapshot.live) {
                    menu.info(root, notice);
                }
            }
        }
        let unsaved = state
            .snapshot
            .as_ref()
            .is_some_and(|s| matches!(s.live, Live::Unmanaged | Live::Unverified(_)));
        menu.item(
            root,
            if unsaved {
                "Save the Current Login"
            } else {
                "Add Account…"
            },
            Some(Action::Add),
            false,
        );
        root.addItem(&NSMenuItem::separatorItem(mtm));

        // Auto-switch and its settings.
        settings_menu(
            &mut menu,
            root,
            config,
            state.snapshot.as_ref(),
            state.checked.as_ref(),
        );

        root.addItem(&NSMenuItem::separatorItem(mtm));
        if let Some(on) = login_item() {
            menu.item(root, "Open at Login", Some(Action::LoginItem(!on)), on);
        }
        menu.item(root, "Refresh", Some(Action::Refresh), false);
        menu.item(root, "Quit bytt", Some(Action::Quit), false);

        if let Some(button) = state.item.button(mtm) {
            button.setTitle(&NSString::from_str(&button_title));
        }
    }
}

/// Auto-Switch on/off and its settings submenu.
#[allow(clippy::too_many_lines)]
fn settings_menu(
    menu: &mut Builder<'_>,
    root: &NSMenu,
    config: &Config,
    snapshot: Option<&Snapshot>,
    checked: Option<&(chrono::DateTime<Local>, String)>,
) {
    let mtm = menu.mtm;
    let policy = config.policy();
    menu.item(
        root,
        "Auto-Switch",
        Some(Action::Auto(!config.auto)),
        config.auto,
    );
    let settings = menu.submenu(root, "Auto-Switch Settings");
    let percents = |current: f64| (10..=20).map(move |n| (f64::from(n) * 5.0, current));
    let five = menu.submenu(&settings, "Switch When 5-Hour Usage Hits");
    for (pct, current) in percents(config.threshold) {
        menu.item(
            &five,
            &format!("{pct:.0}%"),
            Some(Action::Threshold(pct)),
            same(pct, current),
        );
    }
    let weekly = menu.submenu(&settings, "Switch When Weekly Usage Hits");
    for (pct, current) in percents(policy.weekly_threshold) {
        menu.item(
            &weekly,
            &format!("{pct:.0}%"),
            Some(Action::Weekly(pct)),
            same(pct, current),
        );
    }
    for model in models(snapshot, config) {
        let on = policy.watches(&model);
        menu.item(
            &settings,
            &format!("Switch on {model} Usage Too"),
            Some(Action::Model(model.clone(), !on)),
            on,
        );
    }
    let under = menu.submenu(&settings, "Only Switch to Accounts Under");
    for margin in (0..=8).map(|n| f64::from(n) * 5.0) {
        let five = policy.threshold - margin;
        let weekly = policy.weekly_threshold - margin;
        let title = if same(five, weekly) {
            format!("{five:.0}%")
        } else {
            format!("{five:.0}% 5-hour, {weekly:.0}% weekly")
        };
        menu.item(
            &under,
            &title,
            Some(Action::Margin(margin)),
            same(margin, config.margin),
        );
    }
    let cooldown = menu.submenu(&settings, "Wait Between Switches");
    for minutes in [0, 1, 2, 5, 10, 15, 30, 60] {
        let title = if minutes == 0 {
            "No Wait".to_string()
        } else {
            format!("{minutes} min")
        };
        menu.item(
            &cooldown,
            &title,
            Some(Action::Cooldown(minutes * 60)),
            config.cooldown == minutes * 60,
        );
    }
    let interval = menu.submenu(&settings, "Check Usage Every");
    for minutes in [1, 2, 5, 10, 15, 30, 60] {
        menu.item(
            &interval,
            &format!("{minutes} min"),
            Some(Action::Interval(minutes * 60)),
            config.interval == minutes * 60,
        );
    }
    settings.addItem(&NSMenuItem::separatorItem(mtm));
    menu.item(
        &settings,
        "Notification on Switch",
        Some(Action::Notify(!config.notify)),
        config.notify,
    );
    menu.item(
        &settings,
        "Send a Test Notification",
        Some(Action::TestNotification),
        false,
    );
    settings.addItem(&NSMenuItem::separatorItem(mtm));
    if let Some((at, line)) = checked {
        menu.info(
            &settings,
            &format!("Last check {}: {line}", at.format("%H:%M")),
        );
    }
    menu.item(
        &settings,
        "Open Config File",
        Some(Action::OpenConfig),
        false,
    );
}

/// Adds items to a menu, pointing each at the handler with its action's index as tag.
struct Builder<'a> {
    handler: &'a Handler,
    mtm: MainThreadMarker,
    actions: &'a mut Vec<Action>,
}

impl Builder<'_> {
    fn item(
        &mut self,
        menu: &NSMenu,
        title: &str,
        action: Option<Action>,
        checked: bool,
    ) -> Retained<NSMenuItem> {
        let selector = action.as_ref().map(|_| sel!(pick:));
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm),
                &NSString::from_str(title),
                selector,
                &NSString::from_str(""),
            )
        };
        if let Some(action) = action {
            item.setTag(isize::try_from(self.actions.len()).unwrap_or(isize::MAX));
            self.actions.push(action);
            let target: &AnyObject = self.handler;
            unsafe { item.setTarget(Some(target)) };
        }
        item.setState(if checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        menu.addItem(&item);
        item
    }

    /// A line of text that does nothing when clicked.
    fn info(&mut self, menu: &NSMenu, title: &str) -> Retained<NSMenuItem> {
        let item = self.item(menu, title, None, false);
        item.setEnabled(false);
        item
    }

    fn submenu(&mut self, menu: &NSMenu, title: &str) -> Retained<NSMenu> {
        let item = self.item(menu, title, None, false);
        let sub = NSMenu::initWithTitle(NSMenu::alloc(self.mtm), &NSString::from_str(title));
        sub.setAutoenablesItems(false);
        item.setSubmenu(Some(&sub));
        sub
    }
}

fn join(label: &str, says: &ui::Says) -> String {
    if says.detail.is_empty() {
        format!("{label}: {}", says.text)
    } else {
        format!("{label}: {} · {}", says.text, says.detail)
    }
}

fn dot(level: Level) -> &'static str {
    match level {
        Level::Ok => "🟢",
        Level::Near => "🟡",
        Level::Over => "🔴",
        Level::Dim => "⚪️",
    }
}

fn severity(level: Level) -> u8 {
    match level {
        Level::Dim => 0,
        Level::Ok => 1,
        Level::Near => 2,
        Level::Over => 3,
    }
}

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < f64::EPSILON
}

/// Every model with a limit of its own: those in readings, and those watched.
fn models(snapshot: Option<&Snapshot>, config: &Config) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    let read = snapshot
        .into_iter()
        .flat_map(|s| s.state.accounts.values())
        .filter_map(|s| s.reading.as_ref())
        .flat_map(|r| r.models.iter().map(|m| m.name.clone()));
    for name in read.chain(config.models.iter().cloned()) {
        if !models.iter().any(|m| m.eq_ignore_ascii_case(&name)) {
            models.push(name);
        }
    }
    models
}

fn modified(store: &Store) -> Option<SystemTime> {
    fs::metadata(store.config_path())
        .and_then(|m| m.modified())
        .ok()
}

/// A modal alert; returns the index of the button pressed.
fn alert(mtm: MainThreadMarker, title: &str, text: &str, buttons: &[&str]) -> usize {
    NSApplication::sharedApplication(mtm).activate();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(text));
    // AppKit puts the first button on the right and makes it the default; bytt lists
    // buttons left to right, so add them in reverse.
    for button in buttons.iter().rev() {
        alert.addButtonWithTitle(&NSString::from_str(button));
    }
    let pressed = alert.runModal() - NSAlertFirstButtonReturn;
    let pressed = usize::try_from(pressed).unwrap_or(0);
    buttons.len().saturating_sub(1).saturating_sub(pressed)
}

/// Whether bytt opens at login; `None` outside a bundle, where it cannot.
fn login_item() -> Option<bool> {
    match unsafe { SMAppService::mainAppService().status() } {
        SMAppServiceStatus::Enabled | SMAppServiceStatus::RequiresApproval => Some(true),
        SMAppServiceStatus::NotRegistered => Some(false),
        _ => None,
    }
}

fn set_login_item(on: bool) -> Result<(), String> {
    let service = unsafe { SMAppService::mainAppService() };
    let result = unsafe {
        if on {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    result.map_err(|e| e.localizedDescription().to_string())
}

/// The first time the installed app runs, open it at login too. Turning that off in
/// the menu sticks, because the marker stays.
fn first_launch_login_item() {
    let installed = std::env::current_exe().is_ok_and(|p| p.starts_with("/Applications"));
    let Some(home) = std::env::home_dir() else {
        return;
    };
    let marker = home.join(".bytt").join("login-item");
    if installed && !marker.exists() && login_item() == Some(false) {
        let _ = set_login_item(true);
        let _ = fs::write(marker, "");
    }
}
