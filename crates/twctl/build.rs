//! Links the product icon (`packaging/icons/thumbwright.res`, made by
//! `packaging/icons/generate.ps1`) into `twctl.exe`. link.exe and lld-link accept a compiled
//! `.res` as an input, so no resource compiler is needed.

fn main() {
    let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/icons/thumbwright.res");
    println!("cargo:rerun-if-changed={}", res.display());
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
