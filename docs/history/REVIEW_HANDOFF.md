# Review Handoff: Thumbwright (clean-room Explorer thumbnail + context-menu extension)

> Historical document: the brief given to the independent reviewer in the build room, plus the integration notes added afterwards (section 4b). Review results: `REVIEW_FINDINGS.md` next to this file. Paths and toolchain notes describe the original build room.

Workspace at the time: `C:\CleanRoom\02_Build_Room` (not a git repository; there is no history to diff).
Size at the time: ~10.1k lines of Rust across 4 crates, 133 tests.

## 1. Clean-room rules (binding on the reviewer)

1. Do not read, search or access files outside `C:\CleanRoom\02_Build_Room`.
2. Do not search the web or the file system for the reference product (identified in the private clean-room record) or its source.
3. Allowed references: `./FUNCTIONAL_SPEC.md`, official Microsoft Win32/COM documentation, and docs/sources of permissively licensed Rust crates. Vendored crate sources are under `.toolchain/cargo/registry/src/*/`; prefer them.
4. Report findings; do not rewrite large areas. Small, clearly-correct fixes are fine if each is listed in the report.

## 2. Toolchain

- No Visual Studio on this machine. Rust 1.99.1, rust-lld and MSVC CRT/SDK import libraries (via xwin) live in `.toolchain/` (gitignored).
- Always run cargo through `.toolchain\cargo.cmd`, or dot-source `scripts\devenv.ps1` first in PowerShell.
- The wrapper (and `devenv.ps1`) set `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=rust-lld` and `-Lnative=` paths into `.toolchain/xwin` via environment variables; `.cargo/config.toml` only holds the `xtask` alias, so a machine with Visual Studio builds with plain `cargo`.
- **No C compiler:** crates that build C/C++ (`cc`, `*-sys`) cannot be added.
- Commands:

  ```
  .toolchain\cargo.cmd test --workspace
  .toolchain\cargo.cmd clippy --workspace --all-targets -- -D warnings
  .toolchain\cargo.cmd fmt --all -- --check
  .toolchain\cargo.cmd xtask licenses --check
  .toolchain\cargo.cmd build --release --workspace     (11-28 min; LTO)
  ```
- **Current state:** all 133 tests pass; clippy `-D warnings`, fmt and the license audit (135 crates) are clean.
- **Test environment notes:**
  - The terminal session is elevated. Windows ignores HKCU COM registrations for elevated processes, so any live check through the OS thumbnail cache must run at medium integrity. Launching a `.cmd` via `explorer.exe <file>` worked.
  - Defender scans each freshly built or copied exe on its first launch, sometimes for seconds. Timing-sensitive tests warm up or exclude that first launch.

## 3. Architecture

```
explorer.exe ──IThumbnailCache──> DllHost.exe /Processid:{AB8902B4-...}   (OS thumbnail surrogate)
                                     └─ thumbwright_shell.dll  ThumbnailProvider
                                          ├─ in-proc tier: pure-Rust decoders on a budgeted, panic-contained thread
                                          └─ worker tier: tw-worker.exe per request, Job Object limits + watchdog
explorer.exe (in-proc) ── thumbwright_shell.dll ContextMenu ──spawn──> twctl.exe <verb> --notify <files>
```

