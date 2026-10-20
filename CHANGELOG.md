# Changelog

Notable changes to Thumbwright. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) with a pre-1.0 caveat: until 1.0, interfaces and
on-disk formats may still change.

## [Unreleased]

### Changed

- App, extension and game packages have their own **Package** family instead of being listed
  under Design: APK, APKS, XAPK, APKM, NuGet (`.nupkg`), Visual Studio extensions (`.vsix`)
  and Minecraft Bedrock packs, add-ons, worlds and world templates. `twctl formats`, the
  website's format browser and the README count them separately. The `design` and `cad`
  starter kits no longer turn on these types; `thumbnails` and `everything` still do.
- **The Windows 11 right-click popup opens without waiting for the file's preview.** It used
  to hold back until Windows handed over the thumbnail, so it appeared after about 125 to
  140 ms; it now appears after about 80 to 95 ms (median of warm runs on the development
  machine, from starting `twctl` to the menu on screen). The file name and size are in the
  menu at once, with an empty box for the thumbnail above them, and the thumbnail is drawn
  into the box a few tens of milliseconds later. The box is always as tall as the largest
  thumbnail, so a wide picture is centred in it instead of the menu shrinking to fit it. For
  a file Windows has no thumbnail for (a text file, say) the box shows the file's icon,
  centred, so the menu never changes size or moves after it opens.

### Fixed

