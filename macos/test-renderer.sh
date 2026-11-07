#!/bin/bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
ARCH=${1:-$(uname -m)}
[[ $ARCH == arm64 || $ARCH == x86_64 ]] || exit 2
TARGET=$([[ $ARCH == arm64 ]] && echo aarch64-apple-darwin || echo x86_64-apple-darwin)
BUNDLE="$ROOT/macos/build/RendererTests.app"
[[ -d macos/build/web ]] || { echo 'Run macos/build.sh first' >&2; exit 1; }
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources/web"
cp macos/Info.plist "$BUNDLE/Contents/Info.plist"
cp -R macos/build/web/. "$BUNDLE/Contents/Resources/web/"
xcrun swiftc -parse-as-library -swift-version 5 -Onone -target "$ARCH-apple-macos14.0" -import-objc-header macos/include/TesseraCore.h \
  macos/Sources/Core.swift macos/Sources/ChangeMonitor.swift macos/Sources/Treemap.swift macos/Tests/RenderTests.swift \
  -L "target/$TARGET/release" -ltessera_macos -framework Cocoa -framework SwiftUI -framework WebKit \
  -framework CoreServices -framework Security -framework SystemConfiguration -liconv \
  -o "$BUNDLE/Contents/MacOS/Tessera"
arch "-$ARCH" "$BUNDLE/Contents/MacOS/Tessera"
