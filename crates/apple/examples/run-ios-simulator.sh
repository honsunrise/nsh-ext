#!/usr/bin/env bash
# Builds, installs and runs an nsh-ext-apple example in an iOS Simulator.
#
# Usage:
#   crates/apple/examples/run-ios-simulator.sh [acceptance|progress]
#   SIMULATOR="iPhone 16 Pro" crates/apple/examples/run-ios-simulator.sh acceptance
#   xcrun simctl list devices available    # names this accepts
#
# `acceptance` waits for the app's console, checks for the PASS line and exits
# nonzero on any failure. `progress` is the interactive demo (the default).
set -euo pipefail

package_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$package_dir"

mode="${1:-${EXAMPLE:-progress}}"
case "$mode" in
    acceptance|progress) ;;
    *) echo "unknown example '$mode' (acceptance or progress)" >&2; exit 2 ;;
esac

simulator="${SIMULATOR:-iPhone 16}"
case "$(uname -m)" in
    arm64) target="aarch64-apple-ios-sim" ;;
    x86_64) target="x86_64-apple-ios" ;;
    *) echo "unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
esac

cargo build --example "$mode" --target "$target"

target_dir="${CARGO_TARGET_DIR:-target}"
app="$target_dir/ios-simulator/NshExtApple.app"
rm -rf "$app"
mkdir -p "$app"
cp "$target_dir/$target/debug/examples/$mode" "$app/nsh-ext-apple-example"

cat > "$app/Info.plist" <<'PLIST'
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
    <key>UIDeviceFamily</key><array><integer>1</integer><integer>2</integer></array>
    <key>MinimumOSVersion</key><string>16.0</string>
    <key>CFBundleSupportedPlatforms</key><array><string>iPhoneSimulator</string></array>
    <key>UILaunchScreen</key><dict/>
    <key>CADisableMinimumFrameDurationOnPhone</key><true/>
</dict>
</plist>
PLIST

codesign --force --sign - --timestamp=none "$app"

# An already booted simulator makes `boot` fail; that is fine.
xcrun simctl boot "$simulator" 2>/dev/null || true
xcrun simctl bootstatus "$simulator" -b
xcrun simctl terminate "$simulator" dev.nsh.ext.apple.example 2>/dev/null || true
xcrun simctl install "$simulator" "$app"
xcrun simctl terminate "$simulator" dev.nsh.ext.apple.example 2>/dev/null || true

if [ "$mode" = acceptance ]; then
    log="$target_dir/ios-simulator/acceptance.log"
    set +e
    xcrun simctl launch --console-pty "$simulator" dev.nsh.ext.apple.example 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    set -e
    if ! grep -q 'nsh-ext-apple acceptance: PASS' "$log"; then
        echo "nsh-ext-apple acceptance: FAILED (simctl exited $status; log $log)" >&2
        exit 1
    fi
    echo "nsh-ext-apple acceptance: PASSED"
else
    xcrun simctl launch "$simulator" dev.nsh.ext.apple.example
    echo "Launched dev.nsh.ext.apple.example on $simulator"
    echo "Read its stdout with: xcrun simctl launch --console $simulator dev.nsh.ext.apple.example"
fi
