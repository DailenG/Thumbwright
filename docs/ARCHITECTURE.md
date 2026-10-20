# Architecture

Thumbwright is a Windows Explorer thumbnail provider and context-menu toolkit
in four Rust crates. No crate in the dependency tree compiles or links C/C++;
the OS's own codecs are called only from the isolated worker process.

```
explorer.exe ──IThumbnailCache──> DllHost.exe /Processid:{AB8902B4-...}   (OS thumbnail surrogate)
                                     └─ thumbwright_shell.dll  ThumbnailProvider
                                          ├─ in-proc tier: pure-Rust decoders on a budgeted, panic-contained thread
                                          └─ worker tier: tw-worker.exe per request, Job Object limits + watchdog
explorer.exe (in-proc) ── thumbwright_shell.dll ContextMenu ──spawn──> twctl.exe <verb> --notify <files>
```

## Crates

| Crate | Output | Responsibility |
|---|---|---|
| `crates/tw-core` | rlib | IDs/GUIDs (`ids.rs`), format matrix (`formats/`), budgets (`limits.rs`), settings (`config.rs`), log (`log.rs`), routing + decoders (`decode/`), worker wire protocol (`protocol.rs`), worker client with Job Object (`worker.rs`), tier pipeline (`pipeline.rs`), registry engine (`registration.rs`) |
| `crates/tw-shell` | `thumbwright_shell.dll` + `tw-worker.exe` | COM exports and class factory (`lib.rs`, `factory.rs`), thumbnail provider (`thumbnail.rs`), DIB creation (`bitmap.rs`), context menu (`menu.rs`), cloud-folder thumbnail provider (`cloud.rs`), preview handler (`preview.rs`), property handler (`property.rs`), Windows 11 modern menu (`explorer_command.rs`), worker main (`src/bin/tw-worker.rs`), integration tests (`tests/`) |
| `crates/twctl` | `twctl.exe` | CLI and menu-verb back end (`main.rs`, `cmds.rs`), image operations (`ops.rs`), lossless JPEG rotate/flip in the DCT coefficient domain (`jpeg_lossless.rs`), EXIF reader/normaliser/carry-over (`exif.rs`), lossless metadata strip (`strip.rs`), clipboard/wallpaper/message box (`win.rs`), `doctor`, `batch` (`jobs.rs`), `prebuild`, PDF/CBZ (`pdf.rs`), OCR (`ocr.rs`), rename/sort (`organize.rs`, `meta.rs`), AVIF/JXL writers (`modern.rs`), dialogs and eyedropper (`ui.rs`, `eyedropper.rs`), MCP server (`mcp.rs`) |
| `crates/tw-settings` | `thumbwright-settings.exe` | Settings window (`ui.rs`, raw Win32 controls via `win.rs`), applying settings and per-user registration toggles (`model.rs`), thumbnail-cache rebuild (`cache.rs`) |
| `xtask` | dev tool | Dependency audit that generates `THIRD_PARTY_LICENSES.md` |

`twctl --notify` feedback (menu invocations only): `feedback.rs` collects what a verb created (`feedback::created`) or copied (`feedback::confirm`) and, after the verb returned, `feedback::finish` selects the new files in Explorer (`win::reveal`, `SHOpenFolderAndSelectItems`, one call per folder), or shows a small auto-closing confirmation window near the cursor (clipboard-style verbs), or the error box. Per-file jobs (`batch` in `main.rs`) show a topmost progress window with Cancel once they run longer than ~1.5 s (~0.4 s for more than three files). Errors are rewritten into one plain sentence plus the technical detail by `friendly.rs`; Ctrl+C in the box copies the text.

## Invariants (I1-I8)

These are binding on every change. If a change needs to break one, open an
issue first and say which invariant and why.

- **I1 — surface contract.** The thumbnail class implements only
  `IThumbnailProvider` + `IInitializeWithStream`. On failure it returns
  `E_FAIL`, never a placeholder bitmap.
- **I2 — no untrusted decode in the shell's process.** No C/C++ decoder and no
  decoder that cannot prove it is memory- and stack-safe runs inside the
  surrogate (or Explorer). WIC, Media Foundation (video), Windows.Data.Pdf, GDI
  metafile playback, SVG (resvg is not cancellable), audio tags (lofty:
  unbounded recursion and tag inflation), JPEG 2000 and DjVu (pure Rust but
  unable to bound allocation), HEIC/HEIF (`open-heic`: pure Rust and
  `forbid(unsafe)`, but its HEVC decoder is young and panics on corrupt
  streams, so worker isolation is the safe call) and 7z/RAR/Live Photo bundles (real
  decompression with attacker-chosen parameters) run only in `tw-worker.exe`
  (`decode/route.rs` `Decoder::placement`). `twctl` follows the same rule for
  conversions: a test pins every worker-placed decoder except SVG against the
  placement table.
