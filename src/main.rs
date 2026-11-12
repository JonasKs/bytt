#![cfg_attr(test, allow(clippy::float_cmp))]

mod api;
mod app;
mod claude;
mod credential;
mod fsx;
mod keychain;
mod lock;
mod menubar;
mod notify;
mod store;
mod tui;
mod ui;
mod usage;
mod worker;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeDelta};
use clap::{Parser, Subcommand};

use crate::app::{Added, App, Outcome, Scope, Switched};
use crate::claude::Claude;
use crate::store::{Account, AccountId, Store};

/// Keep several Claude Code accounts and swap between them before you hit a limit.
#[derive(Parser)]
#[command(name = "bytt", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the account Claude Code is logged in as
    Add,
    /// Show every account and its usage
    #[command(alias = "ls")]
    List {
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Switch Claude Code to another account
    Switch {
        /// Account number or email; the account with the most room when omitted
        account: Option<String>,
        /// Switch even if the current login is not saved and would be lost
        #[arg(long)]
        force: bool,
    },
    /// Watch usage and switch before the active account reaches its limit
    Auto {
        /// Check once and exit
        #[arg(long)]
        once: bool,
        /// Report what would happen without switching
        #[arg(long)]
        dry_run: bool,
    },
    /// Keep an account out of automatic switching
    Disable { account: String },
    /// Let an account take part in automatic switching again
    Enable { account: String },
    /// Forget an account and delete its saved login
    Remove { account: String },
    /// Show where the configuration file is
    Config,
    /// Show a test notification, like the one sent on a switch
    #[command(hide = true)]
    Notify,
    /// Run the menu bar app (what opening Bytt.app does)
    #[command(hide = true)]
    Menubar,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bytt: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = std::env::home_dir().context("could not find your home directory")?;
    let store = Store::open(home.join(".bytt"))?;
    let config = store.config()?;
    let mut app = App {
        store,
        claude: Claude::for_current_user()?,
        secrets: Box::new(keychain::Keychain),
        api: Box::new(api::Http::new()),
        config,
    };

    // Opening Bytt.app sets BYTT_APP (LSEnvironment in its Info.plist).
    if cli.command.is_none() && std::env::var_os("BYTT_APP").is_some() {
        menubar::run(app);
        return Ok(());
    }
    let Some(command) = cli.command else {
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            return tui::run(app);
        }
        return list(&app, false);
    };
    let resolve = |target: &str| -> Result<AccountId> {
        Ok(app.store.registry()?.resolve(target)?.id.clone())
    };
    match command {
        Cmd::Add => add(&app),
        Cmd::List { json } => list(&app, json),
        Cmd::Switch { account, force } => switch(&app, account.as_deref(), force),
        Cmd::Auto { once, dry_run } => auto(&mut app, once, dry_run),
        Cmd::Disable { account } => {
            let account = app.set_enabled(&resolve(&account)?, false)?;
            println!("{} is out of automatic switching.", account.email());
            Ok(())
        }
        Cmd::Enable { account } => {
            let account = app.set_enabled(&resolve(&account)?, true)?;
            println!(
                "{} takes part in automatic switching again.",
                account.email()
            );
            Ok(())
        }
        Cmd::Remove { account } => {
            let account = app.remove(&resolve(&account)?)?;
            println!("Removed {} and its saved login.", account.email());
            Ok(())
        }
        Cmd::Menubar => {
            menubar::run(app);
            Ok(())
        }
        Cmd::Notify => {
            notify::send("Test notification. bytt shows one like this when it switches account.");
            Ok(())
        }
        Cmd::Config => {
            println!("{}", app.store.config_path().display());
            Ok(())
        }
    }
}

fn describe(account: &Account) -> String {
    match account.organization() {
        Some(organization) => format!("{} ({organization})", account.email()),
        None => account.email().to_string(),
    }
}

fn add(app: &App) -> Result<()> {
    let (verb, account) = match app.add()? {
        Added::New(account) => ("Added", account),
        Added::Updated(account) => ("Updated the saved login for", account),
    };
    println!("{verb} {}.", describe(&account));
    if app.store.registry()?.accounts.len() == 1 {
        println!(
            "\nTo add another account, run /login in Claude Code with it (do not /logout \
             first, that revokes the login you just saved), then run `bytt add` again."
        );
    }
    Ok(())
}

fn list(app: &App, json: bool) -> Result<()> {
    let ttl = TimeDelta::seconds(60);
    let snapshot = app.collect(&Scope {
        active_ttl: ttl,
        others_ttl: ttl,
        disabled: true,
    })?;
    if json {
        println!("{}", ui::json(&snapshot)?);
    } else {
        print!("{}", ui::table(&snapshot, &app.config.policy()));
    }
    Ok(())
}

fn switch(app: &App, target: Option<&str>, force: bool) -> Result<()> {
    let registry = app.store.registry()?;
    let id: AccountId = match target {
        Some(target) => registry.resolve(target)?.id.clone(),
        None => {
            let ttl = TimeDelta::seconds(60);
            let snapshot = app.collect(&Scope {
                active_ttl: ttl,
                others_ttl: ttl,
                disabled: false,
            })?;
            let policy = app.config.policy();
            let roomiest = App::candidates(&snapshot)
                .into_iter()
                .filter_map(|(id, reading)| {
                    Some((id, policy.standing(&reading, snapshot.now)?.room))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
            match roomiest {
                Some((id, _)) => id,
                None => bail!("no other account has a usage reading; name one to switch to"),
            }
        }
    };
    let account = registry.find(&id).context("no such account")?;
    let switched = app.switch_to(&id, force).map_err(|error| {
        if error.is::<app::WouldDiscard>() {
            anyhow::anyhow!("{error} (Pass --force to switch anyway.)")
        } else {
            error
        }
    })?;
    match switched {
        Switched::AlreadyActive => println!("Already on {}.", describe(account)),
        Switched::Done => println!(
            "Switched to {}. Running Claude Code sessions follow within about 30 seconds.",
            describe(account)
        ),
    }
    Ok(())
}

fn auto(app: &mut App, once: bool, dry_run: bool) -> Result<()> {
    let log = |line: &str| println!("{} {line}", Local::now().format("%Y-%m-%d %H:%M:%S"));
    if !once {
        let count = app.store.registry()?.accounts.len();
        log(&format!(
            "watching {count} account{}, checking every {}s",
            if count == 1 { "" } else { "s" },
            app.config.interval
        ));
    }
    // Only changes are logged: a quiet log means nothing has moved.
    let mut last = String::new();
    loop {
        match app.store.config() {
            Ok(config) => app.config = config,
            Err(error) => log(&format!("keeping the previous configuration: {error:#}")),
        }
        let line = match app.check(dry_run) {
            Ok(Outcome::Held(why)) => format!("holding, {why}"),
            Ok(Outcome::WouldSwitch { to, why }) => format!("would switch to {to} ({why})"),
            Ok(Outcome::Switched { to, why }) => {
                if app.config.notify {
                    notify::send(&format!("Switched to {to}"));
                }
                format!("switched to {to} ({why})")
            }
            Err(error) => format!("check failed: {error:#}"),
        };
        if line != last {
            log(&line);
            last = line;
        }
        if once {
            return Ok(());
        }
        sleep(Duration::from_secs(app.config.interval));
    }
}
