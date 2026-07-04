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
| **Status** | `[~] IN PROGRESS` |
| **Order step** | code + snapshot check |
| **Estimated effort** | M |
| **Commit(s)** | — |

**Scope:** Replace the gunmetal/gold palette in `citrate_ui_kit/ui/theme.slint` with the canonical tokens (source: `citrate-landing/src/styles/colors_and_type.css` + explorer `scan.css` dual-mode split). Add a light mode (default) and evergreen dark mode behind the existing Appearance setting. Remove the "spectral" pastel palette; map semantic colors to canonical success/warning/danger/info. Does NOT restructure component layouts.

**Acceptance Criteria** *(Rule 11 — data source named)*

- [ ] `Theme.accent == #8ecc09` and no `.slint` file in the workspace contains `#D4A76A`/`#E8A87C` — verified by `grep -ri 'd4a76a\|e8a87c' gui/` returning empty
- [ ] Light mode surfaces `#f1eee6/#faf8f3/#ffffff`, dark mode `#0c2216/#11301f/#163a26` with accent `#9ad60c` — verified by snapshot tests `ui_visual_tests.rs` in both modes
- [ ] Appearance setting toggles mode at runtime — verified by `e2e_visual` snapshot pair diff

**Tests added:** dark/light snapshot pairs per screen (WP-5 enumerates)

---

### WP-2: Font bundle alignment

| Field | Value |
|-------|-------|
| **Status** | `[ ] NOT STARTED` |
| **Order step** | code |
| **Estimated effort** | S |
| **Commit(s)** | — |

**Scope:** Bundle Geist + Geist Mono + Cormorant TTFs (OFL); keep Space Grotesk for display only; retire IBM Plex Mono; update `theme.slint` font tokens (`font-body: Geist`, `font-serif: Cormorant`, `font-mono: Geist Mono`, `font-display: Space Grotesk`) and `NOTICE-fonts.md`.

**Acceptance Criteria**

- [ ] `theme.slint` imports the four canonical families and `app.slint` default-font-family is Geist — verified by grep + successful `cargo build`
- [ ] OFL license texts present for all bundled families — verified by `ls assets/fonts/`
- [ ] No remaining IBM Plex references — `grep -ri 'plex' gui/` empty

---

### WP-3: Logo asset refresh

| Field | Value |
|-------|-------|
| **Status** | `[ ] NOT STARTED` |
| **Order step** | code |
| **Estimated effort** | S |
| **Commit(s)** | — |

**Scope:** Replace stale `citrate-icon.png` in `citrate_native/assets/images/` and `citrate_ui_kit/assets/images/` with the canonical raster (repo `branding/icons/icon-512.png`, already byte-identical to federation branding). Verify sidebar (`sidebar.slint:99`) and any other references render the canonical mark.

**Acceptance Criteria**

- [ ] `md5sum` of both in-app icon files matches `branding/icons/icon-512.png` — verified by shell check recorded in DAILY.md
- [ ] Sidebar snapshot shows the canonical mark — `ui_visual_tests.rs`

---

### WP-4: CitrateLoader full liquid-morph port

| Field | Value |
|-------|-------|
| **Status** | `[ ] NOT STARTED` |
| **Order step** | failing test → code → adversarial check |
| **Estimated effort** | L |
| **Commit(s)** | — |

**Scope:** Faithful Rust port of `citrate-landing/src/components/loader/CitrateLoader.tsx`: 9 facet paths resampled to N=160 points, polar interpolation about the centroid, smootherstep ease, `buildArc` tapered liquid strokes, peel-out-top-first/reassemble-reverse choreography with holds; driven by a Slint `Path`/timer at ~60fps; props for size/color/speed; honors reduced-motion (static mark). Replaces the pulsing-dot `LoadingButton` indicator usage where a full-screen/section loader is appropriate. Does NOT redesign the choreography.

**Acceptance Criteria**

- [ ] Unit tests on the morph math (resample point count, ease boundary values 0/1, arc geometry invariants) — `citrate_ui_kit` `cargo test`
- [ ] Loader renders in a snapshot at t=0 (assembled triangle) matching the canonical mark geometry — `ui_visual_tests.rs`
- [ ] Loader color binds to `Theme.accent` — verified by snapshot in both modes
- [ ] Visual parity spot-check vs web loader recorded by owner in DAILY.md (subjective gate, named as such)

---

### WP-5: Per-screen snapshot baseline (brand-regression gate)

| Field | Value |
|-------|-------|
| **Status** | `[ ] NOT STARTED` |
| **Order step** | harness |
| **Estimated effort** | M |
| **Commit(s)** | — |

**Scope:** Extend `ui_visual_tests.rs`/`e2e_visual.rs` so every panel in the 24-item inventory (brief §2) has at least one snapshot in light AND dark mode post-re-skin; regenerate baselines; snapshot diff becomes the CI brand gate.

**Acceptance Criteria**

- [ ] Snapshot count ≥ 2× panel count (light+dark), enumerated in test names — `cargo test -p citrate-native ui_visual`
- [ ] CI green on the branch with new baselines — GitHub Actions run link in DAILY.md

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