| Crate | Output | Responsibility |
|---|---|---|
| `crates/tw-core` | rlib | IDs/GUIDs (`ids.rs`), format matrix (`formats/`), budgets (`limits.rs`), settings (`config.rs`), log (`log.rs`), routing + decoders (`decode/`), worker wire protocol (`protocol.rs`), worker client with Job Object (`worker.rs`), tier pipeline (`pipeline.rs`), registry engine (`registration.rs`) |
| `crates/tw-shell` | `thumbwright_shell.dll` + `tw-worker.exe` | COM exports and class factory (`lib.rs`, `factory.rs`), thumbnail provider (`thumbnail.rs`), DIB creation (`bitmap.rs`), context menu (`menu.rs`), cloud-folder thumbnail provider (`cloud.rs`), preview handler (`preview.rs`), property handler (`property.rs`), Windows 11 modern menu (`explorer_command.rs`), worker main (`src/bin/tw-worker.rs`), integration tests (`tests/`) |
| `crates/twctl` | `twctl.exe` | CLI and menu-verb back end (`main.rs`, `cmds.rs`), image operations (`ops.rs`), lossless JPEG rotate/flip in the DCT coefficient domain (`jpeg_lossless.rs`), EXIF reader/normaliser/carry-over (`exif.rs`), lossless metadata strip (`strip.rs`), clipboard/wallpaper/message box (`win.rs`), `doctor`, `batch` (`jobs.rs`), `prebuild`, PDF/CBZ (`pdf.rs`), OCR (`ocr.rs`), rename/sort (`organize.rs`, `meta.rs`), upload over WinHTTP (`upload.rs`), AVIF/JXL writers (`modern.rs`), dialogs and eyedropper (`ui.rs`, `eyedropper.rs`), MCP server (`mcp.rs`) |
| `crates/tw-settings` | `thumbwright-settings.exe` | Settings window (`ui.rs`, raw Win32 controls via `win.rs`), applying settings and per-user registration toggles (`model.rs`), thumbnail-cache rebuild (`cache.rs`) |
| `xtask` | dev tool | License audit that generates `THIRD_PARTY_LICENSES.md` |

**Key invariants (verify they hold):**
- **I1** The thumbnail class implements only `IThumbnailProvider` + `IInitializeWithStream` (REQ-COM-1). On failure it returns `E_FAIL`, never a placeholder bitmap (REQ-COM-7).
- **I2** No C/C++ decoder runs in the surrogate. WIC, Media Foundation (video), Windows.Data.Pdf, GDI metafile playback (OS C++ codecs), SVG (resvg, not cancellable), audio tags (lofty: unbounded recursion and tag inflation), JPEG 2000 and DjVu (pure Rust, but unable to bound allocation or be cancelled), and 7z/RAR/Live Photo bundles (real decompression with attacker-chosen dictionary sizes; Live Photo stills may need WIC) run only in the worker (`decode/route.rs` `Decoder::placement`). `twctl` follows the same rule for conversions (`ops::decode_full` sends every worker-placed decoder except SVG through `tw-worker.exe`; a test pins it against `Decoder::placement`), so `twctl` never initialises COM or hosts an OS codec; a damaged PNG there is a decode error, not "CoInitialize has not been called".
- **I3** Every tier is bounded:
  - in-proc decoders: 15 s;
  - SVG: 13 s;
  - WIC: 120 s;
  - video frame, PDF page and metafile render: 30 s each;
  - worker Job Object: 512 MiB memory (512 MiB plus the input size for audio, PDF and legacy-Office metafile thumbnails; 1.5 GiB plus the input size for video; 512 MiB plus twice the input size for 7z/RAR), 20 s user CPU, 1 process, kill-on-close; audio wall budget 15 s. The shell may keep up to two pre-spawned workers waiting (30 s TTL, none in `twctl`): a pre-spawned worker is already in its job and has read no input, it serves exactly one request, and when a request takes it the job's memory ceiling is re-applied for that request's decoder (the per-decoder limits above, tested in `worker.rs`).

  `Timeout` and `TooLarge` never fall through to the next tier.
- **I4** No thread of ours outlives the DLL. Finished decode threads are joined; an overrunning thread pins the module (`pipeline::pin_this_module`); the menu preview thread is joined or pins the module; `DllCanUnloadNow` kills the idle pre-spawned workers before it can return `S_OK`.
- **I5** Registration journals every key and value before it writes it (one REG_SZ entry per step under `<product>\Registration`, write-ahead). Unregistration validates the whole journal first, restores replaced values, deletes only created keys that are empty (non-recursive), and keeps failed entries for retry. It never writes an extension's default value (REQ-REG-1/2/4).
- **I6** The context menu does no decoding in Explorer. Verbs spawn `twctl.exe`; the preview goes through `IShellItemImageFactory`, i.e. the OS cache and surrogate.
- **I7** Declared dimensions and the decoded byte budget (`limits::DECODE_ALLOC_BYTES`, 512 MiB) are checked before pixel allocation; archive entries are capped per entry and in total, including failed reads (REQ-SEC-1).
- **I8** Writes are atomic in `twctl`: the staging file is created exclusively (`create_new`), then renamed; generated names are claimed with a non-replacing move.

