use std::env;
use std::path::PathBuf;

fn main() {
    // Pick up the path published by citrate-ui-kit's build.rs.
    // `links = "citrate_ui_kit"` + `cargo:UI_KIT=...` in the kit's build.rs
    // turns into the `DEP_CITRATE_UI_KIT_UI_KIT` env var here.
    let ui_kit_lib = env::var("DEP_CITRATE_UI_KIT_UI_KIT")
        .expect("DEP_CITRATE_UI_KIT_UI_KIT not set — is citrate-ui-kit a direct dependency with a links field?");
    eprintln!("citrate-native build.rs: ui-kit lib at {}", ui_kit_lib);

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(
            std::iter::once(("citrate-ui-kit".to_string(), PathBuf::from(ui_kit_lib))).collect()
        );

    if let Err(err) = slint_build::compile_with_config("ui/app.slint", config) {
        panic!("Slint UI compilation failed: {err}");
    }
}
