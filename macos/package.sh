#!/bin/bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/macos/build/Tessera for Mac.app"
if [[ $# -lt 1 || $# -gt 2 ]]; then echo 'Usage: macos/package.sh OUTPUT.dmg [--notarize]' >&2; exit 2; fi
OUTPUT=$1
MODE=${2:-local}
[[ $MODE == local || $MODE == --notarize ]] || { echo 'Invalid packaging mode' >&2; exit 2; }
[[ -d "$APP" ]] || { echo 'Build with macos/build.sh --universal first' >&2; exit 1; }
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/tessera-package.XXXXXXXX")
trap 'rm -rf -- "$STAGE"' EXIT
STAGED_APP="$STAGE/Tessera for Mac.app"
ditto "$APP" "$STAGED_APP"
if [[ $MODE == --notarize ]]; then
  : "${TESSERA_SIGN_IDENTITY:?Set the Developer ID Application identity}"
  : "${TESSERA_NOTARY_PROFILE:?Set the notarytool Keychain profile}"
  slices=" $(lipo -archs "$STAGED_APP/Contents/MacOS/Tessera") "
  [[ $slices == *" arm64 "* && $slices == *" x86_64 "* ]] || { echo "Universal application required" >&2; exit 1; }
  codesign --force --sign "$TESSERA_SIGN_IDENTITY" --options runtime --timestamp \
    --entitlements "$ROOT/macos/Tessera.entitlements" "$STAGED_APP"
  codesign --verify --strict --deep "$STAGED_APP"
  ditto -c -k --keepParent "$STAGED_APP" "$STAGE/app.zip"
  xcrun notarytool submit "$STAGE/app.zip" --keychain-profile "$TESSERA_NOTARY_PROFILE" --wait
  xcrun stapler staple "$STAGED_APP"
  rm "$STAGE/app.zip"
  spctl --assess --type execute --verbose=2 "$STAGED_APP"
fi
ln -s /Applications "$STAGE/Applications"
hdiutil create -srcfolder "$STAGE" -volname 'Tessera for Mac' -format UDZO "$OUTPUT"
if [[ $MODE == --notarize ]]; then
  codesign --sign "$TESSERA_SIGN_IDENTITY" --timestamp "$OUTPUT"
  xcrun notarytool submit "$OUTPUT" --keychain-profile "$TESSERA_NOTARY_PROFILE" --wait
  xcrun stapler staple "$OUTPUT"
  xcrun stapler validate "$OUTPUT"
fi
(cd "$(dirname "$OUTPUT")" && shasum -a 256 "$(basename "$OUTPUT")")
