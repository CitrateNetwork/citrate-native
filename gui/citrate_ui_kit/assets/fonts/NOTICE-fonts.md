---
created: 2026-04-23T00:45:00Z
branch: feat/native-r1-s1-brand-reskin
author: Claude Fable 5
sprint: NATIVE-R1-S1
status: active
---

# Bundled Fonts — Attribution & License Notice

The Citrate desktop GUI bundles four font families, all licensed under
the **SIL Open Font License, Version 1.1** (OFL). OFL explicitly
permits embedding in applications, modification, and redistribution as
part of a derivative work without royalty.

## Space Grotesk (Variable)

- **File:** `SpaceGrotesk-Variable.ttf` (~137 KB, weight axis 300–700)
- **Author:** Florian Karsten
- **Upstream:** https://github.com/floriankarsten/space-grotesk
- **Copyright:** © 2020 The Space Grotesk Project Authors
- **License text:** `SpaceGrotesk-OFL.txt` in this directory

Display typeface — hero and page headings (`Theme.font-display` in
`ui/theme.slint`).

## Geist (Variable)

- **File:** `Geist-Variable.ttf` (~169 KB, weight axis 100–900)
- **Author:** Vercel
- **Upstream:** https://github.com/vercel/geist-font
- **Copyright:** © 2024 The Geist Project Authors
- **License text:** `Geist-OFL.txt` in this directory

App-wide default body typeface (`Theme.font-body` in `ui/theme.slint`,
wired to `default-font-family` in `app.slint`).

## Geist Mono (Variable)

- **File:** `GeistMono-Variable.ttf` (~172 KB, weight axis 100–900)
- **Author:** Vercel
- **Upstream:** https://github.com/vercel/geist-font
- **Copyright:** © 2024 The Geist Project Authors
- **License text:** `Geist-OFL.txt` in this directory

Used for tabular data, transaction hashes, addresses, and any surface
where digit alignment matters (`Theme.font-mono` in `ui/theme.slint`).

## Cormorant (Variable)

- **File:** `Cormorant-Variable.ttf` (~552 KB, weight axis 300–700)
- **Author:** Christian Thalmann (Catharsis Fonts)
- **Upstream:** https://github.com/CatharsisFonts/Cormorant
- **Copyright:** © 2015 The Cormorant Project Authors
- **License text:** `Cormorant-OFL.txt` in this directory

Serif editorial voice — subheadings and long-form copy
(`Theme.font-serif` in `ui/theme.slint`).

## Retired families

- The previous IBM mono family was replaced by Geist Mono in Sprint
  NATIVE-R1-S1 (WP-2) to match the canonical federation type system.

## Total bundled size

~1.03 MB across 4 font files. Exceeds the historical 800 KB P960-F
budget because the canonical four-family system (NATIVE-R1-S1) adds
Cormorant (~552 KB); accepted in the sprint brief — the alternative
(subsetted Cormorant) is a follow-up if binary size becomes a gate.

## Why bundle rather than rely on system fonts

Prior to bundling, the GUI used system fallbacks (`"monospace"`,
`"JetBrains Mono"`) that were not guaranteed to exist on user
machines. Rendering varied wildly between Linux distros, macOS,
and Windows. Bundling the exact TTFs means every user sees the
same glyph set, the same metrics, and the same visual weight —
the brand is actually the brand, not an approximation.

## How to update

1. Replace the TTF file in this directory
2. `cargo check -p citrate-native` — Slint re-embeds on build
3. Update the file size line in this NOTICE
4. If the `OFL.txt` changed, sync that too
5. Commit all three together (TTF + NOTICE + OFL) with a
   `chore(fonts): update <family> to <version>` commit
