# Thumbwright

Clean-room Windows Explorer thumbnail provider and context-menu toolkit, written in Rust. Pre-1.0: interfaces and on-disk formats may still change.

- `thumbwright_shell.dll`: COM server. Thumbnail provider (`IThumbnailProvider` + `IInitializeWithStream`, hosted by the OS thumbnail surrogate) and classic context menu (`IContextMenu{,2,3}`).
- `tw-worker.exe`: per-request decoder process (Job Object: 512 MiB, 20 s CPU, kill-on-close) for WIC, SVG and audio tags.
- `twctl.exe`: CLI, per-user registration, and the back end for every menu verb.

Requirements: `FUNCTIONAL_SPEC.md`. Architecture, invariants and review state: `REVIEW_HANDOFF.md`, `REVIEW_FINDINGS.md`. Open work is tracked as GitHub issues.

## Build

Requires Windows x64 and the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it). Linking needs the MSVC CRT/SDK libraries: either Visual Studio Build Tools ("Desktop development with C++"), or the original build room's self-contained setup (rust-lld + xwin under a gitignored `.toolchain/`, wired up by `scripts/devenv.ps1`). No crate may compile C/C++ (no C compiler is assumed).

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask licenses --check        # dependency audit (permissive licenses, no native code); regenerates THIRD_PARTY_LICENSES.md without --check
cargo build --release --workspace   # thin LTO; slow (11-28 min on the build-room machine)
```

The DLL, `tw-worker.exe` and `twctl.exe` must sit in one directory.

Some tests write a throw-away key under `HKCU\Software\ThumbwrightTest-*` (removed on drop) and files under `%TEMP%`. Timing tests warm up freshly copied executables because antivirus scans them on first launch.

## Try it (per-user, no elevation)

Portable mode is marked by a `Thumbwright.ini` next to the binaries.

```
twctl register            # HKCU only; refuses from a non-portable copy
twctl register --status
twctl register --off      # undoes exactly what register wrote
```

Windows ignores HKCU COM registrations for elevated processes, so test Explorer from a non-elevated session. Run `register --off` before moving the folder (the registration stores an absolute DLL path).

## Clean-room rules

This is an independent implementation built only from `FUNCTIONAL_SPEC.md`, official Microsoft documentation and permissively licensed crate sources. Do not consult the reference product's binaries or source. See `REVIEW_HANDOFF.md` section 1.

## License

Copyright (C) 2026 Dailen Gunter

Thumbwright is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See `LICENSE` for the full text (`GPL-3.0-or-later`).

Third-party notices: `THIRD_PARTY_LICENSES.md`.
