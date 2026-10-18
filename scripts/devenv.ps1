# Dot-source to point the current PowerShell session at the build-room toolchain:
#   . .\scripts\devenv.ps1
# Locates rustup/cargo under .toolchain and, when .toolchain\xwin exists, links with
# rust-lld against its CRT/SDK import libraries (no Visual Studio needed). Machines with
# Visual Studio and a normal rustup install need none of this.
$root = Split-Path -Parent $PSScriptRoot
$env:RUSTUP_HOME = Join-Path $root '.toolchain\rustup'
$env:CARGO_HOME = Join-Path $root '.toolchain\cargo'
$env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:PATH
$xwin = Join-Path $root '.toolchain\xwin'
if (Test-Path $xwin) {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'rust-lld'
    # This variable replaces the rustflags in .cargo/config.toml, so repeat crt-static here.
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = (
        @('-C', 'target-feature=+crt-static') + (
            @('crt\lib\x86_64', 'sdk\lib\um\x86_64', 'sdk\lib\ucrt\x86_64') |
                ForEach-Object { '-Lnative=' + (Join-Path $xwin $_) }
        )
    ) -join ' '
}
