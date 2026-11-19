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

use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use block2::{DynBlock, RcBlock};
use chrono::Local;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAttributedStringNSStringDrawing, NSBezierPath, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFont, NSFontAttributeName, NSFontWeightBold,
    NSFontWeightRegular, NSForegroundColorAttributeName, NSImage, NSImageSymbolConfiguration,
    NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRect,
    NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use objc2_user_notifications::{
    UNNotification, UNNotificationPresentationOptions, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::app::{App, Live, Snapshot};
use crate::notify;
use crate::store::{AccountId, Config, Icon, MenuBar, Store};
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
    // Both hold their delegate weakly; the handler lives as long as the app.
    UNUserNotificationCenter::currentNotificationCenter()
        .setDelegate(Some(ProtocolObject::from_ref(&*handler)));
    application.setDelegate(Some(ProtocolObject::from_ref(&*handler)));
    notify::authorize();
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
    MenuBar(MenuBar),
    Icon(Icon),
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

    // While bytt is in front (its menu open), macOS shows its notifications only if
    // the delegate asks for that; the test notification is sent from the menu.
    unsafe impl UNUserNotificationCenterDelegate for Handler {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List,));
        }
    }

    // Opening Bytt while it runs (Spotlight, Applications) brings a hidden icon back.
    unsafe impl NSApplicationDelegate for Handler {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _application: &NSApplication, _visible: bool) -> bool {
            if self.ivars().borrow().config.menubar == MenuBar::Hidden {
                self.configure(|c| c.menubar = MenuBar::default());
                self.tick_now();
            }
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
            Action::MenuBar(MenuBar::Hidden) => {
                let choice = alert(
                    mtm,
                    "Hide bytt from the menu bar?",
                    "bytt keeps running and switching. To show it again, open Bytt from \
                     Spotlight or Applications.",
                    &["Cancel", "Hide"],
                );
                if choice == 1 {
                    self.configure(|c| c.menubar = MenuBar::Hidden);
                }
            }
            Action::MenuBar(shown) => self.configure(|c| c.menubar = shown),
            Action::Icon(icon) => self.configure(|c| c.icon = icon),
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
        // The active account's highest usage, when the menu bar should show it.
        let mut button_title = String::new();
        // The active account's fullest window, for the ring and the tooltip.
        let mut fullest: Option<Ring> = None;
        let mut tooltip = String::from("bytt");
        match &state.snapshot {
            None => {
                menu.info(root, "Reading usage…");
            }
            Some(snapshot) => {
                for row in ui::rows(snapshot) {
                    let gauges = row
                        .usage
                        .map_or_default(|r| ui::gauges(r, &policy, snapshot.now));
                    if row.active
                        && let Some(top) = gauges.iter().max_by(|a, b| a.pct.total_cmp(&b.pct))
                    {
                        fullest = Some(Ring {
                            pct: top.pct,
                            elapsed: top.elapsed,
                            over: top.level == Level::Over,
                        });
                        tooltip = format!("{}: {} {:.0}%", row.email, top.label, top.pct);
                        if let (Some(reset), Some(at)) = (&top.reset, top.resets_at) {
                            let _ = write!(
                                tooltip,
                                ", resets in {reset} ({})",
                                at.with_timezone(&Local).format("%a %H:%M")
                            );
                        }
                    }
                    if row.active
                        && let Some(top) = gauges.iter().max_by(|a, b| a.pct.total_cmp(&b.pct))
                        && (config.menubar == MenuBar::Usage
                            || matches!(top.level, Level::Near | Level::Over))
                    {
                        button_title = format!("{:.0}%", top.pct);
                    }

                    let account = menu.item(root, row.email, None, row.active);
                    let sub =
                        NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(row.email));
                    sub.setAutoenablesItems(false);
                    for gauge in &gauges {
                        let reset = gauge
                            .resets_at
                            .filter(|_| gauge.reset.is_some())
                            .map_or_default(|at| {
                                format!(
                                    " · resets in {}, {}",
                                    gauge.reset.as_deref().unwrap_or_default(),
                                    at.with_timezone(&Local).format("%a %H:%M")
                                )
                            });
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

                    // A bar per window under the account, so usage shows without
                    // opening it.
                    if gauges.is_empty() {
                        let notice = row.notice().map_or_default(|(text, _)| text);
                        menu.info(root, &notice).setIndentationLevel(1);
                    }
                    for gauge in &gauges {
                        menu.gauge(root, gauge).setIndentationLevel(1);
                    }
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
        settings_menu(&mut menu, root, config, state.snapshot.as_ref());

        root.addItem(&NSMenuItem::separatorItem(mtm));
        if let Some(on) = login_item() {
            menu.item(root, "Open at Login", Some(Action::LoginItem(!on)), on);
        }
        let shown = menu.submenu(root, "Menu Bar");
        for choice in MenuBar::ALL {
            let title = match choice {
                MenuBar::Usage => "Usage",
                MenuBar::Icon => "Icon, Usage When Near the Limit",
                MenuBar::Hidden => "Hidden…",
            };
            menu.item(
                &shown,
                title,
                Some(Action::MenuBar(choice)),
                config.menubar == choice,
            );
        }
        // Each icon drawn as the menu bar would show it, so choosing is seeing.
        shown.addItem(&NSMenuItem::separatorItem(mtm));
        menu.info(&shown, "Icon");
        let usage = config.menubar == MenuBar::Usage;
        let sample = if !usage {
            ""
        } else if button_title.is_empty() {
            "57%"
        } else {
            &button_title
        };
        let height = NSFont::menuFontOfSize(0.0).pointSize() + 6.0;
        for icon in Icon::ALL {
            // Without a number there is nothing to show "Number Only" with.
            if icon == Icon::None && !usage {
                continue;
            }
            let item = menu.item(
                &shown,
                icon.title(),
                Some(Action::Icon(icon)),
                config.icon == icon,
            );
            let ring = fullest.unwrap_or(Ring {
                pct: 57.0,
                elapsed: Some(0.7),
                over: false,
            });
            item.setImage(bar_image(icon, sample, Some(ring), height).as_deref());
        }
        if let Some((at, line)) = &state.checked {
            menu.info(root, &format!("Last check {}: {line}", at.format("%H:%M")));
        }
        menu.item(root, "Refresh", Some(Action::Refresh), false);
        menu.item(root, "Quit bytt", Some(Action::Quit), false);

        state.item.setVisible(config.menubar != MenuBar::Hidden);
        if let Some(button) = state.item.button(mtm) {
            // The icon stands in until there is a number.
            let height = NSStatusBar::systemStatusBar().thickness();
            button.setImage(bar_image(config.icon, &button_title, fullest, height).as_deref());
            button.setTitle(&NSString::from_str(""));
            button.setToolTip(Some(&NSString::from_str(&tooltip)));
        }
    }
}

/// Claude's orange, for the badge.
const ORANGE: (f64, f64, f64) = (217.0 / 255.0, 119.0 / 255.0, 87.0 / 255.0);

/// The SF Symbol behind `icon`.
fn symbol_name(icon: Icon) -> &'static str {
    match icon {
        Icon::Ring | Icon::Badge | Icon::Arrows | Icon::None => "arrow.left.arrow.right",
        Icon::Circle => "arrow.left.arrow.right.circle.fill",
        Icon::Cycle => "arrow.triangle.2.circlepath",
        Icon::Gauge => "gauge.with.dots.needle.67percent",
    }
}

fn symbol(icon: Icon, config: &NSImageSymbolConfiguration) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol_name(icon)),
        Some(&NSString::from_str("bytt")),
    )
    .and_then(|image| image.imageWithSymbolConfiguration(config))
}

