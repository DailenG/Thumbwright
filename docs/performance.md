# Performance: measurements and decisions (issue #13)

Issue #13 listed four suspected costs. Each was measured before anything was changed; this
file records the numbers, the decision for each, and how to reproduce them.

## Decisions

| Suspect | Measured | Decision |
|---|---|---|
| `Settings::load` on every `GetThumbnail` | 32 us (HKCU) to 54 us (INI) median; 0.1 % to 1.3 % of a typical in-proc thumbnail, 3.9 % of a 256 px icon | **No cache.** Not worth the stale-setting risk. |
| One worker process per SVG / WIC / audio request | 20 ms process floor; SVG 24 ms vs 0.6 ms in-proc, MP3 cover 27 ms vs 0.9 ms | **Pre-spawned single-use workers** in the shell (still one request per process). SVG 24 -> 14 ms back to back, 6.5 ms when spaced; MP3 27 -> 16 / 8 ms. |
| Whole stream read through a 1 MiB bounce buffer | 0.28 to 0.31 ms per call on files up to 250 KB (7 % of a 640x480 JPEG call, 23 % of an icon), 13 to 22x the cost of the read itself | **Read straight into the result `Vec`.** 13 to 21 us. |
| `DynamicImage::thumbnail` for 24 MP -> 256 px | 29 to 34 ms (opaque 8-bit RGB) | **`fast_image_resize` Box filter for opaque 8-bit RGB and grey**: 5 ms, same quality (PSNR within 0.3 dB of before, against a Lanczos-3 reference). 8-bit alpha images are premultiplied first, then Box-shrunk (issue #17). 16-bit images keep `DynamicImage::thumbnail`; float images are converted to 8-bit first. |
| Peak memory of reading the whole stream | `file size + 1 MiB` (512 MiB file: 520.8 MiB peak working set) | **Unchanged**; the bounce chunk was the only overhead and is gone. |

Stage totals for one in-process `GetThumbnail` (settings + read + decode + resize, medians,
`perf calls`), before and after the read and resize changes:

| Input | Before | After | Change |
|---|---|---|---|
| JPEG 640x480 | 5361 us | 2833 us | -47 % |
| JPEG 1024x768 | 14981 us | 8925 us | -40 % |
| JPEG 12 MP | 114473 us | 94194 us | -18 % |
| PNG 256x256 (no resize needed) | 1078 us | 795 us | -26 % (all of it the read) |

## Method

- Machine: Windows 10.0.26340 x64, 22 logical processors, Defender real-time protection on
  (it scans each fresh `tw-worker.exe` launch, which is part of the spawn cost measured
  here). Release profile from the workspace `Cargo.toml` (thin LTO, `codegen-units = 1`).
- Harness: `crates/tw-core/examples/perf.rs`, a dev-only example (not shipped).
  Subcommands: `settings`, `calls`, `anatomy`, `worker`, `corpus`, `resize`, `memory`,
  `gen`. It drives the same code the DLL and `twctl` use: `Settings::load`,
  `worker::run`, `Engine::thumbnail`, `to_thumbnail`.
- "Before" is the code at the start of this work. The before/after pairs below are
  measured in the same process, interleaved, so background load hits both alike: the pool
  is switched with `worker::set_prespawn(false/true)` (off is exactly the previous
  one-process-per-request path), and the old resize is called directly as
  `DynamicImage::thumbnail`.
- The first measurement rounds ran while other builds saturated the machine (spawn floor
  42 to 63 ms, wide p95s). The numbers below come from a quiet machine; both median and
  min or p95 are shown where the tail matters.
- Corpus: 194 generated files (`perf gen <dir>`): 59 JPEG (56 from 640x480 to 4000x3000,
  3 at 24 MP), 40 PNG with alpha, 12 BMP, 10 GIF, 8 WebP, 6 TGA, 5 QOI, 5 TIFF, 15 SVG,
  15 MP3 with a cover, 8 KRA, 6 "HEIC" (JPEG data under a `.heic` name, so it takes the real
  WIC worker path; no HEVC codec is needed), 5 corrupt JPEGs (fall through to the WIC
  worker). Files are processed in a fixed pseudo-random order, as a mixed folder would be.

## 1. `Settings::load`

`perf settings 10000` (microseconds per call, 10 000 calls each):

| Mode | median | p95 | min |
|---|---|---|---|
| INI (portable, 9 keys) | 54.2 | 79.3 | 45.5 |
| of which `from_ini` parsing | 1.7 | 2.1 | 1.5 |
| HKCU (installed, 9 values) | 32.4 | 51.2 | 30.4 |
| neither INI nor key (defaults) | 18.9 | 27.4 | 16.9 |
| cache validation: `fs::metadata` on the INI | 28.1 | 49.2 | 26.1 |
| cache validation: key last-write time, key handle reopened | 6.8 | 11.4 | 6.4 |
| cache validation: key last-write time, handle held | 1.7 | 2.5 | 1.5 |

Against a whole in-process `GetThumbnail` (`perf calls`, INI mode, 49.9 us):

| Input | thumbnail call | `Settings::load` share |
|---|---|---|
| JPEG 12 MP | 90.9 ms | 0.1 % |
| JPEG 1024x768 | 7.7 ms | 0.6 % |
| JPEG 640x480 | 3.7 ms | 1.3 % |
| PNG 256x256 | 1.3 ms | 3.9 % |

**Decision: do not cache.** The load is under 2 % of any realistic thumbnail and 3.9 % of the
cheapest one (0.05 ms, in absolute terms). A cache would need invalidation on a change
(INI mtime plus a TTL, or a held registry handle with a last-write-time check), and the
gated values (`ThumbnailsEnabled`, `MaxFileMB`, `DisabledTypes`) are security-relevant
limits that should apply to the very next request. The cheapest sound validation measured
(1.7 us for HKCU) would save 30 us per call; for INI the validation (28 us) costs half of
the load it saves. Revisit only if a much cheaper thumbnail path appears. The same
numbers show what to fix instead: the stream read (section 4), which cost 5 to 6 times the
settings load.

## 2. Worker process per request

`perf anatomy` (30 interleaved rounds; milliseconds; "warm" is a worker spawned 400 ms
earlier, i.e. the best a pre-spawned worker can do):

| Request | `Command::spawn` returns | cold: spawn + request | warm: request only |
|---|---|---|---|
| SVG 1 KB | 3.0 (min 2.1) | 15.9 (min 13.6) | 1.5 (min 1.1) |
| MP3 + 300x300 cover | 2.9 | 17.3 | 3.6 |
| WIC, 1024x768 JPEG | 3.2 | 22.4 | 8.0 |

So the time is not `CreateProcess` returning (3 ms): the remaining 12 to 14 ms of a cold
request is the new process starting up and reaching its read of stdin (Defender's launch
scan is part of that [INFERENCE]; it was not measured separately). Pre-spawning moves all
of it off the request path.

`perf worker 40` (milliseconds, 40 calls each; "one-shot" is the previous behaviour):

| Request | in-proc decode | one-shot worker | pre-spawned, back to back | pre-spawned, 300 ms apart |
|---|---|---|---|---|
| `tw-worker --version` (process floor) | - | 20.0 | - | - |
| empty request | 0.00 | 22.2 | 12.0 | 4.8 |
| SVG 1 KB | 0.63 | 23.9 | 13.8 | 6.5 |
| SVG 60 KB | 21.0 | 52.9 | 32.8 | 16.6 |
| WIC JPEG 1024x768 | 3.6 | 37.8 | 18.5 | 12.9 |
| WIC JPEG 12 MP | 47.3 | 77.3 | 64.9 | 51.0 |
| MP3 + cover | 0.91 | 26.8 | 16.5 | 8.2 |

Process overhead is 22 to 34 ms per request whatever the decode costs, so it dominates
small SVG, audio and WIC requests (97 % of an SVG 1 KB call). Back to back, a pre-spawned
worker is only partly ready (the replacement starts while the previous request decodes);
when requests are spaced, as Explorer's are, it is fully warm.

**Decision: pre-spawn, single-use.** Isolation per request is kept:

- A pre-spawned worker is created inside its Job Object, blocked reading stdin, and has
  read no input. A request takes it, uses it once, and kills it at the end of the request
  exactly as before. It is never returned to the pool, so no worker ever sees two
  requests (`tests/prespawn.rs`).
- Memory ceiling, CPU time, one-process and kill-on-close limits are applied before the
  worker is idle; the per-request memory ceiling (audio grows with its input) is re-applied
  when the worker is taken (`worker::tests::retargeting_a_job_changes_only_the_memory_ceiling`).
- The wall-clock budget still applies to a pre-spawned worker and starts once the worker
  holds the request.
- At most `IDLE_MAX = 2` workers wait, counted so racing refills never start workers the cap
  then throws away (`prespawning_costs_one_extra_process_not_one_per_request`); an idle
  worker older than 30 s or one that died while idle is discarded, never used.
- Only the shell turns it on (`ThumbnailProvider::new`). `twctl` runs one request per
  process, so it keeps the one-shot path and leaves no process behind.
- `DllCanUnloadNow` drains the idle workers before it can return `S_OK`, and a surrogate
  that exits closes the Job Objects (kill-on-close, by design, not separately tested).
  `tests/prespawn_dll.rs` drives the unload check through the real DLL.

Cost: up to two idle `tw-worker.exe` processes (blocked on stdin, no CPU) per surrogate
while thumbnails are being requested, and the installed `tw-worker.exe` stays mapped by
them until they are drained or the surrogate exits (the DLL is already held the same way).

## 3. End-to-end: the 194-file corpus

`perf corpus <dir> <tw-worker.exe> <threads> 5` reads each file and calls
`Engine::thumbnail(.., 256)`, as `twctl thumbnail` does. Wall time for the whole corpus,
interleaved one-shot / pre-spawned, first pair (cold) discarded:

| Threads | one-shot min / median | pre-spawned min / median | Change |
|---|---|---|---|
| 1 | 5352 / 5662 ms | 4678 / 4880 ms | -12.6 % / -13.8 % |
| 4 | 2249 / 2436 ms | 2077 / 2253 ms | -7.6 % / -7.5 % |

Per kind, sum over the files in the last single-thread run (ms; the in-process kinds are
unchanged, as intended):

| Kind (count) | one-shot | pre-spawned |
|---|---|---|
| MP3 (15) | 420.6 | 162.6 |
| SVG (15) | 402.0 | 162.8 |
| "HEIC" via WIC (6) | 255.0 | 177.0 |
| corrupt JPEG, falls through to WIC (5) | 138.5 | 74.8 |
| all worker-first kinds | 1216 | 577 (-52 %) |
| in-process kinds (JPEG, PNG, BMP, GIF, ...) | 4262 | 4217 |

Both columns include the new resize and read paths; their individual effect is in the
stage totals at the top. The real benefit of pre-spawning is in the worker kinds: a folder
of SVGs, MP3s or HEICs takes half the time, while a folder of JPEGs does not change.

## 4. Resize: 24 MP to 256 px

`perf resize <photo.jpg>`. Candidates run on four 6000x4000 inputs: a real photo (the
Windows wallpaper `img0.jpg`, 3840x2400, tiled), a synthetic photo, a zone plate (an
aliasing probe), and an RGBA image with transparent regions. Time is min of 9 runs (ms).
Quality is PSNR in dB of the premultiplied output against a Lanczos-3 reference computed
at full precision (`image::imageops::resize` on premultiplied pixels). Higher is closer
to the reference; it is a proxy for fidelity, not a taste test.

Opaque RGB8:

| Candidate | real photo | synthetic photo | zone plate |
|---|---|---|---|
| `DynamicImage::thumbnail` (before) | 32.7 ms, 42.30 dB | 28.9 ms, 48.32 dB | 33.7 ms, 35.23 dB |
| `to_thumbnail` (shipped: `fast_image_resize` Box) | **5.3 ms, 42.56 dB** | **4.9 ms, 48.54 dB** | **5.4 ms, 35.26 dB** |
| `fast_image_resize` Bilinear | 8.1 ms, 39.56 dB | 8.3 ms, 46.64 dB | 8.5 ms, 40.31 dB |
| `fast_image_resize` CatmullRom | 22.5 ms, 47.59 dB | 20.1 ms, 53.83 dB | 27.1 ms, 46.92 dB |
| `fast_image_resize` Lanczos3 | 43.0 ms, 62.29 dB | 43.1 ms, 62.70 dB | 38.9 ms, 75.66 dB |
| `image::imageops` CatmullRom | 258.5 ms, 47.66 dB | 244.6 ms, 54.02 dB | 247.9 ms, 46.92 dB |

On a real 3840x2400 JPEG the shipped path scores 43.79 dB against 43.80 dB before, and a
side-by-side of the two 256x160 outputs shows no visible difference.

Alpha (RGBA8 with transparent regions, 6000x4000):

| Candidate | time | PSNR |
|---|---|---|
| `DynamicImage::thumbnail` (before the issue #17 fix) | 27.6 ms | 19.35 dB |
| premultiply, then `DynamicImage::thumbnail` | 102.7 ms | 30.10 dB |
| `fast_image_resize` Box | 57.1 ms | 30.69 dB |
| `fast_image_resize` CatmullRom | 77.0 ms | 33.96 dB |

**Decision.** The rule was to switch only for equal or better quality *and* faster.

- Opaque 8-bit RGB and grey (the camera-JPEG case): switch to the `fast_image_resize` Box
  filter. Same quality (within 0.3 dB on every input, in both directions), about 6x
  faster (5 ms vs 29 to 34 ms at 24 MP; 12 MP: 22.5 -> 3.0 ms in the call totals above).
  `fast_image_resize` is MIT OR Apache-2.0, pure Rust (no build script that compiles
  native code, no `*-sys`), without `rayon`; `cargo xtask licenses --check` passes. Its
  SIMD kernels are `unsafe` internally; they run on buffers we allocate, bounded by the
  decoded-size budget, and a resizer error falls back to the previous path.
- Alpha (8-bit RGBA8/LumaA8): **now premultiplied before the Box shrink** (issue #17,
  user decision: fix the fringing and keep Box). Averaging straight (non-premultiplied)
  colour lets the colour hidden under transparent pixels bleed into edges (19.35 dB against
  a reference; 30.7 to 34 dB premultiplied). Cost: one full-size premultiplied BGRA copy
  plus the same Box shrink, measured 44 -> 161 ms at 24 MP RGBA (5 runs, min). 16-bit and
  float images keep `DynamicImage::thumbnail`, so large HDR files do not pay that copy.
- Float images are resized after conversion to 8-bit, because `DynamicImage::thumbnail`
  itself is wrong on them (probe, image 0.25.10): averaging f32 pixels of [1, 0, 0, 1] red
  produces 1.5 in every channel, clamped to grey [128, 128, 128] instead of red. Observed
  with a synthetic probe; real HDR files are rare, small and not representative.
- 16-bit images: **unchanged** (`DynamicImage::thumbnail`). Converting them to 8-bit first
  costs several times the resize itself (measured 61 -> 231 ms at 24 MP RGB16 in the
  alpha-path bench), and 16-bit files are photo sources where the old path is correct.
- CatmullRom would be the next step up and is still faster than the old code on opaque
  images (22.5 vs 32.7 ms), with +5 dB on photos and +11.7 dB on the aliasing probe. It
  was not taken because it changes how thumbnails look (sharper, with overshoot at hard
  edges), which is a product decision rather than a performance one (user decision, #17:
  keep Box).
- CatmullRom was not taken (user decision, #17: keep Box).

## 5. Whole stream in memory

`perf memory 512` (separate process per variant, MiB):

| Variant | peak working set | peak commit |
|---|---|---|
| process start | 7.7 | 1.7 |
| read a 512 MiB stream, 1 MiB bounce buffer (before) | 520.8 | 515.7 |
| read a 512 MiB stream straight into the `Vec` (after) | 519.7 | 514.7 |
| read + decode a 24 MP JPEG | 89.3 | 83.1 |

Reading is one copy of the file plus one MiB. The time is what hurt: the bounce buffer was
allocated and zero-filled (page-faulted) on every call (0.28 to 0.31 ms measured, 13 to 22x
the read of a small file; `perf calls`: 7.4 % of a 640x480 JPEG call and 23 % of a
256 px icon call), while the copy itself is cheap. Reading straight into the result
removed it (13 to 21 us for files up to 250 KB; 12 MP: 2.3 -> 1.5 ms). The 2 GiB input
cap is unchanged. In the surrogate the file is held once; for worker-placed decoders the worker holds a second copy under its
512 MiB Job limit (audio gets extra), as before.

## 6. Cold start through the OS thumbnail cache (issue #21)

The live verification (#3) saw a first PDF thumbnail take 18.8 s and the first PNG, JPEG
and SVG about 3.3 s. To check whether that is antivirus first-launch scanning, and whether
Authenticode signing changes it, a local throwaway harness (not committed, so the numbers below are indicative and not reproducible from this repository)
copied each build to a never-seen directory, registered it per user, killed the thumbnail
surrogate, and timed `IThumbnailCache::GetThumbnail(WTS_FORCEEXTRACTION |
WTS_EXTRACTDONOTCACHE, 256)` twice over the same four files: a 2-page PDF, a PNG, a 12 MP
(3000x4000) camera JPEG and an SVG. Release build, Defender real-time protection on,
three rounds per variant, interleaved.

| First request after a fresh copy | Unsigned (3 rounds) | Signed (3 rounds) |
|---|---|---|
| PDF, pass 1 (DLL load + surrogate start + first worker + Windows.Data.Pdf) | 2246 / 2540 / 2529 ms | 2771 / 2671 / 2929 ms |
| PDF, pass 2 | 2408 / 1165 / 1047 ms | 3451 / 3004 / 3567 ms |
| PNG, pass 1 | 48 / 38 / 73 ms | 89 / 46 / 92 ms |
| 12 MP JPEG, pass 1 | 780 / 606 / 567 ms | 696 / 763 / 723 ms |
| SVG (worker), pass 1 | 27 / 23 / 24 ms | 30 / 21 / 57 ms |

Findings:

- **18.8 s did not reproduce.** The worst first request was 2.9 s (a PDF, including
  starting the surrogate and loading the 14 MB DLL). The live run took its 18.8 s right
  after a 16-minute release build with other builds running [INFERENCE: load and a first
  antivirus scan of never-seen binaries, not product code].
- **Signing does not shorten it.** Signed and unsigned are within noise on PNG, JPEG and
  SVG. Signed PDF pass-2 times are 1-2 s higher in all three rounds; the cause was not
  isolated [INFERENCE: signature checks on each new worker image load].
- **The PDF cost is Windows.Data.Pdf itself**, not process start: pass 2 (surrogate warm,
  worker pre-spawned) still takes 1.0 to 3.6 s. The 30 s PDF budget leaves an 8x margin
  over the worst case measured.
- PNG and SVG first requests are tens of milliseconds; the 12 MP JPEG's ~0.6 to 0.8 s is
  a full-resolution decode (the `image` crate's JPEG decoder has no scaled IDCT).

Decision: no code change for cold start. Possible future work, if PDFs in Explorer feel
slow: keep one pre-spawned worker with `Windows.Data.Pdf` already activated (it must still
serve one request only), or use a scaled JPEG decode for large JPEGs.

## Not acted on

- **Alpha premultiplication** was first listed here as a quality defect and left alone.
  It was fixed under issue #17 (section 4, Decision), at the cost measured there.
- **A pool of reused workers** was not considered: the issue is right that per-request
  isolation is a security property, and the measured gain is available without it.

## Reproduce

```text
cargo build --release --workspace --examples
perf=target\release\examples\perf.exe
worker=target\release\tw-worker.exe
%perf% gen C:\scratch\corpus
%perf% settings 10000
%perf% calls 300
%perf% anatomy %worker% 30
%perf% worker %worker% 40
%perf% corpus C:\scratch\corpus %worker% 1 5
%perf% corpus C:\scratch\corpus %worker% 4 5
%perf% resize C:\Windows\Web\Wallpaper\Windows\img0.jpg
%perf% memory 512
```

Timings depend on antivirus and background load; use a quiet machine and compare the
interleaved pairs, not absolute values across machines. `TW_PERF_OUT=<dir>` makes
`resize` also write each candidate's 256 px output as a PNG.
