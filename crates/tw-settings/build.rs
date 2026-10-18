//! Embeds a manifest that selects Common Controls 6 (visual styles) in the executable. A
//! build script cannot embed a resource without a resource compiler, but the linker can
//! generate and embed a manifest from `/MANIFESTDEPENDENCY` itself (link.exe and lld-link).

fn main() {
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' \
         name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
         processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
