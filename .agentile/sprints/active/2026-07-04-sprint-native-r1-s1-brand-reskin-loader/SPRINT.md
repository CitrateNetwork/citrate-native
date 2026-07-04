---
created: 2026-07-04T17:13:27Z
branch: feat/native-r1-s1-brand-reskin
author: saulbuilds
sprint: NATIVE-R1-S1
status: active
---

# Sprint NATIVE-R1-S1: Brand re-skin + CitrateLoader port

## Sprint Metadata

| Field | Value |
|-------|-------|
| **Sprint ID** | `NATIVE-R1-S1` |
| **Sprint Name** | Brand re-skin + CitrateLoader liquid-morph port |
| **Goal** | citrate-native renders in the canonical Citrate design system (paper-light + evergreen-dark, citrate-green accent, Geist/Cormorant/Space Grotesk/Geist Mono, canonical logos, full liquid-morph loader) with a per-screen snapshot baseline guarding regressions. |
| **Branch** | `feat/native-r1-s1-brand-reskin` |
| **Start Date** | 2026-07-04 |
| **End Date (target)** | 2026-07-08 (pre-audit) |
| **Status** | `IN PROGRESS` |
| **Planset** | `handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md` (Citrate-Labs root) §3, §9 |
| **Predecessors** | AUTHSPINE S3-WP3 (8085d90), EW-S1 WP-8 (dde6c0f), i64 address sync (4e16091) |

## Why this sprint

citrate-native is the daily-user surface for the public network, but its `citrate_ui_kit` still implements the abandoned gunmetal-black + gold design language while every other surface (landing, explorer, buyer, comms-web) has converged on the canonical system: citrate-green `#8ecc09` accent, warm-paper light mode, deep-evergreen dark mode, Space Grotesk/Geist/Cormorant/Geist Mono. The federation-wide internal audit is next week; this sprint is the mechanical, pre-audit-landable slice of the NATIVE-R1 program (owner decisions ratified 2026-07-04, brief §10 — including "full liquid-morph loader in S1").

## Deliverables

- Re-skinned `gui/citrate_ui_kit/ui/theme.slint` — canonical tokens, dual light/dark modes
- Font bundle swap: Geist (body) + Geist Mono (mono) + Cormorant (serif) added; Space Grotesk retained display-only; `NOTICE-fonts.md` updated
- Canonical logo assets replacing stale `citrate-icon.png` rasters (both crates)
- `gui/citrate_ui_kit/ui/loader/citrate_loader.slint` + Rust morph engine — faithful port of `citrate-landing/src/components/loader/CitrateLoader.tsx` (9-facet triangle ⇄ liquid ring)
- Regenerated per-screen snapshot baseline in `ui_visual_tests.rs` (light + dark)

## Test Baseline (start of sprint)

| Metric | Count | Captured | Canonical command |
|--------|-------|----------|-------------------|
| **Tests** | 567 | 4e16091 | `cargo test --workspace --locked -- --list \| grep -c ': test$'` |
| **Formal specs** | 0 (UI sprint; none apply) | 2026-07-04 | n/a |
| **CI tripwires** | live_addresses tripwire + cargo-audit | 2026-07-04 | `.github/workflows/` |
| **Frontmatter coverage** | n/a (first .agentile sprint in this repo) | 2026-07-04 | — |

## Method

Per WP: BDD framing → failing/updated test → code → adversarial check → journal. TLA+ does not apply (no state machines touched; loader is pure render math). Snapshot tests are the primary harness for WP-1–4; WP-5 IS the harness step.

## Work Packages

### WP-1: theme.slint canonical token swap + dual mode

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | code + snapshot check |
| **Estimated effort** | M |
| **Commit(s)** | 7db0953 |

**Scope:** Replace the gunmetal/gold palette in `citrate_ui_kit/ui/theme.slint` with the canonical tokens (source: `citrate-landing/src/styles/colors_and_type.css` + explorer `scan.css` dual-mode split). Add a light mode (default) and evergreen dark mode behind the existing Appearance setting. Remove the "spectral" pastel palette; map semantic colors to canonical success/warning/danger/info. Does NOT restructure component layouts.

**Acceptance Criteria** *(Rule 11 — data source named)*

- [x] `Theme.accent == #8ecc09` and no `.slint` file in the workspace contains `#D4A76A`/`#E8A87C` — verified 2026-07-04: `grep -ri 'd4a76a\|e8a87c' gui/` returns 0 hits; `theme.slint:68` `accent: dark-mode ? #9ad60c : #8ecc09`
- [x] Light mode surfaces `#f1eee6/#faf8f3/#ffffff`, dark mode `#0c2216/#11301f/#163a26` with accent `#9ad60c` — verified by WP-5 dual-mode snapshots (`ui_visual_tests.rs` Part 1b, 18 panels × light+dark, all rendered)
- [x] Appearance setting toggles mode at runtime — verified by `e2e_visual` check 28 (`dark_mode_all_panels`): `Theme.dark-mode` flipped live over 10 panels, light/dark snapshot pairs differ (e.g. `28_dark_dashboard.png` vs `01_dashboard_default.png`)

