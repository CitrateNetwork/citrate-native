# citrate-ui-kit

Shared Slint UI primitives used by `citrate-native` and `citrate-defense_prime-shell`.

Holds:
- `ui/theme.slint` — color tokens, typography, spacing
- `ui/chat/chat.slint` — `ChatView` component + `ChatMessageData` struct
- `ui/shared/feedback.slint` — `PrivacyIndicator` and related shared widgets
- `assets/fonts/` — SpaceGrotesk, Geist, GeistMono, Cormorant variable TTFs
- `assets/images/` — shared brand assets

## How consumer crates use this

Two patterns, both supported:

### 1. Rust types (`use citrate_ui_kit::*`)

```rust
use citrate_ui_kit::{ChatView, ChatMessageData};
let chat = ChatView::new()?;
chat.set_messages(slint::ModelRc::new(slint::VecModel::from(vec![...])));
```

### 2. Slint library imports (`@citrate-ui-kit/...`)

In consumer `build.rs`:

```rust
use std::path::PathBuf;

fn main() {
    let ui_kit_path = std::env::var("DEP_CITRATE_UI_KIT_UI_PATH")
        .expect("citrate-ui-kit must be a direct dependency");

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths([
            ("citrate-ui-kit".to_string(), PathBuf::from(ui_kit_path)),
        ].into_iter().collect());

    slint_build::compile_with_config("ui/app.slint", config)
        .expect("Slint compile failed");
}
```

In consumer `ui/app.slint`:

```slint
import { Theme } from "@citrate-ui-kit/theme.slint";
import { ChatView, ChatMessageData } from "@citrate-ui-kit/chat/chat.slint";

export component App inherits Window {
    background: Theme.bg-primary;
    ChatView { /* ... */ }
}
```

## Why this crate exists

The Citrate monorepo split in May 2026 separated `citrate-native` and `citrate-defense_prime-shell` into sibling repos. Before the split they shared UI files via path imports (`../citrate_native/ui/chat/chat.slint`). Cargo's git-dep fetch only ships Rust source, not arbitrary `.slint`/asset files — so the post-split workaround was for `defense_prime-shell` to **vendor** the gui-native UI tree. That worked but rotted: any change to chat/theme/shared/feedback widgets had to be hand-synced.

This crate solves it by making the shared UI a real Rust crate. Its `.slint` files travel through cargo's git fetch as part of the crate package (`include = [...]` in Cargo.toml).

See `POST_SPLIT_PUNCH_LIST.md` PSL-04 in the monorepo archive for the full backstory.

## License

[MIT](../../LICENSE).
