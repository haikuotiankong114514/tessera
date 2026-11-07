# Tessera for Mac

A macOS fork of [Tessera by 4zuma](https://github.com/4zuma/tessera). The `macos`
branch adds a native SwiftUI/AppKit shell and Touch Bar. The Rust tree, layout
format, and WebGL/Canvas renderer are shared with upstream. The WebView hosts
only the map; it receives visible rectangles, not the file tree.

## Build and test

Requires macOS 14 or newer, Xcode command-line tools, a current stable Rust
compiler and Node 24. From the repository root:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
macos/build.sh --universal
macos/test.sh
macos/test-renderer.sh
```

`macos/build.sh` without `--universal` builds for the current Mac. The application
is in `macos/build`. Development builds use an ad hoc signature. `test.sh`
uses disposable directories for deletion and Trash/Undo checks. The renderer
smoke test exercises the real WebKit bridge without opening a window; it does
not measure visible animation performance or test the physical Touch Bar.

## SSD scanning and measurement

The Darwin scanner preserves the upstream approach: work stealing, one reusable
64 KiB query buffer per worker, batched metadata through `getattrlistbulk`, and
batched tree writes after I/O. It falls back to directory enumeration and
`fstatat` where the bulk operation is unavailable. The default uses performance
cores on Apple Silicon and physical cores on Intel; the CLI can override it.
Hard links are counted once, and scanning does not follow symlinks or read file
contents. Darwin workers disable automatic materialization of cloud files.

```sh
cargo build --release -p tessera-cli
python3 scripts/bench-macos.py /Applications/Xcode.app --label Xcode.app > /tmp/tessera-bench.json
cargo test --release -p tessera-core layout_bench -- --ignored --nocapture
```

The matrix interleaves thread counts and reports five warm samples per count.
It also measures idle CPU with FSEvents enabled. Treat the first observation
separately: the script does not purge caches. `private_bytes` on Darwin is the
process physical footprint, not the Windows private commit metric.

For the Svelte/Tauri comparison build, run `npm run build` in `app`, then
`cargo build --release -p tessera-app` at the root. This comparison build uses
the real Darwin scan core. Set `TESSERA_BENCH_ROOT` to an absolute folder path when launching its executable to auto-scan the same test folder. It is not the macOS distribution app. Compare using
identical scan roots and view sizes; include all attributable WebKit processes
when measuring memory. The native app executable accepts `--scan PATH` and
`--diagnostics FILE` for local scan and binary-bridge diagnostics. Use a visible
window and Instruments for frame pacing and input latency.

## Storage and cleanup

The map's allocated and logical totals describe enumerated files. APFS clones,
snapshots, shared capacity and unreadable directories can make these totals
mismatch volume usage; the inspector shows capacity separately. Scanning works
without Full Disk Access where macOS permits access. The permission help is
available from the Help menu. Finder automation is requested only when the user
explicitly confirms Empty Trash, which affects the entire user's Trash.

Touch Bar defaults change for choosing a location, scanning, browsing and
selection. All actions also exist in the window or menu. Do not consider the
physical Touch Bar, Intel runtime or macOS 14 runtime validated by a build or a
headless smoke test alone.

## Package and release

```sh
macos/package.sh /tmp/Tessera-macOS-developer.dmg
```

For a public beta, first acquire a Developer ID Application identity and set up
a `notarytool` Keychain profile. Build Universal, verify the app on both target
architectures, and run:

```sh
TESSERA_SIGN_IDENTITY='Developer ID Application: …' \
TESSERA_NOTARY_PROFILE='tessera-notary' \
macos/package.sh /tmp/Tessera-macOS-beta.dmg --notarize
```

The script signs with Hardened Runtime, notarizes and staples the app and DMG,
and prints the SHA-256. Test installation on a clean Mac before creating a GitHub
pre-release tagged `macos-v0.1.0-beta.1`. No signing secrets are kept in this repo.
The public beta must wait for signing credentials and foreground acceptance.
