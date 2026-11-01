#!/bin/sh
# Builds dist/OwnTerminal.app (macOS). Copy it to /Applications to launch from Spotlight/Dock.
set -e
cd "$(dirname "$0")"
cargo build --release
app=dist/OwnTerminal.app
version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/own-terminal "$app/Contents/MacOS/"
cp assets/icon.icns "$app/Contents/Resources/"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>OwnTerminal</string>
  <key>CFBundleIdentifier</key><string>dev.vitor.own-terminal</string>
  <key>CFBundleExecutable</key><string>own-terminal</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
# Ad-hoc signature (no developer account): seals the bundle so Apple Silicon does not call it "damaged".
codesign --force --deep --sign - "$app"
echo "ok: $app"
