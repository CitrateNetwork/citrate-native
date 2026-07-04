---
created: 2026-07-04T00:00:00Z
branch: main
author: Larry Klosowski (@SaulBuilds) + Claude Fable 5
status: draft
program: NATIVE-R1 (native desktop revamp)
code: NATIVE-R1-S2 (BETA RELEASE — spec-first)
repo: citrate-gui-native (crate citrate-native)
companion: ../../../handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md (§2 inventory, §8 harness, §9 program)
survey: ../sprints/active/2026-07-04-sprint-native-r1-s1-brand-reskin-loader/SURVEY-2026-07-04.md (owner QA, 2026-07-04)
depends-on: NATIVE-R1-S1 (brand re-skin + loader + snapshot baseline; S1 quick follow-ups land on the S1 branch)
superseded-in-part-by: 2026-07-04-native-r1-s3-pin-storage.md (Models/Storage rebuild — owner RAISED S3 priority; S2 does not touch the models flow at all, not even a stub)
---

> **Status (2026-07-04): DRAFT v2 — rewritten from the owner QA survey.** The original
> S2 draft was "QA + harness"; the owner's 2026-07-04 walkthrough re-framed it:
> *"lets figure out how to align everything for a beta release … an easy release that
> will get us to a higher user threshold fast and allow us to grow the brand this week."*
> Method (owner-directed): every critical feature is buttoned down with **Gherkin specs
> in the agentile methodology BEFORE implementation** — spec files live under
> `.agentile/specs/` and are the acceptance contract for their WP.

# Planset — NATIVE-R1-S2: BETA RELEASE (survey-driven, spec-first)

## Beta-release goal + demo definition

**Goal.** A public beta a stranger can download, install, and demo for 10 minutes
without hitting a bug: onboard → unlock → node starts and connects (and if it ever
crashes, it says WHY) → wallet makes sense to a non-crypto user (send/receive-QR/faucet)
→ chat with the bundled model streams cleanly and accepts a dropped file → DAG Explorer
looks like citrate-explorer and the agent drawer explains what you're looking at →
Compute cards fit their content → Settings look intentional. The known-issues list is
honest and short; nothing in the demo path lies.

**Demo definition (the WP-D4 script proves exactly this, on camera):** fresh install,
10-step onboarding, node start + ≥2 bootnode connections + height advancing, faucet
claim visible in wallet, receive-QR scanned by a phone, one chat turn with a dropped
PDF, one DAG block opened via the agent drawer, compute opt-in toggled. Zero crashes,
zero dead buttons, zero placeholder data on screen.

## Scope (in)
Track A beta blockers (crash telemetry, boot2, encryption-at-rest, address drift,
session timeout, network pin); Track B experience WPs — **each begins with a Gherkin
spec file, reviewed by the owner, before code**; Track C harness lanes (carried from
the prior draft); Track D beta-release checklist (packaging, README, known-issues,
demo script). S1 quick follow-ups (chat-loader clipping, sidebar marquee logo, Space
Grotesk headings) land on the S1 sprint branch, not here.

## Scope (out) — honest boundaries (every survey item placed; see mapping table)
- **Models rebuild + Gemma model seeding + citrate-memories operator integration** →
  NATIVE-R1-S3 / new planset. Owner raised S3 priority: *"I'm not into hiding things,
  lets just prioritize this rebuild as it is crucial for pinning models."* Accordingly
  the old S2 "honest deploy-model stub" WP is DROPPED — no hiding, no stubbing; the
  beta known-issues list (WP-D3) names Models as under-rebuild instead.
- **Learn module rethink** → SPINE-S1. Carry INTO SPINE-S1/federated planning as
  named items: (a) **federated-gateway droplet verification** (docs/site cite a
  droplet that is probably not up — verify/stand up); (b) **always-joinable seeded
  general pool** (a joinable pool must always exist, even when inactive);
  (c) **nat + coop cohesion study** (align Learn with the `nat` repo and the co-op
  campaign work). Beta ships Learn as-is, flagged in WP-D3.
