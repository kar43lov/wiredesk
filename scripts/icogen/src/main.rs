//! One-shot tool: build assets/app-icon.ico from assets/icon-source.png.
//!
//! Replaces the `magick` invocation from the plan when ImageMagick isn't
//! installed. Run from the repo root:
//!   cargo run --manifest-path scripts/icogen/Cargo.toml --release
//!
//! Produces a 4-image .ico (16, 32, 48, 256 px) matching the layout
//! `magick ... -define icon:auto-resize=16,32,48,256` would produce.

use std::path::PathBuf;

use image::imageops::FilterType;

const SIZES: [u32; 4] = [16, 32, 48, 256];

fn main() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("scripts/icogen lives two levels deep")
        .to_path_buf();

    let src = repo_root.join("assets/icon-source.png");
    let dst = repo_root.join("assets/app-icon.ico");

    let img = image::open(&src)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", src.display()))
        .to_rgba8();

    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for &sz in &SIZES {
        let resized = image::imageops::resize(&img, sz, sz, FilterType::Lanczos3);
        let entry = ico::IconImage::from_rgba_data(sz, sz, resized.into_raw());
        icon.add_entry(
            ico::IconDirEntry::encode(&entry).expect("encode ico entry"),
        );
    }

    let file = std::fs::File::create(&dst)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", dst.display()));
    icon.write(file).expect("write ico");

    println!("wrote {} ({} sizes)", dst.display(), SIZES.len());
}
