//! Build-script helper: embeds the app icons (`assets/app.rc` at the workspace root) into an exe;
//! the tray loads them as resources 1 (dark glyph) and 2 (white glyph).
use std::path::PathBuf;

pub fn embed_icon() {
    let assets = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..").join("..").join("assets");
    for f in ["app.rc", "keel.ico", "keel-light.ico"] { println!("cargo:rerun-if-changed={}", assets.join(f).display()); }
    embed_resource::compile(assets.join("app.rc"), embed_resource::NONE).manifest_optional().unwrap();
}