- **Gateway chat mode + MCP `tools/call`** → SPINE-S1 WP-6/WP-7 (chat stays LOCAL
  per ratified decision §10.4).
- **Dashboard "earned" re-sourcing** → VALIDATOR-S1 Phase 4 / ECON-S1 (S2 may re-LABEL
  only). CMO stubs, mempool wiring, edu rosters, `get_reward_address()` — backlog.

## Track A — Beta blockers (stability/trust; riskiest first)

Acceptance criteria name their data source per Rule 11.

| WP | Title | Acceptance (with data source) |
|---|---|---|
| **A1** | **Node-start silent crash: telemetry + supervised repro** — first survey session died silently right after "Node started via settings" (no panic output, no coredump/OOM record); did not reproduce in session 2 | (1) `std::panic::set_hook` + thread-panic propagation installed so ANY node-thread death writes a crash record (backtrace, last log lines, build hash) to a `crash/` dir AND surfaces an in-app banner — a deliberately-injected test panic proves the full path; (2) supervised soak: ≥20 scripted start/stop cycles + 1 long-run under the harness, either reproducing (→ root cause fix in-sprint) or recording the negative. **Source:** crash-record files + `node_service` logs from the soak runs; injected-panic integration test. |
| **A2** | **boot2 (143.198.134.151) `Transport error: eof` after Noise handshake** — reproduced in both survey sessions; do A6 first to rule out protocol drift | Live smoke connects **4/4** bootnodes and holds ≥10 min, OR written root cause + chain-repo issue filed. **Source:** embedded-node peer logs (`node_service`) + boot2 droplet logs; time-boxed 2 days to the root-cause exit. |
| **A3** | **RocksDB encryption-at-rest is FALSE** — startup logs `Storage manager initialized (encryption: false)`; owner: the guarantee's absence *"would break our offering"*; Storage-tab copy must never claim encryption the node doesn't do | Primary path: encryption enabled — startup log reads `encryption: true`, a disk-grep test proves a known plaintext marker written pre-test is NOT findable in the RocksDB dir, and existing unencrypted stores migrate or re-sync cleanly. Fallback (only if the chain-side change can't land this week): an **owner-signed decision note** in this planset + all UI/README copy corrected to make no at-rest-encryption claim (honest-messaging), and the item tops WP-D3's known-issues list. Either way: no screen or doc claims encryption while the log says false. **Source:** node startup log line + disk-grep test artifact; decision note if fallback. |
| **A4** | **ComputeMarketplace address drift — 3 failing tests** pin `0xd7a2…d599`; canonical post-reroll address is `0xf62a…5283` | The 3 tests read `src/generated/addresses.json` (WP-Z canonical book) instead of literals; tripwire asserts no hardcoded ComputeMarketplace literal remains in test code. **Source:** `src/generated/addresses.json` (in sync with the i64 re-roll) + green `cargo test --workspace`. |
| **A5** | **Session-timeout unification** — backend 1h (`wallet_service.rs:18`) vs UI const 8h (`main.rs:107`) | One shared constant (owner picks the value at kickoff); regression test asserts UI countdown and backend expiry read the same source. **Source:** `WalletService` session state (the enforcing side). |
| **A6** | **`citrate-network` pin bump** — pin predates shared `resolve_bootnode` (`node_service.rs:388–391` duplicates it) | Workspace pin at current chain-repo main; local duplicate deleted; workspace tests green on both CI targets. **Source:** `Cargo.toml`/`Cargo.lock` chain-repo rev + `cargo test --workspace`. Rule 12: `[[drift]]` gui-native ← citrate-network. |

## Track B — Experience (Gherkin-spec-first, per owner meta-direction)

**Method for every Track B WP:** step 1 is writing the named Gherkin spec under
`.agentile/specs/`; the owner reviews/ratifies the spec; only then does implementation
start. The spec's scenarios ARE the WP's behavioral acceptance criteria; each scenario
maps to at least one automated test or an explicitly-marked manual demo-script check.

| WP | Title | Spec file (write FIRST) | Acceptance (with data source) |
|---|---|---|---|
| **B1** | **Wallet UX rethink — dual-persona design workshop** — owner: *"How do we make this relative to a housewife and a crypto person. It isn't very user friendly."* Workshop WP produces wireframes + spec BEFORE code; includes **receive QR** | `.agentile/specs/wallet-dual-persona.feature` | (1) Workshop artifact: wireframe set (simple-mode vs power-mode flows) + ratified spec, committed; (2) implemented per spec: send/receive/faucet each pass a "housewife scenario" (no crypto jargon on the primary path) and a "crypto scenario" (full detail reachable); (3) QR renders the selected account address — test asserts QR payload decodes byte-identical to the copy-button string. **Source:** ratified spec scenarios + `wallet_service` selected-account address + snapshot tests of both personas' primary screens. |
| **B2** | **Chat upgrades** — clearer stream formatting; **drag-and-drop for a standardized wide file-type list**; user AND model interact with image-gen/other loaded models; harness extended as far as possible **without breaking the security threshold** (existing approval gate + 7-write relay allow-list are the hard bound — extension adds capability UNDER them, never new bypasses) | `.agentile/specs/chat-streams-and-files.feature` | (1) Stream formatting: markdown/code/tool-call segments render distinctly — snapshot tests per segment type; (2) drag-and-drop accepts the spec's standardized type list (txt/md/pdf/csv/json/png/jpg/… enumerated in the .feature) with per-type handling + a defined rejection UX for everything else — test drops one fixture per listed type; (3) model-to-model: a chat turn can invoke a second loaded model (e.g. image-gen) and the result returns into the stream — mock-backend e2e; (4) security bound: every new tool/file path routes through the EXISTING approval gate; a negative test proves no new callback bypasses it and the relay allow-list is unchanged (diff-asserted). **Source:** spec scenarios + mock chat backend + approval-gate state machine + relay allow-list constant. |
| **B3** | **DAG Explorer redesign + contextual agent drawer** — adopt citrate-explorer's visual design ("steal from citrate-explorer"); owner: all information *"tractionable through a side drawer that brings up the chatbot/agent"* about the current finding. **Build the drawer as an app-wide reusable component** (shell-level, context-payload prop), shipped on DAG first | `.agentile/specs/dag-explorer-agent-drawer.feature` | (1) DAG screens visually match citrate-explorer's design language — side-by-side snapshot review vs citrate-explorer reference captures, owner sign-off recorded; (2) drawer: any block/tx/stat opens the drawer with the chat agent pre-seeded with that finding's context — test asserts the seeded prompt contains the selected block hash from `citrate_getDagStats`/block-detail data; (3) reusability: drawer lives in `ui/shell/` taking a generic context payload; a second-surface smoke instantiation compiles + snapshots (full non-DAG rollout is future work). **Source:** spec scenarios + citrate-explorer reference screenshots + embedded-node DAG RPC data + snapshot tests. |
| **B4** | **Compute fit + super-user settings** — cards don't fit machine info; add advanced granular config for super users | `.agentile/specs/compute-cards-advanced.feature` | (1) Hardware/provider cards fit content at min-window and default sizes — snapshot tests at both, no clipped text (survey's overflow case is a regression fixture); (2) "Advanced" disclosure exposes the spec-enumerated granular settings, persisted and honored by the compute opt-in path — test round-trips each setting to its consuming service. **Source:** spec scenarios + real hardware-probe strings (longest observed as fixtures) + compute service config state. |
| **B5** | **Settings restructure** — own settings sidebar (follow the other federation apps' settings formula); two-column rows: **Node Control &#124; System Health**, **Environment &#124; Peer Connections** (kill accidental full-width empty space); NEW **Identity & federation connections** section (identity settings + connections to the other federation apps); **deeper Knowledge Graph settings** | `.agentile/specs/settings-restructure.feature` | (1) Settings gets its own sidebar; snapshots show the two 2-col rows with no full-width single-card rows; (2) Identity section reads/links the real CITRATE IDENTITY state (tier/role/KYC via the AUTHSPINE card's data source) + lists federation-app connections with live/linked status; (3) KG section exposes the spec-enumerated deeper options (path, ingest toggles, sharing default = private) — round-trip tested. *(Full citrate-memories per-operator integration — global vs private knowledge, granular sharing — is S3/new-planset scope; B5 ships only the settings surface + safe defaults.)* **Source:** spec scenarios + identity service state + KG config store + snapshot tests. |
| **B6** | **Agent Center dummy-proof redesign** — owner: *"I don't even understand how to use it."* | `.agentile/specs/agent-center-dummy-proof.feature` | (1) Spec written from a first-run persona: what each pane is FOR, stated on-screen; empty states explain next actions; (2) a no-context user can complete the spec's "first agent session" scenario without docs — verified live by the owner, sign-off recorded in the sprint doc; (3) every visible control verified functional or removed (no unclear-if-functional controls survive). **Source:** spec scenarios + owner walkthrough sign-off + callback-handler grep proving every Agent Center callback has a live handler. |

