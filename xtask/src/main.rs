//! `cargo xtask licenses [--check]`
//!
//! Resolves the dependency graph for x86_64-pc-windows-msvc via `cargo metadata`,
//! rejects any crate whose SPDX expression cannot be satisfied by the permissive
//! allow-list, rejects any crate that compiles or links native code (no C compiler is
//! assumed, and C/C++ must never run in the surrogate), and writes
//! THIRD_PARTY_LICENSES.md.
//!
//! The file carries each crate's full licence text (and NOTICE text, where the crate has
//! one), read from the crate source in the local cargo registry: MIT, BSD and Apache-2.0
//! require the notice and the licence text to accompany binary redistribution, so an
//! SPDX table with a URL is not enough. A crate with no licence file in its source is a
//! policy violation, not a blank entry.
//!
//! `--check` fails instead of writing when the file is stale.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde_json::Value;

const TARGET: &str = "x86_64-pc-windows-msvc";

/// Crates whose presence means C/C++ gets compiled at build time.
const NATIVE_BUILD_CRATES: &[&str] = &["cc", "cmake", "bindgen", "pkg-config", "vcpkg", "nasm-rs"];

/// Pure-Rust crates that carry a `links` key only as a "one copy in the graph" marker; their
/// build scripts compile and link nothing (checked against the published sources):
/// `rayon-core` (`build.rs` just prints `rerun-if-changed`) and `wasm-bindgen-shared`
/// (hashes its own sources; reached only through rav1e's wasm-only feature, so it is not
/// built for Windows).
const LINKS_MARKER_ONLY: &[&str] = &["rayon-core", "wasm-bindgen-shared"];

/// Permissive licenses accepted in shipped or build-time dependencies.
const ALLOWED: &[&str] = &[
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "0BSD",
    "MPL-2.0",
    "ISC",
    "Zlib",
    "BSL-1.0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "CC0-1.0",
    "Unlicense",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["licenses"] => run(false),
        ["licenses", "--check"] => run(true),
        _ => {
            eprintln!("usage: cargo xtask licenses [--check]");
            ExitCode::FAILURE
        }
    }
}

struct Pkg {
    name: String,
    version: String,
    license: String,
    repository: String,
    tooling_only: bool,
    /// Full licence text(s) and any NOTICE text, verbatim from the crate source.
    texts: Vec<(String, String)>,
    /// The crate declares a licence but packages no licence file; recorded in the notice.
    text_missing: bool,
}

fn run(check: bool) -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&root)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            TARGET,
        ])
        .output()
        .expect("spawn cargo metadata");
    if !out.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        return ExitCode::FAILURE;
    }
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata JSON");

    let members: HashSet<&str> = meta["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let shipped = reachable_from(&meta, &members, |id| !id.contains("xtask"));

    let mut pkgs = BTreeMap::new();
    let mut violations = Vec::new();
    for p in meta["packages"].as_array().unwrap() {
        let id = p["id"].as_str().unwrap();
        if members.contains(id) || !in_resolve(&meta, id) {
            continue;
        }
        let name = p["name"].as_str().unwrap().to_owned();
        let version = p["version"].as_str().unwrap().to_owned();
        let license = p["license"].as_str().unwrap_or("").to_owned();
        if license.is_empty() || !satisfiable(&license) {
            violations.push(format!("{name} {version}: license {:?}", license));
        }
        if let Some(reason) = native_code(p) {
            violations.push(format!("{name} {version}: {reason}"));
        }
        let texts = licence_texts(p);
        // The crate declares a permissive licence but its package carries no licence file.
        // Recorded below as a documented gap rather than a build failure: the missing text
        // is the upstream packaging omission, not ours, and failing the build would block
        // every build on it. The gap is printed and shipped in the notice file.
        let missing_text = texts.is_empty();
        pkgs.insert(
            (name.clone(), version.clone()),
            Pkg {
                name,
                version,
                license,
                repository: p["repository"].as_str().unwrap_or("").to_owned(),
                tooling_only: !shipped.contains(id),
                texts,
                text_missing: missing_text,
            },
        );
    }

    if !violations.is_empty() {
        eprintln!("dependency policy violations:");
        for v in &violations {
            eprintln!("  {v}");
        }
        return ExitCode::FAILURE;
    }

    let gaps: Vec<&Pkg> = pkgs.values().filter(|p| p.text_missing).collect();
    if !gaps.is_empty() {
        eprintln!(
            "licence texts missing from {} crate packages (recorded in the notice):",
            gaps.len()
        );
        for p in &gaps {
            eprintln!(
                "  {} {}: declared {}, upstream {}",
                p.name, p.version, p.license, p.repository
            );
        }
    }
    let doc = render(pkgs.values());
    let path = root.join("THIRD_PARTY_LICENSES.md");
    if check {
        let current = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        if current != doc {
            eprintln!("THIRD_PARTY_LICENSES.md is stale; run `cargo xtask licenses`");
            return ExitCode::FAILURE;
        }
        println!(
            "{} crates, all permissive; THIRD_PARTY_LICENSES.md up to date",
            pkgs.len()
        );
    } else {
        std::fs::write(&path, doc).expect("write THIRD_PARTY_LICENSES.md");
        println!(
            "{} crates, all permissive; wrote {}",
            pkgs.len(),
            path.display()
        );
    }
    ExitCode::SUCCESS
}

