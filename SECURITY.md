# Security Policy

## Supported versions

Only the latest release line is supported.

## Reporting a vulnerability

Please report vulnerabilities privately to the repository owner via
[GitHub security advisories](https://github.com/DailenG/Thumbwright/security/advisories/new).
Do not open a public issue for anything you believe is exploitable. You should
get a response within a few days.

## Threat model, in short

Thumbwright runs in two very different contexts, and the design treats them
differently:

- **Inside Explorer's processes** (the thumbnail surrogate and Explorer
  itself), code faces hostile files by construction: Explorer will hand the
  DLL any file the user looks at. That code path decodes nothing itself when
  the decoder cannot prove it is memory- and stack-safe; such decoding runs in
  `tw-worker.exe`, one process per request, under a Job Object with memory,
  CPU and kill-on-close limits, and with UI restrictions (no clipboard, no
  desktop or display changes, no access to other processes' window handles).
  The worker is started with exactly its stdin, stdout and stderr handles and
  no other inherited handle. Failures return `E_FAIL` to the shell, never a
  placeholder and never a crash. Verified live against a hostile corpus
  (decompression bombs, header bombs, deeply nested metadata): the worker dies,
  the surrogate survives.
- **`twctl` and the settings app** run as your user, launched from the menu.
  They validate before they write (atomic, exclusive staging), refuse
  predictable-name attacks, and ask before anything that leaves the machine
  (upload).

The full review that established this, and the hostile-input findings and
their fixes, are in `docs/history/REVIEW_FINDINGS.md`. The invariants the code
must keep are I1-I8 in `docs/ARCHITECTURE.md`; anyone
changing decoders, the worker protocol, or registration must keep them.

## What is intentionally not defended

- Malicious local software running as your user. Windows per-user registry and
  the settings file are user-writable by design; Thumbwright treats them as
  trusted input like any shell extension does.
- Compressed-data bombs inside formats routed to pure-Rust in-process decoders
  are bounded by byte budgets, but a very slow legitimate file can still take
  its tier's time budget (up to 15 s in-process, 30 s for worker formats).

## Known advisory exceptions

`cargo audit` runs weekly and on dependency changes (`.github/workflows/audit.yml`);
accepted exceptions live in `.cargo/audit.toml`:

- **RUSTSEC-2024-0436, `paste` 1.0.15 (unmaintained, informational; no
  vulnerability).** A proc-macro: it runs only at compile time and is not in the
  shipped binaries. It is pulled in by three upstream crates (`lofty`, `pulp`
  via `exr` via `image`, and `rav1e` via `ravif`), so replacing it means
  replacing or patching all three. To be revisited when they move to a
  maintained fork.
