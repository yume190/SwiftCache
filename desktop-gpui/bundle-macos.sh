#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
    echo "The .app bundle is only available on macOS." >&2
    exit 1
fi
profile="${1:-debug}"
case "$profile" in
    debug|release) ;;
    *) echo "Usage: bash desktop-gpui/bundle-macos.sh [debug|release]" >&2; exit 2 ;;
esac
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
frontend="$root/desktop-gpui/target/$profile/swiftcache-gpui"
daemon="$root/server/target/release/swiftcache"
icon="$root/desktop-gpui/assets/icon.icns"
for binary in "$frontend" "$daemon" "$icon"; do
    if [[ ! -f "$binary" ]]; then
        echo "Missing: $binary. Build the frontend and server first (make gpui-app builds both)." >&2
        exit 1
    fi
done

app="$root/desktop-gpui/target/$profile/bundle/macos/SwiftCache.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$frontend" "$app/Contents/MacOS/swiftcache-gpui"
install -m 755 "$daemon" "$app/Contents/MacOS/swiftcache"
install -m 644 "$icon" "$app/Contents/Resources/AppIcon.icns"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleIdentifier</key><string>dev.yume.SwiftCache.GPUI</string>
    <key>CFBundleName</key><string>SwiftCache</string>
    <key>CFBundleDisplayName</key><string>SwiftCache</string>
    <key>CFBundleExecutable</key><string>swiftcache-gpui</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
codesign --force --deep --sign - "$app"
# Rebuilding in place keeps the path, so Finder and the Dock would keep showing the cached icon.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$app"
printf '%s\n' "$app"