## Track C — Harness (carried from prior draft; proves the beta continuously)

| WP | Title | Acceptance (with data source) |
|---|---|---|
| **C1** | **OIDC/AA link e2e vs mock issuer** (§8 item 2) | e2e: mock OIDC issuer → loopback PKCE callback → AA CREATE2 address derivation asserted against a fixture vector. **Source:** mock issuer's signed tokens + `link` service state; live `bundler.citrate.ai` NOT called in CI. |
| **C2** | **Chat tool-loop e2e** (§8 item 3) — also the regression bed for B2's extensions | Scripted mock backend emits tool calls (send_tx, check_balance, deploy_contract, query_chain) → approval gate blocks until approved → result returns to the loop; max-5-iteration cap asserted. **Source:** mock chat backend script + approval-gate state machine. |
| **C3** | **Live smoke lane** (§8 item 4) — gated, non-CI-blocking; doubles as A1's soak vehicle and A2's verification | Scheduled job: headless launch → ≥2 bootnodes connected → height advances → balance fetch → faucet claim; red lane pages the owner, does not block merges. **Source:** live testnet-beta via the embedded node. |
| **C4** | **Blocking fmt/clippy** (§8 item 6, gui half) — land LAST on a clean tree | `continue-on-error` removed; `cargo fmt --check` + `cargo clippy -D warnings` gate merges; green run on main. **Source:** CI workflow file + a green main run. |

