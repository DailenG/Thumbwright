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

## twctl

`twctl --help` lists every command; `twctl <command> --help` shows one. Besides the image verbs (convert, resize, rotate, compress, strip, clip, wallpaper, ...):

- `doctor [file] [--bundle out.zip]`: sectioned self-check (spec 7.1) with FIX hints; foreign ProgID-level thumbnail handlers are reported.
- `batch <thumbnail|convert|info> <in|dir...> [--json]` and `batch --retry-from report.json`: multi-file jobs on all cores; the JSON report lists each file's outcome and the retry reruns only the failures.
- `prebuild <dir|file...> [--size N,N] [--rebuild-all]`: warms Explorer's thumbnail cache (also the "Build thumbnails here" folder-menu entry).
- `pdf` / `cbz <out> <in...> [--strict] [--json]`: combine images; `pdf --searchable` adds an invisible OCR text layer. The JSON result is `{combined, omitted[{input,cause,detail}], output, requested, status}`.
- `ocr <in>`: Windows' built-in OCR engine (runs in `twctl`, never in the shell DLL).
- `rename`, `sort`, `folder-icon`, `lock-screen`, `pick-color`, `settings`.
- `convert-to avif|jxl`: pure-Rust encoders (AVIF through `ravif`/`rav1e` without assembly; JPEG XL is lossless only).
- `upload <file...> [--copy] [--confirm]`, `upload-hosts [--open]`, `upload-history`: HTTPS POST to the hosts in `upload-hosts.conf` (spec 10.3) through WinHTTP. Nothing is sent without an explicit upload command; the menu verb asks first with Cancel as the default button.
- `--mcp`: MCP server over stdio exposing the CLI's operations plus `view`.

## Clean-room rules

This is an independent implementation built only from `FUNCTIONAL_SPEC.md`, official Microsoft documentation and permissively licensed crate sources. Do not consult the reference product's binaries or source. See `REVIEW_HANDOFF.md` section 1.

## License

Copyright (C) 2026 Dailen Gunter

Thumbwright is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See `LICENSE` for the full text (`GPL-3.0-or-later`).

Third-party notices: `THIRD_PARTY_LICENSES.md`.
