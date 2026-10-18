# Thumbwright

Clean-room Windows Explorer thumbnail provider and context-menu toolkit, written in Rust. Pre-1.0: interfaces and on-disk formats may still change.

- `thumbwright_shell.dll`: COM server. Thumbnail provider (`IThumbnailProvider` + `IInitializeWithStream`, hosted by the OS thumbnail surrogate) and classic context menu (`IContextMenu{,2,3}`). Also the cloud-folder thumbnail provider (off by default; the "Thumbnails in cloud folders" setting is `CloudThumbnails`), and, registered only by the machine-wide installer (`regsvr32`), the preview-pane handler, the property handler and the Windows 11 `IExplorerCommand` menu.
- `tw-worker.exe`: per-request decoder process (Job Object: 512 MiB, 20 s CPU, kill-on-close) for the decoders that must not run in the surrogate: OS codecs (WIC, Media Foundation video, PDF, GDI metafiles), SVG, audio tags, JPEG 2000, DjVu, 7z/RAR and Live Photo bundles.
- `twctl.exe`: CLI, per-user registration, and the back end for every menu verb. It never hosts an OS codec itself: WIC, video, PDF and the other worker-placed decoders run in `tw-worker.exe` for conversions as well as thumbnails.
- `thumbwright-settings.exe`: settings window (General, File types, Menu, Limits, Preview, Maintenance). Saves to `Thumbwright.ini` (portable) or `HKCU\Software\Thumbwright` (installed); toggling a file type updates the per-user registration through the journaled engine. `--self-test` creates, drives and destroys the window without showing it.
- `packaging/msix/`: sparse MSIX package that declares the Windows 11 menu verb. `build.ps1 -Publisher "CN=<signing certificate subject>"` builds it unsigned with `makeappx`; signing and registration (`Add-AppxPackage -ExternalLocation <install dir>`) are release steps.

Formats: 315 of the 363 formats in the spec matrix have a thumbnail route (`twctl formats --json`, entries with `thumbnail: true`; the 48 others are listed with the reason in `docs/history/REVIEW_HANDOFF.md` section 5). They cover camera RAW, JPEG XL, JPEG 2000, HEIC/AVIF (OS codecs), video frames (Media Foundation, offset set by `VideoFramePercent`), PDF, legacy Office and OLE previews, ebooks, archives, Android packages, design and CAD previews, meshes, and the scientific and legacy bitmap long tail. Types without a route are never registered, so Windows' own handlers stay in charge. Measurements of the thumbnail path and the decisions behind pre-spawned workers: `docs/performance.md`.

Requirements: `FUNCTIONAL_SPEC.md`. Architecture, invariants I1-I8, verification state and known gaps: `docs/history/REVIEW_HANDOFF.md` (sections 3-5); security review and its fixes: `docs/history/REVIEW_FINDINGS.md`. Open work is tracked as GitHub issues.

## Build