## 4. What was verified, and how

- Unit and integration tests (see `crates/tw-shell/tests/com_server.rs` and `isolation.rs`). These load the cdylib via `LoadLibrary`/`DllGetClassObject` and validate the `HBITMAP` through GDI.
- **One live run on this machine (release build, portable folder):**
  - `twctl register`, then `IThumbnailCache::GetThumbnail(WTS_FORCEEXTRACTION | WTS_EXTRACTDONOTCACHE)` at medium integrity: `.kra`, `.qoi`, `.svg` and `.tga` gave thumbnails with alpha type 2; random bytes gave `0x8004B200`.
  - The DLL was hosted in `DllHost /Processid:{AB8902B4-...}`, parent `svchost -k DcomLaunch`.
  - The `tw.convert.png` verb through the registered menu class produced a PNG.
  - After `register --off`, an export of `HKCU\Software\Classes` matched the pre-registration export except for six OS timestamp values; zero product keys remained.
- **Not verified:**
  - `DllRegisterServer`/`DllUnregisterServer` against HKLM was never run.
  - JPEG XL: pixel output is checked against the libjxl conformance references (`tw-core/tests/samples.rs`, fixtures in `tests/fixtures/jxl`). Known gaps: CMYK is approximated without a CMS, ICC-profile images that cannot be reduced to an enumerated encoding are rendered in their own colour space, and the `blendmodes` conformance case differs from its reference in semi-transparent regions (jxl-oxide 0.12.6).
  - CR3, CRW, X3F and maker-note parsing in `decode/raw.rs` were checked against about 60 real raw.pixls.us files (CC0); 30 of them are pinned in `scripts/fetch-samples.ps1` and verified by the ignored `real_raw_samples` test (`tests/fixtures/README.md`). CR3 previews come from the `PRVW` box (about 1620x1080), not the full-size JPEG track in `mdat`.
  - The owner-drawn menu preview was never seen in a real Explorer menu.
  - The worker under a Job Object nested inside the surrogate's own job (if any) was only exercised in the live run, not in tests.

## 4b. Current state after integrating the eight work branches

The branches `wip/perf`, `wip/conv-fidelity`, `wip/raw-jxl-verify`, `wip/settings-gui`, `wip/com-classes`, `wip/installer-signing`, `wip/formats` and `wip/menu-verbs` were merged in that order into `integrate`, with cross-slice follow-ups as separate commits.