/// What the menu bar shows for `icon` with `text` (the usage, or empty for the icon
/// alone), `height` points tall. Drawn at display time, so it follows the menu bar's
/// light or dark appearance.
/// `ring` is the active account's fullest window; without one the ring is empty.
fn bar_image(icon: Icon, text: &str, ring: Option<Ring>, height: f64) -> Option<Retained<NSImage>> {
    let font = NSFont::menuBarFontOfSize(0.0);
    let orange = NSColor::colorWithSRGBRed_green_blue_alpha(ORANGE.0, ORANGE.1, ORANGE.2, 1.0);
    let paint = |color: &NSColor| {
        NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_slice(&[color]))
    };
    let regular =
        NSImageSymbolConfiguration::configurationWithPointSize_weight(font.pointSize(), unsafe {
            NSFontWeightRegular
        });

    if icon == Icon::Ring {
        return Some(ring_image(text, ring, height));
    }
    if text.is_empty() {
        // The icon alone: a template, so AppKit tints it, except the orange badge.
        if icon == Icon::Badge {
            let image = symbol(
                icon,
                &regular.configurationByApplyingConfiguration(&paint(&orange)),
            )?;
            image.setTemplate(false);
            return Some(image);
        }
        let image = symbol(icon, &regular)?;
        image.setTemplate(true);
        return Some(image);
    }

    let string = attributed(&[(text, &NSColor::labelColor())], &font);
    let text_size = string.size();
    let text_y = ((height - text_size.height) / 2.0 - 0.5).max(0.0);
    let (size, draw) = match icon {
        Icon::Badge => {
            // Small orange arrows tucked over the number's top-right corner, the way
            // Teams badges its icon.
            let small =
                NSImageSymbolConfiguration::configurationWithPointSize_weight(8.0, unsafe {
                    NSFontWeightBold
                })
                .configurationByApplyingConfiguration(&paint(&orange));
            let badge = symbol(icon, &small);
            let badge_size = badge.as_ref().map_or(NSSize::new(0.0, 0.0), |b| b.size());
            // The badge overlaps the text by a little less than half its width.
            let overlap = badge_size.width * 0.4;
            let size = NSSize::new(text_size.width + badge_size.width - overlap, height);
            let draw = RcBlock::new(move |_rect: NSRect| -> Bool {
                string.drawAtPoint(NSPoint::new(0.0, (text_y - 1.0).max(0.0)));
                if let Some(badge) = &badge {
                    badge.drawInRect(NSRect::new(
                        NSPoint::new(text_size.width - overlap, height - badge_size.height - 2.0),
                        badge_size,
                    ));
                }
                Bool::YES
            });
            (size, draw)
        }
        Icon::None => {
            let size = NSSize::new(text_size.width, height);
            let draw = RcBlock::new(move |_rect: NSRect| -> Bool {
                string.drawAtPoint(NSPoint::new(0.0, text_y));
                Bool::YES
            });
            (size, draw)
        }
        Icon::Ring | Icon::Arrows | Icon::Circle | Icon::Cycle | Icon::Gauge => {
            // The icon, a gap, the number; both in the label color.
            let mark = symbol(
                icon,
                &regular.configurationByApplyingConfiguration(&paint(&NSColor::labelColor())),
            );
            let mark_size = mark.as_ref().map_or(NSSize::new(0.0, 0.0), |m| m.size());
            let gap = 3.0;
            let size = NSSize::new(mark_size.width + gap + text_size.width, height);
            let draw = RcBlock::new(move |_rect: NSRect| -> Bool {
                if let Some(mark) = &mark {
                    mark.drawInRect(NSRect::new(
                        NSPoint::new(0.0, ((height - mark_size.height) / 2.0).max(0.0)),
                        mark_size,
                    ));
                }
                string.drawAtPoint(NSPoint::new(mark_size.width + gap, text_y));
                Bool::YES
            });
            (size, draw)
        }
    };
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(false);
    image.setAccessibilityDescription(Some(&NSString::from_str(&format!("bytt {text}"))));
    Some(image)
}