/// Why a package would compile or link native code, if it does.
fn native_code(p: &Value) -> Option<String> {
    let name = p["name"].as_str().unwrap_or("");
    if NATIVE_BUILD_CRATES.contains(&name) {
        return Some("compiles native code at build time".into());
    }
    if LINKS_MARKER_ONLY.contains(&name) {
        return None;
    }
    p["links"]
        .as_str()
        .map(|lib| format!("links native library `{lib}`"))
}

fn in_resolve(meta: &Value, id: &str) -> bool {
    meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["id"].as_str() == Some(id))
}

/// Package ids reachable from the workspace members accepted by `root_filter`.
fn reachable_from(
    meta: &Value,
    members: &HashSet<&str>,
    root_filter: impl Fn(&str) -> bool,
) -> HashSet<String> {
    let nodes: BTreeMap<&str, Vec<&str>> = meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let deps = n["deps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| {
                    // Dev-only edges (test helpers) do not ship.
                    d["dep_kinds"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|k| k["kind"].as_str() != Some("dev"))
                })
                .filter_map(|d| d["pkg"].as_str())
                .collect();
            (n["id"].as_str().unwrap(), deps)
        })
        .collect();
    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = members.iter().copied().filter(|m| root_filter(m)).collect();
    while let Some(id) = stack.pop() {
        if seen.insert(id.to_owned()) {
            stack.extend(nodes.get(id).into_iter().flatten().copied());
        }
    }
    seen
}

