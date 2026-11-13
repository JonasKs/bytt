# bytt

Multi-account switcher for Claude Code with automatic swapping before a usage limit.
Rust menu bar app (plus CLI and terminal view) for Apple Silicon macOS 14+.

`README.md` is external and user-facing. Maintainer notes, debugging findings and
release mechanics belong in this file.

## Build & Run

```bash
just install                     # Bytt.app → /Applications, opened (the menu bar app)
just bundle                      # target/Bytt.app, signed with your Developer ID if present
just run                         # the TUI, from source
just run list                    # the plain table
just run auto --once --dry-run   # one automatic check, no switching
just check                       # fmt + clippy -D warnings + tests + cargo deny (what CI runs)
just package                     # zipped Bytt.app like CI builds (CI also notarizes)
```

## Architecture

- `src/main.rs` — clap CLI and `bytt auto` loop. Opening Bytt.app sets `BYTT_APP` (Info.plist `LSEnvironment`) and runs the menu bar; bare `bytt` on a TTY opens the TUI, otherwise prints the list
- `src/menubar.rs` — the menu bar app: `NSStatusItem` + an `NSMenu` rebuilt on change, alerts, login item, and the automatic switcher while `auto` is on. The only module allowed `unsafe` (objc2)
- `src/worker.rs` — the thread the TUI and the menu bar send Keychain/network jobs to, one at a time
- `src/notify.rs` — notifications: `UserNotifications` inside the bundle (bytt's name and icon), `osascript` outside it
- `src/tui.rs` — ratatui terminal view: status at the top (auto on/off, what a check would decide, next pick), accounts with an action menu; → on the Auto-switch line opens a settings page edited with ←→ and saved to `config.toml`. Arrow keys / hjkl only, no letter commands. quitting waits for an in-flight switch
- `src/app.rs` — `App`: add, collect usage, switch, one auto `check`. The ownership rules live in its module docs; the tests there run against a fake Keychain, fake Anthropic and a temp home
- `src/usage.rs` — `Reading`, `Policy::standing` / `Policy::decide`: pure switching policy
- `src/store.rs` — `~/.bytt`: `accounts.json` (registry), `state.json` (readings, backoff, needs-login; a cache), `config.toml`; `Locked` is proof of holding bytt's flock
- `src/claude.rs` — Claude Code's side: `~/.claude.json` `oauthAccount` splice, its lock directories
- `src/credential.rs` — the Keychain login blob: tokens, expiry, machine-shared keys (`mcpOAuth`…)
- `src/keychain.rs` — `/usr/bin/security` wrapper behind the `Secrets` trait
- `src/api.rs` — usage, profile and token endpoints behind the `Api` trait
- `src/lock.rs` — `proper-lockfile` compatible directory locks + bytt's flock
- `src/ui.rs` — rows, gauges and the "Now / Next up" outlook shared by the menu, the TUI and `bytt list`
- `macos/` — `Info.plist`, icon (`icon.svg` → `bytt.icns`), `bundle.sh` (assembles and signs Bytt.app)

## Key Conventions

- Apple Silicon macOS only; default Claude Code profile only (`CLAUDE_CONFIG_DIR` is not supported: the brew service would not see it).
- Task runner is `just`. Conventional Commits; `feat:`/`fix:` cut releases.
- Pedantic clippy. `unsafe` is denied everywhere except `menubar.rs`, which needs it for AppKit.
- Every external effect goes through `Secrets` / `Api` (`Send + Sync`, for the worker thread) so `app.rs` tests can run the real flows.
- A switch that would discard an unsaved login fails with `app::WouldDiscard`; the CLI adds the `--force` hint, the TUI and the menu ask.
- One automatic switcher: the menu bar app while `auto = true`, or `bytt auto` for headless use. Don't run both.
- A login enters an account's slot only when it is known to be that account's: same refresh token as the slot, or `/api/oauth/profile` says so. Otherwise the switch refuses (`--force` overrides).
- bytt never refreshes the live login. Slot logins are refreshed under bytt's lock and the successor is stored before use (a refresh token works once).
- No network while Claude Code's locks are held. Usage requests run without bytt's lock too (`collect` gathers tokens, releases, fetches, merges).
- Lock order: bytt's flock, then `~/.claude/.oauth_refresh.lock`, then `~/.claude.lock`; `~/.claude.json.lock` only around the config write.

## Facts about Claude Code this depends on

Verified on a live machine (Claude Code 2.1.3xx, 2026-11):

- Login: Keychain generic password, service `Claude Code-credentials`, account `$USER`.
  JSON with `claudeAiOauth.{accessToken, refreshToken, expiresAt(ms), scopes, …}`.
  Claude Code caches it ~30 s, which is why running sessions follow a switch.
- Identity: `oauthAccount` in `~/.claude.json` (`accountUuid`, `organizationUuid`, `emailAddress`, …).
  Legacy `~/.claude/.config.json` wins when it is non-empty.
- Locks are `proper-lockfile` directories: credential locks stale after 60 s, config lock after 10 s.
- Usage: `GET https://api.anthropic.com/api/oauth/usage` with `anthropic-beta: oauth-2025-04-20`.
  `five_hour` / `seven_day` `{utilization, resets_at}`; Ultra reports `five_hour: null`.
  Per-model weekly limits are in `limits[]` with `scope.model.display_name` and `percent`.
  The endpoint rate-limits per account; bytt reads the active account at most every 30 s
  and the others every 15 min (3 min when the active one is within 15 points of a line).
- Refresh: `POST https://platform.claude.com/v1/oauth/token` with Claude Code's client id.
  Only `error: invalid_grant` on 400/401/403 means the token is dead.

## Releasing

release-please keeps a release PR open on `main`. Merging it tags `vX.Y.Z`, builds
Bytt.app on a macOS arm64 runner (`macos/bundle.sh`), signs, notarizes and staples it,
attaches `Bytt-X.Y.Z-aarch64.zip` to the release, and opens a PR on
`jonasks/homebrew-tap` that rewrites `Casks/bytt.rb`. Merge that PR to publish. The cask
also links the CLI (`binary "#{appdir}/Bytt.app/Contents/MacOS/bytt"`).

### Secrets (repo `jonasks/bytt`), the same as klepp's

| Secret | Purpose |
|--------|---------|
| `HOMEBREW_TAP_TOKEN` | Fine-grained PAT on `JonasKs/homebrew-tap`: Contents + Pull requests, read and write |
| `APPLE_CERTIFICATE` | base64 of the Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password of that `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: … (TEAMID)` |
| `APPLE_API_KEY`, `APPLE_API_ISSUER`, `APPLE_API_KEY_CONTENT` | App Store Connect API key for `notarytool` (`.p8` contents) |

Without every Apple secret the build is ad-hoc signed, the job warns, and the cask gains
a quarantine caveat. The Release workflow can be dispatched with an existing `tag` to
re-open the tap PR.

## Gotchas

- **Testing a real switch moves every running Claude Code session**, including the one
  you are working in. Use `bytt auto --once --dry-run` to test decisions.
- **Notifications need the bundle.** `UserNotifications` needs a bundle identifier and
  permission, asked for at app launch and before each send. The deprecated
  `NSUserNotification` never asks and current macOS silently drops it. While bytt is in
  front (menu open) a banner only shows because `menubar.rs` is the center's delegate
  (`willPresentNotification` → banner). `bytt notify` (hidden) sends a test one.
- **Hidden menu bar icon** comes back through `applicationShouldHandleReopen`: `open`
  on the running app sends a reopen event to it rather than starting a second copy.
- **Open at Login** appears only when run from /Applications (`just install`). Before
  its first registration the app's SMAppService status is `NotFound`, not
  `NotRegistered`; both mean "off".
- **Two identical Developer ID identities** in the login keychain make `codesign -s
  "Developer ID Application: …"` ambiguous; `just bundle` signs by hash.
- **Testing the TUI headlessly:** `tmux new-session -d -s t -x 130 -y 18 ./target/debug/bytt`,
  then `tmux send-keys` / `tmux capture-pane -p`. Opening the `●` account's menu is safe;
  changing a setting rewrites your real `~/.bytt/config.toml`, so put it back.
- **The TUI's Auto-switch** only flips `auto` in the config; the menu bar app does the
  switching and notices the change within seconds.
- **`/logout` revokes the login.** To add accounts, `/login` over the current one.
