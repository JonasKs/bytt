# See https://just.systems/man/en/ for docs

# List available commands
default:
    just --list

# ============ DEVELOP ============

# Build the debug binary
build:
    cargo build

# Run from source, e.g. `just run list` or `just run auto --once --dry-run`
run *args:
    cargo run -- {{args}}

# Run the unit tests
test:
    cargo test

# Format + lint + tests + cargo-deny (what CI runs)
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo deny check

# Build Bytt.app (target/Bytt.app), signed with your Developer ID if the keychain has one
bundle:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --locked
    id=$(security find-identity -v -p codesigning | grep -m1 "Developer ID Application" | awk '{print $2}' || true)
    SIGN_IDENTITY="${id:--}" macos/bundle.sh target/release/bytt target

# Zip Bytt.app the way CI does (target/Bytt-<version>-aarch64.zip; CI also notarizes)
package: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
    ditto -c -k --keepParent target/Bytt.app "target/Bytt-${version}-aarch64.zip"
    shasum -a 256 "target/Bytt-${version}-aarch64.zip"

# ============ SHIP ============

# Build, copy to /Applications and open (it registers itself to open at login)
install: bundle
    -pkill -f 'Bytt.app/Contents/MacOS/bytt$'
    rm -rf /Applications/Bytt.app
    cp -R target/Bytt.app /Applications/Bytt.app
    open /Applications/Bytt.app
    @echo 'Installed. bytt is in the menu bar.'

# Quit the menu bar app and remove it (accounts and settings in ~/.bytt are kept)
uninstall:
    -pkill -f 'Bytt.app/Contents/MacOS/bytt$'
    rm -rf /Applications/Bytt.app

# ============ INSPECT ============

# Show saved accounts and their usage
list:
    cargo run -q -- list

# Show bytt's state file (readings, backoff, needs-login flags)
state:
    @cat ~/.bytt/state.json

# Open the config file in your editor
config:
    open -t ~/.bytt/config.toml
