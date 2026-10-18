# Review Handoff: Thumbwright (clean-room Explorer thumbnail + context-menu extension)

Workspace: `C:\CleanRoom\02_Build_Room` (not a git repository; there is no history to diff).
Size: ~10.1k lines of Rust across 4 crates, 133 tests.

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
| `crates/tw-shell` | `thumbwright_shell.dll` + `tw-worker.exe` | COM exports and class factory (`lib.rs`, `factory.rs`), thumbnail provider (`thumbnail.rs`), DIB creation (`bitmap.rs`), context menu (`menu.rs`), worker main (`src/bin/tw-worker.rs`), integration tests (`tests/`) |
| `crates/twctl` | `twctl.exe` | CLI and menu-verb back end (`main.rs`), image operations (`ops.rs`), lossless metadata strip (`strip.rs`), clipboard/wallpaper/message box (`win.rs`) |
| `xtask` | dev tool | License audit that generates `THIRD_PARTY_LICENSES.md` |

**Key invariants (verify they hold):**
- **I1** The thumbnail class implements only `IThumbnailProvider` + `IInitializeWithStream` (REQ-COM-1). On failure it returns `E_FAIL`, never a placeholder bitmap (REQ-COM-7).
- **I2** No C/C++ decoder runs in the surrogate. WIC (OS C++ codecs), SVG (resvg, not cancellable) and audio tags (lofty: unbounded recursion and tag inflation) run only in the worker (`decode/route.rs` `Decoder::placement`).
- **I3** Every tier is bounded:
  - in-proc decoders: 15 s;
  - SVG: 13 s;
  - WIC: 120 s;
  - worker Job Object: 512 MiB memory (512 MiB plus the input size for audio), 20 s user CPU, 1 process, kill-on-close; audio wall budget 15 s.

  `Timeout` and `TooLarge` never fall through to the next tier.
- **I4** No thread of ours outlives the DLL. Finished decode threads are joined; an overrunning thread pins the module (`pipeline::pin_this_module`); the menu preview thread is joined or pins the module.
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

## 5. Known gaps (by design for this milestone; don't report as bugs, but do report if they cause harm)

- **Coverage:** 160 of 363 spec formats are routed. Unrouted types are never registered, so Windows' own handlers stay in charge. Not handled: video, PDF/DjVu, legacy binary Office, WMA/DSF, APK, most of the legacy raster long tail.
- **Missing COM classes:** cloud-folder, preview handler, property handler, Windows 11 modern menu.
- **Missing menu verbs:** OCR, upload, combine PDF/CBZ, rename, sort, pick color, lock screen, folder icon, settings, AVIF/JXL output. `doctor`, `batch` and `--mcp` CLI commands are also missing.
- **Simplifications:**
  - Rotation re-encodes JPEG at quality 95 instead of rotating losslessly.
  - EXIF is not carried over on conversion.
  - `info` reports a reduced key set.

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
  - a worker process is spawned per SVG/WIC request;
  - `Settings::load` runs on every `GetThumbnail`;
  - thumbnail resize uses `DynamicImage::thumbnail`.

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
