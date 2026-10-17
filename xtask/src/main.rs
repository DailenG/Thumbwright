//! `cargo xtask licenses [--check]`
//!
//! Resolves the dependency graph for x86_64-pc-windows-msvc via `cargo metadata`,
//! rejects any crate whose SPDX expression cannot be satisfied by the permissive
//! allow-list, and writes THIRD_PARTY_LICENSES.md. `--check` fails instead of
//! writing when the file is stale.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde_json::Value;

const TARGET: &str = "x86_64-pc-windows-msvc";

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
            violations.push(format!("{name} {version}: {:?}", license));
        }
        pkgs.insert(
            (name.clone(), version.clone()),
            Pkg {
                name,
                version,
                license,
                repository: p["repository"].as_str().unwrap_or("").to_owned(),
                tooling_only: !shipped.contains(id),
            },
        );
    }

    if !violations.is_empty() {
        eprintln!("license policy violations:");
        for v in &violations {
            eprintln!("  {v}");
        }
        return ExitCode::FAILURE;
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

fn render<'a>(pkgs: impl Iterator<Item = &'a Pkg>) -> String {
    let mut s = String::from(
        "# Third-Party Licenses\n\n\
         Generated by `cargo xtask licenses` from `cargo metadata` for `x86_64-pc-windows-msvc`.\n\
         Do not edit by hand; `cargo xtask licenses --check` fails when this file is stale.\n\n\
         Policy: every crate must satisfy its SPDX expression using only permissive licenses \
         (MIT, Apache-2.0, BSD, MPL-2.0, ISC, Zlib, BSL-1.0, Unicode, CC0, Unlicense). \
         Any GPL, LGPL or AGPL-only crate fails the audit.\n\n\
         \"Scope\" is `shipped` for crates in the dependency graph of the DLL or executables \
         (including compile-time build-script and proc-macro crates), `tooling` for crates \
         used only by repository automation (`xtask`).\n\n\
         | Crate | Version | License (SPDX) | Scope | Repository |\n\
         |---|---|---|---|---|\n",
    );
    for p in pkgs {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            p.name,
            p.version,
            p.license,
            if p.tooling_only { "tooling" } else { "shipped" },
            p.repository
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::satisfiable;

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