## Track D — Beta-release checklist WP

| WP | Title | Acceptance (with data source) |
|---|---|---|
| **D1** | **Version + packaging** — `release.yml` + `release-tier2.yml` exist; make them produce the beta | Tagged `v0.x.0-beta.1` build succeeds on both release workflows; artifacts install + launch on a clean machine (no dev toolchain). **Source:** GitHub Actions release-run artifacts + clean-machine install log. |
| **D2** | **README + first-run UX** | README rewritten for a beta stranger (install, first run, what works); first-run in-app experience matches it; the aliased-host cargo double-fetch known issue addressed or documented. **Source:** README diff + a recorded fresh-install first-run following only the README. |
| **D3** | **Known-issues honesty list** — no hiding (owner), pre-declared for the audit | `KNOWN_ISSUES.md` shipped in-repo + linked from README/about: Models under-rebuild (S3), Learn pending rethink (SPINE-S1), encryption-at-rest status per A3's outcome, mempool 0, dashboard "earned" label, CMO stub. Every D3 line traces to a brief-§2 or survey line. **Source:** brief §2 known-bug list + SURVEY-2026-07-04 + A3 outcome. |
| **D4** | **Demo script** — the brand-growth asset | `demo/DEMO_SCRIPT.md` implementing the demo definition above; executed end-to-end on the beta build with zero bugs, recorded (the recording is the demoable asset). **Source:** the recorded run against testnet-beta + the beta artifact from D1. |