- **I3 — every tier is bounded.** In-proc decoders 15 s; SVG 13 s; WIC 120 s;
  video frame, PDF page and metafile render 30 s each; worker Job Object 512
  MiB base memory (plus input-proportional allowances per decoder: 1.5 GiB for
  video, 2x for archives), 20 s user CPU, one process, kill-on-close. The
  shell may keep up to two pre-spawned workers waiting (30 s TTL): a
  pre-spawned worker is already in its job and has read no input, serves
  exactly one request, and gets the per-decoder memory ceiling re-applied when
  taken. `Timeout` and `TooLarge` never fall through to the next tier.
- **I4 — no thread of ours outlives the DLL.** Finished decode threads are
  joined; an overrunning thread pins the module (`pipeline::pin_this_module`);
  the menu preview thread is joined or pins the module; `DllCanUnloadNow`
  kills idle pre-spawned workers before it can return `S_OK`.
- **I5 — registration is journaled and reversible.** Every key and value is
  recorded (write-ahead, one REG_SZ per step under
  `<product>\Registration`) before it is written. Unregistration validates the
  whole journal first, restores replaced values, deletes only keys the
  install created and only when they are empty (non-recursive), keeps failed
  entries for retry, and never writes an extension's default value or ProgID.
- **I6 — no decoding in Explorer from the menu.** Menu verbs spawn
  `twctl.exe`; the classic menu's preview row goes through
  `IShellItemImageFactory`, i.e. the OS cache and surrogate.
- **I7 — allocation is gated.** Declared dimensions and the decoded byte
  budget (`limits::DECODE_ALLOC_BYTES`, 512 MiB) are checked before pixel
  allocation; archive entries are capped per entry and in total, including
  failed reads.
- **I8 — writes are atomic.** In `twctl` the staging file is created
  exclusively (`create_new`), then renamed; generated output names are
  claimed with a non-replacing move.

## Format coverage

315 of the 363 formats in the spec matrix have a thumbnail route
(`twctl formats --json`, entries with `thumbnail: true`). Unrouted types are
never registered, so Windows' own handlers stay in charge. Which formats are
verified against real files versus synthetic fixtures is tracked in
`docs/history/REVIEW_HANDOFF.md` section 5 and the issues; fixture provenance
is in `crates/tw-core/tests/fixtures/README.md`.

## Deliberate omissions

- **Upload (spec 6.2, 6.4, 10.3).** Removed in 0.1.1: the default hosts were third-party
  services the project does not want to send users' files to. With it gone no component
  performs network I/O, which is stricter than REQ-NET-1/2 require.

## Settings

One schema in `tw-core/src/config.rs`, loaded from `Thumbwright.ini` (portable
mode; its presence is the marker) or `HKCU\Software\Thumbwright` (installed
mode), saved by `Settings::save`, edited by `thumbwright-settings.exe`. The
same keys are honoured by the shell DLL and `twctl`.

### Settings store: why the settings window has no package identity

The Windows 11 menu verbs need a sparse MSIX package (`packaging/msix`), and the package
must name an `Application` executable. `thumbwright-settings.exe` is that executable, but
it deliberately does **not** embed the `<msix>` identity element in its manifest, so it
always runs unpackaged.

Why: a process that has package identity gets registry write virtualization. Its writes to
`HKCU\Software\Thumbwright` land in the package's private hive
(`%LOCALAPPDATA%\Packages\<package family>\SystemAppData\Helium\User.dat`), while the shell
DLL loaded into Explorer has no identity and reads the real `HKCU` key. In 0.1.1 the
settings exe had identity, so every setting saved from the window was invisible to
Explorer (confirmed: the real key was empty while the private hive held the values).

Alternatives considered and rejected: declaring `desktop6:RegistryWriteVirtualization`
disabled plus `rescap:unvirtualizedResources` (a restricted capability to declare and
validate, with no visible failure when it is not honoured), and saving through a
`twctl settings-save` child process (an extra moving
part that exists only to dodge the identity). Dropping the element needs nothing from the
sparse package: the Explorer verb is a COM class the package registers itself, and nothing
in the settings window uses package identity.

Guards: `scripts/release.ps1` fails the release if the built settings exe contains an
`<msix>` element, `scripts/test-install.ps1` checks the installed and the extracted copy,
and `twctl doctor` reports how many values `HKCU\Software\Thumbwright` holds. Values saved
by a 0.1.1 settings window stay in the package's private hive and are read only by
packaged processes (the COM surrogate of the Explorer verb); they disappear when the
package is removed (the MSI removes it on uninstall).

## History

`docs/history/` holds the project's review artefacts: the reviewer brief
(`REVIEW_HANDOFF.md`, which the invariants above are extracted from), the
security review and its fixes (`REVIEW_FINDINGS.md`), and the live-run logs.
They are records, not living documentation; this file is the living one.
