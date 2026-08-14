#!/bin/sh
# Build a distributable macOS .dmg from the already-built .app.
#
# `tauri build` ships a create-dmg helper that mounts a writable volume, runs
# AppleScript, then unmounts — that unmount races Spotlight indexing on this
# large (30k-file) app and fails with "Resource busy". `hdiutil create
# -srcfolder` produces the same compressed image without the mount dance.
set -e
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
APP="$ROOT/src-tauri/target/release/bundle/macos/DeepSeek Harness.app"
OUT_DIR="$ROOT/src-tauri/target/release/bundle/dmg"
ICON="$ROOT/src-tauri/icons/icon.icns"
TMP="$OUT_DIR/.staging.dmg"

if [ ! -d "$APP" ]; then
  echo "make-dmg: $APP not found; run \`pnpm run build\` first." >&2
  exit 1
fi
if [ ! -f "$ICON" ]; then
  echo "make-dmg: $ICON not found; run \`pnpm run icons\` first." >&2
  exit 1
fi

# Read the version from the built .app (matches tauri.conf.json, which
# build-sidecar.mjs syncs from the harness) so the DMG filename always tracks
# the harness version instead of a hardcoded one.
VERSION="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")"
OUT="$OUT_DIR/DeepSeek Harness_${VERSION}_aarch64.dmg"

mkdir -p "$OUT_DIR"
rm -f "$TMP"

# Bake the volume icon and add the standard drag-to-install layout:
#   - mount the writable image,
#   - add an "Applications" symlink next to the app (the drop target), then
#   - drop `.VolumeIcon.icns` at the volume root and mark the volume with
#     SetFile -a C so Finder shows the custom disk icon, and
#   - mark the icon file itself hidden so it doesn't clutter the window.
# A plain `hdiutil create -srcfolder` pass carries none of this over.
hdiutil create -volname "DeepSeek Harness" -srcfolder "$APP" -ov -format UDRW "$TMP"
VOL="$(hdiutil attach -nobrowse "$TMP" | tail -1 | awk -F'\t' '{print $NF}')"
ln -s /Applications "$VOL/Applications"
cp "$ICON" "$VOL/.VolumeIcon.icns"
SetFile -a C "$VOL"
chflags hidden "$VOL/.VolumeIcon.icns"
hdiutil detach "$VOL"
# Let the volume fully detach before converting; an immediate convert can race
# the unmount and fail with "Resource temporarily unavailable".
sleep 2
# Convert to a space-free temp name first: DiskImages.framework (683.100.3 on
# macOS 26.5.x) PAC-crashes `hdiutil convert` when the -o path contains a
# space, so the space in the final "DeepSeek Harness_*.dmg" name must be
# reached via a rename instead of handed straight to hdiutil.
rm -f "$OUT_DIR/.final.dmg"
hdiutil convert "$TMP" -format ULFO -o "$OUT_DIR/.final.dmg"
mv -f "$OUT_DIR/.final.dmg" "$OUT"
rm -f "$TMP"

# Ad-hoc sign the DMG so Gatekeeper sees it as an "unidentified developer"
# artifact instead of an unsigned one. Unsigned artifacts fail to open with no
# bypass; an ad-hoc signature lets the user override it from
# System Settings → Privacy & Security → Open Anyway.
codesign --force --sign - "$OUT"

echo "Created: $OUT"
