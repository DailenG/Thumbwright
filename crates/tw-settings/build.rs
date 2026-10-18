//! Embeds the executable's manifest. A build script cannot embed a resource without a
//! resource compiler, but the linker can generate and embed a manifest itself (link.exe and
//! lld-link):
//!
//! - Always: a dependency on Common Controls 6 (visual styles), via `/MANIFESTDEPENDENCY`.
//! - When `TW_MSIX_PUBLISHER` is set: the `<msix>` element that ties this executable to the
//!   sparse package in `packaging/msix`, so a process started from the install directory gets
//!   that package's identity (FUNCTIONAL_SPEC.md 4.2, REQ-REG-9). The element is merged in with
//!   `/MANIFESTINPUT`.
//!
//! Without `TW_MSIX_PUBLISHER` (every ordinary build) the element is left out and the
//! executable runs unpackaged. `scripts/release.ps1` sets it to the subject of the signing
//! certificate, because the package's `Publisher` must equal that subject and the executable's
//! `publisher` must equal the package's.
//!
//! Environment:
//!   `TW_MSIX_PUBLISHER`      package `Identity/@Publisher`, `CN=...` (enables the element)
//!   `TW_MSIX_PACKAGE_NAME`   package `Identity/@Name` (default `Thumbwright.ShellExtension`)
//!   `TW_MSIX_APPLICATION_ID` package `Application/@Id` (default `Thumbwright`)
//!
//! The defaults match `packaging/msix/build.ps1` (`-PackageName`) and
//! `packaging/msix/AppxManifest.xml` (`Application Id`).

use std::path::PathBuf;
use std::{env, fs};

const DEFAULT_PACKAGE_NAME: &str = "Thumbwright.ShellExtension";
const DEFAULT_APPLICATION_ID: &str = "Thumbwright";

const VARIABLES: [&str; 3] = [
    "TW_MSIX_PUBLISHER",
    "TW_MSIX_PACKAGE_NAME",
    "TW_MSIX_APPLICATION_ID",
];

fn main() {
    for name in VARIABLES {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' \
         name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
         processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );

    let Some(publisher) = variable("TW_MSIX_PUBLISHER") else {
        return;
    };
    let package_name = variable("TW_MSIX_PACKAGE_NAME").unwrap_or(DEFAULT_PACKAGE_NAME.into());
    let application_id =
        variable("TW_MSIX_APPLICATION_ID").unwrap_or(DEFAULT_APPLICATION_ID.into());
    assert!(
        publisher.starts_with("CN="),
        "TW_MSIX_PUBLISHER must be a certificate subject starting with CN=, got {publisher:?}"
    );
    for (name, value) in [
        ("TW_MSIX_PACKAGE_NAME", &package_name),
        ("TW_MSIX_APPLICATION_ID", &application_id),
    ] {
        assert!(
            !value.is_empty()
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "{name} must be letters, digits, '.' and '-' only, got {value:?}"
        );
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let path = out_dir.join("msix-identity.manifest");
    fs::write(
        &path,
        msix_manifest(&publisher, &package_name, &application_id),
    )
    .expect("write the msix identity manifest");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        path.display()
    );
}

/// A set, non-empty environment variable.
fn variable(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

/// The manifest fragment the linker merges into its generated one.
fn msix_manifest(publisher: &str, package_name: &str, application_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
         <assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\">\r\n\
         \x20 <msix xmlns=\"urn:schemas-microsoft-com:msix.v1\"\r\n\
         \x20   publisher=\"{}\"\r\n\
         \x20   packageName=\"{package_name}\"\r\n\
         \x20   applicationId=\"{application_id}\" />\r\n\
         </assembly>\r\n",
        xml_attribute(publisher)
    )
}

/// Escapes a value for a double-quoted XML attribute.
fn xml_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}
