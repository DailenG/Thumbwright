# Changelog

Notable changes to Thumbwright. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) with a pre-1.0 caveat: until 1.0, interfaces and
on-disk formats may still change.

## [Unreleased]

### Fixed

- **Settings now reach Explorer.** `thumbwright-settings.exe` carried the sparse package's
  identity, so Windows redirected its `HKCU` writes into the package's private registry
  hive and Explorer never saw them. The executable no longer embeds the identity element
  (`release.ps1` and `test-install.ps1` check this); see `docs/ARCHITECTURE.md`.
- **Removing a file type or uninstalling no longer deletes another program's handler.** The
  registration journal now records the value Thumbwright wrote, and undo removes or
  restores a value only while it still holds that. A value another program has taken over
  stays, and is reported ("taken over by another program, left alone") by `twctl register
  --off`, the Workbench and `twctl doctor`. Journals written by earlier versions undo as
  before.
- `twctl doctor`: names Windows 11 by its build number, no longer says the cloud and
  embedded-preview settings do not exist, and reports how many values the settings store
  holds. A file with no preview to draw (`unsupported input`) is logged as INFO, not ERROR.

### Changed

- The settings window is now "Thumbwright Workbench": one instance per session (a second
  launch brings the first to the front), the product icon, a dark title bar when apps use
  the dark theme, clearer labels, a single "Show a preview of the file in the menu" check
  box, and an honest status line on installed copies ("Active for all users. N of 315
  types use Thumbwright, M are handled by other apps").

## [0.1.1] - 2026-10-19

### Fixed

- **Settings** and **Pick color** in the context menu failed with "takes exactly 0 file
  argument(s)": the menu passed the selected files to `twctl` commands that take none.

### Added

- Product icon (`packaging/icons/`): embedded in every executable and the DLL, shown for
  the Windows 11 menu entry, in Programs and Features, and in the Windows 11 extension list
  (the sparse package now ships its logos at every scale and target size, indexed in
  `resources.pri`; it previously had single placeholder images and no index, which some
  shell lists rendered blank).

### Removed

- **Upload** (menu verb and the `upload`, `upload-hosts` and `upload-history` commands). Its
  default hosts were third-party services; with it gone no Thumbwright component makes a
  network connection.

## [0.1.0] - 2026-10-19

First public release. Clean-room implementation of a Windows Explorer thumbnail
provider, context-menu toolkit and image CLI, written in Rust. No C or C++ is
compiled anywhere in the dependency tree; OS codecs (WIC, Media Foundation,
Windows.Data.Pdf, GDI metafiles) are called only from the isolated worker
process.

### Added

- **Explorer integration**
  - Thumbnail provider (`IThumbnailProvider` + `IInitializeWithStream`), hosted
    by the OS thumbnail surrogate, for 315 of the 363 formats in the spec matrix:
    camera RAW (embedded previews, verified against 30 real CC0 files), JPEG XL
    (verified against the libjxl conformance set), JPEG 2000, HEIC/AVIF and JXR
    (OS codecs), video frames at a configurable offset, PDF, legacy Office and
    OLE previews, DjVu, ebooks, 7z/RAR archives, Android packages, Live Photos,
    design and CAD previews, meshes, and the scientific and legacy bitmap long
    tail. Unrouted types keep Windows' own handlers.
  - Classic context menu (`IContextMenu{,2,3}`) with an owner-drawn preview row,
    and a Windows 11 modern menu (`IExplorerCommand`) declared by a signed
    sparse MSIX package.
  - Cloud-folder thumbnail provider (off by default; refuses online-only files,
    so a placeholder is never recalled).
  - Preview-pane handler and read-only property handler (installer-only
    registration).
- **Crash isolation**: every decoder that cannot prove it is memory- and
  stack-safe in-process (OS codecs, SVG, audio tags, JPEG 2000, DjVu,
  archives) runs in `tw-worker.exe`, one process per request under a Job Object
  (512 MiB to 1.5 GiB by decoder, 20 s CPU, kill-on-close). Verified live: the
  hostile corpus from the security review (farbfeld bomb, SGI header bomb,
  deeply nested ID3 chapters, compressed APIC) fails cleanly with the surrogate
  surviving and no WER dialog.
- **Journaled registration**: every registry write is recorded before it
  happens; unregister restores exactly what was there, including values the
  install replaced, and deletes only keys it created. Crash-at-any-point and
  corrupt-journal cases are covered by tests.
- **`twctl` CLI** — per-user registration, image verbs (convert, resize, rotate,
  compress, strip, clipboard, data URI, wallpaper, info), `doctor` (13-section
  self-check), `batch` with `--retry-from`, `prebuild` (warms the Explorer
  cache), `pdf`/`cbz` with an optional invisible OCR layer, `ocr`
  (Windows' engine), `rename`/`sort` by EXIF or audio tags, `upload`
  (WinHTTP, explicit confirmation only), AVIF and JPEG XL output (pure Rust),
  `folder-icon`, `lock-screen`, `pick-color`, `settings`, and an MCP server
  over stdio.
- **Conversion fidelity**: JPEG rotate/flip is lossless (DCT-coefficient
  transform; no re-quantisation), with `--trim` for partial edge MCUs. EXIF,
  ICC profiles and XMP carry across conversion into JPEG, PNG, WebP and TIFF
  (`KeepMetadata`, default on), with the output's orientation normalised.
- **`thumbwright-settings.exe`**: settings window (General, File types, Menu,
  Limits, Preview, Maintenance) over the full settings schema, portable INI or
  per-user registry; file-type toggles go through the journaled registration
  engine.
- **Installer**: per-machine WiX MSI that registers through the DLL's own
  journaled engine (`regsvr32` custom actions), provisions the sparse package
  for all users, and registers it for the installing user so the Windows 11
  verb appears without a sign-out. Uninstall leaves zero product keys
  (HKLM and HKCU), verified by a scripted registry diff over install, upgrade,
  repair and uninstall.
- **Release pipeline** (`scripts/release.ps1`): build, Authenticode-sign all
  binaries and the sparse package (Azure Trusted Signing or a local
  certificate; RFC 3161 timestamped), package, verify.
- **Performance**: measured and decided in `docs/performance.md` — settings
  reads stay uncached (µs-scale), the shell pre-spawns single-use workers
  (still one request per process), thumbnails of opaque 8-bit images resize
  through a Box filter, transparent images are premultiplied first so hidden
  colour does not fringe the edges.

### Security

- Registration never touches an extension's ProgID or default value, and
  reports foreign handlers it would be overriding (`doctor`).
- All shipped binaries link the CRT statically (no VC++ redistributable
  needed) and are Authenticode-signed with a timestamp.
- A full hostile-input security review and its fixes are documented in
  `docs/history/REVIEW_FINDINGS.md`; the fixes are re-verified by tests and
  the live runs above.
- The pre-public audit's resource-exhaustion findings are closed for the in-process
  decoders: memory-abort paths (APK manifest strings, Aseprite cels, extreme
  aspect ratios, `IStream` sizes that are declared but never delivered) now
  fail cleanly; every identified quadratic loop (ARSC, EPUB spine, XML
  entities, FB2, DXF, XMP/INDD/EPS, DWG/ICNS payloads, CLIP/SQLite, XCF,
  RAW IFD walk) is bounded or linear; at most two overrunning decode threads
  are tolerated per process before in-process work fails fast. Per-decode
  accounting stays per-buffer inside the individual decoders, see
  `docs/history/AUDIT_DECODE_HARDENING.md` for exactly what remains.
- **MSI**: the install directory is now a private Windows Installer property
  (`InstallFolder`, was the public `INSTALLFOLDER`), so `msiexec ... INSTALLFOLDER=...`
  can no longer redirect the SYSTEM-run `regsvr32` or inject PowerShell into the package
  actions; an install directory containing a single quote is refused (audit P-1, P-2).
  `scripts/test-install.ps1 -DryRun` asserts this.
- **Worker**: the Job Object now also carries UI restrictions (clipboard, desktop, display
  settings, global atoms, system parameters, other processes' USER handles), and the worker
  and the menu's `twctl` launch get exactly their three standard handles and no other
  inherited handle (audit U-1, U-4).
- **Release builds** remap the builder's cargo, rustup, profile and target paths out of the
  binaries and verify it; `release.ps1` has no built-in signing-helper path (`TW_SIGN_HELPER`
  or `-SignHelper`; `-CertThumbprint` is the reproducible route) (audit P-5, P-6).
- **CI**: actions pinned by commit SHA, Dependabot for cargo and actions, a weekly and
  on-change `cargo audit` job, job timeout and `--no-fail-fast` (audit P-7). The one accepted
  advisory (`paste`, RUSTSEC-2024-0436) is documented in `SECURITY.md` (audit H-3).

### Notes

- JPEG XL output is lossless-only; CMYK and unconvertible ICC profiles are
  approximated (documented in the fixtures README).
- Windows 11 may keep a newly installed menu extension hidden until the user
  enables it in **Customize menu**; that is a per-user Windows preference the
  installer deliberately does not write.
- Some format families rest on synthetic fixtures and published layout
  documentation rather than real application files; the list is tracked in
  issue #16 and `docs/history/REVIEW_HANDOFF.md` section 5.
