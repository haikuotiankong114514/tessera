#!/bin/bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
CARGO_BIN=${CARGO:-cargo}
"$CARGO_BIN" test -p tessera-core -p tessera-cli -p tessera-macos
"$CARGO_BIN" build --release -p tessera-macos
mkdir -p macos/build
xcrun swiftc -parse-as-library -swift-version 5 -Onone -import-objc-header macos/include/TesseraCore.h \
  macos/Sources/Core.swift macos/Sources/ChangeMonitor.swift macos/Sources/TouchBar.swift macos/Tests/CoreTests.swift \
  -L target/release -ltessera_macos -framework Cocoa -framework SwiftUI -framework CoreServices \
  -framework Security -framework SystemConfiguration -liconv -o macos/build/native-tests
macos/build/native-tests
(cd app && npm run check && npm test)
