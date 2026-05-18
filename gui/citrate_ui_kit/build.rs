use std::env;
use std::path::PathBuf;

fn main() {
    // Compile our own components so consumers that just want the Rust-side
    // types can `use citrate_ui_kit::ChatView;` etc.
    slint_build::compile("ui/lib.slint").expect("citrate-ui-kit Slint compile failed");

    // Publish the path to `lib.slint` so consumers' build.rs can register
    // it as the `@citrate-ui-kit` library import. Slint's
    // with_library_paths takes a map from library name -> path to a single
    // .slint file (or directory). We use a single file so consumers do
    // `import { Theme, ChatView } from "@citrate-ui-kit"` and get whatever
    // lib.slint re-exports.
    //
    // Cargo turns `cargo:UI_KIT=...` into the `DEP_CITRATE_UI_KIT_UI_KIT`
    // env var visible to dependents' build.rs (requires `links = ...`).
    let lib_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("ui").join("lib.slint");
    println!("cargo:UI_KIT={}", lib_path.display());
    println!("cargo:rerun-if-changed=ui");
}
