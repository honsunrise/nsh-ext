#!/usr/bin/env bash
# Builds and runs an nsh-ext-apple example as a Mac Catalyst app.
#
# Usage:
#   crates/apple/examples/run-catalyst.sh [acceptance|progress]
#
# `acceptance` waits for the app to exit, prints its log and exits nonzero
# unless the PASS line is there. `progress` is the interactive demo (default).
set -euo pipefail

package_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$package_dir"

mode="${1:-${EXAMPLE:-progress}}"
case "$mode" in
    acceptance|progress) ;;
    *) echo "unknown example '$mode' (acceptance or progress)" >&2; exit 2 ;;
esac

case "$(uname -m)" in
    arm64) target="aarch64-apple-ios-macabi" ;;
    x86_64) target="x86_64-apple-ios-macabi" ;;
    *) echo "unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
esac

cargo build --example "$mode" --target "$target"

target_dir="${CARGO_TARGET_DIR:-target}"
app="$target_dir/catalyst/NshExtApple.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$target_dir/$target/debug/examples/$mode" "$app/Contents/MacOS/nsh-ext-apple-example"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>nsh-ext-apple-example</string>
    <key>CFBundleIdentifier</key><string>dev.nsh.ext.apple.example</string>
    <key>CFBundleName</key><string>NSH Ext Apple</string>
    <key>CFBundleDisplayName</key><string>NSH Ext Apple</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.2.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>UIApplicationSceneManifest</key>
    <dict>
        <key>UIApplicationSupportsMultipleScenes</key><false/>
    </dict>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsArbitraryLoads</key><true/>
    </dict>
    <key>UIDeviceFamily</key><array><integer>6</integer></array>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST

codesign --force --sign - --timestamp=none "$app"

if [ "$mode" = acceptance ]; then
    out="$target_dir/catalyst/acceptance.out"
    err="$target_dir/catalyst/acceptance.err"
    : > "$out"
    : > "$err"
    set +e
    open -n -W --stdout "$out" --stderr "$err" "$app"
    status=$?
    set -e
    cat "$out" "$err"
    if ! grep -q 'nsh-ext-apple acceptance: PASS' "$out" "$err"; then
        echo "nsh-ext-apple acceptance: FAILED (open exited $status; logs $out $err)" >&2
        exit 1
    fi
    echo "nsh-ext-apple acceptance: PASSED"
else
    open -n "$app"
    echo "Opened $app (Catalyst; its stdout goes to Console.app)"
fi