**Tests added:** dark/light snapshot pairs per screen (WP-5 enumerates)

---

### WP-2: Font bundle alignment

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | code |
| **Estimated effort** | S |
| **Commit(s)** | 7db0953 |

**Scope:** Bundle Geist + Geist Mono + Cormorant TTFs (OFL); keep Space Grotesk for display only; retire IBM Plex Mono; update `theme.slint` font tokens (`font-body: Geist`, `font-serif: Cormorant`, `font-mono: Geist Mono`, `font-display: Space Grotesk`) and `NOTICE-fonts.md`.

**Acceptance Criteria**

- [x] `theme.slint` imports the four canonical families and `app.slint` default-font-family is Geist — verified 2026-07-04: `theme.slint:16-19` imports SpaceGrotesk/Geist/GeistMono/Cormorant TTFs, `app.slint:45` `default-font-family: Theme.font-body` (= "Geist"); `cargo build --workspace` green
- [x] OFL license texts present for all bundled families — verified: `Cormorant-OFL.txt`, `Geist-OFL.txt` (covers Geist Mono, same license file family), `SpaceGrotesk-OFL.txt` in `gui/citrate_ui_kit/assets/fonts/`
- [x] No remaining IBM Plex references — verified 2026-07-04: `grep -ri 'plex' gui/` returns 0 hits

---

### WP-3: Logo asset refresh

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | code |
| **Estimated effort** | S |
| **Commit(s)** | 7db0953 |

**Scope:** Replace stale `citrate-icon.png` in `citrate_native/assets/images/` and `citrate_ui_kit/assets/images/` with the canonical raster (repo `branding/icons/icon-512.png`, already byte-identical to federation branding). Verify sidebar (`sidebar.slint:99`) and any other references render the canonical mark.

**Acceptance Criteria**

- [x] `md5sum` of both in-app icon files matches `branding/icons/icon-512.png` — verified 2026-07-04: all three = `f7baf731d95c33208959d4e28e89b7fc` (recorded in DAILY.md)
- [x] Sidebar snapshot shows the canonical mark — verified in WP-5 baseline (`dashboard-light/dark-1200x800.png` render the sidebar mark on every panel snapshot)

---

### WP-4: CitrateLoader full liquid-morph port

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | failing test → code → adversarial check |
| **Estimated effort** | L |
| **Commit(s)** | e40f5cd (engine + component); app-side snapshot criteria delivered in WP-5 |

**Scope:** Faithful Rust port of `citrate-landing/src/components/loader/CitrateLoader.tsx`: 9 facet paths resampled to N=160 points, polar interpolation about the centroid, smootherstep ease, `buildArc` tapered liquid strokes, peel-out-top-first/reassemble-reverse choreography with holds; driven by a Slint `Path`/timer at ~60fps; props for size/color/speed; honors reduced-motion (static mark). Replaces the pulsing-dot `LoadingButton` indicator usage where a full-screen/section loader is appropriate. Does NOT redesign the choreography.

**Acceptance Criteria**

- [x] Unit tests on the morph math (resample point count, ease boundary values 0/1, arc geometry invariants) — `cargo test -p citrate-ui-kit`: 9/9 green (8 loader unit tests + 1 .slint compile gate)
- [x] Loader renders in a snapshot at t=0 (assembled triangle) matching the canonical mark geometry — `onboarding_bootstrap_loader_t0-{light,dark}-1200x800.png` (`ui_visual_tests.rs` Part 1b, delivered in WP-5)
- [x] Loader color binds to `Theme.accent` — verified by the same snapshot pair: mark renders `#8ecc09` on paper light / `#9ad60c` on evergreen dark
- [ ] Visual parity spot-check vs web loader recorded by owner in DAILY.md (subjective gate, named as such) — OPEN: awaits owner eyeball; animation is Rust-driven so headless snapshots only prove t=0

---

### WP-5: Per-screen snapshot baseline (brand-regression gate) + loader integration

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | harness |
| **Estimated effort** | M |
| **Commit(s)** | 6864d0c |

**Delivered scope (beyond the harness):** CitrateLoader exported from the
ui-kit root (`lib.slint`) and embedded in the app — onboarding step-6 node
bootstrap (110px, `Theme.accent`, animates while bootstrapping) and the chat
thinking indicator (30px, replaces the pulsing dot). One shared Rust driver
(`citrate_ui_kit::loader::start_loader`) feeds both instances via the
`loader-facets` app property; the timer idles whenever neither state is
active (`loader_set_running` at the 6 toggle sites in `main.rs`).

**Scope:** Extend `ui_visual_tests.rs`/`e2e_visual.rs` so every panel in the 24-item inventory (brief §2) has at least one snapshot in light AND dark mode post-re-skin; regenerate baselines; snapshot diff becomes the CI brand gate.

**Acceptance Criteria**

