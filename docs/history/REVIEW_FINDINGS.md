# Review Findings: Thumbwright

Reviewed 2026-10-17 against `REVIEW_HANDOFF.md` and `FUNCTIONAL_SPEC.md`. Clean-room rules were followed: all reads, probes, fixtures and runtime state stayed inside `C:\CleanRoom\02_Build_Room`. Nothing was registered. HKLM was not touched. The only registry writes came from the existing test sandbox (`HKCU\Software\ThumbwrightTest-*`), which removes itself.

## 1. Summary verdict

**Not ready to ship.** The surrogate host can be terminated by small hostile files, and that was reproduced. `catch_unwind` handles panics correctly, but three in-process decoders can still abort the process: one through stack overflow and two through multi-GB allocation requests. A fourth path allocates without a working limit. Panics are contained; aborts are not. Outside the decoders, the architecture is sound: tier placement, worker Job Object, response validation, COM out-parameters and dual-key registration all hold up.

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 4 |
| Medium | 6 |
| Low | 4 (1 fixed) |
| Info | 2 |

Invariants:

| Inv | Verdict | Evidence |
|---|---|---|
| I1 | Holds | `thumbnail.rs:37` `#[implement(IThumbnailProvider, IInitializeWithStream)]`; failures return `E_FAIL` (`thumbnail.rs:88-98`); QI and hostile-input tests in `com_server.rs:264-360` pass. |
| I2 | Holds | `route.rs:60-65` places only `Svg`/`Wic` in `Worker`; `pipeline.rs:55-72` honours it. WIC is called only from `tw-worker`. |
| I3 | Holds for time; does not bound memory or stack | `pipeline.rs:109-117`, `worker.rs:128-139`, `limits.rs`. `Timeout`/`TooLarge` do not fall through (`decode/mod.rs:42-44`). Aborts in F1-F4 end the surrogate before any budget applies. |
| I4 | **Fails** (menu) | Decode threads: joined or pinned (`pipeline.rs:109-117`). Menu preview: F8. |
| I5 | Holds when operations succeed; **fails on failure and interruption** | F9, F10. Restoring non-string prior values is correct: type and raw bytes are recorded (`registration.rs:161-165`) and restored with `set_bytes` (`:261-271`). |
| I6 | Holds | Menu only spawns `twctl` (`menu.rs:769-793`). The preview uses `IShellItemImageFactory` (`menu.rs:527-539`). |
| I7 | **Fails** | F2, F3, F4, F5. PCX, PSD composite, JXL and ZIP per-entry limits were checked and are fine. |
| I8 | **Qualified failure** | Every write goes through temp + rename, but F6 shows the staging file can redirect the write before the rename. |

## 2. Findings

