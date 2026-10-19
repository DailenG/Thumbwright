# Test fixtures: real files

Used by `crates/tw-core/tests/samples.rs`. Every file here is committed only because its
licence allows redistribution; larger RAW samples are not committed and are downloaded
by `scripts/fetch-samples.ps1` instead.

## `jxl/`: JPEG XL conformance streams

Source: <https://github.com/libjxl/conformance> (`testcases/<name>/input.jxl` and
`ref.png`, renamed `<name>.jxl` / `<name>.ref.png`). The repository is BSD-3-Clause
(`jxl/LICENSE-libjxl-conformance.txt`, Copyright (c) the JPEG XL Project Authors); its
`testcases/README.md` lists the licence of each source image, and all of the cases used
here are listed as **CC0**:

| case | covers |
|---|---|
| `bicycles` | modular mode, squeeze, XYB, 8-bit RGB |
| `sunset_logo` | orientation 7, two layers, 10-bit alpha |
| `bench_oriented_brg` | container, JPEG reconstruction data, orientation 5, ICC profile |
| `alpha_premultiplied` | VarDCT, associated (premultiplied) alpha |
| `alpha_nonpremultiplied` | modular, straight alpha, 12-bit colour (16-bit reference PNG) |
| `patches` | VarDCT patches, extra channel |
| `grayscale` | VarDCT, grayscale ICC profile |

The reference PNGs come from libjxl; the tests require a pixel difference of at most
1-2 levels (8-bit rounding) after weighting colour by alpha. Cases whose reference is
kept in a non-sRGB encoding (e.g. `bike`, `lz77_flower`, `spot`) are deliberately not
included: the decoder converts those to sRGB, so they cannot match the reference PNG.

## `raw/`: camera RAW

| file | source | licence |
|---|---|---|
| `canon-powershot-g1.crw` | raw.pixls.us, Canon PowerShot G1, original name `crw_1693.crw` (id 2073, 2.19 MB), SHA-256 `f39be5acf14057ce04527fd8d0574e57ec8ed5fed082d7bfaff763075a44ee8a` | CC0 |

raw.pixls.us asks every submitter to "release it under the CC0 license into the public
domain" (the upload form at <https://raw.pixls.us/>), and its repository index
(`json/getrepository.php?set=all`) records the licence of each file.
Samples under other licences exist in the repository (CC BY-NC-SA 4.0); none of those are
used here, committed or fetched: every entry in `scripts/fetch-samples.ps1` was taken from
index rows reading "Creative Commons 0 - Public Domain".

## Files generated locally with free tools (issue #16)

These are files we made ourselves, so there is no third-party licence to track. Each was
written by the application the format belongs to (or, for the HEIC still and the BDAV
packets, by the steps listed), then decoded to check the layouts the decoders were written
against. Generators are in `scripts/fixtures/`; tool versions are those used when the
file was made. Total about 0.8 MB.

| file | size | made with | how |
|---|---|---|---|
| `vips/rgb8.vips` | 2,482 | libvips 8.18.7 (`vips-dev-x64-web-8.18.7.zip`, libvips/build-win64-mxe) | `vips copy src.png rgb8.vips`; `src.png` is the 12x8 image from `scripts/fixtures/make_vips_source.py` |
| `vips/rgb16.vips` | 2,770 | libvips 8.18.7 | `vips colourspace src.png rgb16.vips rgb16` (interpretation 25, 16 bit) |
| `vips/grey16.vips` | 2,386 | libvips 8.18.7 | `vips colourspace src.png grey16.vips grey16` (interpretation 26) |
| `vips/bw8.vips` | 2,290 | libvips 8.18.7 | `vips colourspace src.png bw8.vips b-w` (interpretation 1) |
| `vips/rgba8.vips` | 2,578 | libvips 8.18.7 | `vips extract_band src.png a.vips 0 --n 1`, then `vips bandjoin "src.png a.vips" rgba8.vips` (4 bands, sRGB) |
| `blend/cube-cone.blend` | 595,148 | Blender 5.2.2 LTS portable (`blender-5.2.2-windows-x64.zip`) | `blender --factory-startup --python scripts/fixtures/make_blend.py -- cube-cone.blend` |
| `blend/cube-cone-zstd.blend` | 101,222 | Blender 5.2.2 LTS | same, with a trailing `zstd` argument (compressed save) |
| `pdn/quadrants.pdn` | 9,674 | Paint.NET 5.1.12 portable (`paint.net.5.1.12.portable.x64.zip`) | `scripts/fixtures/make_pdn.ps1` opens an 800x600 four-quadrant PNG (`make_quad_png.py out.png 800 600`) in Paint.NET and uses File > Save As with type "Paint.NET (*.pdn)" |
| `m2ts/bdav-192.m2ts` | 52,608 | ffmpeg 9.0.1 (gyan.dev full build) and `scripts/fixtures/ts_to_bdav.py` | see below |
| `heic/quad.heic` | 513 | ffmpeg 9.0.1 (libx265, Main profile) and `scripts/fixtures/hevc_to_heic.py` | see below |
| `livp/live-photo.livp` | 12,818 | the two files above plus ffmpeg | see below |

Findings from each file are recorded in the commit messages; the decoders were checked
field by field against the real headers.

