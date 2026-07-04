---
created: 2026-07-04T20:35:00Z
branch: feat/native-r1-s2-track-a
author: saulbuilds
sprint: NATIVE-R1-S2
status: active
---

# Sprint NATIVE-R1-S2: Beta release

## Sprint Metadata

| Field | Value |
|-------|-------|
| **Sprint ID** | `NATIVE-R1-S2` |
| **Sprint Name** | Beta release — blockers, spec-first experience, harness |
| **Goal** | A demoable public beta of citrate-native this week: a stranger completes the 10-minute demo (planset WP-D4) without hitting a bug, every critical feature Gherkin-specced before code, beta blockers closed. |
| **Branch** | `feat/native-r1-s2-track-a` (Track A; Track B branches per-WP off it) |
| **Start Date** | 2026-07-04 |
| **End Date (target)** | 2026-07-10 |
| **Status** | `IN PROGRESS` |
| **Planset** | `.agentile/planset/2026-07-04-native-r1-s2-qa-harness.md` (authoritative WP definitions A1–A6, B1–B6, C1–C4, D1–D4) |
| **Predecessors** | NATIVE-R1-S1 (9844b4e…1150944, WP-1–6 complete; CI-link AC pending on transient startup_failure re-run) |

## Why this sprint

Owner direction from the 2026-07-04 QA survey (SURVEY-2026-07-04.md in the S1 sprint dir): "align everything for a beta release … this is an easy release that will get us to a higher user threshold fast and allow us to grow the brand this week," with every critical feature "buttoned down … in gherkins in the agentile methodology." S1 made the surface on-brand; S2 makes it trustworthy and demoable.

## Work packages

WP definitions, acceptance criteria (Rule 11), and the survey→WP traceability table live in the planset — not duplicated here. Execution status:

| WP | Name | Status | Commit(s) |
|----|------|--------|-----------|
| A1 | Node-start crash telemetry + soak | `[~] IN PROGRESS` | — |
| A2 | boot2 eof-after-handshake | `[ ] BLOCKED (after A6)` | — |
| A3 | Encryption-at-rest | `[~] IN PROGRESS (scoping)` | — |
| A4 | ComputeMarketplace address drift + tripwire | `[~] IN PROGRESS` | — |
| A5 | Session-timeout unification | `[~] IN PROGRESS` | — |
| A6 | citrate-network pin bump + resolve_bootnode dedup | `[~] IN PROGRESS` | — |
| B1–B6 | Experience (Gherkin-first) | `[ ] NOT STARTED` | — |
| C1–C4 | Harness lanes | `[ ] NOT STARTED` | — |
| D1–D4 | Release checklist | `[ ] NOT STARTED` | — |

## Test Baseline (start of sprint)

| Metric | Count | Captured | Canonical command |
|--------|-------|----------|-------------------|
| **Tests** | 577 (3 failing: marketplace_client address drift — WP-A4 target) | 1150944 | `cargo test --workspace --locked -- --list \| grep -c ': test$'` |
| **Formal specs** | 0 → Track B adds `.agentile/specs/*.feature` | 2026-07-04 | `ls .agentile/specs/` |
| **CI tripwires** | live_addresses + cargo-audit (+A4 adds no-literal-address) | 2026-07-04 | `.github/workflows/` |

## Notes

- S1 not yet closed: its CI-green-link AC awaits a non-transient Actions run; close S1 (RETRO) when green.
- A3 decision gate: primary path is enabling real encryption; fallback is owner-signed honest messaging (planset A3) — do not let a screen claim encryption that isn't real.