- **Settings:** one schema in `tw-core/src/config.rs` (loaded from `Thumbwright.ini` or `HKCU\Software\Thumbwright`, saved by `Settings::save`, edited by `thumbwright-settings.exe`). Keys: `ThumbnailsEnabled`, `UseEmbeddedThumbs`, `CloudThumbnails`, `MaxFileMB`, `MaxThumbPx`, `VideoFramePercent`, `JpegQuality`, `PngCompression`, `KeepMetadata`, `PreserveFileDate`, `PdfMargin`, `MenuEnabled`, `MenuAllFileTypes`, `MenuBuildHere`, `MenuPreview`, `MenuOrder`, `DisabledTypes`, `HiddenMenuItems`, `VerboseLog`. The EXIF carry-over switch is `KeepMetadata` (the conversion branch's `KeepExif` was merged into it).
- **Wired in `twctl`:** `JpegQuality` (default of `--quality` and of any JPEG written without one), `PngCompression`, `KeepMetadata`, `PreserveFileDate` (output gets the source's modification time in `convert`, `convert-to`, `resize`, `rotate`), `VideoFramePercent`, `UseEmbeddedThumbs`. `email` (quality 85), `compress` and the lossy-rotate fallback (quality 95) keep their own fixed qualities. `UseEmbeddedThumbs` only changes camera RAW chains (embedded JPEG first, or the OS RAW codec first); Office, design, archive and PSD previews have no second decoder to prefer.
- **Menus:** `menu::TREE` is the one tree; `config::MENU_ITEMS` covers every TREE id (tested), the classic menu and the Windows 11 `IExplorerCommand` menu both build from `menu::display_nodes` (saved `MenuOrder`), hide `HiddenMenuItems`, and always show Settings.
- **Registration:** `twctl register` registers files, `Directory`, `Directory\Background` and the cloud class (journaled); `DllRegisterServer` adds the preview and property handlers and the `Approved` entries (`Layout.approved`). The crash-after-every-journal-entry test runs the full plan with and without `approve`; the machine-scope test covers install, repair, upgrade and uninstall with zero diff.
- **Packaging:** the MSI includes `thumbwright-settings.exe` (`IncludeSettings` is set automatically; an MSI built from this tree's debug binaries lists the DLL, `tw-worker.exe`, `twctl.exe`, `thumbwright-settings.exe`, `LICENSE` and `THIRD_PARTY_LICENSES.md`). `scripts/release.ps1` now requires and signs the settings exe.
- **Tests:** 585 test functions in the workspace (584 run by `cargo test --workspace`, plus the ignored `real_raw_samples`, which reads `TW_SAMPLES`). No test from any branch was dropped; one was renamed and widened (`audio_memory_limit_grows_with_input_and_others_stay_fixed` became `whole_file_decoders_get_input_proportional_memory_and_others_stay_fixed`, which still asserts the audio cases). Tests run in parallel and several start workers or write sandbox registry keys; two consecutive full runs passed.
- **Verified this round (debug build):** `twctl --help`, `twctl formats --json` (363 entries, 315 with `thumbnail: true`), `twctl thumbnail` on PNG, JPEG, SVG, MP4, PDF, JXL and CRW, `twctl rotate` on a JPEG, `twctl convert-to avif` then `twctl info` on the AVIF (decoded through WIC in the worker), `twctl doctor`, `thumbwright-settings.exe --self-test`, and `JpegQuality`/`PngCompression`/`PreserveFileDate` from a portable `Thumbwright.ini` (file sizes and modification time changed as set).
- **Not verified:**
  - Everything that needs elevation: the MSI install, upgrade, repair and uninstall (`scripts/test-install.ps1`), `DllRegisterServer` against HKLM, and the signing steps of `scripts/release.ps1`.
  - The preview handler inside `prevhost.exe`, the property handler in the real property system, the sparse MSIX package and the Windows 11 menu in a real Explorer.
  - The settings window's Rebuild thumbnail cache, Repair and Turn on actions against a live Explorer. The owner-drawn menu preview was checked only in a probe process that hosts the menu class (see the live run below), not in Explorer's own menu.
  - Upload to any public host (hosts are from public API knowledge, never contacted).
  - Most of the 155 formats added by the format-coverage branch were checked only against synthetic fixtures or external tools, not real files from the applications (see the unverified-layout list in section 5 and the branch notes).
  - Release-profile numbers: `docs/performance.md` is from the perf branch's own runs and was not repeated on the integrated tree.

### Live run (release build, medium integrity, 2026-10-18)

Release build (`--release`, own target dir) staged in a portable folder with an empty `Thumbwright.ini`, Windows 11 build 10.0.26340, non-elevated session (Mandatory Label: Medium). `twctl register` (315 file types), then throwaway probes (C# via `csc`, PowerShell, Python; kept outside the repo) drove the real OS thumbnail cache and a menu host. Registry baseline taken before, `twctl register --off` after.

- **Thumbnails** through `IThumbnailCache::GetThumbnail(WTS_FORCEEXTRACTION | WTS_EXTRACTDONOTCACHE, 256)`: every case below returned `S_OK` with alpha type 2 (`WTSAT_ARGB`) and a non-blank bitmap.

  | File | Thumbnail size |
  |---|---|
  | PNG, JPEG (real 7 MB photo), SVG | 256x171, 192x256, 256x171 |
  | TGA, QOI, KRA | 64x48, 64x48, 200x150 |
  | MP3 with cover art | 120x120 |
  | CRW fixture, real HEIC, JXR (written through WIC `WmpBitmapEncoder`) | 256x192, 192x256, 256x192 |
  | MP4 (three real files, 0.36, 4.6 and 138 MB) | 256x106, 144x256, 256x144 |
  | PDF (made with `twctl pdf`) | 256x171 |
  | JXL fixtures `sunset_logo`, `bicycles` | 171x256, 256x158 |

  An MP3 without cover art returned `0x8004B200`. Corrupt PNG, JPEG, SVG and MP4 (random bytes, an unclosed `<svg`, `ZZZ...`) returned `0x8004B200` and each produced a `GetThumbnail: failed ... ext=...` line in `%LOCALAPPDATA%\Thumbwright.log` from the surrogate's pid, which shows the product provider (not a Windows handler) served PNG, JPEG, SVG and MP4. The DLL logs failures only: `VerboseLog=1` adds no success lines (`verbose_log` is read by the settings schema and the settings window, not by the DLL).
- **Hosting:** the provider ran inside `DllHost.exe /Processid:{AB8902B4-09CA-4BB6-B78D-A8F59079A8D5}`, parent `svchost -k DcomLaunch`, with `thumbwright_shell.dll` in its module list. `tw-worker.exe` children with the surrogate as parent were seen during the MP3 cover-art, MP4, PDF, JXL and slow-SVG requests. The surrogate exits when idle, so its pid changed between runs.
- **Hostile corpus** (files built as in `REVIEW_FINDINGS.md`: 16-byte farbfeld 20000x20000, 512-byte SGI 20000x20000x4, 200,000-deep nested `CHAP` MP3 of 5.6 MB, 64 MiB zlib `APIC` MP3 of 65 KB), interleaved with a good PNG: all four returned `0x8004B200` in 85, 91, 286 and 94 ms; the surrogate (one pid, 103068) survived all of them and the PNG kept thumbnailing through that same pid (256x171, alpha 2). Log causes: `input exceeds resource budget` (ff, sgi), `unsupported input` (zbomb), `malformed input: worker failed: failed to fill whole buffer` (deep.mp3, the worker died).
- **Job Object:** `IsProcessInJob` was true for every `tw-worker.exe` seen (pre-spawned idle ones included, and the ones serving a 10.8 s filter-heavy SVG and a 138 MB MP4), and for the surrogate itself, so the worker's job nests inside the surrogate's job and the assignment works. After the hostile run (deep-CHAP MP3 included) and the slow run: no `WerFault.exe` process (polled every 15 ms), no Application Error or Windows Error Reporting event, no new file in `%LOCALAPPDATA%\CrashDumps`.
- **Owner-drawn menu preview** (probe CoCreates the menu class, `IShellExtInit` with a PNG data object, `QueryContextMenu` on a popup, `TrackPopupMenu` on its own window, forwards `WM_INITMENUPOPUP`/`WM_MEASUREITEM`/`WM_DRAWITEM` through `IContextMenu3::HandleMenuMsg2`, screen capture of the `#32768` windows): `MenuPreview=2` shows the 140x140 preview above the "Thumbwright" entry; `MenuPreview=1` shows it at the top of the submenu with the full tree below it (screenshots `menu_preview_mode2.png`, `menu_preview_mode1.png` in the run folder). Every message returned success. 200 `QueryContextMenu`/measure/draw/destroy cycles per setting (mode 1, mode 2, and mode 2 with a second `QueryContextMenu` on the same object): process GDI object count 0 before and after, USER objects flat at 3 to 7, handle count flat (384 or 392). The harness was checked by leaking one bitmap per cycle on purpose: GDI rose by 2 per cycle.
- **Registry:** after `register --off`, `twctl register --status` reported OFF and `HKCU\Software\Thumbwright` did not exist. Against the baseline export of `HKCU\Software\Classes` (16,184 keys): zero product keys added or removed. 6 keys added and 19 changed, all under `Local Settings\Software\Microsoft\Windows\Shell` (Explorer folder-view state for the folder windows the probe opened, `MuiCache` strings, `PCT`/`PTT`/`ICT`/`ITT` timestamps, `MRUListEx`). The one product-name reference that exists (a `MuiCache` `FriendlyAppName` for an earlier `thumbwright-settings.exe` run) is in both exports.
- **Not done, Explorer's own menu:** this build's Windows 11 context menu has no "Show more options" item (UI Automation lists `App extensions` and `Customize menu`), `Shift+F10` opens the new XAML menu, and after five attempts `thumbwright_shell.dll` was not loaded in `explorer.exe`, so the classic `IContextMenu` handler is never reached without the sparse package. The 50-cycle `explorer.exe` GDI sample was therefore not taken (`explorer_gdi.ps1` is in the run folder; it aborts safely when no classic menu opens).
- **Timing seen:** first requests after registration were slow while the DLL and worker were first launched (PNG/JPEG/SVG about 3.2 to 3.4 s, first PDF 18.8 s); the same PDF took 0.4 s on the next run.

## 5. Known gaps (by design for this milestone; don't report as bugs, but do report if they cause harm)

- **Coverage:** 315 of 363 spec formats are routed (`twctl formats --json`, `thumbnail: true`). Unrouted types are never registered, so Windows' own handlers stay in charge. Not routed (48), with reason:
  - MPEG-1/2 and Flash video (`flv mpg mpeg m1v m2v mpv mp2v m2p vob`): Media Foundation cannot open them on the reference OS (no FLV source; the MPEG program-stream source fails without the optional MPEG-2 video extension).
  - Layout not known with confidence and no real files to verify against: `fl32 jnx pdb pwp rmf sct sf3 sfw sti tm2 wpg`, SketchUp (`skp skb`), Rhino `3dm`, 3ds Max `max`, Cinema 4D `c4d`, SolidWorks (`sldprt sldasm slddrw`), `spla`, `cmx`, Paint Shop Pro (`psp pspimage pspbrush pspframe psptube pspshape pspselection pspmask tub`), Affinity (`af afphoto afdesign afpub aftemplate`), and the RAW variants `mdc bay cap pxn`.
  - Metafile thumbnails inside legacy Office files are rendered (`Decoder::OleMetafile`); metafile previews inside DOS EPS and DWG files are not decoded.
- **Missing COM classes:** none. The cloud-folder class (off by default; refuses online-only files, REQ-ISO-5), preview handler and property handler (installer-only: `DllRegisterServer` under HKLM) and the `IExplorerCommand` modern menu exist. Not done: linking the cloud class to a specific sync root, and registering the sparse package (`packaging/msix/build.ps1` only builds it, unsigned; signing and `Add-AppxPackage -ExternalLocation` belong to the release/installer step).
- **Menu verbs:** all spec 6.2 verbs now exist except "Convert..." (dialog) and "Save video frame". Per-item visibility is the `HiddenMenuItems` setting (item ids = the `id` fields in `menu.rs` `TREE`; Settings is never hidden). Order is the `MenuOrder` setting (`config::MENU_ITEMS`), edited on the settings window's Menu page.
- **Conversion fidelity limits (issue #11):**
  - JPEG rotate/flip is lossless (coefficients are permuted, never re-quantised). It folds the EXIF Orientation into the transform and writes Orientation 1. It refuses, and `rotate` then re-encodes at quality 95 with a `note:` line, when a mirrored axis is not a whole number of MCUs (the partial edge MCU would move to the near edge), unless `rotate --trim` is given: that drops the partial edge MCU row/column on mirrored axes (jpegtran `-trim` semantics) and stays lossless, with a `note:` giving the new size; it still re-encodes for arithmetic/12-bit/lossless/hierarchical JPEG, or when the data is damaged. A progressive input is written as a sequential JPEG with identical pixels. Data after EOI (MPO secondary images, vendor trailers such as Samsung motion-photo/depth data, which describe the unrotated picture) and the MPF segment are dropped, reported by a `note:` line; the embedded EXIF thumbnail is dropped too.
  - Metadata (`KeepMetadata`, default on; issue #18) is carried from JPEG, PNG, WebP and TIFF sources by `convert`, `convert-to`, `resize`, `email`, `compress`, `batch convert` and the rotate fallback (`carry.rs`). EXIF (JPEG `APP1`, PNG `eXIf`, WebP `EXIF`, TIFF IFD0 + EXIF IFD + GPS; a TIFF source's maker note is not carried) goes into JPEG, PNG, WebP, TIFF, AVIF (`Exif` item) and JPEG XL (`Exif` box) with Orientation set to 1 and the pixel dimensions updated; the embedded thumbnail is dropped. The ICC profile (JPEG chunked `APP2`, PNG `iCCP`, WebP `ICCP`, TIFF tag 34675) is written unchanged into JPEG, PNG, WebP and TIFF; pixels are not colour converted, so a profile whose data colour space is not the one written (CMYK; grey for RGB output) is dropped with a note, as are incomplete or oversized chunk sequences. XMP (JPEG `APP1`, PNG `iTXt`, WebP `XMP `, TIFF tag 700; Extended XMP is not carried) goes into JPEG, PNG, WebP, TIFF and JPEG XL (`xml ` box) with `tiff:Orientation` set to 1 and `exif:PixelXDimension`/`PixelYDimension`, `tiff:ImageWidth`/`ImageLength` updated. AVIF holds no ICC or XMP and JPEG XL no ICC (the writers cannot embed them); a note says so. `compress` retries without metadata, with a note, when the size limit cannot hold it. A losslessly rotated JPEG keeps all `APPn` segments (XMP is rewritten as above); with `KeepMetadata` off it is stripped of everything but its ICC profile. TIFF output gets IFD0 descriptive tags, resolution, the EXIF IFD (without maker note), GPS, ICC and XMP.
  - `info` reports `bit_depth, datetime, dpi_x, dpi_y, gps, height, make, model, width` (null when unrecorded) plus `channels, file_size, format`; `gps` is `"lat,lon"` in signed decimal degrees.

## 6. Review focus (prioritised)

### A. Security / robustness (highest priority)
1. **Hostile input in in-proc decoders** (`decode/zip_preview.rs`, `psd.rs`, `raw.rs`, `pcx.rs`, `sgi.rs`, `audio.rs`, `jxl.rs`, `raster.rs`). Look for:
   - panics: indexing, slicing, arithmetic overflow, `unwrap` outside tests;
   - unbounded allocation from file-declared sizes;
   - quadratic or exponential loops;
   - IFD/box recursion depth;
   - zip-bomb accounting (does the total cap include every read?);
   - EPUB/OPF XML handling and path traversal inside archives.

   A panic is contained by `catch_unwind`. An abort (OOM, stack overflow, `panic = "abort"`) is not, and would kill the surrogate. Check stack depth and allocation failure paths. Note: `jxl-oxide` is configured without rayon, and `lofty` parses tags in-proc.
2. **Unsafe code:** `tw-shell/src/*.rs`, `tw-core/src/{worker,pipeline,decode/wic}.rs`, `twctl/src/win.rs`. Check:
   - pointer validity and COM out-parameter contracts (`GetCommandString` buffer widths, `InvokeCommand` EX/Unicode detection, `HandleMenuMsg2` `lparam` casts);
   - `STGMEDIUM`/`HDROP` lifetime;
   - `DIBSECTION` copy sizes;
   - clipboard `HGLOBAL` ownership on failure paths;
   - `GetModuleHandleExW` pinning.
3. **Worker isolation** (`tw-core/src/worker.rs`, `protocol.rs`, `src/bin/tw-worker.rs`):
   - Can the worker run decoder code before it is assigned to the job? (It blocks on stdin first; verify.)
   - What happens if `AssignProcessToJobObject` fails inside a restrictive parent job?
   - Response validation before allocation.
   - Behaviour when the worker writes partial output and then hangs.
   - Handle and thread leaks per request.
   - Is `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` + `SetErrorMode` enough to prevent WER dialogs?
4. **Registry engine** (`registration.rs`):
   - manifest corruption or tampering, since it lives under HKCU and is user-writable; consider what happens when unregistering from HKLM with crafted entries;
   - interrupted registration;
   - the "identical value already present" path, which records nothing;
   - restoration of non-string prior values;
   - the `SystemFileAssociations` parent cleanup.
5. **twctl as the menu back end:**
   - command-line injection or argument confusion with file names starting with `--`;
   - the `--files-from` temp file (predictable name in `%TEMP%`, deleted by `twctl`);
   - `to-folder` races;
   - `strip` writing in place (atomicity, symlinks);
   - the wallpaper path in `%LOCALAPPDATA%`.
6. **SVG:**
   - `from_data_nested` is meant to block external file images; confirm that it does, including for `xlink:href` to UNC paths;
   - fontdb loading only when `<text` appears (SVGZ is assumed to contain text);
   - gzip bombs, which are bounded only by the worker memory limit.

### B. Correctness / spec conformance
- Compare against `FUNCTIONAL_SPEC.md`: sections 3.2-3.5, 4.1, 6.1-6.4, 7, 11, 12 (conformance checklist C1-C11).
- `route::plan`: extension-first routing and content sniffing. Are there misroutes (e.g. TIFF-based RAW sniffed as TIFF)?
- Premultiplication and channel order (`decode/mod.rs::to_thumbnail`); never-enlarge rule; EXIF orientation.
- Negative cache key `(stream name, size, mtime)`: stale entries, cap behaviour, caching of timeouts caused by system load.
- `DllCanUnloadNow` accounting (`ModuleRef` in every object and the factory, `LockServer`).
- Menu: id range handling near `idCmdLast`, separator collapsing, `CMF_DEFAULTONLY`, multi-selection gating, hidden-item settings.

### C. Quality / maintainability
- Error types and messages; logging coverage (`log.rs`, which takes a global mutex per line).
- Duplication between `twctl` and `tw-core`.
- Test quality: remove tests that only check wiring or tautologies; find missing boundary tests.
- Performance:
  - the whole stream is read into memory (cap 2 GiB);
  - measured and decided in `docs/performance.md`: worker start-up (pre-spawned single-use workers, shell only), the stream read (straight into the result buffer), `Settings::load` (about 50 microseconds, deliberately not cached) and the resize (`fast_image_resize` Box filter for opaque 8-bit RGB and grey). Open: alpha fringing in the old resize path and whether to use CatmullRom.

### D. QOL
- Missing CLI help or `--help` per command.
- Error text shown in `--notify` message boxes.
- Install/registration ergonomics (machine-wide installer path, a `doctor`-style self-check).
- Build-time pain: release builds take 11-28 min; profile settings live in the root `Cargo.toml`.

## 7. Deliverable

Write `REVIEW_FINDINGS.md` in the workspace root:

1. **Summary verdict**, plus the counts per severity.
2. **Findings table:** `ID | Severity (Critical/High/Medium/Low/Info) | Category (Security/Correctness/Quality/QOL) | File:line | Finding | Evidence | Recommended fix`.
   - Evidence must be concrete: a code excerpt, a failing input, a reproduction command, or a spec clause. Mark unverified reasoning `[INFERENCE]`.
3. **Fixes applied:** file, change and verification command output. Only small, clearly-correct fixes.
4. **Proposed tests:** each one names the plausible bug it would catch.
5. **Spec conformance notes:** C1-C11 status as you assess it.

Re-run the commands in section 2 after any change, and report their results verbatim.
