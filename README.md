# bytt

Keep several Claude Code accounts on one Mac and swap between them from the menu bar,
by hand or automatically before the one you're on hits its usage limit.

Made for me, by agents. Feel free to use.

- The menu bar shows the active account's usage. Open it for every account's 5-hour
  and weekly usage; click one to switch. Running Claude Code sessions follow within
  about 30 seconds; no restart, no `/login`.
- Turn on Auto-Switch and bytt moves you to the account with the most room left when
  the active one reaches 90%.
- Logins stay in your macOS Keychain. bytt talks only to Anthropic.

Apple Silicon Macs, macOS 14 or later.

## Install

```sh
brew install --cask jonasks/tap/bytt
```

Open bytt from Applications once. It lives in the menu bar (⇆) and opens at login from
then on.

## Add your accounts

Log in to Claude Code with your first account, then choose **Add Account…** in the
menu. For each further account, run `/login` in Claude Code with it, then **Add
Account…** again. Don't `/logout` first: that revokes the login bytt just saved.

## The menu

```
⇆ 11%
├─ Auto-switch on · switches at 90%
├─ Now: on anna@example.com · 5h 83%, switches at 90%
├─ Next up: bob@example.com · 7d 9%
├─ ✓ anna@example.com                 ▸  usage, reset times, switch, include, remove
│      🟡 5h 83%  ·  7d 18%
├─   bob@example.com                  ▸
│      🟢 7d 9%
├─ Add Account…
├─ Auto-Switch                         on/off
├─ Auto-Switch Settings               ▸
├─ Open at Login
├─ Refresh
└─ Quit bytt
```

🟢 is well below the switch point, 🟡 is getting close, 🔴 is at or past it. An
account without a 5-hour figure has no 5-hour limit (Ultra).

## Switch automatically

Turn on **Auto-Switch**. While bytt runs it checks usage every ten minutes. When the
active account's 5-hour or weekly usage reaches the switch point, bytt moves to the
account with the most room left, as long as that one is under 80%. When every account
is near its limit it stays put, unless the active one has run out completely. After a
switch it waits five minutes before switching again.

**Auto-Switch Settings** has all of it:

| Setting | Default | |
|---|---|---|
| Switch when 5-hour usage hits | 90% | |
| Switch when weekly usage hits | 90% | |
| Switch on Fable usage too | off | Models with their own weekly limit |
| Only switch to accounts under | 80% | So bytt doesn't hop to one about to run out too |
| Wait between switches | 5 min | Unless the account runs out |
| Check usage every | 10 min | |
| Notification on switch | on | With a test button |

An account's submenu can leave it out of automatic switching.

## In a terminal

`bytt` in a terminal opens the same thing as a full-screen view, driven by the arrow
keys (or `hjkl`). For scripts:

```sh
bytt list             # every account and its usage (also what `bytt` prints when piped)
bytt list --json
bytt switch           # switch to the account with the most room left
bytt switch 2         # switch to account 2
bytt switch anna@     # or by (part of) the email
bytt add              # save the account Claude Code is logged in as
bytt auto             # the automatic switcher, without the menu bar app
```

## Configuration

The settings live in `~/.bytt/config.toml`; the menu and the terminal view edit it,
and you can too. bytt picks up changes within seconds:

```toml
auto = false             # switch automatically while the menu bar app runs
threshold = 90           # switch when 5-hour usage hits this %
# weekly_threshold = 95  # ...or weekly usage hits this % (default: threshold)
models = []              # also switch on these models' own weekly usage, e.g. ["Fable"]
margin = 10              # only switch to accounts at least this far below those %s
interval = 600           # seconds between checks
cooldown = 300           # seconds to wait between switches
notify = true            # macOS notification on switch
```

## When a login dies

An account whose saved login can no longer be renewed shows `needs a new login` and is
never switched to. Log in with it in Claude Code and choose **Add Account…** again.

## How it works

Claude Code keeps its login in the Keychain item `Claude Code-credentials`, and who
that login belongs to in the `oauthAccount` section of `~/.claude.json`. bytt keeps a
copy of each account's login under the Keychain service `bytt`, and on a switch writes
the target's into both places. It holds Claude Code's own lock files while it does, so
a switch never collides with Claude Code renewing its token.

Before it overwrites the current login, bytt saves it to its account. If the login
can't be confirmed as that account's (you ran `/login` with an account bytt doesn't
know), it asks before discarding it; `bytt switch --force` doesn't ask.

bytt never renews the login Claude Code is using; Claude Code does that itself, and
bytt picks the result up. Saved logins of the other accounts are renewed when they
expire, and the new token is stored before it is used.

## Build from source

Requires Rust and [just](https://just.systems).

```sh
just install     # build Bytt.app, copy it to /Applications and open it
just run list    # run the CLI from source
just check       # what CI runs
```

## Uninstall

```sh
brew uninstall --cask bytt
rm -rf ~/.bytt
security delete-generic-password -s bytt   # once per saved account
```

## License

MIT
