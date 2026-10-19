//! Embeds the executable's manifest and product icon. A build script cannot embed a resource
//! without a resource compiler, but the linker can generate and embed a manifest itself
//! (link.exe and lld-link): a dependency on Common Controls 6 (visual styles), via
//! `/MANIFESTDEPENDENCY`.
//!
//! This executable deliberately carries NO package identity (no `<msix>` element), although
//! the sparse package in `packaging/msix` names it as its `Application` executable. A process
//! with package identity has its `HKCU` writes redirected into the package's private registry
//! hive, so the settings window would save into a copy that Explorer (which loads the shell
//! DLL without identity) never reads. Without the element the executable always runs
//! unpackaged and writes the real `HKCU\Software\Thumbwright`. The sparse package does not
//! need the executable to opt in: the Explorer verb is a COM class the package registers
//! itself. See docs/ARCHITECTURE.md ("Settings store").

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The product icon (packaging/icons/generate.ps1); link.exe and lld-link accept a
    // compiled .res as an input, so no resource compiler is needed.
    let res = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"))
        .join("../../packaging/icons/thumbwright.res");
    println!("cargo:rerun-if-changed={}", res.display());
    println!("cargo:rustc-link-arg-bins={}", res.display());

    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' \
         name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
         processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
