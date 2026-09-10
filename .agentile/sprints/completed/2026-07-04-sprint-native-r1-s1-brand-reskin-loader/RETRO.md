---
created: 2026-07-09T00:00:00Z
branch: main
author: Claude Fable 5, directed by Larry Klosowski (@SaulBuilds)
sprint: NATIVE-R1-S1
status: complete
---

# Sprint NATIVE-R1-S1 — Retrospective

## Outcome

| Field | Value |
|-------|-------|
| **Goal achieved?** | YES |
| **WPs planned / closed** | 6 / 6 (WP-6 was added mid-sprint from the owner survey) |
| **Carry-forward WPs** | none (S1 scope); survey findings became the S2 backlog by design |
| **Closing branch** | `feat/native-r1-s1-brand-reskin` merged via PR #15; CI-green on `main` at `dea5548` (run 29039872660, 2026-07-09) |

## Metrics delta

| Axis | Start | End | Δ |
|------|-------|-----|----|
| Tests | 567 | 577 | +10 (loader morph-math units + .slint compile gate) |
| Snapshots | partial baseline | 36 dual-mode panel snapshots (97 artifacts) | new brand-regression gate |
| Formal specs | 0 | 0 | UI sprint; none applied (S2's method) |
| CI tripwires | 2 | 2 | unchanged |

The 577 figure included 3 failing marketplace address-drift tests — known,
flagged in DAILY.md at close of WP-5, and deliberately routed to S2 (WP-A4)
rather than patched off-scope. They were fixed there.

## What worked

- **Token-swap-first re-skin.** Confining WP-1 to `theme.slint` values +
  a light mode (no layout restructuring) made the biggest visual change of
  the sprint the least risky one. The grep-sweep adversarial check
  (`d4a76a|e8a87c` → 0 hits) is a two-second proof the old brand is gone.
- **Porting the loader math, not reimagining it.** The morph engine is a
  faithful Rust port of the canonical TSX (resample→polar→smootherstep),
  unit-tested at the math layer where headless CI can actually see it.
  One shared driver feeds both loader instances; the timer idles when
  neither is active.
- **The owner survey as sprint output.** The 2026-07-04 walkthrough
  (SURVEY-2026-07-04.md) turned "re-skin done" into a 24-item verdict list
  that became WP-6 (three quick fixes, landed in-sprint) and the entire
  S2 planset. The survey is the highest-leverage artifact this sprint
  produced.

## What didn't work

- **The CI-green acceptance criterion sat unsatisfiable for five days.**
  WP-5's "CI green on the branch" AC was written assuming CI existed. The
  org-wide Actions outage (billing lockout from artifact-storage overage,
  ~2026-06-22 → 2026-07-09) meant PR #15 merged with zero CI validation
  and S1 could not close. Cost: this retro is five days late, and a compile
  error in a test target (from the S2-track PR that followed) sat invisible
  on main the whole time.
- **Snapshot baseline has no stored goldens.** WP-5's harness renders and
  saves PNGs to a gitignored `target/` dir, diffed by eyeball/CI artifact
  review. That is a smoke-render gate, not a true regression diff — churn
  risk was the stated reason, but it means a brand regression that renders
  *something* still passes. Honest label: the "brand-regression gate" is
  half-built.
- **WP-4's subjective parity gate was never formally recorded.** The AC
  said "owner spot-check recorded in DAILY.md." The owner *did* eyeball the
  loader — the survey's loader-clipping finding proves it — but the formal
  recording never happened. The survey serves as the de-facto record;
  noting the gap here rather than back-filling a checkbox.

## What surprised us

- **The loader's ring geometry doesn't fit the mark's square.** The morph
  ring overflows the 100-unit viewBox (radius 52 + stroke 18), so any host
  smaller than ~1.45× the loader size clips it. Both integrations hit this
  and WP-6 fixed both. A component-level minimum-host contract in the
  loader header now documents it — a constraint the web original never
  surfaced because CSS overflow is visible by default.
- **`sprint.sh` anchors to its own repo root.** The sprint dir was created
  in the agentile repo and had to be relocated. Upstream fix still owed.
- **Silence read as green.** Nobody noticed CI was *absent* rather than
  *passing* for over two weeks. Every local signal (tests, builds) was
  green, and the missing external verifier was invisible until someone
  went looking for the run link this sprint's own AC required. That AC —
  bureaucratic as it felt — is what surfaced the outage.

## Carry-forward

| Item | Where it goes | Why deferred |
|------|---------------|--------------|
| Stored-golden snapshot diffing (true brand gate) | S2.1 / harness backlog | needs per-OS baseline strategy; smoke-render gate is in place |
| `sprint.sh` root-anchoring fix | agentile repo | out of this repo's scope |
| 3 marketplace address-drift test failures | S2 WP-A4 | root cause was canonical-book drift, an S2 QA concern — **closed there** |

## Decisions ratified mid-sprint

- Full liquid-morph loader in S1 (not the rotate-static fallback) — brief §10, owner-ratified 2026-07-04.
- Sidebar carries the theme-aware marquee wordmark; triangle mark only on the collapsed rail (WP-6, from survey).
- Space Grotesk on heading-level text + stat displays only; body stays Geist (WP-6, from survey).

## Action items for next sprint

- [x] Route survey findings into a spec-first S2 planset (done — S2 planset v2, 2026-07-04)
- [x] Fix the 3 address-drift failures via the canonical book (done — S2 WP-A4)
- [ ] Stored-golden snapshot diffing (owner: harness lane, S2.1)
- [ ] Upstream `sprint.sh` fix (owner: agentile repo)

## Notes

S1's close was gated on a CI run that could not exist during the org-wide
Actions outage; the full recovery story (billing, artifact purge, what
merged unverified and what it cost) is written up in the S2 sprint doc's
postmortem note and in the journal/essay/case-study set produced at the
2026-07-09 close-out. Velocity calibration: the mechanical re-skin scope
(WP-1–3) was accurately sized; the loader port (WP-4) ran long exactly at
its predicted risk point (Slint Path integration), and the survey→WP-6
loop added a day well spent.