Requires Windows x64 and the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it). Linking needs the MSVC CRT/SDK libraries: either Visual Studio Build Tools ("Desktop development with C++"), or the original build room's self-contained setup (rust-lld + xwin under a gitignored `.toolchain/`, wired up by `scripts/devenv.ps1`). The CRT is linked statically (`.cargo/config.toml`), so the binaries need no VC++ redistributable. No crate may compile C/C++ (no C compiler is assumed).

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask licenses --check        # dependency audit (permissive licenses, no native code); regenerates THIRD_PARTY_LICENSES.md without --check
cargo build --release --workspace   # thin LTO; slow (11-28 min on the build-room machine)
```

The DLL, `tw-worker.exe`, `twctl.exe` and `thumbwright-settings.exe` must sit in one directory.

Some tests write a throw-away key under `HKCU\Software\ThumbwrightTest-*` (removed on drop) and files under `%TEMP%`. Timing tests warm up freshly copied executables because antivirus scans them on first launch.

## Try it (per-user, no elevation)

Portable mode is marked by a `Thumbwright.ini` next to the binaries.

```
twctl register            # HKCU only; refuses from a non-portable copy
twctl register --status
twctl register --off      # undoes exactly what register wrote
```

Windows ignores HKCU COM registrations for elevated processes, so test Explorer from a non-elevated session. Run `register --off` before moving the folder (the registration stores an absolute DLL path).

The settings window (`thumbwright-settings.exe`) edits the same settings. The conversion defaults it saves are honoured by `twctl`: `JpegQuality` is the default for `--quality` (and for any JPEG written without one), `PngCompression` sets the PNG level, `KeepMetadata` decides whether EXIF is carried over by `convert`, `convert-to`, `resize` and the rotate fallback (and stripped from a losslessly rotated JPEG when off), and `PreserveFileDate` gives the output the source's modification time in `convert`, `convert-to`, `resize` and `rotate`. `UseEmbeddedThumbs` decides whether a camera RAW file's embedded JPEG preview is tried before the OS RAW codec (default) or after it; other embedded previews (Office, design, archive covers) have no alternative decoder, so the setting does not apply to them. With a registration present, Apply adds or removes exactly the handler keys of the file types you toggled and broadcasts `SHCNE_ASSOCCHANGED`. Maintenance has Repair (re-register, then clear the thumbnail cache), Rebuild thumbnail cache, Turn on/off thumbnails and Open log. Windows has no API to purge the thumbnail cache, so Rebuild ends this user's `explorer.exe`, deletes `%LOCALAPPDATA%\Microsoft\Windows\Explorer\thumbcache_*.db`, and makes sure the shell is running again; the window asks for confirmation first.

## Install (per-machine MSI)

`Thumbwright-<version>-x64.msi` installs to `Program Files\Thumbwright` for all users and needs elevation (REQ-REG-8). It ships `thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe`, `thumbwright-settings.exe`, `LICENSE` and `THIRD_PARTY_LICENSES.md`, and never a `Thumbwright.ini`, so an installed copy is not in portable mode.

```
msiexec /i Thumbwright-0.1.0-x64.msi /qn      # silent install
msiexec /fa Thumbwright-0.1.0-x64.msi /qn     # repair (re-registers)
msiexec /x  Thumbwright-0.1.0-x64.msi /qn     # uninstall
```

Registration is done by the DLL, not by the MSI's registry tables: a deferred custom action runs `regsvr32 /s thumbwright_shell.dll` (`DllRegisterServer`, `Scope::Machine`: HKLM class and handler keys plus `Shell Extensions\Approved`, journaled under `HKLM\Software\Thumbwright\Registration`); uninstall runs `regsvr32 /u` before the files are removed, and upgrade and repair re-register cleanly. Details and the reasoning: `packaging/wix/README.md`. Per-user (`twctl register`) is for portable copies only and refuses to run from an installed copy.

`scripts/test-install.ps1` (elevated, on a test machine) installs, upgrades, repairs and uninstalls the MSI and fails on any registry difference (HKLM and HKCU) between the state before install and after uninstall. `scripts/test-install.ps1 -DryRun` needs no elevation and inspects the package tables instead.

## Release build and signing

```
scripts\release.ps1 -SkipSign                     # unsigned dev build -> dist\Thumbwright-<version>-x64.msi
scripts\release.ps1                               # sign with Azure Trusted Signing (sign-helper.ps1)
scripts\release.ps1 -CertThumbprint <sha1>        # or signtool with a certificate in the store
```

The script runs the dependency audit, `cargo build --release --workspace --locked`, collects the binaries into `dist\`, Authenticode-signs `thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe` and `thumbwright-settings.exe` (REQ-SEC-4), builds the MSI from the signed files, signs the MSI, and then runs `signtool verify /pa /v` on every file, failing on any that is unsigned or not RFC 3161 timestamped. It needs the Windows SDK's `signtool.exe` and WiX v7 (`dotnet tool install --global wix --version 7.0.0`); WiX v7 requires accepting its Open Source Maintenance Fee EULA once (`wix eula accept wix7`; free below US$10,000/year of revenue, see `packaging/wix/README.md`). Azure Trusted Signing needs `AzureTrustedSignTool` on PATH and `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TRUSTED_SIGNING_ACCOUNT` and `AZURE_TRUSTED_SIGNING_PROFILE` in the environment. `dist\` is gitignored.

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

This is an independent implementation built only from `FUNCTIONAL_SPEC.md`, official Microsoft documentation and permissively licensed crate sources. Do not consult the reference product's binaries or source. The reference product is deliberately not named in this repository. See `docs/history/REVIEW_HANDOFF.md` section 1.

## License

Copyright (C) 2026 Dailen Gunter

Thumbwright is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See `LICENSE` for the full text (`GPL-3.0-or-later`).

Third-party notices: `THIRD_PARTY_LICENSES.md`.
