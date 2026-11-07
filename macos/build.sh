#!/bin/bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CARGO_BIN=${CARGO:-cargo}
BUILD="$ROOT/macos/build"
APP="$BUILD/Tessera for Mac.app"
export MACOSX_DEPLOYMENT_TARGET=14.0
cd "$ROOT"
if [[ ${1:-} == --universal ]]; then
  ARCHS=(arm64 x86_64)
else
  ARCHS=("$(uname -m)")
fi
(cd app && npm ci --ignore-scripts && npx vite build --config vite.macos.config.ts)
mkdir -p "$BUILD"
for ARCH in "${ARCHS[@]}"; do
  if [[ $ARCH == arm64 ]]; then TARGET=aarch64-apple-darwin; else TARGET=x86_64-apple-darwin; fi
  "$CARGO_BIN" build --release -p tessera-macos --target "$TARGET"
  xcrun swiftc -parse-as-library -swift-version 5 -O -target "$ARCH-apple-macos14.0" \
    -import-objc-header "$ROOT/macos/include/TesseraCore.h" "$ROOT"/macos/Sources/*.swift \
    -L "$ROOT/target/$TARGET/release" -ltessera_macos -framework Cocoa -framework SwiftUI -framework WebKit \
    -framework CoreServices -framework Security -framework SystemConfiguration -liconv \
    -o "$BUILD/tessera-$ARCH"
done
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [[ ${#ARCHS[@]} == 2 ]]; then lipo -create "$BUILD/tessera-arm64" "$BUILD/tessera-x86_64" -output "$APP/Contents/MacOS/Tessera"; else cp "$BUILD/tessera-${ARCHS[0]}" "$APP/Contents/MacOS/Tessera"; fi
cp -R "$BUILD/web" "$APP/Contents/Resources/"
cp "$ROOT/macos/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/app/src-tauri/icons/icon.png" "$APP/Contents/Resources/icon.png"
cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$APP/Contents/Resources/"
codesign --force --deep --sign - --entitlements "$ROOT/macos/Tessera.entitlements" "$APP"
echo "$APP"
