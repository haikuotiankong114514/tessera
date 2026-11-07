# Tessera

> This branch is **Tessera for Mac**, a performance-first macOS fork of
> [4zuma/tessera](https://github.com/4zuma/tessera). See the [macOS build, testing, and packaging guide](macos/README.md). The upstream Windows documentation follows.
>
> **macOS contributor** · [OpenAI Codex](https://github.com/codex) helped implement the port.

A lightweight, fast disk-space visualizer for Windows: scan a drive, see it
as a treemap, delete what you don't need.

> Status: 1.0, the first stable release. Download the portable `.exe` from
> [Releases](https://github.com/4zuma/tessera/releases) (Windows 10/11, x64 or
> Arm64, needs WebView2; the build is not code-signed yet). The repository holds the
> Rust core (scanner, file tree, live updates, deletion, treemap layout), the
> Tauri 2 + Svelte app in [`app/`](app/README.md) and a command-line harness.

## What the core does

* **Scanning without admin rights.** A multithreaded directory walk over
  `NtQueryDirectoryFile` (one 64 KiB buffer per thread, a rayon pool sized
  from the CPU count). Hard links are counted once, junctions and symlinks
  are not followed, cloud placeholders are listed without being downloaded.
* **Scanning with admin rights.** On NTFS volumes Tessera reads the master
  file table directly (skipping free regions, parsing in parallel) and
  builds the same tree in a couple of seconds.
* **Compact tree.** Every file or folder is a 40-byte node in a chunked
  arena; names are stored once in a shared pool (identical names are
  de-duplicated during a scan). Each node keeps its size on disk, logical
  size, flags and last-write time. The full tree stays in Rust; the UI only
  ever receives the rectangles it draws.
* **Live updates.** After a scan, `ReadDirectoryChangesExW` (or the NTFS
  USN journal after an MFT scan) keeps the tree current, applied in batches
  at most every 500 ms. Idle CPU use is near zero.
* **Treemap layout.** A squarified layout that returns one compact binary
  buffer per view (see `crates/tessera-core/src/layout.rs`), with rank
  hysteresis so tiles don't jump around between updates.
* **Deletion.** Move to the Recycle Bin (with undo) or delete permanently
  through the Windows shell (`IFileOperation`), with friendly reasons for
  locked or protected items.

## Command-line harness

```text
tessera scan  <PATH> [--mode auto|walk|mft] [--threads N] [--top N] [--json]
tessera bench <PATH> [--mode auto|walk|mft] [--threads N] [--runs N] [--idle SECS] [--watch]
tessera watch <PATH> [--mode auto|walk|mft] [--secs N]
```

`scan` prints totals, memory use and the largest files. `bench` scans
several times, times treemap layouts of the result, then measures idle CPU
(with `--watch`: with live updates running) and prints one JSON object.
`watch` prints each batch of live updates.

`--mode auto` (the default) reads the MFT when the process is elevated and
the path is an NTFS volume root, and walks the directories otherwise.

## Building and testing

Requires Rust 1.85 or newer.

```sh
cargo build --release -p tessera-cli
cargo test --workspace
```

The MFT and USN-journal tests need an elevated prompt; they are ignored by
default. Run them with:

```powershell
$env:TESSERA_ADMIN_TESTS = "1"
cargo test -p tessera-core --test mft --test watch -- --include-ignored
```

The MFT parser is also tested against real NTFS images built with `mkntfs`
and `ntfs-3g` on Linux (`cargo test -p tessera-core --lib image_tests`).
The scanner itself only runs on Windows.

## Continuous integration

Each push and pull request runs only the checks its files can affect
(`.github/scripts/ci-changes.sh`): a change to docs only (`*.md`, `docs/`)
runs none, a change to the app only skips the core's jobs, and a change to
the CLI only skips the app's. A skipped job still reports its check as
passed, so required checks keep working.

The app exe is built on `main`. For a test build of a pull request, add the
`build-exe` label to it: that starts an `app` run whose artifact
`tessera-app-windows-x64` holds the exe, and later pushes build one too.

The smoke scan walks `C:\Users` and `C:\ProgramData`, since a walk of all of
C: takes up to 35 minutes on GitHub's runners; run the `CI` workflow by hand
with *full_walk* to walk the whole drive.

## Releasing

1. Set the new version everywhere it is written down (Cargo, npm and Tauri
   each keep a copy):

   ```sh
   node scripts/bump-version.mjs 0.3.1
   ```

   `node scripts/bump-version.mjs --check` lists the versions and fails if
   they differ.
2. Write the release notes, in English, in the pull request's description,
   between a line `<!-- release-notes -->` and a line `<!-- /release-notes -->`
   (GitHub doesn't show either). That text becomes the release notes; without
   it GitHub writes them from the merged pull requests. To fix the notes of a
   published release, edit them on the Releases page.
3. Commit, open a pull request, and merge it once CI is green. Merging is
   the release: when a push to `main` changes the version, the `release`
   workflow builds the Windows binaries (x64 and Arm64), tags the merge commit
   `v0.3.1` and publishes the release. Pushes that leave the version alone, or whose tag
   already exists, release nothing.
4. To release by hand (for example after a failed run), run the `release`
   workflow from the Actions tab on `main` with version `0.3.1`. It refuses a
   version that differs from the one in the code, and takes the notes from
   the pull request that merged the commit it runs on. For a version that is
   already released it only adds missing files, built from that version's
   tag; the current release gets them by itself when the workflow gains a
   new architecture.

## First measurements

Full scan of a system drive (Windows 11, Ryzen 9 7945HX, NVMe, about
3.3 million files and folders), compared with SpaceSniffer on the same
machine:

| | Tessera, no admin | Tessera, admin (MFT) | SpaceSniffer |
|---|---|---|---|
| Scan time (warm cache) | 9.4 s | 2.2 s | 318 s |
| Memory after scan | 234 MB | 258 MB | 1,447 MB |
| Idle CPU, live updates on | < 1 % of one core | < 1 % of one core | about 10–20 % |

These were measured before name de-duplication, which should lower memory
further.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
