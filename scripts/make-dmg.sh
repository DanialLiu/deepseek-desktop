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
OUT="$OUT_DIR/DeepSeek Harness_0.1.0_aarch64.dmg"
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
mkdir -p "$OUT_DIR"
rm -f "$TMP"

# Bake the volume icon: build a raw (writable) image, mount it, drop a
# `.VolumeIcon.icns` at the volume root and mark it with SetFile -a C, then
# convert to the compressed ULFO (lzfse) image. (A plain `hdiutil create
# -srcfolder` pass does not carry the Finder custom-icon bit over, so Finder
# would keep showing the generic disk-image icon.)
hdiutil create -volname "DeepSeek Harness" -srcfolder "$APP" -ov -format UDRW "$TMP"
VOL="$(hdiutil attach -nobrowse "$TMP" | tail -1 | awk -F'\t' '{print $NF}')"
cp "$ICON" "$VOL/.VolumeIcon.icns"
SetFile -a C "$VOL"
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
echo "Created: $OUT"
