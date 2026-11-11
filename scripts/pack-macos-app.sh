#!/usr/bin/env bash
# Package the release binary + icon into snack.app (macOS Finder/Dock icon).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="target/release/snack"; ICON="icon/snack-transparent.icns"; APP="snack.app"
[ -f "$BIN" ]  || { echo "Missing $BIN — run: cargo build --release"; exit 1; }
[ -f "$ICON" ] || { echo "Missing $ICON — run: bash icon/build-transparent.sh"; exit 1; }
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/snack"
cp "$ICON" "$APP/Contents/Resources/snack.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Snack</string>
    <key>CFBundleDisplayName</key><string>Snack</string>
    <key>CFBundleIdentifier</key><string>org.snack.snack</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleExecutable</key><string>snack</string>
    <key>CFBundleIconFile</key><string>snack</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
touch "$APP"
echo "Built $APP (v${VERSION}) — open with: open $APP"
