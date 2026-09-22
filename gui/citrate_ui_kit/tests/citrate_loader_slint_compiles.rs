//! Compile-gate for `ui/loader/citrate_loader.slint`.
//!
//! The loader component is intentionally not (yet) exported from
//! `ui/lib.slint` — that file is owned by the concurrent brand re-skin work
//! — so the crate's build.rs never sees it. This test runs the Slint
//! compiler over it directly so a syntax or type error in the component
//! fails `cargo test -p citrate-ui-kit` instead of surfacing later in the
//! consumer app's build.

#[test]
fn citrate_loader_slint_compiles() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let slint_file = manifest_dir
        .join("ui")
        .join("loader")
        .join("citrate_loader.slint");
    assert!(slint_file.is_file(), "missing {}", slint_file.display());

    // slint-build is written for build scripts and reads OUT_DIR from the
    // environment; point it at a scratch dir.
    let out_dir = std::env::temp_dir().join("citrate-ui-kit-loader-slint-compile-test");
    std::fs::create_dir_all(&out_dir).expect("create scratch OUT_DIR");
    std::env::set_var("OUT_DIR", &out_dir);

    slint_build::compile(&slint_file)
        .unwrap_or_else(|e| panic!("citrate_loader.slint failed to compile: {e}"));
}
