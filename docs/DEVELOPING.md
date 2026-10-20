# Developing Thumbwright

Technical reference for people who build, package, debug or extend Thumbwright. Everything an end user needs (install, features, troubleshooting) is in the [README](../README.md); this page holds the component list, build and signing steps, the packaging details and the diagnostics log.

- [Components](#components)
- [Formats](#formats)
- [Documentation map](#documentation-map)
- [Requirements](#requirements)
- [Build](#build)
- [Try it (per-user, no elevation)](#try-it-per-user-no-elevation)
- [Install (per-machine MSI)](#install-per-machine-msi)
- [Release build and signing](#release-build-and-signing)
- [twctl](#twctl)
- [Diagnostics log](#diagnostics-log)
- [Clean-room rules](#clean-room-rules)

## Components

- `thumbwright_shell.dll`: COM server. Thumbnail provider (`IThumbnailProvider` + `IInitializeWithStream`, hosted by the OS thumbnail surrogate) and classic context menu (`IContextMenu{,2,3}`). Also the cloud-folder thumbnail provider (off by default; the "Thumbnails in cloud folders" setting is `CloudThumbnails`), and, registered only by the machine-wide installer (`regsvr32`), the preview-pane handler, the property handler and the two Windows 11 `IExplorerCommand` entries (`ExplorerCommand` for files, `FolderCommand` for folders and the folder background; Windows 11 lists only packaged verbs at the top level of its menu, and copies them into "Show more options", where the classic handler hides its own twin).
- `tw-worker.exe`: per-request decoder process (Job Object: 512 MiB, 20 s CPU, kill-on-close) for the decoders that must not run in the surrogate: OS codecs (WIC, Media Foundation video, PDF, GDI metafiles), HEIC/HEIF (`open-heic`, pure Rust; its HEVC decoder is young and can panic), SVG, audio tags, JPEG 2000, DjVu, 7z/RAR and Live Photo bundles.
- `twctl.exe`: CLI, per-user registration, and the back end for every menu verb. It never hosts an OS codec itself: WIC, video, PDF and the other worker-placed decoders (HEIC included) run in `tw-worker.exe` for conversions as well as thumbnails.
- `thumbwright-settings.exe`: settings window (General, File types, Menu, Limits, Preview, Maintenance). Saves to `Thumbwright.ini` (portable) or `HKCU\Software\Thumbwright` (installed); toggling a file type updates the per-user registration through the journaled engine. `--self-test` creates, drives and destroys the window without showing it.
- [`packaging/msix/`](../packaging/msix/): sparse MSIX package that declares the Windows 11 menu verbs (files, folders, folder background). `build.ps1 -Publisher "CN=<signing certificate subject>"` builds it unsigned with `makeappx`; [`scripts/release.ps1`](../scripts/release.ps1) signs it with the release identity and the MSI registers it for all users ([`packaging/wix/README.md`](../packaging/wix/README.md)).

## Formats

Formats: 315 of the 363 formats in the spec matrix have a thumbnail route (`twctl formats --json`, entries with `thumbnail: true`; the 48 others and the verification state are tracked in [`ARCHITECTURE.md`](ARCHITECTURE.md), "Format coverage"). They cover camera RAW, JPEG XL, JPEG 2000, HEIC/HEIF (built-in decoder, OS codecs as fallback), AVIF/AVCI (OS codecs), video frames (Media Foundation, offset set by `VideoFramePercent`), PDF, legacy Office and OLE previews, ebooks, archives, Android packages, design and CAD previews, meshes, and the scientific and legacy bitmap long tail. Types without a route are never registered, so Windows' own handlers stay in charge. Measurements of the thumbnail path and the decisions behind pre-spawned workers: [`performance.md`](performance.md).

## Documentation map

Requirements: [`FUNCTIONAL_SPEC.md`](../FUNCTIONAL_SPEC.md). Architecture and invariants I1-I8: [`ARCHITECTURE.md`](ARCHITECTURE.md). Review records (the original reviewer brief, verification state and the security review with its fixes) are archived in [`history/`](history/). Open work is tracked as GitHub issues.

## Requirements

- Windows 10 version 2004 (build 19041) or later, or Windows 11, x64. The sparse MSIX that carries the Windows 11 menu declares 10.0.19041.0 as its minimum.
- Tested on Windows 11 build 26340 only. Windows 10 behaviour (classic menu only, no modern menu) has not been exercised.
- HEIC and HEIF thumbnails are decoded by `open-heic` (pure Rust, HEVC), so they need no Store codec; the Microsoft HEIF and HEVC extensions are only the fallback for variants it cannot read (JPEG-coded items, overlays). Known limit: Apple's separate alpha plane is not applied, so such images decode opaque. AVCI and AVIF thumbnails come from the OS codecs and need the Microsoft Store HEIF and AV1 extensions; without them those types show no thumbnail. JPEG XL, camera RAW, JPEG 2000 and the other pure-Rust formats need nothing extra.
- Video frames need Media Foundation (part of Windows, absent from some "N" editions without the Media Feature Pack); some codecs inside video files need the matching Store extension.
- `twctl doctor` reports which of these OS codecs are present on the machine.

## Build

Requires Windows x64 and the Rust toolchain pinned in [`rust-toolchain.toml`](../rust-toolchain.toml) (rustup installs it). Linking needs the MSVC CRT/SDK libraries: either Visual Studio Build Tools ("Desktop development with C++"), or a self-contained local setup (rust-lld + xwin under a gitignored `.toolchain/`, wired up by [`scripts/devenv.ps1`](../scripts/devenv.ps1); optional, and not needed with Visual Studio). The CRT is linked statically ([`.cargo/config.toml`](../.cargo/config.toml)), so the binaries need no VC++ redistributable. No crate may compile C/C++ (no C compiler is assumed).

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask licenses --check        # dependency audit (permissive licenses, no native code); regenerates THIRD_PARTY_LICENSES.md without --check
cargo build --release --workspace   # thin LTO; slow (11-28 min on the maintainer's machine)
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

The settings window (`thumbwright-settings.exe`) edits the same settings. The conversion defaults it saves are honoured by `twctl`: `JpegQuality` is the default for `--quality` (and for any JPEG written without one), `PngCompression` sets the PNG level, `KeepMetadata` decides whether EXIF, the ICC colour profile and XMP are carried over from JPEG, PNG, WebP and TIFF sources by `convert`, `convert-to`, `resize`, `email`, `compress`, `batch convert` and the rotate fallback (a losslessly rotated JPEG is stripped of EXIF and XMP when off; its ICC profile stays), and `PreserveFileDate` gives the output the source's modification time in `convert`, `convert-to`, `resize`, `rotate` and `batch convert`. The profile is written unchanged and the pixels are never colour converted, so a profile that does not describe the pixels written (CMYK, or grey for RGB output) is dropped with a note; AVIF output holds EXIF only and JPEG XL output EXIF and XMP (neither writer can embed an ICC profile). `rotate --trim` makes a JPEG flip or turn lossless even when the image is not a whole number of MCUs, by dropping the partial edge row/column (like `jpegtran -trim`). `UseEmbeddedThumbs` decides whether a camera RAW file's embedded JPEG preview is tried before the OS RAW codec (default) or after it; other embedded previews (Office, design, archive covers) have no alternative decoder, so the setting does not apply to them. With a registration present, Apply adds or removes exactly the handler keys of the file types you toggled and broadcasts `SHCNE_ASSOCCHANGED`. Maintenance has Repair (re-register, then clear the thumbnail cache), Rebuild thumbnail cache, Turn on/off thumbnails and Open log. Windows has no API to purge the thumbnail cache, so Rebuild ends this user's `explorer.exe`, deletes `%LOCALAPPDATA%\Microsoft\Windows\Explorer\thumbcache_*.db`, and makes sure the shell is running again; the window asks for confirmation first.

## Install (per-machine MSI)

`Thumbwright-<version>-x64.msi` installs to `Program Files\Thumbwright` for all users and needs elevation (REQ-REG-8). It ships `thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe`, `thumbwright-settings.exe`, `Thumbwright.msix`, `LICENSE` and `THIRD_PARTY_LICENSES.md`, and never a `Thumbwright.ini`, so an installed copy is not in portable mode.

```
msiexec /i Thumbwright-0.1.1-x64.msi /qn      # silent install
msiexec /fa Thumbwright-0.1.1-x64.msi /qn     # repair (re-registers)
msiexec /x  Thumbwright-0.1.1-x64.msi /qn     # uninstall
```

Registration is done by the DLL, not by the MSI's registry tables: a deferred custom action runs `regsvr32 /s thumbwright_shell.dll` (`DllRegisterServer`, `Scope::Machine`: HKLM class and handler keys plus `Shell Extensions\Approved`, journaled under `HKLM\Software\Thumbwright\Registration`); uninstall runs `regsvr32 /u` before the files are removed, and upgrade and repair re-register cleanly. Details and the reasoning: [`packaging/wix/README.md`](../packaging/wix/README.md). Per-user (`twctl register`) is for portable copies only and refuses to run from an installed copy.

The Windows 11 modern menu verb needs package identity. The MSI ships `Thumbwright.msix` (a sparse package; [`packaging/msix/`](../packaging/msix/)) and registers it for all users the way Microsoft documents for machine-wide installers: `Add-AppxPackage -Stage ... -ExternalLocation <install dir>` followed by `Add-AppxProvisionedPackage -Online`; uninstall deprovisions it and removes it for all users. Windows registers a provisioned package for each user at their next sign-in, so the verb appears after the next sign-in (right away with `Add-AppxPackage -RegisterByFamilyName -MainPackage <family name>`). A failure to register does not fail the install (the exit codes are in the verbose MSI log). Reasoning and alternatives: [`packaging/wix/README.md`](../packaging/wix/README.md).

[`scripts/test-install.ps1`](../scripts/test-install.ps1) (elevated, on a test machine) installs, upgrades, repairs and uninstalls the MSI and fails on any registry difference (HKLM and HKCU) between the state before install and after uninstall; for a signed package it also checks that the Windows 11 menu package is provisioned after install and gone after uninstall. `scripts/test-install.ps1 -DryRun` needs no elevation and inspects the package tables and the `.msix` inside instead.

## Release build and signing

```
scripts\release.ps1 -SkipSign                     # unsigned dev build -> dist\Thumbwright-<version>-x64.msi
scripts\release.ps1 -CertThumbprint <sha1>        # signtool with a code-signing certificate in your store: the reproducible route
scripts\release.ps1                               # Azure Trusted Signing: needs $env:TW_SIGN_HELPER (or -SignHelper) and the AZURE_* variables
```

There is no default signing helper: a release with neither `-CertThumbprint` nor a usable helper stops before building, with a message naming both routes. The helper runs with the Azure signing secrets in its environment, so keep it in a folder only you can write to (not a synced or shared one).

Release binaries must not carry the builder's paths (rustc embeds cargo registry paths in panic locations). `release.ps1` therefore passes `--config target.x86_64-pc-windows-msvc.rustflags=[...]` to `cargo build` with a `--remap-path-prefix` for the builder's `USERPROFILE`, rustup home, cargo home, `CARGO_TARGET_DIR` and the repository root. The prefixes are derived from the current machine at run time, so every builder remaps their own paths with no edit, and the script scans the binaries afterwards and fails if any of those paths is still in them. `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` replace the rustflags in `.cargo/config.toml` outright, so `release.ps1` refuses to run with either set. A plain `cargo build --release` outside the script does not remap and its binaries must not be distributed.

The script runs the dependency audit, `cargo build --release --workspace --locked`, collects the binaries into `dist\`, Authenticode-signs `thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe` and `thumbwright-settings.exe` (REQ-SEC-4), and reads the subject of the certificate that signed the DLL. That subject is the sparse package's `Publisher` (it must equal the signing certificate's subject exactly, so it is derived, never typed in). It checks that `thumbwright-settings.exe` embeds no `<msix>` identity element (a packaged process would have its `HKCU` writes redirected away from Explorer; see `docs/ARCHITECTURE.md`, "Settings store"), builds `dist\Thumbwright.msix` with `packaging\msix\build.ps1 -Publisher <that subject>`, signs it the same way and checks its signer subject against the manifest. The MSI is built from the signed files (the `.msix` inside) and signed, and then `signtool verify /pa /v` runs on every file, failing on any that is unsigned or not RFC 3161 timestamped. With `-SkipSign` the package carries the placeholder publisher `-DevPublisher` and is unsigned, so Windows cannot register it. It needs the Windows SDK's `signtool.exe` and `makeappx.exe` and WiX v7 (`dotnet tool install --global wix --version 7.0.0`); WiX v7 requires accepting its Open Source Maintenance Fee EULA once (`wix eula accept wix7`; free below US$10,000/year of revenue, see `packaging/wix/README.md`). Azure Trusted Signing needs `AzureTrustedSignTool` on PATH and `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TRUSTED_SIGNING_ACCOUNT` and `AZURE_TRUSTED_SIGNING_PROFILE` in the environment. `dist\` is gitignored.

## twctl

`twctl --help` lists every command; `twctl <command> --help` shows one. Besides the image verbs (convert, resize, rotate, compress, strip, clip, wallpaper, ...):

- `doctor [file] [--bundle out.zip]`: sectioned self-check (spec 7.1) with FIX hints; foreign ProgID-level thumbnail handlers are reported.
- `batch <thumbnail|convert|info> <in|dir...> [--json]` and `batch --retry-from report.json`: multi-file jobs on all cores; the JSON report lists each file's outcome and the retry reruns only the failures.
- `prebuild <dir|file...> [--size N,N] [--rebuild-all]`: warms Explorer's thumbnail cache (also the "Build thumbnails here" folder-menu entry).
- `pdf` / `cbz <out> <in...> [--strict] [--json]`: combine images; `pdf --searchable` adds an invisible OCR text layer. The JSON result is `{combined, omitted[{input,cause,detail}], output, requested, status}`.
- `ocr <in>`: Windows' built-in OCR engine (runs in `twctl`, never in the shell DLL).
- `rename`, `sort`, `folder-icon`, `lock-screen`, `pick-color`, `settings`.
- `convert-to avif|jxl`: pure-Rust encoders (AVIF through `ravif`/`rav1e` without assembly; JPEG XL is lossless only).
- `--mcp`: MCP server over stdio exposing the CLI's operations plus `view`.

## Diagnostics log

All components append to `%LOCALAPPDATA%\Thumbwright.log`. Failures are always logged (`ERROR GetThumbnail: failed ... ext=<ext> size=<bytes>`). With "Verbose logging" on in the settings window (`VerboseLog=1`), the shell DLL also logs one `TRACE` line per successful request: the class (`thumbnail`, `cloud`, `preview`, `property`, `menu`), the file extension (never the path), the requested size, the decoder that produced the result with its position in the routed chain (`attempt`), the `tw-worker.exe` pid (`-` for an in-process tier), elapsed milliseconds and the output size. Example: `TRACE GetThumbnail: ok class=thumbnail ext=svg size=112 req=256 decoder=Svg attempt=1 worker_pid=4812 ms=21 out=256x128`. With the setting off nothing is formatted or written.

## Clean-room rules

This is an independent implementation built only from [`FUNCTIONAL_SPEC.md`](../FUNCTIONAL_SPEC.md), official Microsoft documentation and permissively licensed crate sources. Do not consult the reference product's binaries or source. The reference product is deliberately not named in this repository.
