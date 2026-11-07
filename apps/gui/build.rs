//! Embeds the app icons into keel-gui.exe (window icon: resource 1) and compiles the UI (ui/*.slint).
fn main() {
    keel_core::build::embed_icon();
    let cfg = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", cfg).expect("ui/main.slint");
}