- **Right-click menu: rows no longer start halfway across** ([#27]). With a preview at the top,
  every item was pushed right to where the thumbnail ended, in the Windows 11 popup and in the
  classic menu's Thumbwright submenu. The cause: the thumbnail was attached as the header's
  menu bitmap, and Windows reserves one bitmap column of that width for every row. The header
  is now two rows: the thumbnail on its own row, and under it an ordinary text row with the
  file name and size. Items start at the normal left edge, the thumbnail lines up with them,
  and the menu keeps its Windows look in light and dark. The name and size are native menu
  text, so screen readers read them, with or without a thumbnail.
- **"Build thumbnails for this folder" is on the Windows 11 menu** ([#26]), for a folder and
  for a folder's background. Windows 11 shows only packaged verbs at the top level, and the
  folder entry was a classic handler only, so it was hidden under "Show more options". It is
  now a packaged verb too, and the classic entry steps aside while the package is registered,
  so "Show more options" does not list it twice.
- **"Convert to" selects the new file in an Explorer window that is already open.** When the
  output replaced a file deleted moments earlier, the window had not yet caught up and cleared
  its selection instead. Thumbwright now tells the shell about the new file (a flushed
  `SHChangeNotify`) before asking Explorer to select it; 10 of 10 runs selected it.
- **"Build thumbnails for this folder" no longer ends in a box you must click OK on.** It now
  behaves like the other menu actions: a progress window with Cancel when the folder takes a
  while ("Building thumbnails for 40 files... 2 of 40"), then a short confirmation that closes
  on its own ("Thumbnails ready for 40 files"). Only a failure still opens a message box.
- **Every Thumbwright window shows the Thumbwright icon in its title bar**, not just in the
  taskbar: progress windows, prompts (for example "Rename with pattern"), text windows and
  message boxes in both `twctl` and Workbench. Their title bars follow dark mode like
  Workbench's.
- **Window layout:** the progress window's right margin was cut off by the window frame, and
  the rename prompt had uneven margins, a grey box behind its label and an old system font.
  Both are now laid out inside the window's client area with even margins, in Segoe UI.
  Workbench's message boxes say "Thumbwright Workbench" instead of the old name "Thumbwright
  Settings".
- **Workbench pages are aligned and use the current names** ([#28]). The Menu page said
  "Settings is always in the menu" and "Build thumbnails here"; it now says "Thumbwright
  Workbench" and "Build thumbnails for this folder", as the menu does. The General and
  Thumbnails pages share one label column, input column and hint column, with ranges written
  as "1-100" everywhere, and the General page's inputs sit next to their labels with a right
  margin. The File types status line no longer wraps over the search box.

[#26]: https://github.com/DailenG/Thumbwright/issues/26
[#27]: https://github.com/DailenG/Thumbwright/issues/27
[#28]: https://github.com/DailenG/Thumbwright/issues/28

## [0.1.3] - 2026-10-19

### Changed

- **HEIC and HEIF thumbnails and conversions no longer need Microsoft's Store codecs.**
  `.heic`, `.heif`, `.hif`, `.heics` and `.heifs` (iPhone, Android and camera photos: HEVC
  stills, tiled grids, embedded thumbnails, rotation and mirroring, 8/10-bit) are decoded by
  [`open-heic`](https://github.com/DailenG/open-heic), a pure-Rust reader (MIT OR Apache-2.0,
  `forbid(unsafe_code)`) pinned by commit. It runs in the isolated worker, because its HEVC
  decoder is young and panics on corrupt streams; the shell asks for the file's embedded
  thumbnail first, which takes milliseconds. Windows' own codecs stay as the fallback for what
  `open-heic` cannot read (JPEG-coded HEIF items, overlays, 4:2:2/4:4:4 chroma), and a JPEG
  saved under a `.heic` name is decoded in-process. Live Photo (`.livp`) stills use the same
  path. Known limit: the separate transparency plane Apple writes for portrait-mode cutouts is
  not applied, so those images show opaque. AVIF and AVCI are unchanged (Windows codecs only).
- `twctl doctor` now says HEIC/HEIF thumbnails are built in and lists the Store HEIF and HEVC
  extensions as an optional fallback.
- The format table no longer lists an OS codec for the HEIC family.

## [0.1.2] - 2026-10-19

### Changed

- **Workbench, File types:** a new "Before Thumbwright" column shows which handler each type
  replaced (for example Microsoft's Photo Thumbnail Provider), and the page says that unticking
  a type hands it back to that program. Types Thumbwright added fresh show nothing.
- **Right-click menu, Windows 11:** the packaged entry is now one "Thumbwright" item (it was
  a submenu that Windows 11 showed empty, or cut off after 16 commands). Clicking it opens
  Thumbwright's own popup at the cursor (`twctl menu`), in the system's dark or light theme,
  with a header row (file name, size, preview). The classic menu no longer adds a second
  "Thumbwright" entry on Windows 11 while the package is registered for the user (folders
  keep "Build thumbnails for this folder"). `THUMBWRIGHT_CLASSIC_MENU=1` brings the classic
  menu back for diagnosis.
- **One menu structure, shorter and clearer,** shared by both menus: Convert to, Resize,
  Rotate & flip, Make smaller (email sizes, under 1/5/10 MB, remove location & camera info),
  Copy text from image, Copy, Combine into (2+ files), Organise (rename, sort, move into a new
  folder), Details, Set as (lock screen, folder icon), Thumbwright Workbench. Items that do not
  apply to the selection are hidden, not greyed (audio renames only for audio, Combine only
  for 2+ files, Set as only for one file). Saved menu order and hidden items keep working for
  the items that kept their id (`convert_into`, `resize`, `rotate`, `copy_text`, `copy`,
  `image_info`, `settings`). Items that merged into groups (`email`, `compress`, `strip_meta`
  into `make_smaller`; the combine, rename, sort and folder items into `combine` and
  `organise`; `lock_screen` and `folder_icon` into `set_as`) start visible again, and their
  old hidden or ordered entries are ignored.
- **Every action now shows its result.** A file-producing action opens (or reuses) an
  Explorer window with the new files selected; a clipboard action shows a short "Copied"
  confirmation near the cursor; actions over many files or taking longer than a moment show a
  small progress window with Cancel. Error messages are plain language with the technical
  detail on a second line (Ctrl+C copies them), and never show command usage text.
- The settings window is now "Thumbwright Workbench": one instance per session (a second
  launch brings the first to the front), the product icon, a dark title bar when apps use
  the dark theme, clearer labels, a single "Show a preview of the file in the menu" check
  box (both old placement values read as on), and an honest status line on installed copies
  ("Active for all users. N of 315 types use Thumbwright, M are handled by other apps").
- **Installer:** a minimal setup wizard (welcome, licence summary, progress, finish with
  "Open Thumbwright Workbench"), a "Thumbwright Workbench" Start Menu shortcut, and silent
  presets: `msiexec /i Thumbwright-0.1.2-x64.msi /qn MENU=0` installs thumbnails only;
  `KIT=photos|paperwork|design|cad|thumbnails|everything` picks a Starter kit. Chocolatey:
  `--params "/Menu:0 /Kit:photos"`. Silent installs show no UI. A kit is the first-run
  default for users who have not saved settings: it turns off the file-type families it does
  not need and hides the right-click items it does not use (Photos: RAW, images and video,
  no Copy or Set as; Paperwork: documents, ebooks, comics and images, with Convert to, Make
  smaller, Copy text and Combine; Design: design files and images, with Convert to, Resize,
  Copy and Set as; CAD: design files, menu off; Thumbnails: every type, menu off;
  Everything: no change). `MENU=0` still wins over a kit's menu.
- New product icon, website and README.

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
- Classic menu: when the shell offered a short command-id range, the whole "Thumbwright"
  entry vanished; it now takes the first id and shows what fits.
- Classic menu: the preview row broke dark mode (light preview background, white separator
  bars, invisible submenu arrows). The preview is now a themed item bitmap.
- Windows 11 popup (`twctl menu`): the header row often had no preview. The popup gave the
  shell 150 ms to hand over the thumbnail, which a file the shell had not thumbnailed yet (or
  a busy machine) overran. It now waits up to 400 ms and starts the fetch first.
- Labels containing `&` ("Rotate & flip") lost the character to a menu mnemonic.
- `twctl doctor`: names Windows 11 by its build number, no longer says the cloud and
  embedded-preview settings do not exist, and reports how many values the settings store
  holds. A file with no preview to draw (`unsupported input`) is logged as INFO, not ERROR.

### Removed

- "Set as wallpaper" (Windows has "Set as desktop background") and "Pick color" from the
  file menu. `twctl wallpaper` and `twctl pick-color` remain.

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
