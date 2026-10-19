//! Links the product icon (`packaging/icons/thumbwright.res`, made by
//! `packaging/icons/generate.ps1`) into `thumbwright_shell.dll` and `tw-worker.exe`. link.exe
//! and lld-link accept a compiled `.res` as an input, so no resource compiler is needed. The
//! Windows 11 menu verb's icon is `<dll>,-1`, the group icon with id 1 in this resource.

fn main() {
    let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/icons/thumbwright.res");
    println!("cargo:rerun-if-changed={}", res.display());
    println!("cargo:rustc-link-arg-cdylib={}", res.display());
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
