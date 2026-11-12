#!/usr/bin/env bash
# Wrap the release binary in Bytt.app, so its notifications carry bytt's name and icon.
#   macos/bundle.sh <binary> <out-dir>
# Signs with $SIGN_IDENTITY (a Developer ID Application identity), or ad-hoc without it.
set -euo pipefail
binary=$1
out=$2
here=$(cd "$(dirname "$0")" && pwd)
version=$(grep -m1 '^version = ' "$here/../Cargo.toml" | cut -d'"' -f2)
app="$out/Bytt.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/bytt"
cp "$here/bytt.icns" "$app/Contents/Resources/bytt.icns"
sed "s/VERSION/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

identity=${SIGN_IDENTITY:--}
# Notarization needs a secure timestamp; an ad-hoc signature cannot have one.
if [ "$identity" = "-" ]; then timestamp=--timestamp=none; else timestamp=--timestamp; fi
codesign --force --options runtime "$timestamp" --sign "$identity" "$app"
codesign --verify --strict "$app"
echo "$app"
