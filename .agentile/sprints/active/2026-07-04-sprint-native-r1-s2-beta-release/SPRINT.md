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
| A1 | Node-start crash telemetry (telemetry half; soak rides C3, in-app banner deferred to B-track) | `[x] COMPLETE` | 2315065 |
| A2 | boot2 eof-after-handshake — **CLOSED 2026-07-09**: root cause was the A6 stale-genesis pin. Live smoke (`tests/live_bootnode_smoke.rs`, gated) connected 4/4 bootnodes incl. boot2 and held 600s with zero drops; locally computed canonical genesis byte-matches live 40204 (`0x6b6d…3e2f`). | `[x] COMPLETE` | (smoke test commit) |
| A3 | Encryption-at-rest — **OWNER DECISION 2026-07-04: BETA BLOCKS on real encryption** (fallback rejected). **LANDED via PR #17 (ENCRYPT-S1 WP-1/4/5/9a + chain PR #62); VERIFIED 2026-07-09**: `encrypted_node_rocksdb_values_are_ciphertext_on_disk` (QSSP envelope on raw RocksDB bytes, plaintext marker not findable), keyring-key stability across restart, and plaintext→encrypted mismatch wipe-and-resync all green; encryption defaults ON. | `[x] COMPLETE` | PR #17 |
| A4 | ComputeMarketplace address drift + tripwire (book was canonical; 3 literals drifted incl. 2 newly found; chain DEPLOYED_ADDRESSES.md systematically stale — owner follow-up in chain repo) | `[x] COMPLETE` | 255bad1 |
| A5 | Session-timeout unification (3600s single source) | `[x] COMPLETE` | 2315065 |
| A6 | citrate-chain pin bump 0f2d16b→ca40429 + resolve_bootnode dedup. **FINDING: old pin computed stale genesis vs live 40204; new pin verified byte-identical (0x6b6d…3e2f) vs eth_getBlockByNumber(0x0)** — likely contributor to desktop sync symptoms/A2. | `[x] COMPLETE` | 47f8844 |
| B1–B6 | Experience (Gherkin-first) — **OWNER DESCOPE 2026-07-09: moved to S2.1** (per the planset's honest-sizing slip mechanism; specs to be written first there) | `[>] MOVED → S2.1` | — |
| C1–C2 | Harness lanes (OIDC e2e, chat tool-loop e2e) — ride to S2.1 with Track B | `[>] MOVED → S2.1` | — |
| C3 | Live smoke lane — seeded: `tests/live_bootnode_smoke.rs` (gated `--ignored`; A2's verification vehicle). Scheduled-job wiring pending. | `[~] SEEDED` | — |
| C4 | Blocking fmt/clippy | `[ ] IN SCOPE (land last)` | — |
| D1–D4 | Release checklist — **the remaining beta gate** | `[ ] NOT STARTED` | — |

## Test Baseline (start of sprint)

| Metric | Count | Captured | Canonical command |
|--------|-------|----------|-------------------|
| **Tests** | 577 (3 failing: marketplace_client address drift — WP-A4 target) | 1150944 | `cargo test --workspace --locked -- --list \| grep -c ': test$'` |
| **Formal specs** | 0 → Track B adds `.agentile/specs/*.feature` | 2026-07-04 | `ls .agentile/specs/` |
| **CI tripwires** | live_addresses + cargo-audit (+A4 adds no-literal-address) | 2026-07-04 | `.github/workflows/` |

## Notes

- S1 not yet closed: its CI-green-link AC awaits a non-transient Actions run; close S1 (RETRO) when green.
- A3 decision gate: primary path is enabling real encryption; fallback is owner-signed honest messaging (planset A3) — do not let a screen claim encryption that isn't real. **Resolved on the primary path — real encryption landed and verified (see A3 row).**
- **2026-07-09 CI outage postmortem (context for the gap in this sprint's CI evidence):** GitHub Actions was disabled org-wide from ~2026-06-22 to 2026-07-09 (artifact-storage overage tripped billing; every run `startup_failure`). PRs #15/#17 merged unvalidated during the outage. Recovery: billing fixed by owner; 357 stale artifacts (55.9 GB) purged org-wide; first post-outage CI run found 2 stale AA vector pins (fixed in c64a731, recaptured from live chain) and 4 accumulated RustSec advisories (2 fixed via lockfile bump, 2 quick-xml ignores with dated justification — real fix tracked in citrate-learning-center#3).
- **2026-07-09 owner descope (beta ships from Track A + D):** Track B and C1/C2 move to S2.1 spec-first; SFL-01 (school-pilot slim/llama work in the working tree) is out of beta scope and handled separately — beta commits must not include those files.
