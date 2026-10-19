# Contributing to Thumbwright

Thanks for the interest. Thumbwright is a clean-room project, and that
constrains how it can accept contributions. Please read this before opening a
PR.

## The clean-room rule (binding on all contributors)

Thumbwright implements `FUNCTIONAL_SPEC.md`, a black-box specification written
from public observation only. To keep it that way:

- **Do not** decompile, disassemble, debug, or in any way run code from, any
  product with similar functionality — including free or abandonware versions.
- **Do not** paste code, file layouts, strings, or internal symbol names from
  such products into issues, PRs, or chat.
- If your knowledge of a file format comes from reverse-engineering such a
  product, do not contribute code for that format. Published format
  specifications, books, and documentation are fine and preferred.

## Ground rules

- **Pure Rust only.** No crate may compile or link native code: no `cc`, no
  `*-sys` with a `links` key. `cargo xtask licenses --check` fails the build if
  one appears, and it also rejects copyleft-only dependencies. If a format
  needs an OS codec (WIC, Media Foundation, Windows.Data.Pdf), it must decode
  in `tw-worker.exe`, never inside the shell surrogate (invariant I2 in
  `docs/ARCHITECTURE.md`).
- **Hostile inputs come first.** Every decoder checks declared dimensions
  against a budget before allocating and counts every byte read against a cap
  (invariant I7). A new decoder without truncated-header and
  lying-dimension tests will not be merged.
- **Registration must stay journaled and reversible.** Anything that writes to
  the registry goes through the engine in `tw-core/src/registration.rs`; there
  are no direct registry writes in features.
- **Keep the invariants.** `docs/ARCHITECTURE.md` lists I1-I8. If a change
  needs to break one, open an issue first.

## Development

Windows x64 with the toolchain pinned in `rust-toolchain.toml` (rustup installs
it). Linking needs the MSVC CRT/SDK libraries (Visual Studio Build Tools with
"Desktop development with C++"), or nothing else.

## Checks before a push

CI on GitHub runs only the **format** check and the **dependency policy**
(`licenses`), on Linux. The Windows build, clippy and the test suite are **not**
run on push or pull request (the Windows job in `.github/workflows/ci.yml` is
manual, for release candidates). A green CI badge therefore says nothing about
whether the code builds or passes its tests on Windows, so the local gate below
is required: run all four commands on Windows and make them pass **before every
push and before you open or update a pull request**.

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo xtask licenses --check
```

- `fmt` and `licenses` are what CI repeats; `clippy` and `test` are yours alone.
- Run `cargo fmt --all` to fix formatting. If `licenses` rejects a dependency,
  see the ground rules above (pure Rust, permissive licences).
- A pull request that was not run through the local gate will be asked to do so
  before review.

Some tests write a throw-away key under `HKCU\Software\ThumbwrightTest-*`
(removed on drop) and files under `%TEMP%`.

## Issues

- Bug reports: the file, what you expected, what happened. For a crash, the
  `%LOCALAPPDATA%\Thumbwright.log` section around it helps.
- Format support: check `twctl formats --json` first — a format listed with
  `thumbnail: true` that misbehaves is a bug; one listed without it is a
  feature request.
- The 48 unrouted formats (issue #15) mostly need a real sample file we can
  legally commit, or a documented layout. Pointing at one is a great
  contribution.

## Pull requests

Small, one concern each. New decoders should come with: the fixture it decodes,
the truncation and lying-dimension tests, the placement decision (in-proc or
worker, with reasoning), and routing in `formats/table.rs`. Remember: you must
be able to legally publish everything you submit (code and fixtures) — GPL-3.0-or-later
compatible, and CC0/BSD/MIT for binary fixtures.