## Sequencing
A6 → A2 (rule out pin drift first). A1 telemetry lands early so every later live run
is a diagnostic sample; C3 doubles as A1's soak + A2's verification. A3 decision
(enable vs honest-messaging fallback) made at kickoff — it gates D3 copy. All Track B
spec files are written and owner-ratified in the first 1–2 days (parallel batch),
implementation follows; B1's workshop is the first session of the sprint. C1/C2
parallel-safe; C4 lands last. Track D starts once Track A is green; D4 is the final
gate — the sprint closes when the demo recording exists.

## Honest sizing
This is an aggressive 1-week target per the owner ("this week"). Track A + specs +
C-lanes + D fit the week; Track B implementation is prioritized B1 → B3 → B2 → B4/B5
→ B6, and any B-item whose spec is ratified but whose code slips moves to an immediate
S2.1 follow-on WITH its spec already done — the spec-first method is what makes the
slip cheap. A2/A3 carry explicit fallback exits so no open-ended diagnosis blocks the
release.

## References / rules
- Survey: `.agentile/sprints/active/2026-07-04-sprint-native-r1-s1-brand-reskin-loader/SURVEY-2026-07-04.md`
- Master brief: `/home/saul/Projects/Citrate-Labs/handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md` (§2, §8, §9, §10)
- `citrate-federation/.agentile/rules/CORE_RULES.md` — Rule 1 (no mocks in prod paths; D3 is the honesty instrument), Rule 11 (ACs name data sources), Rule 12 (`[[drift]]` gui-native ← citrate-network on A6).
- Gherkin specs live in `.agentile/specs/*.feature`; spec ratification is recorded in the sprint daily log.

## SURVEY → S2 mapping (audit traceability)

| Survey item | Placed at |
|---|---|
| Meta-direction: beta this week, Gherkin-spec-first | Whole planset frame; Track B method; D1–D4 |
| Node-start intermittent silent crash | A1 |
| boot2 eof after Noise handshake | A2 |
| `encryption: false` at rest ("would break our offering") | A3 (+ D3 copy) |
| Dashboard: marquee logo / Space Grotesk headings / loader clipping | S1 quick follow-ups (S1 sprint branch, per survey §"Quick S1 follow-ups") |
| Wallet: dual-persona UX rethink ("housewife and a crypto person") | B1 (workshop + spec) |
| Wallet: receive QR | B1 |
| DAG: steal citrate-explorer design | B3 |
| DAG: side-drawer chatbot on every finding (app-wide pattern) | B3 (reusable shell component, DAG first) |
| Chat: clearer stream formatting | B2 |
| Chat: drag-and-drop standardized file-type list | B2 |
| Chat: interact with image-gen / other loaded models | B2 |
| Chat: extend harness without breaking security threshold | B2 (bounded by approval gate + relay allow-list) + C2 regression bed |
| Models: prioritize rebuild, no hiding; pinning/contribution crucial | Deferred → NATIVE-R1-S3 (priority RAISED; old stub-WP dropped) + D3 honesty line |
| Compute: cards don't fit machine info | B4 |
| Compute: advanced super-user settings | B4 |
| Compute: seed/propagate Gemma models for initial distribution | Deferred → S3 + distribution plan (gated on PIN) |
| Storage: user must FEEL and BE guaranteed encryption at rest | A3 + D3; tab rebuild itself → S3 |
| Learn: rethink w/ parents'/teachers' accounts + missions | Deferred → SPINE-S1 (named carry-in) |
| Learn: federated-gateway droplet probably not up — verify | Deferred → SPINE-S1/federated planning (named carry-in a) |
| Learn: always-joinable seeded general pool | Deferred → SPINE-S1/federated planning (named carry-in b) |
| Learn: study nat repo + coop work for cohesion | Deferred → SPINE-S1/federated planning (named carry-in c) |
| Agent Center: "I don't even understand how to use it" | B6 |
| Settings: Identity + federation-app connections | B5 |
| Settings: 2-col rows (Node Control&#124;System Health, Environment&#124;Peer Connections) | B5 |
| Settings: own sidebar, follow other apps' settings formula | B5 |
| Settings: deeper Knowledge Graph settings | B5 |
| Settings: citrate-memories per-operator integration (global/private, granular sharing) | B5 surface + safe defaults; full integration deferred → S3/new planset (named) |
