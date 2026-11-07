# Tessera for Mac

A macOS disk space analyzer based on [Tessera by 4zuma](https://github.com/4zuma/tessera). Scan a disk or folder, see where the space goes in an interactive treemap, and clean up files using the Mac's Trash.

## Try the developer build

Tessera targets **macOS 14 or later** on **Apple Silicon and Intel**. The macOS CI workflow builds a Universal developer DMG. Sign in to GitHub, open a successful run, and download **Tessera-macOS-developer** from its Artifacts section. Developer builds use ad hoc signing. Developer ID signing, notarization, and foreground acceptance are pending for a public beta. [macOS builds](https://github.com/haikuotiankong114514/tessera/actions/workflows/macos.yml)

## Using Tessera

- Choose a disk in the sidebar or press `⌘O` to scan a folder. Double-click a folder tile to drill down, or use the file list to browse by size.
- Toggle between **Allocated** and **Logical** size and inspect a file's details or show it in Finder. APFS clones, snapshots, shared space, and unreadable directories can make scan totals differ from volume usage.
- Press `⌘Delete` to move the selection to Trash and `⌘Z` to undo the move during the current scan. Disk Access Help is available from the Help menu.
- On Macs with a Touch Bar, controls change as you choose a location, scan, browse, and select files.

## How the port works

The app uses SwiftUI and AppKit for the window and controls, with Tessera's shared Rust file tree and WebGL/Canvas treemap. The Darwin scanner reads directory metadata in batches with reusable worker buffers. It counts hard links once and avoids downloading cloud placeholders during enumeration. FSEvents updates changed directories as you browse.

On a 16 GB M1 MacBook Pro, a CLI scan of **167,226 entries** in `Xcode.app` took a median of **362 ms with four threads** and **1,067 ms with one thread**, using five warm runs per thread count. A startup disk scan enumerated approximately 2.06 million entries in 11.68 s and reported 836 inaccessible items. The benchmark command and cache settings are documented in the [macOS development guide](macos/README.md).

## Build from source

With Xcode command-line tools, stable Rust, and Node 24 installed, run the following from the repository root.

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
macos/build.sh --universal
```

The app is built at `macos/build/Tessera for Mac.app`. See the [macOS development guide](macos/README.md) for tests, benchmarking, and packaging.

## Credits and license

Original Tessera by [4zuma and upstream contributors](https://github.com/4zuma/tessera/graphs/contributors). [OpenAI Codex](https://github.com/codex) helped implement the macOS port.

Available under the [MIT License](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE), at your option.
