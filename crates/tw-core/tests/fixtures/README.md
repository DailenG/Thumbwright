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