| ID | Severity | Category | File:line | Finding | Evidence | Recommended fix |
|---|---|---|---|---|---|---|
| F1 | High | Security | `crates/tw-core/src/decode/audio.rs:21-34` (consumed via `pipeline.rs:96-107`) | lofty parses nested ID3 `CHAP`/`CTOC` frames recursively with no depth limit. Audio decoding runs in-process in the surrogate. A stack overflow is an abort, so `catch_unwind` cannot contain it. | **Reproduced, release build.** Fixture: ID3v2.3 tag with 200,000 nested `CHAP` frames (28 B each, 5.6 MB file) plus 4 MPEG frames, run through `pipeline::run_inproc(Decoder::Audio, ...)`, the exact surrogate path: `thread 'tw-decode' has overflowed its stack`, exit 253. A 32-level control returns `Err(Unsupported)`. Recursion chain in vendored lofty 0.25.4: `id3/v2/items/chapter_frame.rs:197-198` -> `id3/v2/read.rs:155-165` -> `frame/content.rs:74-76`. | Move `Decoder::Audio` to `Placement::Worker` (one-line change in `route.rs:61-62`; the worker already has a 512 MiB / 20 s job). Alternatively, pre-scan ID3 and reject tags that contain `CHAP`/`CTOC` before calling lofty. |
| F2 | High | Security | `crates/tw-core/src/decode/raster.rs:105-111` | `image_limits().max_alloc` (512 MiB) has no effect on this path. `into_decoder()` + `DynamicImage::from_decoder()` skips the `limits.reserve(total_bytes())` that only `ImageReader::decode()` performs (vendored `image-0.25.10/src/io/image_reader_type.rs:312-322`). Formats with no `header_dimensions` arm, such as farbfeld, allocate up to `MAX_PIXEL_AREA` x bytes-per-pixel before reading any pixels. | **Reproduced.** A 16-byte file `farbfeld` + 20000 + 20000 (no payload), run through `run_inproc(Decoder::Raster, ..)`: `memory allocation of 3200000000 bytes failed`, process aborted. The 512 MP ceiling allows 20000x20000, and RGBA16 makes that 8 B/px. | Before `from_decoder`, require `decoder.total_bytes() <= limits::WORKER_MEMORY_BYTES` (or a smaller in-proc budget); otherwise return `TooLarge`. Apply the same fix to the identical pattern in `raw.rs:147-155` [INFERENCE: same code shape, not run]. |
| F3 | High | Security | `crates/tw-core/src/decode/sgi.rs:58-69` | SGI allocates the whole output (`w*h*channels`) before checking that any pixel data or RLE table exists. | **Reproduced.** A 512-byte SGI header (dim 3, 20000x20000x4, verbatim, no body): `memory allocation of 1600000000 bytes failed`, process aborted. Compare `pcx.rs` and `psd.rs`, which check that the body exists before allocating. | Move the body-length check (verbatim) and the table-presence check (RLE) ahead of the allocation, and cap the output bytes against an explicit in-proc byte budget. |
| F4 | High | Security | `crates/tw-core/src/decode/audio.rs:34` | lofty's default `id3v2_compression_support` inflates a compressed `APIC` frame into an unbounded `Vec`. lofty's 16 MiB allocation limit does not cover this path. Cover-art size is never checked before full inflation. | **Reproduced (bounded).** A 65 KB MP3 whose zlib `APIC` inflates to 64 MiB produced a 134,217,728-byte allocation request in `run_inproc(Decoder::Audio, ..)` (measured with a recording allocator), 8x lofty's stated cap. The ratio is roughly 1000:1 at this size and grows with payload size [INFERENCE: a few MB of input reaches multi-GB, as in F2]. | F1's fix (audio in the worker) covers this too. Otherwise set `default-features = false` on lofty, without `id3v2_compression_support`. |
| F5 | Medium | Security | `crates/tw-core/src/decode/zip_preview.rs:230-237` | When `read_to_end` fails, `?` returns before `self.used` is updated, so the bytes are never counted. CRC errors are reported at EOF, after full decompression (vendored `zip-9.0.4/src/crc32.rs:71-79`). `first_decodable` (`:267-271`) and the contact sheet keep going after `Malformed`. Up to `MAX_DECODE_ATTEMPTS` x 256 MiB = 4 GiB can be inflated against the 1 GiB total cap. | Source evidence: the `?` at line 234 runs before the accounting at line 236. [INFERENCE: not executed.] Peak memory per entry stays bounded by `take(entry_max+1)`; the harm is CPU and memory churn up to the 15 s in-proc budget, after which the thread keeps running pinned. | Count bytes with a counting reader, charge `used` before propagating the error, and cap the reader at `min(entry_max, total_max - used) + 1`. |
| F6 | Medium | Security | `crates/twctl/src/ops.rs:107-121` | `write_atomic` writes the predictable staging name `<dest>.<pid>-<seq>.twtmp` with `std::fs::write`, which truncates and follows an existing hard link or file symlink. Anyone who can write to the destination folder can pre-create the link and redirect the write into an arbitrary file the victim can write. This covers `strip` in place, `wallpaper` and all converts. | **Reproduced.** A disposable harness hard-linked `output.png.<pid>-0.twtmp` to `sentinel.txt` and called `ops::write_atomic(output.png, b"REPLACED")`. Result `Ok(())`; sentinel afterwards: `REPLACED`. | Create the temp file with `OpenOptions::new().write(true).create_new(true)`, retry on `AlreadyExists` with the next sequence number, and write through that handle. |
| F7 | Medium | Correctness | `crates/twctl/src/ops.rs:124-138`, `crates/twctl/src/main.rs:277-282` | `unique_output` checks `exists()` but does not reserve the name. `batch` runs in parallel, and `rename` replaces the destination. Two inputs that share a stem silently overwrite each other's output, and both are reported as successful. This breaks spec 6.4 ("collisions get ` (1)`"). | **Reproduced.** `twctl convert-to png same.ppm same.pnm` (512x512 red and blue): exit 0, stdout listed `...\same.png` twice, and only one PNG existed. | Reserve the name with `create_new` (the same primitive as F6) inside the selection loop, or publish with a non-replacing rename (`MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`) and retry the next number on collision. |
| F8 | Medium | Security | `crates/tw-shell/src/menu.rs:518-556` (vs `:290-307`) | `start_preview_fetch` overwrites `self.preview`. `Preview` has no `Drop`, so the previous `JoinHandle` is detached without being joined or pinning the module. That breaks I4. A previously received `HBITMAP`, and any handle still queued in the channel, also leaks. | Source: only `ContextMenu::drop` joins or pins, and it only sees the current slot. Breaking I4 requires `QueryContextMenu` twice on one object while the first `GetImage` is still running, then release and `DllCanUnloadNow`. [INFERENCE: Explorer does re-query some menu objects; not observed here.] | Implement `Drop for Preview` (join if finished, else `pin_this_module`; drain `rx` and `DeleteObject` every received handle; delete `bitmap`), or refuse to start a second fetch when one exists. |
| F9 | Medium | Correctness | `crates/tw-core/src/registration.rs:198-223`, `:236-288` | (a) `register` writes the journal only after every mutation (`:221`). If the process is killed mid-loop, the changes are left with no record, including overwritten foreign handler values. (b) `unregister` deletes `Manifest`/`DllPath` (`:241-250`) *before* replaying them, turns every replay failure into `false`, and returns `Ok`. A corrupt manifest (wrong type) becomes an empty list via `unwrap_or_default()` (`:236-238`). Either way `register --off` reports OFF while keys remain, and the information needed to retry is gone. | Source evidence. The handoff asks about "interrupted registration". Error mid-loop is handled correctly (the closure result is journaled before `result?`); a crash or kill is not. [INFERENCE: not run against the registry.] | Append each journal line before its mutation (rewrite the `MULTI_SZ` per step, or write per-entry values). In `unregister`, validate the whole manifest first, replay it, collect failures, and delete the manifest only after a clean replay; otherwise return an error and keep the remaining lines. |
| F10 | Low | Correctness | `crates/tw-core/src/registration.rs:273-275` | `is_empty` followed by `remove_tree` (recursive `RegDeleteTreeW`) is check-then-act. A subkey another program creates in between (e.g. under a newly created `SystemFileAssociations`) is deleted recursively. | Vendored `windows-registry-0.6.1/src/key.rs:45-48`: `remove_tree` = `RegDeleteTreeW`. [INFERENCE: narrow race.] | Use non-recursive `RegDeleteKeyW` (`Key::remove_key`-style). It fails on a non-empty key, which is the desired outcome. |
| F11 | Low | Correctness | `crates/tw-shell/src/menu.rs:682-698` | The ordinal-vs-string decision reads only `lpVerb`. A Unicode caller (`CMINVOKECOMMANDINFOEX` + `CMIC_MASK_UNICODE`) that passes a string in `lpVerbW` and `NULL` in `lpVerb` is treated as offset 0: the preview slot, so `E_INVALIDARG`, or with the preview off, the first verb, "Convert to PNG". | Source: `verb_ptr >> 16 == 0` is tested before the Unicode branch. [INFERENCE: Explorer usually fills both fields; programmatic callers may not.] | When `cbSize >= sizeof(EX)` and `CMIC_MASK_UNICODE` is set, classify `lpVerbW` (IS_INTRESOURCE) first; otherwise use `lpVerb`. |
| F12 | Medium | Correctness | `crates/twctl/src/win.rs:62-76` | `OpenClipboard(None)` then `EmptyClipboard()`. The Win32 docs for `OpenClipboard` say that with a NULL `hWndNewOwner`, `EmptyClipboard` sets the owner to NULL, "which causes SetClipboardData to fail". "Copy to clipboard" and "Copy as data URI" could therefore wipe the user's clipboard and then fail. | Spec clause and MS docs (OpenClipboard remarks). **Not executed**, deliberately, to avoid destroying this machine's clipboard. [INFERENCE: the handoff's live run did not cover these verbs.] | Create a message-only window (`HWND_MESSAGE`) and pass it to `OpenClipboard`; keep it alive until `CloseClipboard`. |
| F13 | Low | Security/QOL | `crates/twctl/src/main.rs:84-93` | `--files-from` deletes *whatever path it is given*, even when the command is invalid. A user running `twctl strip --files-from mylist.txt` loses the list. Because Explorer's list is written with a predictable name in `%TEMP%` through `std::fs::write` (`menu.rs:775-784`), F6's link-following issue applies there too. | **Reproduced.** `twctl not-a-command --files-from ordinary-list.txt`: exit 1, `unknown command`, file deleted. | Delete only files matching the shell's own `tw-list-*.txt` pattern in `%TEMP%`, after a successful parse, or use a private flag for shell-generated lists. Create the list with `create_new`. |
| F14 | Low (**fixed**) | Correctness | `crates/twctl/src/ops.rs:207` | `parse_size` multiplied without an overflow check. `compress --max-size 18446744073709551615MB` panicked (exit 101) instead of exiting 1 with a message (spec 7). | **Reproduced before the fix:** `panicked at crates\twctl\src\ops.rs:207:18: attempt to multiply with overflow`, exit 101. After: `twctl: bad size 18446744073709551615MB`, exit 1. | Applied: `checked_mul`. Regression test added (section 3). |
| F15 | Info | QOL | `crates/twctl/src/main.rs:24-41, 63-83` | No `--help`/`-h`, global or per command, and no `--` end-of-options marker. A file whose relative name starts with `--` cannot be passed. Shell launches use absolute paths, so the menu is not affected. | **Observed:** `twctl --help` -> `unknown command --help` (exit 1); `twctl convert --help` -> `unknown option --help`; `twctl strip -- --x.png` -> `unknown option --`. | Print `USAGE` with exit 0 for `--help`/`-h`/`help`, add per-command synopsis lines, and treat `--` as the end of options. |
| F16 | Info | Quality | `crates/tw-shell/src/thumbnail.rs:176, 203-211`; `menu.rs:429` | (a) `Settings::load` (an INI read or HKCU enumeration) runs on every `GetThumbnail` and every `QueryContextMenu`. (b) The negative cache stores `Timeout` as well as decode errors, so a timeout caused by system load is cached for the life of the surrogate. `worker.rs:98-100` already records that this happened once. | Source. | (a) Cache settings with a short TTL or a change notification. (b) Do not cache `Timeout`, or cache it with an expiry. Keep caching `Malformed`/`Unsupported`/`TooLarge`. |

Checked and rejected (not findings):

- **Worker before job:** the worker blocks on `read_request` (`tw-worker.rs:33-36`) before decoding. If the assignment fails, the worker is killed and the call fails cleanly (`worker.rs:107-112`).
- **Partial output then hang:** caught by the `recv_timeout` -> `TerminateJobObject` -> joins (`worker.rs:128-139`).
- **Response validation:** dimensions are checked against `edge` before allocating (`protocol.rs:133-136`).
- **DIB copy:** the producers make `bgra.len()` exactly `w*h*4`.
- **`GetCommandString` widths:** `menu.rs:721-736` is correct.
- **`HDROP`:** paths are copied before `ReleaseStgMedium`.
- **Clipboard `HGLOBAL`:** freed only on failure.
- **SVG:** `from_data_nested` nulls the string resolver (no external or UNC fetch).
- **EPUB:** paths never reach the filesystem, and there is no DTD/entity expansion.
- **RAW:** IFD traversal is iterative with a visited set.
- **HKCU manifest -> HKLM:** no cross-hive path; each scope reads its own root.

## 3. Fixes applied

| File | Change |
|---|---|
| `crates/twctl/src/ops.rs:207` | `.map(\|n\| n * mul)` changed to `.and_then(\|n\| n.checked_mul(mul))` (F14). |
| `crates/twctl/src/ops.rs:280-287` | New test `size_suffix_overflow_is_rejected`: the largest representable `MB` value passes, one more fails, `u64::MAX` with `KB` fails, and plain `u64::MAX` passes. Before the fix it failed with `attempt to multiply with overflow` at `ops.rs:207:18`; after the fix it passes. |
| `crates/tw-shell/tests/isolation.rs:59` | The missing-worker test used the absolute path `C:\nonexistent\tw-worker.exe`, outside the clean-room tree. It now uses `worker().join("nonexistent-worker.exe")`. The behaviour under test is unchanged. |

Throwaway probes (`crates/twctl/examples/review_atomic.rs`, `crates/tw-core/examples/review_probe.rs`) were deleted after use. Fixtures and runtime state are in `target/review-tmp/`, and `TEMP`/`TMP`/`LOCALAPPDATA` were redirected there for all runs.

Verification, run after all changes (exit codes plus the tool's own summary lines):

```
> .toolchain\cargo.cmd test --workspace                                   exit 0
test result: ok. 73 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.34s    (tw-core)
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s     (tw-shell lib)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s     (tw-worker)
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.56s   (com_server)
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.39s     (isolation)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 38.84s   (twctl)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s     (xtask)
(106 tests = 105 existing + 1 new)

> .toolchain\cargo.cmd clippy --workspace --all-targets -- -D warnings    exit 0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.98s

> .toolchain\cargo.cmd fmt --all -- --check                               exit 0
(no output)

> .toolchain\cargo.cmd xtask licenses --check                             exit 0
135 crates, all permissive; THIRD_PARTY_LICENSES.md up to date
```

## 4. Proposed tests

Each test below must fail on the current code (except where noted as regression guards) and pass after the matching fix.

| Test | Catches |
|---|---|
| `audio_deep_chap_nesting_is_rejected_without_overflow`: 200,000 nested `CHAP` through `Engine::thumbnail(..., Some("mp3"))` returns `Err` and the test process survives. | F1: a stack-overflow abort of the surrogate. |
| `farbfeld_header_bomb_is_too_large`: 16-byte `farbfeld` 20000x20000 -> `Err(TooLarge)` with no allocation over budget (assert with a small allocator in a `#[test]` subprocess, or just the error value after the fix). | F2: `max_alloc` being ignored on the `from_decoder` path; the same test with a `.raw`-embedded preview covers `raw.rs`. |
| `sgi_truncated_header_does_not_allocate`: 512-byte SGI 20000x20000x4 -> `Err(Malformed)`. | F3: allocating before validating the body. |
| `compressed_apic_bomb_is_bounded`: a 65 KB MP3 inflating to 64 MiB -> `Err` with a peak allocation under 16 MiB. | F4: unbounded inflation by lofty. |
| `zip_failed_reads_count_toward_total`: `extract_with` budget 1024/1536 and three CRC-corrupted 1024-byte entries -> the second read returns `TooLarge`. | F5: a total cap that skips error paths. |
| `write_atomic_refuses_preexisting_staging_link`: hard-link the staging name to a sentinel; the sentinel must be unchanged. | F6: link-following staging writes. |
| `convert_to_same_stem_yields_distinct_outputs`: `same.ppm` + `same.pnm` -> `same.png` and `same (1).png`, with different pixels. | F7: the name-reservation race that loses output. |
| `menu_requery_retires_previous_preview`: `QueryContextMenu` twice on one object (registration-free loader) -> the first thread is joined or pinned; GDI object count is stable across 50 repetitions. | F8: an I4 violation and HBITMAP leak. |
| `unregister_keeps_manifest_when_restore_fails` / `corrupt_manifest_is_an_error` (sandbox layout; deny write on one restored key via ACL, or store a DWORD `Manifest`). | F9: losing the undo journal, and a false "OFF". |
| `invoke_unicode_string_verb_with_null_ansi`: `CMINVOKECOMMANDINFOEX` with `lpVerb=NULL`, `lpVerbW=L"tw.convert.png"` -> `twctl` is spawned with `convert-to png` (assert through a stub `twctl.exe` in the staging dir that records its argv). | F11: Unicode string verbs treated as ordinal 0. |
| `files_from_is_not_deleted_on_usage_error`. | F13: deleting a user's file. |
| `timeouts_are_not_negatively_cached` (inject an `Engine` whose `max_tier_time` is about 1 ms, then retry with a normal budget for the same key). | F16b: load-caused timeouts disabling thumbnails until the surrogate restarts. |

## 5. Spec conformance (C1-C11)

| # | Status | Notes |
|---|---|---|
| C1 | **Pass** | `#[implement(IThumbnailProvider, IInitializeWithStream)]`; QI test rejects `IInitializeWithFile`/`Item`, `IExtractImage`, `IShellExtInit` (`com_server.rs:338-360`). |
| C2 | **Pass** (per handoff live run) | Registration writes no `DisableProcessIsolation` (`registration.rs:175-185`); the handoff observed the DLL hosted in `DllHost /Processid:{AB8902B4-...}`. |
| C3 | **Partial** | The worker is per-request, takes input over a pipe, and has Job Object limits of 512 MiB, 20 s CPU, 1 process and kill-on-close (`worker.rs:35-59`). It is a product worker, not a long-tail external decoder, so the policy-file clauses of REQ-DEC-2 / 11.2 have no counterpart. Audio (F1/F4) should also be in the worker. |
| C4 | **Fail** | Every tier is bounded in wall time, but F1-F3 terminate the host on small inputs, which contradicts REQ-COM-7 ("MUST NOT crash"). |
| C5 | **Pass** | Both key locations written (`registration.rs:204-209`); ProgID untouched; sandbox test `registers_both_handler_locations_without_touching_progid`. |
| C6 | **Partial** | The successful path restores the pre-registration state, and the handoff's live diff showed zero residue. Interruption, failed restore and corrupt manifest do not (F9, F10). |
| C7 | **Pass** | `WTSAT_ARGB` + top-down 32-bpp premultiplied BGRA DIB, verified through GDI (`com_server.rs:206-220`). |
| C8 | **Partial** | The tree order matches 6.2 for the implemented verbs (missing verbs are a known gap). Item visibility uses the product's own keys. Unicode string invocation is defective (F11). |
| C9 | **Partial** | Exit 0/1 and the `twctl: <message>` stderr format hold, and arity/option checks are present. Defects: F7 (silent output overwrite), F13, F14 (fixed), F15 (no help). `doctor`/`batch`/`--mcp` are known gaps. |
| C10 | **Partial** | A shell-relevant subset of section 8 under the product's own key names (as 13.1.5 requires), with spec defaults (`config.rs:35-49`, test `defaults_follow_spec`). Most GUI-only settings are absent (no GUI app in this milestone). |
| C11 | **Pass** for GUIDs | All seven product GUIDs are fresh v4 and distinct (`ids.rs:81-97`). There is no modern-menu package or signing identity yet, so REQ-REG-9 cannot be assessed. |

The larger changes above were later applied on request (section 6).

## 6. Resolution (2026-10-17)

An independent advisor session ranked the open findings. Its main corrections: F2 also reaches containers through `decode_bytes`; the SGI RLE path needs a byte cap, not a reorder; PCX and PSD composites also needed the cap; F8 fires in the default menu configuration; never caching `Timeout` would multiply runaway threads. Everything ranked "fix now" was applied. F15 (CLI help) and F16a (settings caching) were deferred as Info with no measured cost.

| ID | Status | Change | Regression test |
|---|---|---|---|
| F1, F4 | Fixed | `Decoder::Audio` runs in the worker (`route.rs`), wall budget 15 s, Job memory limit 512 MiB + input size (`worker::memory_limit`). | `isolation.rs`: `deeply_nested_audio_chapters_fail_without_killing_the_host`, `audio_cover_art_is_decoded_through_the_worker`; `worker.rs` memory-limit unit test; route placement test. |
| F2 | Fixed | `decode::decode_limited` reserves `decoder.total_bytes()` against `limits::DECODE_ALLOC_BYTES` before `from_decoder`; used by `raster::decode_as` (so all containers) and `raw.rs`. | `raster::farbfeld_header_bomb_is_too_large`, `raw::embedded_jpeg_with_huge_sof_is_too_large`, `zip_preview::farbfeld_header_bomb_entry_is_too_large`. |
| F3 (+PCX, PSD) | Fixed | `decode::check_alloc` before output allocation in `sgi.rs`, `pcx.rs`, `psd.rs`. | `sgi::verbatim_header_bomb_is_too_large`, `sgi::rle_rows_sharing_one_offset_are_too_large`, `pcx::output_over_byte_budget_is_too_large`, `psd::rle_composite_over_byte_budget_is_too_large`. |
| F5 | Fixed | Partial reads charged before the error propagates; reader capped at the remaining total. | `zip_preview::zip_failed_reads_count_toward_total`. |
| F6, F7 | Fixed | `ops::stage` creates the temp file with `create_new` (retries on collision); `write_new` claims ` (n)` names with a non-replacing `MoveFileExW`; `unique_output` removed. | `staging_never_writes_through_existing_link`, `concurrent_write_new_yields_distinct_files`, `convert_to_same_stem_keeps_both_outputs`. |
| F8 | Fixed | `impl Drop for Preview`; the fetch thread and the menu object hand the bitmap over through a mutex-guarded slot (`abandoned` flag + condvar), so whichever side runs second frees it on every interleaving. Slot replacement now cleans up. | `com_server.rs::undrawn_menu_previews_do_not_leak_gdi_bitmaps`; mutation-checked: disabling the cleanup makes it fail. |
| F9 | Fixed | Write-ahead journal: one REG_SZ per step, written before the change. `unregister` validates first, replays newest first, keeps failed entries, removes metadata only after a clean replay. Old format dropped (unreleased). | `crash_after_any_journal_entry_is_fully_undone` (crash injected after every N), `corrupt_journal_entry_is_an_error_and_changes_nothing`, `unregister_is_idempotent`. |
| F10 | Fixed | `RegDeleteKeyW` (non-recursive) after the empty check. | `foreign_subkey_under_created_key_survives_unregister` (outcome guard; the race itself is not deterministic). |
| F11 | Fixed | `resolve_verb` follows the documented order (Unicode string, ANSI string, ordinal). | `unicode_verb_wins_over_null_lpverb`, `ordinal_and_ansi_verbs`. |
| F12 | **Not reproduced; downgraded to Info** | Clipboard opened with a message-only window owner (kept: matches the documented contract at no cost). Checked in a private window station (`CreateWindowStationW` + `CreateDesktopW`, whose clipboard is separate from the user's): the old `OpenClipboard(NULL)` sequence **succeeded** on this build (text read back), so the documented failure does not occur here; the new code also round-trips. The probe was throwaway and its temporary Cargo features were reverted. | None permanent (probe needs extra `windows` features). |
| F13 | Fixed | `--files-from` list deleted only when it is a `tw-list-*.txt` in `%TEMP%`; the menu creates it with `create_new`. | `files_from_user_list_is_kept`, `files_from_shell_list_is_consumed`. |
| F16b | Fixed | Cached `Timeout` failures expire after 5 minutes; other failures stay cached. | `timeouts_are_not_negatively_cached_forever`. |
| F15 | Fixed | `twctl --help`/`-h`/`help` prints usage (exit 0); `<command> --help` prints that command's synopsis; `--` ends options. | `help_succeeds_without_running_the_command`, `double_dash_ends_options`. |
| F16a | Deferred | Info; no measured cost of `Settings::load` per call. | - |

Also aligned: the JPEG XL allocation tracker uses the shared `limits::DECODE_ALLOC_BYTES`. Em dashes were removed from the project's own files (two in `REVIEW_HANDOFF.md`); vendored sources under the gitignored `.toolchain/` and build output under `target/` were not touched.

`twctl` audio isolation (follow-up to F1): `ops::decode_full` sends audio to the worker too (cover art up to 4096 px), so a hostile nested-chapter MP3 given to a menu verb now yields the `--notify` error box instead of a silent `twctl` crash. Smoke: `twctl convert deep.mp3 out.png` exits 1 with `malformed input: worker failed`; a real cover-art MP3 converts to a 4x2 PNG. Test: `worker_pixels_are_unpremultiplied_to_rgba` (the conversion shared with `twctl thumbnail`, previously untested).

Test robustness: `hostile_inputs_fail_cleanly_with_e_fail` failed once with `noise.png took 22.09s`. Cause: antivirus scanning the freshly staged `tw-worker.exe` on its first launch (a fresh `twctl thumbnail` on the same file took 13.5 s, then 1.2 s). The test now launches the staged worker once before timing, following the handoff's warm-up convention; the 20 s budget is unchanged.

Remaining known limit: a crash between creating the journal key and writing its first entry can leave an empty `<product>\Registration` key.

End-to-end smoke (`twctl thumbnail`, which uses the same `Engine` as the DLL, with the original fixtures):

```
bomb.ff    exit 1  input exceeds resource budget          (was: 3.2 GB allocation, abort)
bomb.sgi   exit 1  input exceeds resource budget          (was: 1.6 GB allocation, abort)
zbomb.mp3  exit 1  unsupported input                      (now inflates in the worker)
deep.mp3   exit 1  malformed input: worker failed: ...    (was: stack overflow in host; now only the worker dies)
convert-to png same.ppm same.pnm -> same.png, same (1).png (was: one file, overwritten)
clip a b --files-from list.txt  -> usage error, list kept (was: list deleted)
```

Verification after all changes:

```
> .toolchain\cargo.cmd test --workspace                                   exit 0
test result: ok. 86 passed; 0 failed ...   (tw-core)
test result: ok. 5 passed; 0 failed ...    (tw-shell lib)
test result: ok. 0 passed; 0 failed ...    (tw-worker)
test result: ok. 13 passed; 0 failed ...   (com_server)
test result: ok. 6 passed; 0 failed ...    (isolation)
test result: ok. 22 passed; 0 failed ...   (twctl)
test result: ok. 1 passed; 0 failed ...    (xtask)
(133 tests)

> .toolchain\cargo.cmd clippy --workspace --all-targets -- -D warnings    exit 0
> .toolchain\cargo.cmd fmt --all -- --check                               exit 0
> .toolchain\cargo.cmd xtask licenses --check                             exit 0
135 crates, all permissive; THIRD_PARTY_LICENSES.md up to date
```