- [x] Snapshot count ≥ 2× panel count (light+dark), enumerated in snapshot names — `cargo test -p citrate-native ui_visual` renders 36 dual-mode snapshots (18 panels/states × light+dark: 10 main panels + chat-thinking-loader + 4 CMO configs + onboarding welcome + onboarding bootstrap/loader-t0 + lock screen) at 1200×800; 97 total artifacts in `target/gui-snapshots/`. Names enumerate panel×mode (single-process Slint constraint keeps them inside the one `ui_visual_proof_suite` test fn — the pre-existing pattern). Harness smoke-renders + saves PNG artifacts; no stored goldens (artifacts land in gitignored `target/`, diffed by eyeball/CI artifact review).
- [ ] CI green on the branch with new baselines — GitHub Actions run link in DAILY.md (OPEN: branch not yet pushed at close of WP-5 work session; local `cargo test` green modulo 3 pre-existing marketplace address-drift failures, flagged as S2 QA item in DAILY.md)

---

### WP-6: Owner-survey quick follow-ups (loader clearance + marquee + Space Grotesk)

| Field | Value |
|-------|-------|
| **Status** | `[x] COMPLETE` |
| **Order step** | polish (post-survey, pre-S2) |
| **Estimated effort** | S |
| **Commit(s)** | 5c9feb5 |

**Scope:** The three "Quick S1 follow-ups" from SURVEY-2026-07-04 (owner QA
walkthrough): chat loader clipped by its container, sidebar should carry the
marquee wordmark not the icon mark, and Space Grotesk missing from all
headings (re-skin set body=Geist; nothing applied `Theme.font-display`).

**Acceptance Criteria**

- [x] Chat + onboarding loaders render the full morph ring with no clipping —
  the ring overflows the mark square (radius 52 + thickness 18 on the
  100-unit viewBox ⇒ host must be ≥ ~1.45× the loader size). Chat thinking
  indicator resized 30px → 24px inside its existing 40px host (1.67×);
  onboarding bootstrap loader 110px → 100px inside its 150px host (1.5×).
  No `clip: true` between either loader and its margin host (chat's
  Flickable clips only at the scroll-area boundary, outside the host).
  Evidence: geometry in `citrate_loader.slint` header + `MorphConfig`
  defaults; e2e/ui_visual smoke-renders green post-change.
- [x] Sidebar shows the theme-aware marquee wordmark — canonical
  `citrate_marquee_black.svg` (light mode, black ink on warm paper) /
  `citrate_marquee_white.svg` (dark mode, white on deep evergreen) copied
  from `branding/` into `gui/citrate_native/assets/images/` and rendered at
  141×44 (native ~3.2:1 aspect preserved) in the 52px logo band; SVG decodes
  via resvg, already in the pinned Slint 1.16.1 dep tree. Triangle icon mark
  retained for the collapsed/narrow rail (`logo-expanded` follows
  compact/hover state). Evidence: `tests/screenshots/01_dashboard_default.png`
  shows the black marquee lockup.
- [x] Space Grotesk applied to heading-level text, tastefully — 40 sites got
  `font-family: Theme.font-display`: every screen's top-level title
  (Dashboard, Files, DAG Explorer, Compute, Learning Center, Agent Center,
  Models, Settings, Chat, Tenancy, Compliance Matrix, placeholder-tab title),
  shared settings `SectionHeader`, dialog titles (send/create/import/export
  wallet flows, create-learning-pool), all 9 onboarding step titles, lock
  screen, panel headers (Transaction, Block #, Education, Recent Blocks),
  and the large stat/balance display values (StatCard, CmoStatCard,
  operations StatCard, wallet hero balance, session earnings). Body copy,
  mono eyebrows, buttons, and chat message text untouched. Evidence:
  snapshots show SG on titles + stat values; `cargo build --workspace` and
  both visual suites green.

---

## Dependencies

| Dependency | Status | Impact if blocked |
|------------|--------|-------------------|
| Canonical tokens (landing `colors_and_type.css`) | Available | WP-1 |
| Geist/Geist Mono TTFs (not vendored anywhere in federation; Cormorant + Space Grotesk TTFs exist in `citrate-explorer/public/fonts/`) | Needs acquisition (OFL download) | WP-2 |
| Slint `Path` dynamic-commands support at pinned Slint version | Believed available — verify early | WP-4 (fallback: rotate-static-mark interim, but owner ratified full morph in S1) |

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Slint Path perf at 60fps on low-end machines | Med | Med | Cap point count; pre-compute frames per cycle; reduced-motion fallback |
| Light-mode introduction breaks hardcoded `#000/#fff` in chat.slint | High | Low | grep sweep for hardcoded colors is part of WP-1 adversarial check |
| Snapshot churn makes CI flaky cross-platform | Med | Med | Software-renderer snapshots only (already the pattern); per-OS baselines if needed |

## Notes

- Owner decisions ratified 2026-07-04 (brief §10): loader full morph in S1; local-first defaults; gateway.citrate.ai canonical; emission 0.00001 SALT/2s; desktop validators w/ min stake → 3,200; EduRole full replacement w/ education entitlement subtree.
- Sprint dir was initially created by sprint.sh in the agentile repo (script anchors to its own root); relocated here — worth a sprint.sh fix upstream.
