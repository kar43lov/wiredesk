// Windows-only build artefacts for the client binary. Mirrors the host's
// build.rs (apps/wiredesk-host/build.rs) — see there for the reasoning
// behind each step; the two differ only in the manifest name and in the
// DPI declaration below.
//
// The manifest matters more here than on the host: the client positions its
// own window on a caller-chosen display, so it must see real per-monitor
// coordinates rather than the ones Windows virtualises for a DPI-unaware
// process. winit also asks for Per-Monitor-V2 at runtime; declaring it in
// the manifest as well means the awareness is set before the first window
// exists, which is what the docs recommend.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("WireDesk.Client").dpi_awareness(DpiAwareness::PerMonitorV2))
            .expect("unable to embed Windows application manifest");

        // Icon resource — only when the build host has rc.exe / windres.
        let host = std::env::var("HOST").unwrap_or_default();
        if host.contains("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("../../assets/app-icon.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=icon embed failed: {e}");
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app-icon.ico");
}