* **`.vips`**: the header is `magic(4) width height bands bbits format coding type ...` in
  little-endian order on x64 (`b6 a6 f2 08` first). libvips has no CLI switch for a
  big-endian file (`vips byteswap` swaps the samples but leaves a little-endian header, and
  `vips copy --swap` is deprecated and writes the same bytes), so the big-endian reader
  stays covered by the synthetic unit tests only. A real file exposed a decoder bug: the
  colour-space numbers were wrong (sRGB is 22, RGB16 25, GREY16 26; the decoder accepted
  15, 20, 21, i.e. CMYK and two Lab variants, and rejected every file libvips writes).
* **`.blend`**: a headless (`-b`) Blender 5.2.2 does not render a thumbnail; it writes the
  stock thumbnail of the startup file whatever the scene contains (the `TEST` block was
  byte-identical for scenes that differed in objects, materials and camera). `make_blend.py` therefore runs with a
  window and `file_preview_type = CAMERA`, so the 128x128 thumbnail is of the scene. Header
  `BLENDER17-01v0502`, 32-byte block headers, `TEST` block after `REND`: the decoder's
  Blender 5 layout was correct for both the plain and the zstd-compressed file.
* **`.pdn`**: `PDN3`, 3-byte length (618 bytes), then
  `<pdnImage width height layers savedWithVersion><custom><thumb png="BASE64" /></custom>`.
  The thumbnail is a palette PNG scaled so the long edge is 256. The decoder's layout was
  correct. Paint.NET has no command line for saving; `make_pdn.ps1` drives the Save As
  dialog (UI Automation to find the dialog, a mouse click on the "Save as type" list, then
  Enter), refusing to send input unless Paint.NET owns the foreground window.

### `m2ts/bdav-192.m2ts`, `heic/quad.heic`, `livp/live-photo.livp`

```powershell
# 3 s, 1280x720: one second each of red, green, blue; H.264 High + AAC
ffmpeg -f lavfi -i "sine=frequency=440:duration=3" -filter_complex "color=c=red:s=1280x720:r=24:d=1,format=yuv420p[a];color=c=green:s=1280x720:r=24:d=1,format=yuv420p[b];color=c=blue:s=1280x720:r=24:d=1,format=yuv420p[c];[a][b][c]concat=n=3:v=1:a=0[v]" -map "[v]" -map 0:a -c:v libx264 -crf 23 -g 24 -pix_fmt yuv420p -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709 -c:a aac -b:a 64k -shortest rgb720.mp4
# MP4 -> MPEG-TS (188-byte packets) -> BDAV (192-byte packets)
ffmpeg -i rgb720.mp4 -map 0:v -map 0:a -c copy -bsf:v h264_mp4toannexb -f mpegts rgb720.ts
python scripts/fixtures/ts_to_bdav.py rgb720.ts bdav-192.m2ts

# HEIC still: four flat quadrants (make_quad_png.py quad.png 160 120), HEVC Main profile,
# wrapped in a single-image HEIF container
ffmpeg -i quad.png -frames:v 1 -c:v libx265 -pix_fmt yuv420p -crf 14 -x265-params "log-level=error:repeat-headers=1:aud=0" -f hevc quad.hevc
python scripts/fixtures/hevc_to_heic.py quad.hevc quad.heic 160 120

# Live Photo bundle: the HEIC and a 1 s MOV, named as an iPhone names them
ffmpeg -t 1 -i rgb.mp4 -map 0:v -map 0:a -c copy -f mov IMG_0001.MOV    # rgb.mp4: the recipe above at 320x180, -crf 20
python scripts/fixtures/make_livp.py quad.heic IMG_0001.MOV live-photo.livp
```

The BDAV arrival time stamps must agree with the stream's PCRs: with a constant-rate stamp
(the first version of `ts_to_bdav.py`) Media Foundation took the file's timeline from the
stamps, and the same elementary stream gave different frames at the same offset than as a
188-byte `.ts`. The script now interpolates each packet's stamp from the PCRs, as a BDAV
muxer would.

ffmpeg cannot write a BDAV file or a HEIF still, so those two containers are written by
the small scripts above. They are spec-conformant but not byte-identical to what a camera or
an iPhone writes. The `.livp` and `.m2ts` decoders were therefore also run (not committed)
against real camera-style files: a BDAV remux of `Dailen-Logo-Dark.mp4` (a real H.264/AAC
MP4 from Downloads), and a `.livp` made from a real iPhone-style `IMG_0074.heic` (450x600,
HEVC) plus the MOV above. Both decoded to the right content (256x106 frame; 192x256 still).

## Larger samples (not committed)

`scripts/fetch-samples.ps1` downloads 30 further CC0 RAW files from raw.pixls.us
(about 320 MB: Canon CR3 / CRW / CR2, Sigma X3F including SD Quattro, Nikon NEF, Sony
ARW, Olympus ORF, Panasonic RW2, Pentax PEF, Fujifilm RAF, Minolta MRW, Adobe DNG) into
`target/samples`, verifying each against a pinned SHA-256. Then:

```powershell
.\scripts\fetch-samples.ps1
$env:TW_SAMPLES = (Resolve-Path target\samples).Path
cargo test -p tw-core --test samples -- --ignored
```

For each sample the test compares `raw::extract` against the embedded JPEG decoded
separately from a hand-recorded file offset, including the orientation to be applied
(portrait CR3 / CRW / CR2 / NEF / ARW files are in the set).
