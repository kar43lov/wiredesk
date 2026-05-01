// Embed a Windows application manifest declaring a dependency on Common
// Controls 6.0. native-windows-gui calls APIs like `GetWindowSubclass`
// that only exist in comctl32.dll v6+; without this manifest Windows
// supplies the legacy v5 and the binary fails to start with "entry
// point not found in DLL".
//
// `embed-manifest` is pure-Rust (no windres / RC.exe), so this build
// script works fine on macOS / Linux during cross-checks.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("WireDesk.Host"))
            .expect("unable to embed Windows application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
