//! citrate-ui-kit — shared Slint UI primitives for the Citrate Network.
//!
//! Two ways to consume this crate:
//!
//! 1. **As Rust types** — `use citrate_ui_kit::{ChatView, ChatMessageData};` —
//!    works because [`slint::include_modules`] re-exports every `export
//!    component` and `export struct` from `ui/lib.slint`.
//!
//! 2. **As Slint library imports** — consumer's `build.rs` calls
//!    [`slint_build::compile_with_config`] with `with_library_paths`
//!    including `("citrate-ui-kit", DEP_CITRATE_UI_KIT_UI_PATH)`. Then
//!    consumer's `app.slint` can do `import { ChatView } from
//!    "@citrate-ui-kit/chat/chat.slint";`.
//!
//! `citrate-native` uses pattern (2). `citrate-defense_prime-shell` uses
//! pattern (2). The two patterns can coexist within one consumer crate.

slint::include_modules!();

pub mod loader;