/// A window drawn as a ring: how full, and how far through it is.
#[derive(Clone, Copy)]
struct Ring {
    pct: f64,
    elapsed: Option<f64>,
    over: bool,
}

/// A circle that fills clockwise from the top in Claude's orange (red past the switch
/// point), a needle at how far through the window it is, then `text`. Usage short of
/// the needle is under pace: there is room to spend before the reset.
fn ring_image(text: &str, ring: Option<Ring>, height: f64) -> Retained<NSImage> {
    let font = NSFont::menuBarFontOfSize(0.0);
    let string = attributed(&[(text, &NSColor::labelColor())], &font);
    let text_size = if text.is_empty() {
        NSSize::new(0.0, 0.0)
    } else {
        string.size()
    };
    let diameter = (height - 8.0).clamp(10.0, 16.0);
    let gap = if text.is_empty() { 0.0 } else { 4.0 };
    let has_text = !text.is_empty();
    // Room either side so the outline's stroke isn't clipped.
    let pad = 1.0;
    let size = NSSize::new(pad * 2.0 + diameter + gap + text_size.width, height);
    let draw = RcBlock::new(move |_rect: NSRect| -> Bool {
        let center = NSPoint::new(pad + diameter / 2.0, height / 2.0);
        let radius = diameter / 2.0;
        let at = |degrees: f64, r: f64| {
            let radians = degrees.to_radians();
            NSPoint::new(center.x + r * radians.cos(), center.y + r * radians.sin())
        };
        if let Some(ring) = ring {
            let fill = if ring.over {
                NSColor::systemRedColor()
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(ORANGE.0, ORANGE.1, ORANGE.2, 1.0)
            };
            fill.setFill();
            let pct = ring.pct.clamp(0.0, 100.0);
            if pct >= 99.5 {
                NSBezierPath::bezierPathWithOvalInRect(circle(center, radius)).fill();
            } else if pct > 0.5 {
                let wedge = NSBezierPath::bezierPath();
                wedge.moveToPoint(center);
                wedge.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                    center,
                    radius,
                    90.0,
                    90.0 - 360.0 * pct / 100.0,
                    true,
                );
                wedge.closePath();
                wedge.fill();
            }
        }
        NSColor::labelColor().setStroke();
        let outline = NSBezierPath::bezierPathWithOvalInRect(circle(center, radius - 0.6));
        outline.setLineWidth(1.2);
        outline.stroke();
        if let Some(elapsed) = ring.and_then(|r| r.elapsed) {
            let needle = NSBezierPath::bezierPath();
            needle.moveToPoint(center);
            needle.lineToPoint(at(90.0 - 360.0 * elapsed, radius - 0.6));
            needle.setLineWidth(1.4);
            needle.stroke();
        }
        if has_text {
            let text_y = ((height - text_size.height) / 2.0 - 0.5).max(0.0);
            string.drawAtPoint(NSPoint::new(pad + diameter + gap, text_y));
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(false);
    let what = ring.map_or_else(
        || "bytt".to_string(),
        |r| format!("bytt, {:.0}% used", r.pct),
    );
    image.setAccessibilityDescription(Some(&NSString::from_str(&what)));
    image
}

fn circle(center: NSPoint, radius: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(center.x - radius, center.y - radius),
        NSSize::new(radius * 2.0, radius * 2.0),
    )
}

/// Auto-Switch on/off and its settings submenu.
#[allow(clippy::too_many_lines)]
fn settings_menu(
    menu: &mut Builder<'_>,
    root: &NSMenu,
    config: &Config,
    snapshot: Option<&Snapshot>,
) {
    let mtm = menu.mtm;
    let policy = config.policy();
    let lines = if same(policy.threshold, policy.weekly_threshold) {
        format!("{:.0}%", policy.threshold)
    } else {
        format!(
            "{:.0}% 5-Hour, {:.0}% Weekly",
            policy.threshold, policy.weekly_threshold
        )
    };
    menu.item(
        root,
        &format!("Auto-Switch at {lines}"),
        Some(Action::Auto(!config.auto)),
        config.auto,
    );
    // Each setting's title carries its value, so the list reads without opening it.
    let settings = menu.submenu(root, "Auto-Switch Settings");
    let percents = |current: f64| (10..=20).map(move |n| (f64::from(n) * 5.0, current));
    let minutes = |seconds: u64| match seconds {
        0 => "No Wait".to_string(),
        s => format!("{} min", s / 60),
    };
    let five = menu.submenu(
        &settings,
        &format!("Switch at 5-Hour Usage: {:.0}%", policy.threshold),
    );
    for (pct, current) in percents(config.threshold) {
        menu.item(
            &five,
            &format!("{pct:.0}%"),
            Some(Action::Threshold(pct)),
            same(pct, current),
        );
    }
    let weekly = menu.submenu(
        &settings,
        &format!("Switch at Weekly Usage: {:.0}%", policy.weekly_threshold),
    );
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
    let under = menu.submenu(
        &settings,
        &format!(
            "Only Switch to Accounts Under: {:.0}%",
            policy.threshold - policy.margin
        ),
    );
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
    let cooldown = menu.submenu(
        &settings,
        &format!("Wait Between Switches: {}", minutes(config.cooldown)),
    );
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
    let interval = menu.submenu(
        &settings,
        &format!("Check Usage Every: {}", minutes(config.interval)),
    );
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

    /// A line of text that does nothing when clicked. Disabled, so it never highlights,
    /// but `AppKit` greys out a disabled item's plain title; an attributed title keeps
    /// the color it is given, so these read like any other item.
    fn info(&mut self, menu: &NSMenu, title: &str) -> Retained<NSMenuItem> {
        let font = NSFont::menuFontOfSize(0.0);
        self.styled(menu, &[(title, &NSColor::labelColor())], &font)
    }

    /// An info line made of differently colored pieces.
    fn styled(
        &mut self,
        menu: &NSMenu,
        parts: &[(&str, &NSColor)],
        font: &NSFont,
    ) -> Retained<NSMenuItem> {
        let title: String = parts.iter().map(|(text, _)| *text).collect();
        let item = self.item(menu, &title, None, false);
        item.setAttributedTitle(Some(&attributed(parts, font)));
        item.setEnabled(false);
        item
    }

    /// One usage window as a line: a bar colored by how near it is to the switch
    /// point, then the figure and the window's name. Bars first, so they line up.
    fn gauge(&mut self, menu: &NSMenu, gauge: &ui::Gauge) -> Retained<NSMenuItem> {
        let size = NSFont::menuFontOfSize(0.0).pointSize();
        let font =
            unsafe { NSFont::monospacedDigitSystemFontOfSize_weight(size, NSFontWeightRegular) };
        let color = match gauge.level {
            Level::Ok => NSColor::systemGreenColor(),
            Level::Near => NSColor::systemOrangeColor(),
            Level::Over => NSColor::systemRedColor(),
            Level::Dim => NSColor::secondaryLabelColor(),
        };
        let filled = "━".repeat(gauge.filled);
        let empty = "━".repeat(10 - gauge.filled);
        let reset = gauge
            .reset
            .as_ref()
            .map_or_default(|reset| format!(" · resets in {reset}"));
        let text = format!("  {}  {}{reset}", gauge.figure(), gauge.label);
        self.styled(
            menu,
            &[
                (&filled, &color),
                (&empty, &NSColor::tertiaryLabelColor()),
                (&text, &NSColor::labelColor()),
            ],
            &font,
        )
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

/// `parts` joined, each in its own color, all in `font`.
fn attributed(parts: &[(&str, &NSColor)], font: &NSFont) -> Retained<NSAttributedString> {
    let whole = NSMutableAttributedString::new();
    for (text, color) in parts {
        let keys = unsafe { [NSForegroundColorAttributeName, NSFontAttributeName] };
        let values: [&AnyObject; 2] = [*color, font];
        let attributes = NSDictionary::from_slices(&keys, &values);
        let part = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(text),
                Some(&attributes),
            )
        };
        whole.appendAttributedString(&part);
    }
    Retained::into_super(whole)
}

fn dot(level: Level) -> &'static str {
    match level {
        Level::Ok => "🟢",
        Level::Near => "🟡",
        Level::Over => "🔴",
        Level::Dim => "⚪️",
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

/// Whether bytt opens at login; `None` unless it runs from /Applications.
fn login_item() -> Option<bool> {
    if !std::env::current_exe().is_ok_and(|p| p.starts_with("/Applications")) {
        return None;
    }
    match unsafe { SMAppService::mainAppService().status() } {
        SMAppServiceStatus::Enabled | SMAppServiceStatus::RequiresApproval => Some(true),
        // A never-registered app reports NotFound rather than NotRegistered.
        SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => Some(false),
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
    let Some(home) = std::env::home_dir() else {
        return;
    };
    let marker = home.join(".bytt").join("login-item");
    if !marker.exists() && login_item() == Some(false) {
        let _ = set_login_item(true);
        let _ = fs::write(marker, "");
    }
}
