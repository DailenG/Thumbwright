# Decoder and pipeline hardening after the pre-public audit

The pre-public audit (slices DA and DB, plus U-2 and U-9 from the shell slice) found two
root causes behind its High and Medium decoder findings:

- **RC-1 (CPU).** The 15 s in-process timeout abandons the decode thread, it does not stop
  it, so any quadratic or aliased-input loop kept a core busy until the process exited.
- **RC-2 (memory).** The 512 MiB budget covered one buffer, not the decode, and three paths
  asked the allocator for more than the process had (an allocation failure aborts the
  surrogate; `catch_unwind` cannot contain it).

This note records what was changed for each finding and, precisely, what is still
per-buffer. Regression tests carry the finding ID in their doc comment; allocation
assertions use the `cfg(test)` per-thread tracking allocator in `tw-core/src/testalloc.rs`
(peak live heap and largest single request of the code under test).

## Memory aborts (RC-2)

| ID | Change |
|---|---|
| DA-1 | `Axml::parse` stores string attributes as pool indices and resolves only the one or two a caller asks for; name comparisons no longer materialise strings; retained elements (65,536) and attributes (262,144) are capped. |
| DB-01 | Aseprite cels are sorted into draw order and then decoded and drawn one at a time straight from their samples (no RGBA expansion, no retained cels); cels outside the canvas are not decoded; the draw loop visits only the on-canvas part; summed cel bytes are budgeted (1 GiB per file) and canvas plus one cel must fit one buffer budget. |
| DB-06 | `check_dimensions` bounds each side (`limits::MAX_IMAGE_AXIS`, 2^20) as well as the area. The resizer's tables (8 B per long-axis pixel) are charged to the conversion budget. |
| U-2 | `read_all` (thumbnail, preview and property) no longer trusts `Stat`'s size: it allocates at most 64 MiB before data arrives, grows by doubling with `try_reserve_exact`, and reports an allocation failure as `E_OUTOFMEMORY`. Files up to 64 MiB still use one exactly sized allocation. |
| DA-8, DB-07 (conversion part) | `to_thumbnail` takes the decoded image by value and returns `Result`. RGBA8 is premultiplied where it lies (the full-size copy is gone); every other conversion buffer, the resizer tables and the output are charged to `decode::Budget` (decoded image plus 64 MiB of slack must fit 576 MiB). |
| DA-11 | Lenient (maker-note) RAW previews, the only candidates that were copied, are capped at 16 MiB, and their header is inspected without a patched copy. |

## Quadratic and aliased-input loops (RC-1)

| ID | Change |
|---|---|
| DA-3 | Sparse ARSC `entryCount` is clamped to what the chunk can hold. |
| DA-4 | EPUB manifest ids are indexed once (first id wins); the spine is resolved lazily; manifest items and spine references are capped at 65,536 each. |
| DA-5 | `unescape` looks for `;` in the next 11 bytes only. |
| DA-6 | FB2 `<binary` scan caches the next `>` so scans never overlap. |
| DA-9 | RAW IFD queue accepts only as many offsets as the walk can still visit; `first()` reads one value; repeated strip tags are ignored. |
| DB-02 | DXF is one pass over lines: a section is a `THUMBNAILIMAGE` line directly after a `2` line. |
| DB-03 | SQLite walk charges every record's size against the file size (real records occupy distinct bytes, so their sum is at most the file) and treats an overflow page reached twice as corruption. |
| DB-04 | DWG, ICNS and XMP try at most `limits::MAX_PAYLOAD_ATTEMPTS` (3) undecodable payloads; DWG skips records that alias one `(start, size)`. |
| DB-05 | XMP open tags reuse the first close tag at or after them; open tags with no close tag cost one pass. |
| DB-08 | XCF layer pointers are deduplicated (each still counts toward the 1024 cap) and tile-table entries read, decoded or not, are budgeted per file (2^21). |
| DB-10 | XCF offset arithmetic on 64-bit file pointers is checked. |
| DA-10 | `AOC\0` maker note shorter than its signature plus byte order returns instead of slicing past the end. |

## Abandoned threads (U-9, DA-2)

Both in-process entry points (`run_inproc`, `run_probe`) now share `run_contained`. A thread
that overruns still cannot be stopped, so it is still abandoned (I4: the module is pinned
under it), but it is counted while it runs and stops counting when it returns. At
`limits::MAX_ABANDONED_DECODE_THREADS` (2) running abandoned threads, in-process work fails
fast with `Timeout` without starting a thread. The shell already negative-caches `Timeout`
for five minutes and retries afterwards, which is when the threads may have returned.

## What is deliberately not fixed here

- **Per-decode accounting inside decoders is still per buffer.** `check_alloc` bounds each
  decode buffer at 512 MiB, and `decode::Budget` covers the conversion after the decode
  (decoded image, premultiply, resizer tables, output). A decoder that holds several large
  buffers at once can still exceed 512 MiB in aggregate (DB-07: Utah RLE 967 MB, MAT
  979 MB, VTF 1.1 GB measured). The decoder interface returns a `DynamicImage` and knows
  nothing of a shared counter; threading one through ~100 decoders is a rewrite, and the
  structural fix (hand-written parsers in the Job-Object-limited worker) removes the
  problem instead of accounting for it.
- **DA-8, third-party part.** zune-jpeg's progressive coefficient storage (2-3x the output)
  and `apply_orientation`'s second buffer for orientations 5 to 8 happen inside the `image`
  crate.
- **DA-7** (progressive JPEG with many tiny scans) is third-party CPU; worker placement of
  JPEG is the structural answer.
- **DA-12** (`twctl` runs decoders in its own process), **DB-09** (legitimate-size inputs
  near 15 s end in `Timeout`) and the bounded G-code and BGCODE retry loops (4 attempts) are
  unchanged.
- The abandoned-thread cap bounds how many threads can be busy at once, not how long one
  runs. A hostile file still costs a core until its thread returns or the process exits.