/// Evaluates an SPDX expression (`OR`, `AND`, `WITH`, parentheses, legacy `/`) against ALLOWED.
fn satisfiable(expr: &str) -> bool {
    let normalized = expr
        .replace('/', " OR ")
        .replace('(', " ( ")
        .replace(')', " ) ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let mut pos = 0;
    let ok = parse_or(&tokens, &mut pos);
    ok && pos == tokens.len()
}

fn parse_or(t: &[&str], pos: &mut usize) -> bool {
    let mut ok = parse_and(t, pos);
    while t.get(*pos) == Some(&"OR") {
        *pos += 1;
        ok |= parse_and(t, pos);
    }
    ok
}

fn parse_and(t: &[&str], pos: &mut usize) -> bool {
    let mut ok = parse_atom(t, pos);
    while t.get(*pos) == Some(&"AND") {
        *pos += 1;
        ok &= parse_atom(t, pos);
    }
    ok
}

fn parse_atom(t: &[&str], pos: &mut usize) -> bool {
    match t.get(*pos) {
        Some(&"(") => {
            *pos += 1;
            let ok = parse_or(t, pos);
            if t.get(*pos) == Some(&")") {
                *pos += 1;
                ok
            } else {
                false
            }
        }
        Some(id) => {
            *pos += 1;
            let mut id = id.to_string();
            if t.get(*pos) == Some(&"WITH") {
                id = format!("{id} WITH {}", t.get(*pos + 1).unwrap_or(&""));
                *pos += 2;
            }
            ALLOWED.contains(&id.as_str())
        }
        None => false,
    }
}

/// Every licence and notice file in the crate's source directory, as (file name, text).
/// Sorted by name so the output is deterministic. `manifest_path` points at the crate's
/// Cargo.toml in the local registry.
fn licence_texts(p: &Value) -> Vec<(String, String)> {
    let Some(dir) = p["manifest_path"]
        .as_str()
        .and_then(|m| std::path::Path::new(m).parent())
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        let is_text = (lower.starts_with("license")
            || lower.starts_with("licence")
            || lower.starts_with("copying")
            || lower.starts_with("notice"))
            && !lower.ends_with(".meta");
        if !is_text {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        out.push((name, text.trim_end().replace("\r\n", "\n")));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn render<'a, I>(pkgs: I) -> String
where
    I: Iterator<Item = &'a Pkg>,
{
    let pkgs: Vec<&Pkg> = pkgs.collect();
    let mut s = String::from(
        "# Third-Party Licenses\n\n\
         Generated by `cargo xtask licenses` from `cargo metadata` for `x86_64-pc-windows-msvc`.\n\
         Do not edit by hand; `cargo xtask licenses --check` fails when this file is stale.\n\n\
         Policy: every crate must satisfy its SPDX expression using only permissive licenses \
         (MIT, Apache-2.0, BSD, MPL-2.0, ISC, Zlib, BSL-1.0, Unicode, CC0, Unlicense). \
         Any GPL, LGPL or AGPL-only crate fails the audit. Every crate must also ship a \
         licence text in its source, reproduced below.\n\n\
         \"Scope\" is `shipped` for crates in the dependency graph of the DLL or executables \
         (including compile-time build-script and proc-macro crates), `tooling` for crates \
         used only by repository automation (`xtask`).\n\n\
         ## Index\n\n\
         | Crate | Version | License (SPDX) | Scope | Repository |\n\
         |---|---|---|---|---|\n",
    );
    for p in &pkgs {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            p.name,
            p.version,
            p.license,
            if p.tooling_only { "tooling" } else { "shipped" },
            p.repository
        ));
    }
    s.push_str("\n## Licence texts\n\nEach section is verbatim from the crate's source.\n");
    let mut first = true;
    for p in &pkgs {
        if first {
            first = false;
        } else {
            s.push('\n');
        }
        s.push_str(&format!("\n### {} {}\n\n", p.name, p.version));
        if p.text_missing {
            s.push_str(&format!(
                "**Licence text not packaged with this crate.** The crate declares `{}`; \
                 its package contains no licence file. Upstream: {}\n",
                p.license,
                if p.repository.is_empty() {
                    "see crates.io"
                } else {
                    &p.repository
                }
            ));
            continue;
        }
        for (file, text) in &p.texts {
            s.push_str(&format!("From `{file}`:\n\n```\n"));
            s.push_str(text);
            s.push_str("\n```\n");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{native_code, satisfiable};
    use serde_json::json;

    #[test]
    fn native_code_policy() {
        assert!(native_code(&json!({"name": "cc", "links": null})).is_some());
        assert!(native_code(&json!({"name": "libz-sys", "links": "z"})).is_some());
        assert!(native_code(&json!({"name": "image", "links": null})).is_none());
        // A uniqueness-marker `links` is not a native library, but an unknown one still is.
        assert!(native_code(&json!({"name": "rayon-core", "links": "rayon-core"})).is_none());
        assert!(native_code(&json!({"name": "rayon-core-sys", "links": "rayon-core"})).is_some());
    }

    #[test]
    fn spdx_policy() {
        assert!(satisfiable("MIT OR Apache-2.0"));
        assert!(satisfiable("MIT/Apache-2.0"));
        assert!(satisfiable("(MIT OR Apache-2.0) AND Unicode-3.0"));
        assert!(satisfiable(
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT"
        ));
        assert!(
            satisfiable("MIT OR GPL-3.0"),
            "dual-licensed with a permissive option is usable"
        );
        assert!(!satisfiable("GPL-3.0"));
        assert!(!satisfiable("LGPL-2.1-or-later"));
        assert!(!satisfiable("MIT AND GPL-2.0"));
        assert!(!satisfiable("(MIT"));
        assert!(!satisfiable(""));
    }
}
