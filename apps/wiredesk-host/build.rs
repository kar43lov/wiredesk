// Embed Windows-only build artefacts:
//
// 1. Application manifest declaring a dependency on Common Controls 6.0.
//    native-windows-gui calls APIs like `GetWindowSubclass` that only
//    exist in comctl32.dll v6+; without this manifest Windows supplies
//    the legacy v5 and the binary fails to start with "entry point not
//    found in DLL". `embed-manifest` is pure-Rust (no windres / RC.exe),
//    so it works for cross-checks on macOS/Linux too.
//
// 2. Application icon (.ico) compiled into a Win32 resource section so
//    Explorer / taskbar / Alt+Tab / the title bar show a real WireDesk
//    icon instead of the generic Rust executable glyph. Requires `rc.exe`
//    (Windows SDK) or `windres` (mingw-w64), so we only attempt it when
//    the *build host* is Windows. Cross-compiling from macOS skips icon
//    embedding gracefully — the binary still runs, just without the
//    embedded resource.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("WireDesk.Host"))
            .expect("unable to embed Windows application manifest");

        // Icon resource — only when the build host has rc.exe/windres.
        // The HOST env var is the triple of whatever's invoking cargo;
        // checking that it contains "windows" is the safest gate.
        let host = std::env::var("HOST").unwrap_or_default();
        if host.contains("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("../../assets/app-icon.ico");
            if let Err(e) = res.compile() {
                // Don't fail the build — produce a warning so the user
                // knows why the binary lacks an icon (most likely the
                // Windows SDK / mingw is not on PATH).
                println!("cargo:warning=icon embed failed: {e}");
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app-icon.ico");
}
