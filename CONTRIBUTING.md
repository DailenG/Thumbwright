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
  `docs/history/REVIEW_HANDOFF.md`).
- **Hostile inputs come first.** Every decoder checks declared dimensions
  against a budget before allocating and counts every byte read against a cap
  (invariant I7). A new decoder without truncated-header and
  lying-dimension tests will not be merged.
- **Registration must stay journaled and reversible.** Anything that writes to
  the registry goes through the engine in `tw-core/src/registration.rs`; there
  are no direct registry writes in features.
- **Keep the invariants.** Section 3 of `docs/history/REVIEW_HANDOFF.md` lists
  I1-I8. If a change needs to break one, open an issue first.

## Development

Windows x64 with the toolchain pinned in `rust-toolchain.toml` (rustup installs
it). Linking needs the MSVC CRT/SDK libraries (Visual Studio Build Tools with
"Desktop development with C++"), or nothing else.

```
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo xtask licenses --check
```

All four must pass. CI runs them on every push.

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
