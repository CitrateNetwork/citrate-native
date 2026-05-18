---
created: 2026-04-23T00:45:00Z
branch: main
author: Claude Opus 4.7 (1M context)
sprint: P960-F
status: active
---

# Bundled Fonts — Attribution & License Notice

The Citrate desktop GUI bundles two font families, both licensed under
the **SIL Open Font License, Version 1.1** (OFL). OFL explicitly
permits embedding in applications, modification, and redistribution as
part of a derivative work without royalty.

## Space Grotesk (Variable)

- **File:** `SpaceGrotesk-Variable.ttf` (~137 KB, weight axis 300–700)
- **Author:** Florian Karsten
- **Upstream:** https://github.com/floriankarsten/space-grotesk
- **Copyright:** © 2020 The Space Grotesk Project Authors
- **License text:** `SpaceGrotesk-OFL.txt` in this directory

Used as the app-wide default body + display typeface (`Theme.font-body`
in `ui/theme.slint`).

## IBM Plex Mono

- **Files:**
  - `IBMPlexMono-Regular.ttf` (~156 KB)
  - `IBMPlexMono-Medium.ttf` (~157 KB)
  - `IBMPlexMono-Bold.ttf` (~158 KB)
- **Author:** IBM / Bold Monday
- **Upstream:** https://github.com/IBM/plex
- **Copyright:** © 2017 IBM Corp. with Reserved Font Name "Plex"
- **License text:** `IBMPlexMono-OFL.txt` in this directory

Used for tabular data, transaction hashes, addresses, and any surface
where digit alignment matters (`Theme.font-mono` in `ui/theme.slint`).

## Total bundled size

~607 KB across 4 font files. Well under the 800 KB budget defined in
`GUI_CLOSE_PLANSET.md` (P960-F gate).

## Why bundle rather than rely on system fonts

Prior to this sprint, the GUI used system fallbacks (`"monospace"`,
`"JetBrains Mono"`) that were not guaranteed to exist on user
machines. Rendering varied wildly between Linux distros, macOS,
and Windows. Bundling the exact TTFs means every user sees the
same glyph set, the same metrics, and the same visual weight —
the brand is actually the brand, not an approximation.

## How to update

1. Replace the TTF file in this directory
2. `cargo check -p citrate-gui-native` — Slint re-embeds on build
3. Update the file size line in this NOTICE
4. If the `OFL.txt` changed, sync that too
5. Commit all three together (TTF + NOTICE + OFL) with a
   `chore(fonts): update <family> to <version>` commit
